//! Remote-edit sessions (打开方式 → 本机临时副本 + 保存自动回传).
//!
//! `files/remote-edit/open` downloads one remote file into a per-connection
//! temp workspace, launches the user's chosen local app (or the OS default)
//! and keeps a watcher loop polling the local copy: when an editor save
//! settles (size+mtime stable across [`STABLE_TICKS`] consecutive polls),
//! the wiring layer in `main.rs` streams the changed file back to the exact
//! remote path — the FinalShell-style "edit remote file locally" loop.
//!
//! Session bookkeeping lives here (std-Mutex map, short sections only); the
//! actual rclone traffic (download, `upload_staged_exact` sync-back) and the
//! progress/history integration stay in main.rs. Local copies are removed
//! when a session is closed (`files/remote-edit/close` or local deletion);
//! a sidecar exit leaves them under the OS temp directory where the OS's
//! own cleanup eventually reclaims them. Like the download history
//! allowlist in `local_downloads.rs`, the workspace root can be redirected
//! for tests via `DBX_FILES_EDIT_DIR`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::local_downloads::sanitize_file_name;

/// Redirects the remote-edit workspace base (tests / docker deployments that
/// mount a temp dir). Mirrors `local_downloads::DOWNLOAD_DIR_ENV`.
pub const EDIT_DIR_ENV: &str = "DBX_FILES_EDIT_DIR";

/// Watch poll interval. Two stable ticks ≈ 3 s of unchanged size+mtime
/// before a save is considered settled — long enough for editors that write
/// in several passes, short enough that sync-back feels immediate.
pub const WATCH_POLL: Duration = Duration::from_millis(1500);
/// Consecutive unchanged polls required before a change triggers sync-back.
pub const STABLE_TICKS: u32 = 2;
/// A failed sync-back retries every this-many ticks (≈30 s) while the local
/// file still differs from the synced baseline — transient network errors
/// recover without user intervention.
pub const ERROR_RETRY_TICKS: u32 = 20;

/// Session lifecycle states (`status` in `files/remote-edit/status`).
pub const STATUS_DOWNLOADING: &str = "downloading";
pub const STATUS_WATCHING: &str = "watching";
pub const STATUS_SYNCING: &str = "syncing";
pub const STATUS_ERROR: &str = "error";

/// Baseline stat snapshot of the local copy: `(size, mtime epoch millis)`.
/// `None` means "never observed" (copy missing since creation).
pub type FileStamp = (u64, i64);

/// One active remote-edit session. Plain data; every
/// mutation goes through `EditEngine::update` so watchers and RPC arms
/// serialize on the engine's std Mutex. `Clone` hands watchers a snapshot
/// for read-only inspection; mutations still go through the engine.
#[derive(Clone)]
pub struct EditSession {
    pub key: String,
    pub connection_id: String,
    pub remote_path: String,
    pub local_path: String,
    /// Validated external app path (`None` = OS default app).
    pub app: Option<PathBuf>,
    pub status: &'static str,
    pub last_error: Option<String>,
    /// Last successful sync-back, unix epoch millis.
    pub last_sync_at: Option<u64>,
    pub created_at: u64,
    /// Watcher bookkeeping (never serialized).
    pub baseline: Option<FileStamp>,
    pub pending_stable: u32,
    pub error_ticks: u32,
    pub sync_seq: u64,
    /// Set by `files/remote-edit/close`; the watcher closes on the next tick
    /// and an in-flight open skips app launch.
    pub closing: bool,
}

/// Watcher tick outcome consumed by the main.rs watch loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchDecision {
    /// Nothing to do this tick.
    Continue,
    /// Local copy changed and settled: sync it back to the remote.
    Sync,
    /// Local copy deleted (or close requested): drop the session.
    Close,
}

