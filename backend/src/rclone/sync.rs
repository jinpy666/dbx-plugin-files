//! rc-backed directory sync/copy jobs (F-RCLONE phase C, Agent F1).
//!
//! Contract: docs/IMPL_PLAN_RCLONE.zh-CN.md §6 and the F1 task brief. The
//! public API below is frozen — Agent F2's main.rs wiring depends on it.
//!
//! Built on live-verified rclone v1.75.1 behavior (do not re-assume):
//!
//! - The sub-path rides INSIDE the fs string (`srcFs = "<fs>/<rel>"`,
//!   named remotes `dbxName:root/rel`); `sync/copy` has no separate
//!   relative-path parameter. Copying `fs/sub` lands sub's CONTENTS at the
//!   destination root — tests pin this with a sentinel sibling so a wrong
//!   composition (whole-bucket copy) fails loudly.
//! - rc option keys are snake_case (`dry_run`, `max_delete`). The camelCase
//!   CLI spellings (`dryRun`, `maxDelete`) are SILENTLY IGNORED by rc
//!   parameter reshaping — a "dry run" would write to disk.
//! - `max_delete` via rc: omitted = unlimited, `0` = refuse every deletion
//!   (files kept, job ends `success:false` "failed to delete N files"),
//!   `N` = allow exactly N deletions before the job fails.
//! - Every rc call is itself a job: jobids jump (never assume 1) and
//!   finished records expire (~60s) — afterwards `job/status` answers HTTP
//!   500 `{"error":"job not found"}`, the `None` of [`query_status`].
//! - Progress source `core/stats {group}`: `bytes` (transferred so far,
//!   includes server-side copy bytes), `totalBytes`, `speed` (B/s) and
//!   `totalTransfers` (final file count). An unknown group answers zeroed
//!   stats — that is never completion; `job/status` is the only truth.
//! - `job/stop {jobid}` → `{}`; the stopped job turns up `finished:true,
//!   success:false` with a transport-specific error string. Stopping an
//!   expired/unknown jobid is a `job not found` error = no-op here.
//! - `core/stats-reset {group}` clears the group counters after the
//!   terminal event (§6 leak prevention).

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde_json::Value;

use super::rc::{RcClient, RcError};
use crate::transfers::Throttle;

/// Per-job poll cadence: `core/stats {group}` + `job/status {jobid}`.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Consecutive `job/status` failures tolerated before the job is declared
/// Failed (loopback rc; a real hiccup clears within one tick).
const MAX_POLL_ERRORS: u32 = 3;

/// Terminal-event wait ceiling used by the inline tests.
#[cfg(test)]
const TERMINAL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SyncKind {
    /// `sync/copy`: copy src into dst, never delete dst extras.
    Copy,
    /// `sync/sync`: mirror — delete dst files missing from src.
    Sync,
    /// `sync/move`: copy src into dst, then delete the source tree —
    /// the server-side move (files/rename on a directory).
    Move,
}

#[derive(Debug, Clone)]
pub struct SyncJobParams {
    pub task_id: String,
    pub kind: SyncKind,
    /// Full fs string (the wiring layer composes `call_fs` + root).
    pub src_fs: String,
    pub dst_fs: String,
    /// Policy-resolved relative path, no leading/trailing `/`.
    pub src_rel: String,
    pub dst_rel: String,
    pub dry_run: bool,
    /// `Some(0)` refuses all deletions; `Some(n)` caps them at n (job
    /// fails once exceeded); `None` = unlimited (rc default).
    pub max_delete: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum SyncEvent {
    Progress { transferred: u64, total: u64, rate: Option<f64> },
    Completed { bytes: u64, files: u64 },
    Failed { message: String },
    Canceled,
}

#[derive(Debug, Clone)]
pub struct SyncJobHandle {
    pub jobid: u64,
    pub group: String,
}

/// Job groups+ids marked stopped by [`stop_job`] and not yet consumed by
/// their poll task — a cancel wins over any remote terminal state observed
/// afterwards. Keyed by `(group, jobid)` because jobids are only unique per
/// rcd instance: parallel engines (tests spawn several) all start counting
/// at 1. Entries are removed by the poll task; a marker for an
/// already-terminal job can linger until [`start_job`] clears it, which
/// bounds it to canceled jobs per rcd lifetime.
fn canceled_jobs() -> &'static Mutex<HashSet<(String, u64)>> {
    static CANCELED: LazyLock<Mutex<HashSet<(String, u64)>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));
    &CANCELED
}

