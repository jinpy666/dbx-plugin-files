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
//!   parameter reshaping — a "dry run" would write to disk. Live-verified
//!   v1.75.1 for the batch-6 filters too: `minSize` is ignored while
//!   `min_size` filters.
//! - Unlike `transfers`/`checkers`/`retries`, the size/age filters are NOT
//!   silently tolerated when malformed: rc rejects the REQUEST itself with
//!   HTTP 500 (`couldn't parse config item "min_size" = "xyz" as
//!   fs.SizeSuffix`, `"min_age" = "nonsense" as fs.Duration`) before any job
//!   starts — `_async` included. [`start_job`] therefore surfaces those as a
//!   plain error and no jobid is ever minted.
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
    /// `operations/check`: compare src against dst without writing anything.
    /// The terminal event is [`SyncEvent::CheckFinished`] with rclone's
    /// report (missingOnSrc/missingOnDst/differ/error lists); differences
    /// are data, not a job failure.
    Check,
    /// `sync/bisync`: bidirectional sync (rclone beta). Both paths are
    /// written and deleted; the first ever run of a pair must be a resync.
    /// Terminal event: [`SyncEvent::BisyncFinished`] with rclone's session
    /// report (session name, workdir, log tail).
    Bisync,
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
    /// `--include` glob patterns (rc snake_case array); empty/`None` = no filter.
    pub include: Option<Vec<String>>,
    /// `--exclude` glob patterns (rc snake_case array); empty/`None` = no filter.
    pub exclude: Option<Vec<String>>,
    /// `--backup-dir`, relative to the DESTINATION CONNECTION ROOT (not the
    /// synced subtree): overwritten (copy/sync) and deleted (sync) files are
    /// moved here preserving hierarchy. Root-relative lets the UI point it
    /// outside the synced path — inside would let a later mirror sync purge
    /// the backups it made.
    pub backup_dir_rel: Option<String>,
    /// `--suffix` appended to backed-up file names (pair with backup_dir to
    /// keep the originals distinguishable).
    pub suffix: Option<String>,
    /// `--metadata`: preserve/copy object metadata (mode, times, extended
    /// attributes — backend-dependent). Copy/Sync/Move only; live-verified
    /// v1.75.1 that `operations/check` also accepts the flag, but filtering
    /// or metadata-flagging a comparison silently narrows the report, so
    /// check/bisync never carry it.
    pub metadata: bool,
    /// `--min-size`: files smaller than this (e.g. `"100k"`) are filtered
    /// out. rc validates the value itself — a malformed string is rejected
    /// with HTTP 500 before the job starts (NOT silently ignored).
    pub min_size: Option<String>,
    /// `--max-size`: files larger than this (e.g. `"1M"`) are filtered out.
    /// Same rc-side validation as [`SyncJobParams::min_size`].
    pub max_size: Option<String>,
    /// `--min-age`: only files modified before this age/date (`"1d"`,
    /// `"2024-01-01"`) travel. rc validates as `fs.Duration`; a far-future
    /// value (live-verified `99999d`) is legal and skips EVERYTHING with a
    /// successful zero-transfer job — that is how the tests pin the filter.
    pub min_age: Option<String>,
    /// `--max-age`: only files modified within this age/date (`"1h"`,
    /// `"2024-01-01"`) travel. Same rc-side validation as
    /// [`SyncJobParams::min_age`].
    pub max_age: Option<String>,
    /// Per-job overrides of rclone's global concurrency/retry flags
    /// (`--transfers` / `--checkers` / `--retries`). rc parameter reshaping
    /// silently ignores values it cannot parse, so the request layer keeps
    /// the wire permissive while the UI clamps sane ranges.
    pub transfers: Option<u32>,
    pub checkers: Option<u32>,
    pub retries: Option<u32>,
    /// Check-only: one-way comparison (src → dst), skipping the reverse
    /// missing-on-src scan. rc command param keeps the documented camelCase.
    pub check_one_way: bool,
    /// Check-only: compare by downloading instead of trusting stored hashes
    /// (for backends whose hashes are unreliable or absent).
    pub check_download: bool,
    /// Check-only SUM verification (batch 7): connection-root-relative path
    /// of the checksum file (`files/hashsum` output). When set, the rc body
    /// switches to rclone's checksum-file mode (`checkFile*` params) and
    /// `srcFs` stays ABSENT — see the body construction in [`start_job`].
    pub sum_remote: Option<String>,
    /// Check-only SUM verification: rclone hash type recorded in the
    /// checksum file (md5/sha1/sha256/sha512/crc32).
    pub sum_hash: Option<String>,
    /// Bisync-only: persistent state directory. rclone's default (its own
    /// cache dir) outlives our temp config but is shared and unmanaged —
    /// the wiring layer pins it under the plugin data dir instead.
    pub bisync_workdir: Option<String>,
    /// Bisync-only: first run of a pair (initializes listings; both sides
    /// converge to the newer/asked-for side per `bisync_resync_mode`).
    pub bisync_resync: bool,
    /// Bisync-only resync conflict policy (`newer`/`older`/`larger`/…);
    /// only sent alongside `bisync_resync`.
    pub bisync_resync_mode: Option<String>,
}

