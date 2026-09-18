//! rclone rcd engine foundation (F-RCLONE phase A).
//!
//! Replaces the OpenDAL protocol layer with a managed `rclone rcd`
//! subprocess: the sidecar spawns rcd on `127.0.0.1:<ephemeral>` with a
//! random session credential and an isolated temp config, then performs all
//! storage work through the rc HTTP API (`rc.rs`). Endpoint set and the
//! async-job model mirror the patterns proven by yet-another-rclone-dashboard
//! (`_async:true` + `job/status` + `core/stats` + `job/stop`).
//!
//! Lifecycle is owned here, not by the user: unlike a standalone dashboard,
//! the sidecar resolves the binary (bundle → PATH), waits for health,
//! detects crashes and respawns lazily, and tears the process down on drop.
//! Storage backends themselves are rclone's problem — SMB, SFTP password
//! auth and bucket enumeration all come for free, retiring the three custom
//! OpenDAL adapters.

pub mod archive;
pub mod bytes_channel;
pub mod ops;
pub mod proc;
pub mod rc;
pub mod registry;
pub mod sync;

pub use proc::{RcdHandle, RcdSupervisor, MIN_RCLONE_VERSION};
pub use rc::{RcClient, RcError};

/// Phase B upload staging table entry: the [`bytes_channel::UploadStaging`]
/// sink plus the wiring metadata the finish path needs. The root-relative
/// `remote` was resolved (path whitelist + `lock_to_root`) at
/// `files/upload/start`. `crate`-visible only — constructed and driven by
/// `main.rs`'s rclone arms (the connection/job bookkeeping lives in the
/// `jobs` table records, not here).
pub(crate) struct UploadTask {
    pub staging: bytes_channel::UploadStaging,
    pub fs: String,
    pub remote: String,
    pub declared_size: u64,
    pub throttle: crate::transfers::Throttle,
}

/// Phase B download pump slot (the `transfers::DownloadSlot` twin without the
/// OpenDAL reader): pump coordination flags plus the save_to_local `.part`
/// staging path. `cancel` is cooperative — the pump observes it between
/// chunks and owns the cleanup while it lives; `pump_done` is settled by a
/// drop guard on every pump exit path so `finish`'s grace wait always
/// terminates.
#[derive(Clone)]
pub(crate) struct DownloadTask {
    pub size: u64,
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub pump_done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub staging: Option<std::path::PathBuf>,
}

/// Dual-engine facade held by the plugin: the rcd supervisor plus the
/// connection→remote registry. `main.rs` routes Phase A methods here when
/// `DBX_FILES_ENGINE=rclone` and falls back to the OpenDAL engine otherwise
/// (docs/IMPL_PLAN_RCLONE.zh-CN.md §2).
pub struct RcloneEngine {
    pub supervisor: tokio::sync::Mutex<RcdSupervisor>,
    pub registry: registry::Registry,
    /// Phase B upload staging sinks keyed by taskId (`files/upload/start`
    /// inserts, `finish`/`cancel`/append-failure remove). `handle_binary`'s
    /// dual-engine branch reads membership FIRST — a miss falls through to
    /// the OpenDAL JobTable, so this map is also the engine-selection check.
    /// std Mutex: every hold is a short sync section; nothing awaits under
    /// the lock.
    pub uploads: std::sync::Mutex<std::collections::HashMap<String, UploadTask>>,
    /// Phase B download pump slots keyed by taskId.
    pub downloads: std::sync::Mutex<std::collections::HashMap<String, DownloadTask>>,
    /// Phase B single-file job records with the exact `TransferJob` payload
    /// shape the OpenDAL JobTable carries (progress events, terminal finish
    /// replay). Kept after terminal states, like the JobTable, so a late
    /// finish replays the stored outcome instead of reporting not-found.
    pub jobs: std::sync::Mutex<std::collections::HashMap<String, crate::transfers::TransferJob>>,
    /// Phase D transfers-history persistence: hydrated `Option<Arc<Store>>`
    /// (set once by `Plugin::new`). Terminal single-file jobs are appended to
    /// the same `transfers.json` the OpenDAL JobTable writes — the reveal/
    /// open local-download whitelist and the restart-safe panel history
    /// therefore work identically under both engines. Dir jobs (syncDir/
    /// copyDir) stay memory-only: `TransferRecord.kind` is
    /// upload|download only, and the wire-visible DirJob shape has no
    /// persisted counterpart. Concurrency: writes are serialized on the
    /// rclone side by [`crate::history_write_lock`]; the OpenDAL JobTable
    /// keeps its own write path (each write is an atomic tmp+rename, so a
    /// cross-engine race costs at most one dropped history line, never a
    /// corrupt file).
    pub history: std::sync::Mutex<Option<std::sync::Arc<crate::store::Store>>>,
}