fn take_canceled(group: &str, jobid: u64) -> bool {
    canceled_jobs()
        .lock()
        .map(|mut jobs| jobs.remove(&(group.to_string(), jobid)))
        .unwrap_or(false)
}

/// `fs` + relative path composition (live-verified): the sub-path is part
/// of the fs string. A bare `remote:` must not gain a leading slash (rc
/// reads `remote:/abs` as an absolute remote path — named-local remotes
/// would list their CWD, see plan semantics #7); everything else joins
/// with `/`.
fn compose_fs(fs: &str, rel: &str) -> String {
    let rel = rel.trim_matches('/');
    if rel.is_empty() {
        return fs.to_string();
    }
    if fs.ends_with(':') {
        format!("{fs}{rel}")
    } else {
        format!("{}/{rel}", fs.trim_end_matches('/'))
    }
}

/// Starts an rc `_async` sync job and spawns its 500ms poll task
/// (`core/stats{group}` + `job/status`). Events reach `on_event`, throttled
/// for Progress via [`Throttle`] (200ms / 1%). The module owns the callback
/// until the terminal event; afterwards the poll task exits — no leak, and
/// the callback never fires again.
pub async fn start_job(
    client: RcClient,
    params: SyncJobParams,
    on_event: Box<dyn FnMut(SyncEvent) + Send>,
) -> Result<SyncJobHandle, String> {
    let method = match params.kind {
        SyncKind::Copy => "sync/copy",
        SyncKind::Sync => "sync/sync",
        SyncKind::Move => "sync/move",
    };
    let mut body = serde_json::json!({
        "srcFs": compose_fs(&params.src_fs, &params.src_rel),
        "dstFs": compose_fs(&params.dst_fs, &params.dst_rel),
        "_async": true,
        "_group": params.task_id,
    });
    if params.dry_run {
        // snake_case or nothing: camelCase `dryRun` is silently ignored.
        body["dry_run"] = Value::Bool(true);
    }
    if let Some(max_delete) = params.max_delete {
        body["max_delete"] = Value::from(max_delete);
    }
    let response = client
        .call(method, &body)
        .await
        .map_err(|error| error.to_string())?;
    let jobid = response
        .get("jobid")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("rc {method} did not return a jobid: {response}"))?;
    // Jobids only collide with a stale cancel marker after an rcd restart;
    // clear it so a new job is not born pre-canceled.
    if let Ok(mut jobs) = canceled_jobs().lock() {
        jobs.remove(&(params.task_id.clone(), jobid));
    }
    let handle = SyncJobHandle { jobid, group: params.task_id };
    let callback = Arc::new(Mutex::new(on_event));
    tokio::spawn(poll_job(client, handle.clone(), callback));
    Ok(handle)
}