/// Pure watcher transition, unit-testable without a live rcd: compares the
/// current stat snapshot against the synced baseline and decides what the
/// wiring layer should do this tick. Mutates the session's watcher
/// bookkeeping (stability counters, retry counter) but never its status.
pub fn tick_watch(session: &mut EditSession, current: Option<FileStamp>) -> WatchDecision {
    if session.closing {
        return WatchDecision::Close;
    }
    let Some(current) = current else {
        // The user deleted the local copy: nothing left to watch or sync.
        return WatchDecision::Close;
    };
    if session.status == STATUS_SYNCING {
        // Defensive: a sync is owned by the same single loop; never stack.
        return WatchDecision::Continue;
    }
    if session.status == STATUS_ERROR {
        session.error_ticks += 1;
        if session.error_ticks < ERROR_RETRY_TICKS {
            return WatchDecision::Continue;
        }
        // Window elapsed: seed the stability counter so the very next poll
        // re-syncs when the file still differs from the baseline (instead
        // of waiting another full stability window).
        session.error_ticks = 0;
        if session.baseline != Some(current) {
            session.pending_stable = STABLE_TICKS - 1;
        }
    }
    if session.baseline != Some(current) {
        session.pending_stable += 1;
        if session.pending_stable >= STABLE_TICKS {
            session.pending_stable = 0;
            session.error_ticks = 0;
            return WatchDecision::Sync;
        }
    } else {
        session.pending_stable = 0;
    }
    WatchDecision::Continue
}

/// `(size, mtime epoch millis)` of a regular file, `None` when it cannot be
/// stat'ed (missing or unreadable — the watcher treats both as gone).
pub fn snapshot_stat(path: &Path) -> Option<FileStamp> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    Some((metadata.len(), mtime_ms))
}

/// Workspace base: `DBX_FILES_EDIT_DIR` override, else the OS temp dir.
pub fn workspace_base(lookup: impl Fn(&str) -> Option<std::ffi::OsString>) -> PathBuf {
    match lookup(EDIT_DIR_ENV) {
        Some(value) if !value.to_string_lossy().trim().is_empty() => {
            PathBuf::from(value.to_string_lossy().trim().to_string())
        }
        _ => std::env::temp_dir().join("dbx-files-remote-edit"),
    }
}

/// Deterministic local copy path for `(connection, remote path)`: sanitized
/// per remote path segment under `<base>/<connection>/<dirs…>/<name>`, so
/// same-name files in different remote directories (or connections) never
/// collide and re-opening the same file reuses the same copy. Separators,
/// dots-only segments and traversal attempts are neutralized by
/// `sanitize_file_name` (each segment can only ever become a plain name).
pub fn local_copy_path(
    base: &Path,
    connection_id: &str,
    remote_path: &str,
) -> Result<PathBuf, String> {
    let connection = sanitize_file_name(connection_id.trim());
    if connection.is_empty() {
        return Err("Invalid connection id for the remote-edit workspace".to_string());
    }
    let mut segments: Vec<String> = Vec::new();
    for segment in remote_path.trim_matches('/').split(['/', '\\']) {
        let segment = segment.trim();
        if segment.is_empty() || segment == "." || segment == ".." {
            continue;
        }
        segments.push(sanitize_file_name(segment));
    }
    let Some(file) = segments.pop() else {
        return Err(format!("Invalid remote path '{remote_path}': no file name"));
    };
    let mut path = base.join(connection);
    for segment in segments {
        path = path.join(segment);
    }
    Ok(path.join(file))
}

/// Session registry shared between RPC arms and watcher tasks. `Clone` hands
/// out another handle onto the same map (like `RcloneSyncRecord` tables).
#[derive(Clone, Default)]
pub struct EditEngine {
    sessions: Arc<Mutex<HashMap<String, EditSession>>>,
}