#[derive(Debug, Clone)]
pub enum SyncEvent {
    Progress { transferred: u64, total: u64, rate: Option<f64> },
    Completed { bytes: u64, files: u64 },
    /// Check jobs only: rclone's comparison report (`output` of the finished
    /// job). Fired instead of `Completed`; differences are not failures.
    CheckFinished { report: Value },
    /// Bisync jobs only: rclone's session report (`output` of the finished
    /// job — session name, workdir, log tail). `success` mirrors the job's
    /// own flag: false means bisync aborted (caller extracts the ERROR
    /// lines from the report log for a readable message).
    BisyncFinished { report: Value, success: bool },
    Failed { message: String },
    Canceled,
}

#[derive(Debug, Clone)]
pub struct SyncJobHandle {
    pub jobid: u64,
    pub group: String,
    pub kind: SyncKind,
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
pub(crate) fn compose_fs(fs: &str, rel: &str) -> String {
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

/// `Some(patterns)` only when the list carries at least one non-blank entry
/// (blank patterns would silently filter everything out server-side).
fn non_empty(patterns: &Option<Vec<String>>) -> Option<Vec<String>> {
    patterns
        .as_ref()
        .map(|list| {
            list.iter()
                .map(|pattern| pattern.trim().to_string())
                .filter(|pattern| !pattern.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|list| !list.is_empty())
}

/// Whether two connection-root-relative paths overlap (equal or nested in
/// either direction). Empty = the connection root, which overlaps everything.
fn paths_overlap(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim_matches('/'), b.trim_matches('/'));
    if a.is_empty() || b.is_empty() {
        return true;
    }
    let nested = |outer: &str, inner: &str| {
        inner == outer || inner.strip_prefix(outer).is_some_and(|rest| rest.starts_with('/'))
    };
    nested(a, b) || nested(b, a)
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
        SyncKind::Check => "operations/check",
        SyncKind::Bisync => "sync/bisync",
    };
    // SUM mode (batch 7): verify the directory in `dst_rel` against an
    // existing checksum file instead of a live source tree. Live-pinned
    // v1.75.1 quirks: `srcFs` must be ABSENT (sending it trips the misleading
    // upstream 400 "only supply dstFs when using checkFileHash" — the real
    // requirement is the opposite) and the three checkFile* params are only
    // accepted together ("need all of checkFileFs, ..."). The checksum file
    // is addressed relative to the connection root, so `checkFileFs` is the
    // bare root fs while `dst_rel` keeps the verified directory inside the
    // same fs string. oneWay/download/filters never ride along — they would
    // quietly narrow the report.
    let sum_mode = params.kind == SyncKind::Check && params.sum_remote.is_some();
    let mut body = if sum_mode {
        serde_json::json!({
            "dstFs": compose_fs(&params.dst_fs, &params.dst_rel),
            "checkFileFs": compose_fs(&params.dst_fs, ""),
            "checkFileRemote": params.sum_remote.clone().unwrap_or_default(),
            "checkFileHash": params.sum_hash.clone().unwrap_or_default(),
            "_async": true,
            "_group": params.task_id,
        })
    } else {
        serde_json::json!({
            "srcFs": compose_fs(&params.src_fs, &params.src_rel),
            "dstFs": compose_fs(&params.dst_fs, &params.dst_rel),
            "_async": true,
            "_group": params.task_id,
        })
    };
    if params.dry_run && !sum_mode {
        // snake_case or nothing: camelCase `dryRun` is silently ignored.
        body["dry_run"] = Value::Bool(true);
    }
    if let Some(max_delete) = params.max_delete {
        body["max_delete"] = Value::from(max_delete);
    }
    if let Some(include) = non_empty(&params.include) {
        body["include"] = serde_json::json!(include);
    }
    if let Some(exclude) = non_empty(&params.exclude) {
        body["exclude"] = serde_json::json!(exclude);
    }
    if let Some(backup) = params
        .backup_dir_rel
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        // rclone refuses an overlapping --backup-dir ("destination and
        // parameter to --backup-dir mustn't overlap", live-verified
        // v1.75.1) — and an inside-the-mirror backup would be purged by the
        // next sync anyway. Reject early with a readable message instead of
        // surfacing rclone's job error after the fact.
        let backup_rel = backup.trim_matches('/');
        if paths_overlap(backup_rel, params.dst_rel.trim_matches('/')) {
            return Err(format!(
                "backup dir '{}' must live outside the synced destination '{}'",
                backup, params.dst_rel
            ));
        }
        // Composed onto the destination connection root (see the field doc):
        // the fs part of dst_fs carries the root, only the rel varies.
        body["backup_dir"] = Value::String(compose_fs(&params.dst_fs, backup_rel));
    }
    if let Some(suffix) = params
        .suffix
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        body["suffix"] = Value::String(suffix.to_string());
    }
    // Batch-6 condition filters: sync/copy/move only. operations/check
    // accepts the same snake_case params (live-verified v1.75.1) but a
    // filtered/metadata-flagged comparison quietly narrows the difference
    // report — a misleading "OK" — so check and bisync never receive them.
    if matches!(params.kind, SyncKind::Copy | SyncKind::Sync | SyncKind::Move) {
        if params.metadata {
            body["metadata"] = Value::Bool(true);
        }
        if let Some(min_size) = params
            .min_size
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            body["min_size"] = Value::String(min_size.to_string());
        }
        if let Some(max_size) = params
            .max_size
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            body["max_size"] = Value::String(max_size.to_string());
        }
        if let Some(min_age) = params
            .min_age
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            body["min_age"] = Value::String(min_age.to_string());
        }
        if let Some(max_age) = params
            .max_age
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            body["max_age"] = Value::String(max_age.to_string());
        }
    }
    if let Some(transfers) = params.transfers {
        body["transfers"] = Value::from(transfers);
    }
    if let Some(checkers) = params.checkers {
        body["checkers"] = Value::from(checkers);
    }
    if let Some(retries) = params.retries {
        body["retries"] = Value::from(retries);
    }
    if params.kind == SyncKind::Check && !sum_mode {
        if params.check_one_way {
            body["oneWay"] = Value::Bool(true);
        }
        if params.check_download {
            body["download"] = Value::Bool(true);
        }
    }
    if params.kind == SyncKind::Bisync {
        // Bisync names its sides path1/path2 (full fs strings, same shape
        // the sync family composes). Params probed live against v1.75.1:
        // `workdir`, `resync`, `resyncMode`, `dry_run` all honored.
        let path1 = body["srcFs"].take();
        let path2 = body["dstFs"].take();
        body["path1"] = path1;
        body["path2"] = path2;
        if let Some(workdir) = params
            .bisync_workdir
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            body["workdir"] = Value::String(workdir.to_string());
        }
        if params.bisync_resync {
            body["resync"] = Value::Bool(true);
            if let Some(mode) = params
                .bisync_resync_mode
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                body["resyncMode"] = Value::String(mode.to_string());
            }
        }
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
    let handle = SyncJobHandle { jobid, group: params.task_id, kind: params.kind };
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
                // Bisync jobs always answer with a session report — success
                // or abort — so hand the decision to the caller.
                if let Some(report) = status
                    .get("output")
                    .filter(|output| output.get("session").is_some())
                {
                    emit(SyncEvent::BisyncFinished {
                        report: report.clone(),
                        success: status.get("success").and_then(Value::as_bool).unwrap_or(false),
                    });
                    return;
                }
                // Check jobs carry their report in `output` (live-verified
                // v1.75.1: missingOnSrc/missingOnDst/differ/error arrays +
                // a human `status` line). Presence of the report beats the
                // zeroed counters fallback.
                if message.is_empty() {
                    if let Some(report) = status
                        .get("output")
                        .filter(|output| output.get("missingOnSrc").is_some())
                    {
                        emit(SyncEvent::CheckFinished {
                            report: report.clone(),
                        });
                        return;
                    }
                }
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
                // A FAILED bisync job can surface as a repeated rc error
                // response (HTTP 500 with an `error` body) instead of a
                // readable status record — treat that as the terminal abort
                // signal rather than losing track of the job.
                if handle.kind == SyncKind::Bisync && rc_error_body(&error).is_some() {
                    emit(SyncEvent::BisyncFinished {
                        report: Value::Object(serde_json::Map::new()),
                        success: false,
                    });
                    return;
                }
                poll_errors += 1;
                if poll_errors >= MAX_POLL_ERRORS {
                    // A failed job surfaces here as a repeated rc error body
                    // (not a readable status record) — the body IS the
                    // terminal reason, so surface it directly instead of the
                    // misleading "lost track of job" prefix.
                    let message = rc_error_body(&error).unwrap_or_else(|| {
                        format!("lost track of job {}: {error}", handle.jobid)
                    });
                    emit(SyncEvent::Failed { message });
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
/// Error-body text for structured rc failures (HTTP/JSON envelope), `None`
/// for transport-level hiccups.
fn rc_error_body(error: &RcError) -> Option<String> {
    match error {
        RcError::Http { body, .. } => Some(body.clone()),
        RcError::Rclone { message } => Some(message.clone()),
        _ => None,
    }
}

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
                | SyncEvent::CheckFinished { .. }
                | SyncEvent::BisyncFinished { .. }
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
            include: None,
            exclude: None,
            backup_dir_rel: None,
            suffix: None,
            metadata: false,
            min_size: None,
            max_size: None,
            min_age: None,
            max_age: None,
            transfers: None,
            checkers: None,
            retries: None,
            check_one_way: false,
            check_download: false,
            sum_remote: None,
            sum_hash: None,
            bisync_workdir: None,
            bisync_resync: false,
            bisync_resync_mode: None,
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

    #[test]
    fn paths_overlap_catches_equal_and_nested() {
        assert!(paths_overlap("_backups", "_backups"));
        assert!(paths_overlap("_backups/sub", "_backups"));
        assert!(paths_overlap("data", "data/2024"));
        assert!(paths_overlap("", "anything"));
        assert!(!paths_overlap("_backups", "sub"));
        assert!(!paths_overlap("_backups", "backups2"));
        // Cosmetic slashes do not change the answer.
        assert!(!paths_overlap("/_backups/", "/sub/"));
    }

    /// A backup dir inside the mirrored subtree is refused before any rc
    /// call (rclone itself rejects the overlap, live-verified v1.75.1).
    #[tokio::test]
    async fn backup_dir_inside_the_destination_is_refused() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let (src, dst) = sandbox();
        let mut job = params(SyncKind::Copy, src.path(), dst.path());
        job.backup_dir_rel = Some("deep/keep".to_string()); // inside `sub`
        let (on_event, _rx) = event_channel();
        let error = start_job(rcd.client(), job, on_event)
            .await
            .expect_err("overlapping backup dir must fail fast");
        assert!(error.contains("outside the synced destination"), "{error}");
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

    /// The include array is honored (rc snake_case filter param): only
    /// `*.txt` travels, the `.log` sentinel stays behind.
    #[tokio::test]
    async fn include_filter_keeps_only_matching_files() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let (src, dst) = sandbox();
        write_file(&src.path().join("sub").join("skip.log"), "never travels");
        let mut job = params(SyncKind::Copy, src.path(), dst.path());
        job.include = Some(vec!["*.txt".to_string()]);
        let (on_event, mut rx) = event_channel();
        let _handle = start_job(rcd.client(), job, on_event).await.expect("start filtered copy");
        let (_progress, event) = wait_terminal(&mut rx).await;
        assert!(matches!(event, SyncEvent::Completed { .. }), "got {event:?}");
        let names = dir_names(&dst.path());
        assert!(names.contains(&"a.txt".to_string()), "{names:?}");
        assert!(!names.contains(&"skip.log".to_string()), "include must skip .log: {names:?}");
    }

    /// The exclude array removes matching files even though include is unset.
    #[tokio::test]
    async fn exclude_filter_skips_matching_files() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let (src, dst) = sandbox();
        write_file(&src.path().join("sub").join("skip.log"), "never travels");
        let mut job = params(SyncKind::Copy, src.path(), dst.path());
        job.exclude = Some(vec!["*.log".to_string()]);
        let (on_event, mut rx) = event_channel();
        let _handle = start_job(rcd.client(), job, on_event).await.expect("start excluded copy");
        let (_progress, event) = wait_terminal(&mut rx).await;
        assert!(matches!(event, SyncEvent::Completed { .. }), "got {event:?}");
        let names = dir_names(&dst.path());
        assert!(names.contains(&"a.txt".to_string()), "{names:?}");
        assert!(!names.contains(&"skip.log".to_string()), "exclude must skip .log: {names:?}");
    }

    /// `backup_dir` (destination-root-relative) receives the overwritten
    /// destination file, preserving its name; the copy itself proceeds.
    #[tokio::test]
    async fn backup_dir_preserves_overwritten_dst_files() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let (src, dst) = sandbox();
        let mut job = params(SyncKind::Copy, src.path(), dst.path());
        // Mirror layout the overlap guard requires: the synced subtree lands
        // in `mirror/`, the backup dir sits at the connection root — OUTSIDE
        // the synced subtree.
        job.dst_rel = "mirror".to_string();
        job.backup_dir_rel = Some("_backups".to_string());
        job.suffix = Some(".bak".to_string());
        write_file(&dst.path().join("mirror").join("a.txt"), "old payload");
        let (on_event, mut rx) = event_channel();
        let _handle = start_job(rcd.client(), job, on_event).await.expect("start backed-up copy");
        let (_progress, event) = wait_terminal(&mut rx).await;
        assert!(matches!(event, SyncEvent::Completed { .. }), "got {event:?}");
        // New content landed...
        assert_eq!(
            std::fs::read_to_string(dst.path().join("mirror").join("a.txt")).expect("new a.txt"),
            "sentinel-a"
        );
        // ...and the overwritten original moved into the backup dir. Suffix
        // acceptance is rclone-version dependent, so accept either spelling.
        let backup_names = dir_names(&dst.path().join("_backups"));
        assert!(
            backup_names.iter().any(|name| name.starts_with("a.txt")),
            "overwritten file must be backed up: {backup_names:?}"
        );
    }

    /// Two-way check reports every difference class without touching disk:
    /// a.txt identical, b.txt missing on dst, c.txt missing on src, d.txt
    /// differs. The terminal event is CheckFinished (never Completed).
    #[tokio::test]
    async fn check_reports_differences_without_writing() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let src = tempfile::tempdir().expect("src tempdir");
        let dst = tempfile::tempdir().expect("dst tempdir");
        write_file(&src.path().join("a.txt"), "same");
        write_file(&src.path().join("b.txt"), "only on src");
        write_file(&src.path().join("d.txt"), "src version");
        write_file(&dst.path().join("a.txt"), "same");
        write_file(&dst.path().join("c.txt"), "only on dst");
        write_file(&dst.path().join("d.txt"), "dst version");
        let mut job = params(SyncKind::Check, src.path(), dst.path());
        job.src_rel = String::new();
        let (on_event, mut rx) = event_channel();
        let _handle = start_job(rcd.client(), job, on_event).await.expect("start check");
        let (_progress, event) = wait_terminal(&mut rx).await;
        let SyncEvent::CheckFinished { report } = event else {
            panic!("expected CheckFinished, got {event:?}");
        };
        let listed = |key: &str| -> Vec<String> {
            report
                .get(key)
                .and_then(Value::as_array)
                .map(|array| {
                    array
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        assert!(listed("missingOnSrc").iter().any(|path| path.contains("c.txt")));
        assert!(listed("missingOnDst").iter().any(|path| path.contains("b.txt")));
        assert!(listed("differ").iter().any(|path| path.contains("d.txt")));
        // Nothing moved: both trees unchanged.
        assert!(src.path().join("b.txt").is_file() && dst.path().join("c.txt").is_file());
        assert_eq!(std::fs::read_to_string(dst.path().join("d.txt")).unwrap(), "dst version");
    }

    /// Identical trees report empty difference lists; one-way skips the
    /// missing-on-src scan (live-verified shape: `oneWay` rc param).
    #[tokio::test]
    async fn check_one_way_skips_missing_on_src() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let src = tempfile::tempdir().expect("src tempdir");
        let dst = tempfile::tempdir().expect("dst tempdir");
        write_file(&src.path().join("a.txt"), "same");
        write_file(&dst.path().join("a.txt"), "same");
        write_file(&dst.path().join("extra.txt"), "dst-only, invisible one-way");
        let mut job = params(SyncKind::Check, src.path(), dst.path());
        job.src_rel = String::new();
        job.check_one_way = true;
        let (on_event, mut rx) = event_channel();
        let _handle = start_job(rcd.client(), job, on_event).await.expect("start one-way check");
        let (_progress, event) = wait_terminal(&mut rx).await;
        let SyncEvent::CheckFinished { report } = event else {
            panic!("expected CheckFinished, got {event:?}");
        };
        let empty = |key: &str| {
            report
                .get(key)
                .and_then(Value::as_array)
                .map(Vec::is_empty)
                .unwrap_or(false)
        };
        assert!(empty("missingOnSrc"), "one-way must not scan the source side");
        assert!(empty("missingOnDst") && empty("differ"), "identical overlap: {report}");
    }

    /// SUM verification (batch 7): `operations/check` in checksum-file mode
    /// compares a directory against a checksum file instead of a live source
    /// tree. Live-pinned v1.75.1 layout (probed before this batch): the
    /// checksum file lives NEXT TO the directory and its lines are relative
    /// to it — a clean tree verifies empty (success), a corrupted file lands
    /// in `differ` with success=false. End-to-end passing also pins the wire
    /// shape: with `srcFs` present rclone would reject the whole request.
    #[tokio::test]
    async fn check_sum_mode_verifies_directory_against_checksum_file() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let root = tempfile::tempdir().expect("root tempdir");
        write_file(&root.path().join("data").join("a.txt"), "alpha");
        write_file(&root.path().join("data").join("deep").join("b.txt"), "beta");
        let client = rcd.client();
        // Batch-2 generator run against the directory itself, so the lines
        // are relative to it ("a.txt", "deep/b.txt").
        let sum = client
            .operations_hashsum(
                &root.path().join("data").to_string_lossy(),
                "",
                "md5",
                false,
            )
            .await
            .expect("hashsum");
        let lines: Vec<String> = sum
            .get("hashsum")
            .and_then(Value::as_array)
            .map(|array| {
                array
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        assert!(!lines.is_empty(), "hashsum must list the sandbox files");
        // Written NEXT TO the directory (outside the verified tree), so the
        // verification cannot flag the checksum file itself as extra.
        std::fs::write(
            root.path().join("data.md5"),
            format!("{}\n", lines.join("\n")),
        )
        .expect("write checksum file");

        let listed = |report: &Value, key: &str| -> Vec<String> {
            report
                .get(key)
                .and_then(Value::as_array)
                .map(|array| {
                    array
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        let sum_job = |task: &str| {
            let mut job = params(SyncKind::Check, root.path(), root.path());
            job.task_id = task.to_string();
            job.src_rel = String::new(); // unused in SUM mode
            job.dst_rel = "data".to_string();
            job.sum_remote = Some("data.md5".to_string());
            job.sum_hash = Some("md5".to_string());
            job
        };

        // Clean tree: the report must be all-empty and successful.
        let (on_event, mut rx) = event_channel();
        let _handle = start_job(rcd.client(), sum_job("test-sum-clean"), on_event)
            .await
            .expect("start SUM check");
        let (_progress, event) = wait_terminal(&mut rx).await;
        let SyncEvent::CheckFinished { report } = event else {
            panic!("expected CheckFinished, got {event:?}");
        };
        assert_eq!(report.get("success").and_then(Value::as_bool), Some(true));
        assert!(
            listed(&report, "differ").is_empty()
                && listed(&report, "missingOnDst").is_empty()
                && listed(&report, "missingOnSrc").is_empty(),
            "clean tree must verify empty: {report}"
        );

        // Corrupt one file: the rerun must flag it in `differ` and fail.
        write_file(&root.path().join("data").join("a.txt"), "corrupted");
        let (on_event, mut rx) = event_channel();
        let _handle = start_job(rcd.client(), sum_job("test-sum-corrupt"), on_event)
            .await
            .expect("start SUM recheck");
        let (_progress, event) = wait_terminal(&mut rx).await;
        let SyncEvent::CheckFinished { report } = event else {
            panic!("expected CheckFinished, got {event:?}");
        };
        assert_eq!(report.get("success").and_then(Value::as_bool), Some(false));
        assert!(
            listed(&report, "differ").iter().any(|path| path.contains("a.txt")),
            "corrupted file must differ: {report}"
        );
    }

    /// `files/hashsum` 数量预检口径（follow-up to the batch-7 scope fix）：
    /// operations/size 只解析 fs、静默忽略 `remote`（live-pinned v1.75.1，
    /// 与 operations/hashsum 同一族）——目录并进 fs（compose_fs + 空
    /// remote）后 count 精确等于目录内容；旧口径（fs=连接根 + remote=
    /// 目录）数的是整棵根，大根下的小目录会被 HASHSUM_MAX_FILES 误拒
    /// （方向保守、不漏放，但确实是错的）。大根 + 小目录沙箱钉死两端：
    /// 目录级口径 count=2（预检放行、hashsum 只见 2 行），旧口径
    /// `remote` 被无视、count=整根（>cap，即被移除的误拒路径）。
    #[tokio::test]
    async fn operations_size_scopes_to_the_fs_remote_is_ignored() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let client = rcd.client();
        let root = tempfile::tempdir().expect("root tempdir");
        // 大根 + 小目录：根下 HASHSUM_MAX_FILES+10 个文件，目标目录 small
        // 只放 2 个 —— 旧预检会误拒的形状。
        for index in 0..(crate::HASHSUM_MAX_FILES + 10) {
            write_file(
                &root.path().join(format!("fill-{index:05}.txt")),
                "filler",
            );
        }
        write_file(&root.path().join("small").join("a.txt"), "alpha");
        write_file(&root.path().join("small").join("sub").join("b.txt"), "beta");
        let root_fs = root.path().to_string_lossy().into_owned();
        let count = |fs: String, remote: String| {
            let client = client.clone();
            async move {
                client
                    .operations_size(&fs, &remote)
                    .await
                    .expect("operations/size")
                    .get("count")
                    .and_then(Value::as_u64)
                    .expect("count field")
            }
        };

        // 修复后的预检口径（compose_fs(目录) + remote=""，与 hashsum 生成
        // 调用同形）：count 只数目标目录 —— 预检放行，hashsum 也只见 2 行。
        let dir_fs = compose_fs(&root_fs, "small");
        assert_eq!(
            count(dir_fs.clone(), String::new()).await,
            2,
            "directory-level scope must count only the target directory"
        );
        let sum = client
            .operations_hashsum(&dir_fs, "", "md5", false)
            .await
            .expect("hashsum");
        let lines = sum
            .get("hashsum")
            .and_then(Value::as_array)
            .expect("hashsum array");
        assert_eq!(lines.len(), 2, "hashsum sees exactly the directory: {lines:?}");

        // 旧口径钉死 live 行为：`remote` 被静默无视，数的是整棵连接根
        // —— HASHSUM_MAX_FILES+12 > cap，即本次修复移除的误拒路径。
        let whole_root = count(root_fs, "small".to_string()).await;
        assert_eq!(
            whole_root,
            crate::HASHSUM_MAX_FILES + 12,
            "root scope counts every file including the directory (remote ignored)"
        );
        assert!(whole_root > crate::HASHSUM_MAX_FILES);
    }

    /// Full bisync lifecycle against real rclone (live-verified v1.75.1):
    /// resync initializes the pair, edits on BOTH sides converge on the next
    /// run, and the state survives an rcd restart (workdir outlives the
    /// temp config). The aborted run without prior state reports failure.
    #[tokio::test]
    async fn bisync_resync_run_and_restart_survival() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let p1 = tempfile::tempdir().expect("p1");
        let p2 = tempfile::tempdir().expect("p2");
        let workdir = tempfile::tempdir().expect("workdir");
        write_file(&p1.path().join("a.txt"), "one");
        write_file(&p2.path().join("c.txt"), "three");
        let workdir_s = workdir.path().to_string_lossy().into_owned();
        let mut job = params(SyncKind::Bisync, p1.path(), p2.path());
        job.src_rel = String::new();
        job.bisync_workdir = Some(workdir_s.clone());

        // A plain run without prior state must fail (needs resync first).
        {
            let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
            let (on_event, mut rx) = event_channel();
            let _handle = start_job(rcd.client(), job.clone(), on_event)
                .await
                .expect("start aborted bisync");
            let (_progress, event) = wait_terminal(&mut rx).await;
            let SyncEvent::BisyncFinished { success, .. } = event else {
                panic!("expected BisyncFinished, got {event:?}");
            };
            assert!(!success, "first-ever run without resync must abort");
        }

        // Resync initializes the session, converging both sides.
        let mut resync_job = job.clone();
        resync_job.bisync_resync = true;
        resync_job.bisync_resync_mode = Some("newer".to_string());
        {
            let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
            let (on_event, mut rx) = event_channel();
            let _handle = start_job(rcd.client(), resync_job, on_event)
                .await
                .expect("start resync");
            let (_progress, event) = wait_terminal(&mut rx).await;
            let SyncEvent::BisyncFinished { success, report } = event else {
                panic!("expected BisyncFinished, got {event:?}");
            };
            assert!(success, "resync must succeed: {report:?}");
            assert!(report.get("session").and_then(Value::as_str).is_some());
        }
        assert!(p1.path().join("c.txt").is_file(), "resync copies p2 → p1");
        assert!(p2.path().join("a.txt").is_file(), "resync copies p1 → p2");

        // State files landed in OUR workdir (not rclone's global cache).
        let names = dir_names(workdir.path());
        assert!(
            names.iter().any(|name| name.ends_with(".path1.lst")),
            "session listing must live in the pinned workdir: {names:?}"
        );

        // Kill the rcd and run again on a FRESH handle with edits on both
        // sides — state must survive the restart and converge the changes.
        let mut rcd = RcdHandle::start(&binary, None).await.expect("rcd respawn");
        write_file(&p1.path().join("from-p1.txt"), "edited on p1");
        write_file(&p2.path().join("from-p2.txt"), "edited on p2");
        let (on_event, mut rx) = event_channel();
        let _handle = start_job(rcd.client(), job.clone(), on_event)
            .await
            .expect("start incremental bisync");
        let (_progress, event) = wait_terminal(&mut rx).await;
        let SyncEvent::BisyncFinished { success, report } = event else {
            panic!("expected BisyncFinished, got {event:?}");
        };
        assert!(success, "incremental run must succeed: {report:?}");
        assert!(p1.path().join("from-p2.txt").is_file(), "p2 edit traveled to p1");
        assert!(p2.path().join("from-p1.txt").is_file(), "p1 edit traveled to p2");
        drop(rcd);
    }

    /// `operations/list_filtered` (files/search base): recursive, files-only,
    /// case-insensitive substring pruning — directories without matches are
    /// pruned by rclone itself (live-verified v1.75.1).
    #[tokio::test]
    async fn list_filtered_prunes_non_matching_subtrees() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let root = tempfile::tempdir().expect("root");
        write_file(&root.path().join("Report-2024.txt"), "hit");
        write_file(&root.path().join("unrelated.log"), "miss");
        write_file(&root.path().join("deep").join("old-report.dat"), "hit");
        write_file(&root.path().join("empty-here").join("nothing.txt"), "miss");
        let client = rcd.client();
        let fs = root.path().to_string_lossy().into_owned();
        let result = client
            .operations_list_filtered(&fs, "", "**report**", true)
            .await
            .expect("filtered list");
        let paths: Vec<String> = result
            .get("list")
            .and_then(Value::as_array)
            .map(|array| {
                array
                    .iter()
                    .filter_map(|entry| entry.get("Path").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        assert!(paths.iter().any(|path| path.contains("Report-2024.txt")), "{paths:?}");
        assert!(paths.iter().any(|path| path.contains("old-report.dat")), "{paths:?}");
        assert!(!paths.iter().any(|path| path.contains("unrelated")), "{paths:?}");
        assert!(!paths.iter().any(|path| path.contains("nothing")), "empty subtrees pruned: {paths:?}");
    }

    /// `operations/copyurl` (files/copyurl base): the rcd host fetches the
    /// URL and uploads the bytes — exercised here against a loopback HTTP
    /// server serving one file (same spawn pattern as the webdav tests).
    #[tokio::test]
    async fn copyurl_pulls_from_a_local_http_url() {
        use std::io::{Read as _, Write as _};
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("addr").port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let body = b"downloaded-bytes";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(body);
        });
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let dst = tempfile::tempdir().expect("dst");
        let client = rcd.client();
        let fs = dst.path().to_string_lossy().into_owned();
        client
            .operations_copyurl(&fs, "pulled.bin", &format!("http://127.0.0.1:{port}/file.bin"), false)
            .await
            .expect("copyurl");
        server.join().expect("server thread");
        assert_eq!(std::fs::read(dst.path().join("pulled.bin")).expect("read"), b"downloaded-bytes");
    }

    /// `core/bwlimit` set + read roundtrip on the shared rcd (live-verified
    /// shape: the response echoes the canonical rate string, "off" clears).
    #[tokio::test]
    async fn bwlimit_set_and_read_roundtrip() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let client = rcd.client();
        let applied = client.core_bwlimit(Some("10M")).await.expect("set bwlimit");
        assert_eq!(applied.get("rate").and_then(Value::as_str), Some("10Mi"));
        let read = client.core_bwlimit(None).await.expect("read bwlimit");
        assert_eq!(read.get("rate").and_then(Value::as_str), Some("10Mi"));
        let cleared = client.core_bwlimit(Some("off")).await.expect("clear bwlimit");
        assert_eq!(cleared.get("rate").and_then(Value::as_str), Some("off"));
        // Unparsable values are rejected by rcd itself (500 bad bwlimit).
        assert!(client.core_bwlimit(Some("notanumber")).await.is_err());
    }

    /// `min_size` (rc snake_case, live-verified v1.75.1) keeps small files
    /// behind: only the 16k payload travels, both text sentinels stay. The
    /// job still completes successfully — filtering is not an error.
    #[tokio::test]
    async fn min_size_filter_skips_small_files() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let (src, dst) = sandbox();
        write_file(&src.path().join("sub").join("bulk.bin"), &"x".repeat(16 * 1024));
        let mut job = params(SyncKind::Copy, src.path(), dst.path());
        job.min_size = Some("10k".to_string());
        let (on_event, mut rx) = event_channel();
        let _handle = start_job(rcd.client(), job, on_event)
            .await
            .expect("start min-size copy");
        let (_progress, event) = wait_terminal(&mut rx).await;
        assert!(matches!(event, SyncEvent::Completed { .. }), "got {event:?}");
        let names = dir_names(&dst.path());
        assert!(names.contains(&"bulk.bin".to_string()), "big file travels: {names:?}");
        assert!(
            !names.contains(&"a.txt".to_string()),
            "sentinel under 10k must stay: {names:?}"
        );
        assert!(!dst.path().join("deep").join("b.txt").is_file(), "deep small file stays");
    }

    /// `min_age` with a far-future value (live-verified `99999d` on
    /// v1.75.1) is a legal filter that skips EVERYTHING: the job completes
    /// with zero transfers and the destination stays untouched.
    #[tokio::test]
    async fn min_age_far_future_skips_every_file() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let (src, dst) = sandbox();
        let mut job = params(SyncKind::Copy, src.path(), dst.path());
        job.min_age = Some("99999d".to_string());
        let (on_event, mut rx) = event_channel();
        let _handle = start_job(rcd.client(), job, on_event)
            .await
            .expect("start far-future min-age copy");
        let (_progress, event) = wait_terminal(&mut rx).await;
        assert!(
            matches!(event, SyncEvent::Completed { bytes: 0, files: 0 }),
            "far-future min_age must complete empty, got {event:?}"
        );
        assert!(dir_names(&dst.path()).is_empty(), "nothing may land: {:?}", dir_names(dst.path()));
    }

    /// `metadata` (rc snake_case, live-verified v1.75.1) is accepted by
    /// sync/copy without altering the outcome: the tree still mirrors.
    #[tokio::test]
    async fn metadata_flag_copies_with_metadata_preserved() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let (src, dst) = sandbox();
        let mut job = params(SyncKind::Copy, src.path(), dst.path());
        job.metadata = true;
        let (on_event, mut rx) = event_channel();
        let _handle = start_job(rcd.client(), job, on_event)
            .await
            .expect("start metadata copy");
        let (_progress, event) = wait_terminal(&mut rx).await;
        assert!(matches!(event, SyncEvent::Completed { files: 2, .. }), "got {event:?}");
        assert_eq!(dir_names(dst.path()), vec!["a.txt", "deep"]);
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
        let ghost = SyncJobHandle {
            jobid: i64::MAX as u64,
            group: "ghost".to_string(),
            kind: SyncKind::Copy,
        };
        assert_eq!(query_status(&rcd.client(), &ghost).await.expect("query ghost"), None);
    }

    /// 本机共享 live 冒烟（serve/start → 回环 GET → serve/list → serve/stop，
    /// 实测锚点 rclone v1.75.1）：端口 0 让 rcd 自动挑空闲回环端口并回传
    /// 实际地址；用 std TcpStream 直取文件内容证明分享真的可用。
    #[tokio::test]
    async fn serve_start_fetch_and_stop_roundtrip() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = RcdHandle::start(&binary, None).await.expect("rcd spawn");
        let dir = tempfile::tempdir().expect("serve tempdir");
        write_file(&dir.path().join("hello.txt"), "serve-sentinel");

        let answer = rcd
            .client()
            .serve_start(&dir.path().to_string_lossy(), "http", "127.0.0.1:0")
            .await
            .expect("serve/start");
        let serve_id = answer
            .get("id")
            .and_then(Value::as_str)
            .expect("serve id")
            .to_string();
        let addr = answer
            .get("addr")
            .and_then(Value::as_str)
            .expect("serve addr")
            .to_string();
        assert!(serve_id.starts_with("http-"), "unexpected id {serve_id}");
        assert!(addr.starts_with("127.0.0.1:"), "unexpected addr {addr}");

        let body = http_get(&addr, "/hello.txt");
        assert_eq!(body, "serve-sentinel");

        let list = rcd.client().serve_list().await.expect("serve/list");
        let ids: Vec<&str> = list
            .get("list")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.get("id").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default();
        assert!(ids.contains(&serve_id.as_str()), "list misses {serve_id}: {list}");

        rcd.client().serve_stop(&serve_id).await.expect("serve/stop");
        let list = rcd
            .client()
            .serve_list()
            .await
            .expect("serve/list after stop");
        assert!(
            list.get("list")
                .and_then(Value::as_array)
                .map(|entries| entries.is_empty())
                .unwrap_or(true),
            "serve list not empty after stop: {list}"
        );
        // rcd 对未知 id 报 rclone 错误 → RcError（幂等停止由分发臂按成功处理）。
        assert!(rcd.client().serve_stop(&serve_id).await.is_err());
    }

    /// Minimal loopback HTTP GET over std TcpStream (sidecar tests carry no
    /// HTTP client dependency): returns the body after the blank line.
    /// HTTP/1.0 + `Connection: close` keeps rclone's answer non-chunked for
    /// known-length files, so read-to-EOF captures the whole body.
    fn http_get(addr: &str, path: &str) -> String {
        use std::io::{Read, Write};
        let mut parts = addr.splitn(2, ':');
        let host = parts.next().unwrap_or("127.0.0.1");
        let port: u16 = parts
            .next()
            .and_then(|port| port.parse().ok())
            .expect("serve port");
        let mut stream = std::net::TcpStream::connect((host, port)).expect("tcp connect");
        write!(
            stream,
            "GET {path} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
        )
        .expect("write request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read response");
        response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or_default()
    }
}