impl RcloneEngine {
    pub fn new() -> Self {
        Self {
            supervisor: tokio::sync::Mutex::new(RcdSupervisor::new()),
            registry: registry::Registry::new(),
            uploads: std::sync::Mutex::new(std::collections::HashMap::new()),
            downloads: std::sync::Mutex::new(std::collections::HashMap::new()),
            jobs: std::sync::Mutex::new(std::collections::HashMap::new()),
            history: std::sync::Mutex::new(None),
        }
    }

    /// The rclone engine is opt-in; the default stays OpenDAL until Phase D
    /// retires it.
    pub fn enabled() -> bool {
        std::env::var("DBX_FILES_ENGINE")
            .map(|value| value.trim().eq_ignore_ascii_case("rclone"))
            .unwrap_or(false)
    }

    /// Live rc client, spawning or respawning rcd as needed.
    pub async fn client(&self) -> Result<RcClient, String> {
        self.supervisor.lock().await.client().await
    }

    /// Connection-id → binding lookup shared by the workbench route and the
    /// MCP route. Folds in the built-in `__local__` connection (the
    /// dual-pane local column): the synthesized root-`/` fs connection,
    /// identical to the OpenDAL operator-table rule
    /// (`engine::local_connection`). The binding is never registered — local
    /// fs stays config-free, so a rcd respawn cannot orphan it.
    pub fn binding(&self, connection_id: &str) -> Result<registry::RemoteBinding, String> {
        if connection_id == crate::engine::LOCAL_CONNECTION_ID {
            return registry::binding_for(&crate::engine::local_connection());
        }
        self.registry
            .get(connection_id)
            .ok_or_else(|| "Connection is not connected (rclone engine)".to_string())
    }
}

impl Default for RcloneEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// The `fs` string handed to rc calls. Local connections use the root path
/// itself; named remotes carry the connection root inside the fs string
/// (`dbxName:root`) because rc `remote` params are relative to the remote
/// root (registry task, live-process finding #2).
pub fn call_fs(binding: &registry::RemoteBinding) -> String {
    if binding.backend_type == "local" || binding.root.is_empty() {
        binding.remote_fs.clone()
    } else {
        format!("{}{}", binding.remote_fs, binding.root)
    }
}

#[cfg(test)]
mod wiring_tests {
    use super::*;

    fn binding(backend_type: &'static str, remote_fs: &str, root: &str) -> registry::RemoteBinding {
        registry::RemoteBinding {
            remote_fs: remote_fs.to_string(),
            backend_type,
            root: root.to_string(),
            lock_to_root: false,
            read_only: false,
            allow_delete: true,
        }
    }

    #[test]
    fn call_fs_composes_named_remote_root() {
        assert_eq!(call_fs(&binding("s3", "dbxAb12:", "")), "dbxAb12:");
        assert_eq!(
            call_fs(&binding("s3", "dbxAb12:", "srv/data")),
            "dbxAb12:srv/data"
        );
        // Local bindings already carry the root as the fs string.
        assert_eq!(call_fs(&binding("local", "/tmp/data", "/tmp/data")), "/tmp/data");
    }

    /// The built-in `__local__` id resolves without registration (rooted fs
    /// binding, no policy gates) while unknown ids stay registry errors —
    /// the dual-pane local column depends on the first half, the MCP route
    /// on both.
    #[test]
    fn binding_folds_in_built_in_local_connection() {
        let engine = RcloneEngine::new();
        assert_eq!(
            engine.binding("not-connected").err().as_deref(),
            Some("Connection is not connected (rclone engine)")
        );
        let local = engine
            .binding(crate::engine::LOCAL_CONNECTION_ID)
            .expect("built-in local binding");
        assert_eq!(local.backend_type, "local");
        assert_eq!(local.remote_fs, "/");
        assert_eq!(local.root, "/");
        assert!(local.allow_delete && !local.read_only && !local.lock_to_root);
    }

    /// End-to-end through a live rcd: the fs-protocol wiring path — call_fs
    /// hands the absolute local root to ops::list directly (named-local +
    /// absolute-path composition is deliberately NOT exercised here; see
    /// IMPL_PLAN_RCLONE.zh-CN.md §5 note).
    #[tokio::test]
    async fn composed_fs_lists_through_named_remote() {
        let Some(binary) = proc::resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("sub").join("a.txt"), b"hello").expect("write");

        let handle = RcdHandle::start(&binary).await.expect("rcd spawn");
        let client = handle.client();
        let fs_binding = binding("local", &dir.path().to_string_lossy(), &dir.path().to_string_lossy());
        let entries = ops::list(&client, &call_fs(&fs_binding), "/", false, &fs_binding.root, false)
            .await
            .expect("list");
        assert_eq!(entries.len(), 1, "entries: {entries:?}");
        assert_eq!(entries[0].name, "sub");
        assert_eq!(entries[0].kind, "dir");
    }
}