impl EditEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, session: EditSession) {
        self.lock().insert(session.key.clone(), session);
    }

    pub fn get(&self, key: &str) -> Option<EditSession> {
        self.lock().get(key).cloned()
    }

    /// The live session for `(connection, remote path)` — re-open reuse.
    pub fn find_for_path(&self, connection_id: &str, remote_path: &str) -> Option<EditSession> {
        self.lock()
            .values()
            .find(|session| {
                session.connection_id == connection_id && session.remote_path == remote_path
            })
            .cloned()
    }

    /// Runs `mutate` under the map lock; `false` when the session is gone.
    pub fn update(
        &self,
        key: &str,
        mutate: impl FnOnce(&mut EditSession),
    ) -> bool {
        match self.lock().get_mut(key) {
            Some(session) => {
                mutate(session);
                true
            }
            None => false,
        }
    }

    /// Runs `f` under the map lock and returns its value; `None` when the
    /// session is gone. The watcher uses this so `tick_watch`'s counter
    /// mutations persist across polls (a `get` clone would lose them).
    pub fn map_mut<T>(
        &self,
        key: &str,
        f: impl FnOnce(&mut EditSession) -> T,
    ) -> Option<T> {
        self.lock().get_mut(key).map(f)
    }

    pub fn remove(&self, key: &str) -> Option<EditSession> {
        self.lock().remove(key)
    }

    /// Marks the session for closing; `false` when it does not exist.
    pub fn request_close(&self, key: &str) -> bool {
        self.update(key, |session| session.closing = true)
    }

    /// CamelCase projection for `files/remote-edit/status`, optionally
    /// filtered per connection. Paths only — no secrets can live here.
    pub fn snapshot(&self, connection_filter: Option<&str>) -> Vec<Value> {
        let guard = self.lock();
        let mut sessions: Vec<&EditSession> = guard
            .values()
            .filter(|session| {
                connection_filter
                    .map(|id| session.connection_id == id)
                    .unwrap_or(true)
            })
            .collect();
        sessions.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        sessions
            .into_iter()
            .map(|session| {
                let mut entry = json!({
                    "key": session.key,
                    "connectionId": session.connection_id,
                    "remotePath": session.remote_path,
                    "localPath": session.local_path,
                    "status": session.status,
                    "createdAt": session.created_at,
                });
                if let Some(app) = &session.app {
                    entry["app"] = json!(app.to_string_lossy());
                }
                if let Some(error) = &session.last_error {
                    entry["lastError"] = json!(error);
                }
                if let Some(synced) = session.last_sync_at {
                    entry["lastSyncAt"] = json!(synced);
                }
                entry
            })
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, EditSession>> {
        match self.sessions.lock() {
            Ok(guard) => guard,
            // A watcher panicked while holding the lock: the registry is
            // unusable anyway; recover instead of poisoning every caller.
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> EditSession {
        EditSession {
            key: "k".to_string(),
            connection_id: "c1".to_string(),
            remote_path: "/docs/a.txt".to_string(),
            local_path: "/tmp/a.txt".to_string(),
            app: None,
            status: STATUS_WATCHING,
            last_error: None,
            last_sync_at: None,
            created_at: 1,
            baseline: Some((10, 100)),
            pending_stable: 0,
            error_ticks: 0,
            sync_seq: 0,
            closing: false,
        }
    }

    #[test]
    fn local_copy_path_is_deterministic_and_traversal_safe() {
        let base = Path::new("/tmp/edits");
        let first = local_copy_path(base, "c1", "/docs/reports/a b.txt").unwrap();
        let again = local_copy_path(base, "c1", "/docs/reports/a b.txt").unwrap();
        assert_eq!(first, again);
        assert!(first.starts_with(base.join("c1")));

        // Traversal and separators collapse into plain segments — the copy
        // can never escape the workspace regardless of the remote name.
        let evil = local_copy_path(base, "c1", "/../..//etc/passwd").unwrap();
        assert!(evil.starts_with(base.join("c1")));
        assert!(!evil.to_string_lossy().contains(".."));

        // Distinct connections / remote directories never collide.
        assert_ne!(
            local_copy_path(base, "c1", "/docs/a.txt").unwrap(),
            local_copy_path(base, "c2", "/docs/a.txt").unwrap()
        );
        assert_ne!(
            local_copy_path(base, "c1", "/docs/a.txt").unwrap(),
            local_copy_path(base, "c1", "/docs/old/a.txt").unwrap()
        );

        assert!(local_copy_path(base, "c1", "/").is_err());
    }

    #[test]
    fn workspace_base_prefers_the_override() {
        let base = workspace_base(|key| {
            (key == EDIT_DIR_ENV).then(|| std::ffi::OsString::from("/data/edits "))
        });
        assert_eq!(base, PathBuf::from("/data/edits"));
        let fallback = workspace_base(|_| None);
        assert!(fallback.ends_with("dbx-files-remote-edit"));
    }

    #[test]
    fn tick_watch_requires_stability_before_syncing() {
        let mut session = session();
        session.baseline = Some((10, 100));
        // First changed poll: not yet settled.
        assert_eq!(
            tick_watch(&mut session, Some((20, 200))),
            WatchDecision::Continue
        );
        assert_eq!(session.pending_stable, 1);
        // Same stamp again → save settled → sync.
        assert_eq!(
            tick_watch(&mut session, Some((20, 200))),
            WatchDecision::Sync
        );
        assert_eq!(session.pending_stable, 0);
        // Back to baseline: counters reset, no spurious sync.
        assert_eq!(
            tick_watch(&mut session, Some((10, 100))),
            WatchDecision::Continue
        );
        assert_eq!(session.pending_stable, 0);
    }

    #[test]
    fn tick_watch_closes_on_missing_or_requested_close() {
        let mut state = session();
        assert_eq!(tick_watch(&mut state, None), WatchDecision::Close);

        let mut state = session();
        state.closing = true;
        assert_eq!(
            tick_watch(&mut state, Some((10, 100))),
            WatchDecision::Close
        );
    }

    #[test]
    fn tick_watch_retries_errors_after_the_backoff_window() {
        let mut errored = session();
        errored.status = STATUS_ERROR;
        errored.last_error = Some("network down".to_string());
        for _ in 0..ERROR_RETRY_TICKS - 1 {
            assert_eq!(
                tick_watch(&mut errored, Some((20, 200))),
                WatchDecision::Continue
            );
        }
        // Window elapsed with the file still differing from the baseline:
        // retry the sync-back.
        assert_eq!(
            tick_watch(&mut errored, Some((20, 200))),
            WatchDecision::Sync
        );
        // While watching (no error), every unchanged poll resets the counter.
        let mut watching = session();
        watching.baseline = Some((10, 100));
        assert_eq!(
            tick_watch(&mut watching, Some((10, 100))),
            WatchDecision::Continue
        );
        assert_eq!(watching.pending_stable, 0);
    }

    #[test]
    fn snapshot_projects_camel_case_and_filters_by_connection() {
        let engine = EditEngine::new();
        let mut first = session();
        first.status = STATUS_SYNCING;
        first.last_sync_at = Some(42);
        engine.insert(first);
        let mut second = session();
        second.key = "k2".to_string();
        second.connection_id = "c2".to_string();
        second.created_at = 2;
        second.app = Some(PathBuf::from("/Applications/TextEdit.app"));
        engine.insert(second);

        let all = engine.snapshot(None);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0]["key"], json!("k"));
        assert_eq!(all[0]["status"], json!(STATUS_SYNCING));
        assert_eq!(all[0]["lastSyncAt"], json!(42));
        assert_eq!(all[1]["app"], json!("/Applications/TextEdit.app"));
        assert!(all[0].get("lastError").is_none());
        assert!(all[0].get("baseline").is_none());

        let filtered = engine.snapshot(Some("c2"));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["connectionId"], json!("c2"));

        // find_for_path matches both keys; request_close flips the flag.
        assert_eq!(
            engine.find_for_path("c1", "/docs/a.txt").unwrap().key,
            "k"
        );
        assert!(engine.request_close("k2"));
        assert!(engine.get("k2").unwrap().closing);
        assert!(!engine.request_close("missing"));
    }
}