/// Stops the remote job (`job/stop`). The poll task reports [`SyncEvent::
/// Canceled`] on its next tick regardless of what rclone did in between.
/// Stopping an already-finished or expired job is a no-op success.
pub async fn stop_job(client: &RcClient, handle: &SyncJobHandle) -> Result<(), String> {
    // Mark before the remote stop: the poll loop then cannot race a terminal
    // event past the cancel.
    if let Ok(mut jobs) = canceled_jobs().lock() {
        jobs.insert((handle.group.clone(), handle.jobid));
    }
    match client
        .call("job/stop", &serde_json::json!({ "jobid": handle.jobid }))
        .await
    {
        Ok(_) => Ok(()),
        Err(error) if is_job_not_found(&error) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

/// One-shot status peek: `Some(payload)` while the job record exists
/// (`finished` true/false), `None` once it is gone (expired after ~60s or
/// never existed). Transport errors surface as `Err`.
pub async fn query_status(
    client: &RcClient,
    handle: &SyncJobHandle,
) -> Result<Option<Value>, String> {
    match client
        .call("job/status", &serde_json::json!({ "jobid": handle.jobid }))
        .await
    {
        Ok(status) => Ok(Some(status)),
        Err(error) if is_job_not_found(&error) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// Poll loop: stats → throttled Progress, status → terminal event. Cancel
/// is checked right after every interval sleep, so a stop that lands
/// mid-tick always yields `Canceled` before anything else.
async fn poll_job(
    client: RcClient,
    handle: SyncJobHandle,
    callback: Arc<Mutex<Box<dyn FnMut(SyncEvent) + Send>>>,
) {
    let emit = |event: SyncEvent| {
        if let Ok(mut handler) = callback.lock() {
            handler(event);
        }
    };
    let mut throttle = Throttle::default();
    let mut poll_errors: u32 = 0;
    loop {
        // Fresh job: give it its first interval before the first poll.
        tokio::time::sleep(POLL_INTERVAL).await;
        if take_canceled(&handle.group, handle.jobid) {
            let _ = reset_group_stats(&client, &handle).await;
            emit(SyncEvent::Canceled);
            return;
        }
        // Progress snapshot; kept as the completion-totals candidate.
        let mut final_totals: Option<(u64, u64)> = None;
        if let Ok(stats) = client
            .call("core/stats", &serde_json::json!({ "group": handle.group }))
            .await
        {
            let transferred = stats.get("bytes").and_then(Value::as_u64).unwrap_or(0);
            let total = stats.get("totalBytes").and_then(Value::as_u64).unwrap_or(0);
            let rate = stats.get("speed").and_then(Value::as_f64);
            if throttle.should_emit(transferred, Some(total)) {
                emit(SyncEvent::Progress { transferred, total, rate });
            }
            let files = stats
                .get("totalTransfers")
                .and_then(Value::as_u64)
                .or_else(|| stats.get("transfers").and_then(Value::as_u64))
                .unwrap_or(0);
            final_totals = Some((transferred, files));
        }
        match client
            .call("job/status", &serde_json::json!({ "jobid": handle.jobid }))
            .await
        {
            Ok(status) => {
                poll_errors = 0;
                if !status.get("finished").and_then(Value::as_bool).unwrap_or(false) {
                    continue;
                }
                // Terminal: clear the group counters (§6) before reporting.
                let _ = reset_group_stats(&client, &handle).await;
                let message = status
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if !message.is_empty() {
                    emit(SyncEvent::Failed {
                        message: message.to_string(),
                    });
                } else {
                    let (bytes, files) = match final_totals {
                        Some((bytes, files)) if bytes > 0 || files > 0 => (bytes, files),
                        _ => transferred_totals(&client, &handle).await,
                    };
                    emit(SyncEvent::Completed { bytes, files });
                }
                return;
            }
            Err(error) if is_job_not_found(&error) => {
                // We polled this jobid since submission, so a vanished
                // record means rcd was restarted (fresh port/credential) —
                // the job is gone, not succeeded.
                let _ = reset_group_stats(&client, &handle).await;
                emit(SyncEvent::Failed {
                    message: format!(
                        "job {} no longer exists on rclone (rcd restarted?)",
                        handle.jobid
                    ),
                });
                return;
            }
            Err(error) => {
                poll_errors += 1;
                if poll_errors >= MAX_POLL_ERRORS {
                    emit(SyncEvent::Failed {
                        message: format!("lost track of job {}: {error}", handle.jobid),
                    });
                    return;
                }
            }
        }
    }
}

/// Completion-totals fallback when the group stats were unreadable:
/// per-file records from `core/transferred {group}`. Their `bytes` run 0
/// for server-side copies, so this under-reports — stats stay preferred.
async fn transferred_totals(client: &RcClient, handle: &SyncJobHandle) -> (u64, u64) {
    match client
        .call("core/transferred", &serde_json::json!({ "group": handle.group }))
        .await
    {
        Ok(value) => match value.get("transferred").and_then(Value::as_array) {
            Some(entries) => (
                entries
                    .iter()
                    .filter_map(|entry| entry.get("bytes").and_then(Value::as_u64))
                    .sum(),
                entries
                    .iter()
                    .filter(|entry| {
                        entry.get("error").and_then(Value::as_str).unwrap_or("").is_empty()
                    })
                    .count() as u64,
            ),
            None => (0, 0),
        },
        Err(_) => (0, 0),
    }
}

/// Clears the job group's counters (§6 leak prevention). Best effort.
async fn reset_group_stats(client: &RcClient, handle: &SyncJobHandle) -> Result<(), RcError> {
    client
        .call("core/stats-reset", &serde_json::json!({ "group": handle.group }))
        .await
        .map(|_| ())
}

/// `job not found` recognition: rcd answers HTTP 500 with a JSON error body
/// for expired/unknown jobids; both that and a (theoretical) 200-with-error
/// envelope map onto the same condition.
fn is_job_not_found(error: &RcError) -> bool {
    let body = match error {
        RcError::Http { body, .. } => body.as_str(),
        RcError::Rclone { message } => message.as_str(),
        _ => return false,
    };
    let parsed = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.get("error").and_then(Value::as_str).map(str::to_string));
    match parsed {
        Some(message) => message.contains("job not found"),
        None => body.contains("job not found"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::rclone::proc::{RcdHandle, resolve_binary};
    use tokio::sync::mpsc::{Receiver, channel};

    fn write_file(path: &std::path::Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir -p");
        }
        std::fs::write(path, content).expect("write");
    }

    fn dir_names(path: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(path)
            .expect("read_dir")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Drains progress events until the terminal one. Async receive so the
    /// single-threaded `#[tokio::test]` runtime can still drive the spawned
    /// poll task while we wait (std blocking recv would starve it).
    async fn wait_terminal(rx: &mut Receiver<SyncEvent>) -> (Vec<SyncEvent>, SyncEvent) {
        let deadline = Instant::now() + TERMINAL_TIMEOUT;
        let mut progress = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for a terminal sync event");
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(event @ (SyncEvent::Completed { .. }
                | SyncEvent::Failed { .. }
                | SyncEvent::Canceled))) => return (progress, event),
                Ok(Some(event @ SyncEvent::Progress { .. })) => progress.push(event),
                Ok(None) => panic!("event channel closed before a terminal sync event"),
                Err(_) => panic!("timed out waiting for a terminal sync event"),
            }
        }
    }

    fn event_channel() -> (
        Box<dyn FnMut(SyncEvent) + Send>,
        Receiver<SyncEvent>,
    ) {
        let (tx, rx) = channel(64);
        let on_event = Box::new(move |event: SyncEvent| {
            let _ = tx.try_send(event);
        });
        (on_event, rx)
    }

    /// Standard sandbox: src/outside.txt (sentinel proving subtree-only
    /// copying), src/sub/{a.txt, deep/b.txt}, optional stale files in dst.
    fn sandbox() -> (tempfile::TempDir, tempfile::TempDir) {
        let src = tempfile::tempdir().expect("src tempdir");
        let dst = tempfile::tempdir().expect("dst tempdir");
        write_file(&src.path().join("outside.txt"), "must not travel");
        write_file(&src.path().join("sub").join("a.txt"), "sentinel-a");
        write_file(&src.path().join("sub").join("deep").join("b.txt"), "sentinel-b");
        (src, dst)
    }

    fn params(kind: SyncKind, src: &std::path::Path, dst: &std::path::Path) -> SyncJobParams {
        SyncJobParams {
            task_id: format!("test-{kind:?}-{}", uuid::Uuid::new_v4().simple()),
            kind,
            src_fs: src.to_string_lossy().into_owned(),
            dst_fs: dst.to_string_lossy().into_owned(),
            src_rel: "sub".to_string(),
            dst_rel: String::new(),
            dry_run: false,
            max_delete: None,
        }
    }

    #[test]
    fn compose_fs_joins_fs_and_relative_path() {
        // Local absolute fs: plain `/` join.
        assert_eq!(compose_fs("/tmp/data", "sub"), "/tmp/data/sub");
        assert_eq!(compose_fs("/tmp/data/", "/sub/"), "/tmp/data/sub");
        // Named remote with root inside the fs string (call_fs form).
        assert_eq!(compose_fs("dbxAb12:srv/data", "sub"), "dbxAb12:srv/data/sub");
        // Bare `remote:` must not grow a leading slash.
        assert_eq!(compose_fs("dbxAb12:", "sub"), "dbxAb12:sub");
        // Empty rel keeps the fs untouched.
        assert_eq!(compose_fs("/tmp/data", ""), "/tmp/data");
    }

    #[tokio::test]
    async fn copy_transfers_only_the_relative_subtree() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let (src, dst) = sandbox();
        let (on_event, mut rx) = event_channel();
        let handle = start_job(
            rcd.client(),
            params(SyncKind::Copy, src.path(), dst.path()),
            on_event,
        )
        .await
        .expect("start copy job");

        // The record exists right away (running or finished).
        let status = query_status(&rcd.client(), &handle).await.expect("query").expect("some");
        assert!(status.get("id").and_then(Value::as_u64) == Some(handle.jobid));

        let (_progress, event) = wait_terminal(&mut rx).await;
        let (bytes, files) = match event {
            SyncEvent::Completed { bytes, files } => (bytes, files),
            other => panic!("expected Completed, got {other:?}"),
        };
        assert_eq!(files, 2, "two files transferred");
        assert!(bytes > 0, "sentinel bytes counted, got {bytes}");
        assert_eq!(dir_names(dst.path()), vec!["a.txt", "deep"]);
        assert!(dst.path().join("deep").join("b.txt").is_file());
        // Wrong-composition guards: neither the whole bucket nor its
        // sentinel sibling may arrive.
        assert!(!dst.path().join("outside.txt").exists());
        assert!(!dst.path().join("sub").exists());
    }

    #[tokio::test]
    async fn sync_mirror_deletes_extraneous_dst_files() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let (src, dst) = sandbox();
        write_file(&dst.path().join("stale.txt"), "to be mirrored away");
        let (on_event, mut rx) = event_channel();
        let _handle = start_job(
            rcd.client(),
            params(SyncKind::Sync, src.path(), dst.path()),
            on_event,
        )
        .await
        .expect("start sync job");
        let (_progress, event) = wait_terminal(&mut rx).await;
        assert!(
            matches!(event, SyncEvent::Completed { files: 2, .. } if true),
            "expected Completed{{files:2}}, got {event:?}"
        );
        assert_eq!(dir_names(dst.path()), vec!["a.txt", "deep"]);
        assert!(!dst.path().join("stale.txt").exists());
    }

    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let (src, dst) = sandbox();
        let mut job = params(SyncKind::Sync, src.path(), dst.path());
        job.dry_run = true;
        let (on_event, mut rx) = event_channel();
        let _handle = start_job(rcd.client(), job, on_event).await.expect("start dry-run");
        let (_progress, event) = wait_terminal(&mut rx).await;
        assert!(
            matches!(event, SyncEvent::Completed { .. }),
            "expected Completed, got {event:?}"
        );
        // snake_case `dry_run` honored: nothing landed, stale dst untouched.
        assert!(dir_names(dst.path()).is_empty(), "dry run must not write");
    }

    #[tokio::test]
    async fn max_delete_cap_fails_the_job() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let (src, dst) = sandbox();
        write_file(&dst.path().join("stale1.txt"), "s1");
        write_file(&dst.path().join("stale2.txt"), "s2");
        let mut job = params(SyncKind::Sync, src.path(), dst.path());
        job.max_delete = Some(1);
        let (on_event, mut rx) = event_channel();
        let _handle = start_job(rcd.client(), job, on_event).await.expect("start capped sync");
        let (_progress, event) = wait_terminal(&mut rx).await;
        let SyncEvent::Failed { message } = event else {
            panic!("expected Failed for exceeded max_delete, got {event:?}");
        };
        assert!(message.contains("delete"), "rc error text: {message}");
        // Exactly one deletion went through before the cap tripped.
        let remaining = dir_names(dst.path());
        assert_eq!(remaining.len(), 3, "one stale file must survive: {remaining:?}");
    }

    #[tokio::test]
    async fn max_delete_zero_refuses_all_deletions() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let (src, dst) = sandbox();
        write_file(&dst.path().join("stale.txt"), "keep me");
        let mut job = params(SyncKind::Sync, src.path(), dst.path());
        job.max_delete = Some(0);
        let (on_event, mut rx) = event_channel();
        let _handle = start_job(rcd.client(), job, on_event).await.expect("start no-delete sync");
        let (_progress, event) = wait_terminal(&mut rx).await;
        assert!(
            matches!(event, SyncEvent::Failed { .. }),
            "expected Failed for refused deletions, got {event:?}"
        );
        // The deletion was refused: file still there.
        assert!(dst.path().join("stale.txt").is_file());
    }

    #[tokio::test]
    async fn stop_job_emits_canceled() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        // Enough files that the job plausibly outlives the stop call.
        let src = tempfile::tempdir().expect("src tempdir");
        let dst = tempfile::tempdir().expect("dst tempdir");
        for index in 0..800 {
            write_file(
                &src.path().join(format!("f{index:04}.txt")),
                &format!("payload-{index}"),
            );
        }
        let mut job = params(SyncKind::Copy, src.path(), dst.path());
        job.src_rel = String::new();
        let (on_event, mut rx) = event_channel();
        let handle = start_job(rcd.client(), job, on_event).await.expect("start copy");
        stop_job(&rcd.client(), &handle).await.expect("stop job");
        let (_progress, event) = wait_terminal(&mut rx).await;
        assert!(matches!(event, SyncEvent::Canceled), "expected Canceled, got {event:?}");
        // stop_job on the expired/finished record stays a success no-op.
        stop_job(&rcd.client(), &handle).await.expect("stop again");
    }

    #[tokio::test]
    async fn query_status_tracks_lifecycle_and_expiry() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let (src, dst) = sandbox();
        let (on_event, mut rx) = event_channel();
        let handle = start_job(
            rcd.client(),
            params(SyncKind::Copy, src.path(), dst.path()),
            on_event,
        )
        .await
        .expect("start copy");
        let _ = wait_terminal(&mut rx).await;

        // Finished records stay queryable for ~60s after completion.
        let status = query_status(&rcd.client(), &handle).await.expect("query");
        let status = status.expect("finished record still present");
        assert_eq!(status.get("finished").and_then(Value::as_bool), Some(true));
        assert_eq!(status.get("success").and_then(Value::as_bool), Some(true));
        assert_eq!(status.get("group").and_then(Value::as_str), Some(handle.group.as_str()));

        // Unknown/expired jobid → None, not Err.
        let ghost = SyncJobHandle { jobid: i64::MAX as u64, group: "ghost".to_string() };
        assert_eq!(query_status(&rcd.client(), &ghost).await.expect("query ghost"), None);
    }
}
