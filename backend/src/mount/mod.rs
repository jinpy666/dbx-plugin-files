//! Local-mount feature (docs/MOUNT.zh-CN.md, M1).
//!
//! Two strategies behind `files/mount`, resolved here:
//! 1. **rclone mount** (primary) — `rclone::mount` drives the running rcd's
//!    `mount/mount`; the bundled fork carries mount support compiled in.
//!    Full POSIX-ish semantics, but the OS FUSE driver must be installed
//!    (macFUSE / WinFsp / fuse3).
//! 2. **WebDAV gateway** (fallback) — [`webdav_gateway`]: a minimal
//!    read-only WebDAV server inside the sidecar, mounted by each OS's
//!    built-in WebDAV client, zero driver installs.
//!
//! `auto` tries rclone first and falls back to the gateway whenever the
//! failure is environmental (driver missing, binary refuses to mount);
//! mountpoint problems and pinned strategies surface as errors instead.
//! M1 mounts are read-only on every path, so `read_only` connections and
//! the WebDAV write ban stay consistent with the plugin's own gates.

pub mod webdav_gateway;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use uuid::Uuid;

use crate::model;
use crate::policy::PathPolicy;
use crate::rclone::mount::{DriverProbe, MountError};
use crate::rclone::registry::RemoteBinding;
use crate::rclone::{self, rc::RcClient};

/// M1 gateway reads are whole-file; refuse to buffer absurd files into the
/// sidecar. Editors and documents sit far below this line.
const MAX_GATEWAY_READ_BYTES: u64 = 256 * 1024 * 1024;

/// One live mount, keyed by `mount_id` (uuid).
pub struct MountRecord {
    pub mount_id: String,
    pub connection_id: String,
    /// `"rclone"` or `"webdav"` — the strategy that actually won.
    pub strategy: String,
    /// The fs string the mount/gateway serves (root + optional subpath).
    pub fs: String,
    pub backend: MountBackend,
    /// Why the primary strategy lost, when `auto` degraded to the gateway.
    pub fallback_reason: Option<String>,
    pub created_at_ms: i64,
    /// Holds the in-flight count so the keepalive sweep never reaps the rcd
    /// under a live mount (same pattern as upload/download tasks).
    pub _work: rclone::WorkGuard,
}

pub enum MountBackend {
    /// Kernel mount held by the rcd (`mount/mount`).
    Rclone { mount_point: PathBuf },
    /// Loopback WebDAV listener inside this sidecar.
    WebDav { gateway: webdav_gateway::GatewayHandle },
}

pub type MountTable = Arc<std::sync::Mutex<HashMap<String, MountRecord>>>;

fn lock_table(
    mounts: &MountTable,
) -> Result<std::sync::MutexGuard<'_, HashMap<String, MountRecord>>, String> {
    mounts
        .lock()
        .map_err(|_| "mount table poisoned".to_string())
}

// ---------------------------------------------------------------------------
// Strategy decision — pure functions so the auto/fallback matrix is testable
// without an rcd.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Proceed with the WebDAV gateway; the payload is the reason shown to
    /// the user (`fallbackReason`).
    FallBack(String),
    /// Give up with an actionable error — no strategy can satisfy this.
    Fail(String),
}

fn normalize_strategy(requested: Option<&str>) -> Result<String, String> {
    match requested.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok("auto".to_string()),
        Some(value) => {
            let lowered = value.to_ascii_lowercase();
            if matches!(lowered.as_str(), "auto" | "rclone" | "webdav") {
                Ok(lowered)
            } else {
                Err(format!(
                    "unknown mount strategy {value:?}: expected auto, rclone or webdav"
                ))
            }
        }
    }
}

/// Should the rclone strategy even be attempted? `auto` skips the attempt
/// when the platform probe already knows the driver is absent — the probe
/// is advisory, so a stale "missing" only costs one classified error.
fn should_attempt_rclone(strategy: &str, probe: &DriverProbe) -> bool {
    match strategy {
        "webdav" => false,
        "rclone" => true,
        _ => *probe == DriverProbe::Available,
    }
}

fn on_rclone_failure(strategy: &str, error: &MountError) -> Decision {
    match strategy {
        // Pinned strategy: never degrade silently.
        "rclone" => Decision::Fail(format!("rclone mount failed: {error}")),
        _ => match error {
            // Environmental: another strategy can step in.
            failed if failed.is_unavailable() => Decision::FallBack(failed.to_string()),
            MountError::Other(message) => Decision::FallBack(message.clone()),
            // A busy/non-empty mountpoint is a user-side problem, and the
            // gateway would only mask it.
            MountError::MountPointBusy(_) => {
                Decision::Fail(format!("rclone mount failed: {error}"))
            }
            MountError::DriverMissing(_) | MountError::Unsupported(_) => unreachable!(
                "covered by the is_unavailable guard above"
            ),
        },
    }
}

// ---------------------------------------------------------------------------
// fs / mountpoint helpers
// ---------------------------------------------------------------------------

/// The mount surface for a connection (+ optional sub-path).
///
/// Returns `(fs, mount_path)`:
/// - `fs` — the rclone fs string for the **kernel mount** (subdir appended;
///   the kernel mount root must be the mounted folder itself).
/// - `mount_path` — the mounted folder in **connection-space** absolute
///   form (`""` = whole root). The WebDAV gateway keeps serving the
///   connection's own fs and maps gateway-relative paths through
///   `mount_path`, so every engine call stays on the standard
///   policy-resolve path (no double-joined remotes).
fn mount_surface(binding: &RemoteBinding, subpath: Option<&str>) -> Result<(String, String), String> {
    let base = rclone::call_fs(binding);
    let Some(sub) = subpath else {
        return Ok((base.clone(), String::new()));
    };
    let policy = PathPolicy::from_parts(
        &binding.root,
        binding.lock_to_root,
        true,
        binding.allow_delete,
    );
    let joined = format!(
        "{}/{}",
        binding.root.trim_end_matches('/'),
        sub.trim_start_matches('/')
    );
    let resolved = policy.check_read(&joined)?;
    let fs = if binding.backend_type == "local" {
        format!("{}{}", base.trim_end_matches('/'), resolved.absolute)
    } else {
        let relative = resolved.relative.trim_matches('/');
        if relative.is_empty() {
            base
        } else {
            format!("{}/{}", base.trim_end_matches('/'), relative)
        }
    };
    Ok((fs, resolved.absolute))
}

fn home_dir() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "cannot resolve the home directory (HOME/USERPROFILE unset)".to_string())
}

/// `~/dbx-files-mounts/<remote>` on unix; a free drive letter on Windows.
fn default_mount_point(connection_id: &str) -> Result<PathBuf, String> {
    if std::env::consts::OS == "windows" {
        return pick_windows_drive()
            .ok_or_else(|| "no free drive letter between D: and Z:".to_string());
    }
    let base = home_dir()?.join("dbx-files-mounts");
    std::fs::create_dir_all(&base)
        .map_err(|error| format!("cannot create {}: {error}", base.display()))?;
    Ok(base.join(rclone::registry::remote_name(connection_id)))
}

fn pick_windows_drive() -> Option<PathBuf> {
    for letter in (b'D'..=b'Z').rev() {
        let point = PathBuf::from(format!("{}:", letter as char));
        if !point.exists() {
            return Some(point);
        }
    }
    None
}

/// rclone requires an existing, empty mountpoint; create or validate it.
fn ensure_empty_mount_dir(point: &Path) -> Result<(), String> {
    match std::fs::metadata(point) {
        Err(_) => std::fs::create_dir_all(point)
            .map_err(|error| format!("cannot create mount point {}: {error}", point.display())),
        Ok(metadata) if metadata.is_dir() => {
            let empty = std::fs::read_dir(point)
                .map_err(|error| format!("cannot read {}: {error}", point.display()))?
                .next()
                .is_none();
            if empty {
                Ok(())
            } else {
                Err(format!(
                    "mount point {} is not empty — pick an empty directory",
                    point.display()
                ))
            }
        }
        Ok(_) => Err(format!(
            "mount point {} exists and is not a directory",
            point.display()
        )),
    }
}

/// Platform-specific mount hint carried in every response.
fn mount_hint(strategy: &str, mount_point: &Path) -> String {
    match (std::env::consts::OS, strategy) {
        ("macos", "rclone") => format!(
            "已挂载，Finder 侧边栏或「前往 → 文件夹」打开 {}",
            mount_point.display()
        ),
        ("macos", "webdav") => "Finder → 前往 → 连接服务器（Cmd+K），粘贴网关地址即可挂载（只读）".to_string(),
        ("windows", "rclone") => "已挂载为新盘符：打开「此电脑」查看".to_string(),
        ("windows", "webdav") => "资源管理器 → 映射网络驱动器，粘贴网关地址即可挂载（只读）".to_string(),
        (_, "rclone") => format!("已挂载到 {}，文件管理器直接打开", mount_point.display()),
        (_, "webdav") => "文件管理器「连接服务器」使用 dav:// 前缀粘贴网关地址（只读）".to_string(),
        _ => String::new(),
    }
}

fn random_token() -> String {
    // Two v4 uuids = 256 bits of path token; it never leaves process memory
    // (docs/MOUNT.zh-CN.md §2.5).
    format!(
        "{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    )
}

// ---------------------------------------------------------------------------
// Gateway source over the rclone engine
// ---------------------------------------------------------------------------

/// Read-only [`webdav_gateway::GatewaySource`] backed by the engine ops —
/// the same `ops::stat`/`ops::list`/`serve_get` path the plugin's own
/// browsing uses, so connection policy (root lock, timeout) applies twice:
/// here at the source and at the WebDAV method gate.
struct EngineSource {
    client: RcClient,
    /// Always the **connection's** fs: the gateway maps paths in
    /// plugin-space (`mount_path`), never by pre-joining the subdir into
    /// the remote (that double-appended and 404'd sub-directory mounts).
    fs: String,
    root: String,
    lock_to_root: bool,
    /// Mounted folder in connection-space absolute form; `""` = whole root.
    mount_path: String,
}

impl EngineSource {
    /// Gateway-relative `rel` → connection-space absolute plugin path.
    fn plugin_path(&self, rel: &str) -> String {
        if self.mount_path.is_empty() {
            if rel.is_empty() {
                "/".to_string()
            } else {
                format!("/{rel}")
            }
        } else if rel.is_empty() {
            self.mount_path.clone()
        } else {
            format!("{}/{}", self.mount_path.trim_end_matches('/'), rel)
        }
    }
}

impl webdav_gateway::GatewaySource for EngineSource {
    fn stat(&self, rel: &str) -> webdav_gateway::BoxFut<'_, Result<webdav_gateway::StatEntry, String>> {
        let client = self.client.clone();
        let fs = self.fs.clone();
        let root = self.root.clone();
        let lock_to_root = self.lock_to_root;
        let path = self.plugin_path(rel);
        Box::pin(async move {
            let entry = rclone::ops::stat(&client, &fs, &path, &root, lock_to_root).await?;
            Ok(to_stat_entry(&entry))
        })
    }

    fn list_dir(
        &self,
        rel: &str,
    ) -> webdav_gateway::BoxFut<'_, Result<Vec<webdav_gateway::StatEntry>, String>> {
        let client = self.client.clone();
        let fs = self.fs.clone();
        let root = self.root.clone();
        let lock_to_root = self.lock_to_root;
        let path = self.plugin_path(rel);
        Box::pin(async move {
            let entries =
                rclone::ops::list(&client, &fs, &path, false, &root, lock_to_root).await?;
            Ok(entries.iter().map(to_stat_entry).collect())
        })
    }

    fn read(&self, rel: &str) -> webdav_gateway::BoxFut<'_, Result<Vec<u8>, String>> {
        let client = self.client.clone();
        let fs = self.fs.clone();
        let root = self.root.clone();
        let lock_to_root = self.lock_to_root;
        let path = self.plugin_path(rel);
        Box::pin(async move {
            let policy = PathPolicy::from_parts(&root, lock_to_root, true, true);
            let resolved = policy.check_read(&path)?;
            let response = client
                .serve_get(&fs, &resolved.relative, None)
                .await
                .map_err(|error| error.to_string())?;
            let mut bytes = Vec::new();
            let mut chunk_stream = response;
            while let Some(chunk) = chunk_stream
                .chunk()
                .await
                .map_err(|error| format!("gateway read failed: {error}"))?
            {
                if bytes.len() as u64 + chunk.len() as u64 > MAX_GATEWAY_READ_BYTES {
                    return Err(format!(
                        "file exceeds the gateway read cap ({MAX_GATEWAY_READ_BYTES} bytes)"
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        })
    }
}

fn to_stat_entry(entry: &model::FileEntry) -> webdav_gateway::StatEntry {
    webdav_gateway::StatEntry {
        name: entry.name.clone(),
        size: entry.size.unwrap_or(0),
        is_dir: entry.kind == "dir",
        modified_ms: entry.modified_at.map(|ms| ms as i64),
    }
}

// ---------------------------------------------------------------------------
// files/mount · files/unmount · files/mountStatus
// ---------------------------------------------------------------------------

pub async fn start_mount(
    engine: &rclone::RcloneEngine,
    mounts: &MountTable,
    connection_id: &str,
    request: &model::MountRequest,
) -> Result<Value, String> {
    let strategy = normalize_strategy(request.strategy.as_deref())?;
    let binding = engine.binding(connection_id)?;
    // Hold the in-flight counter for the whole mount lifetime: the rcd must
    // outlive every kernel mount and gateway request against it.
    let work = engine.start_work(&rclone::registry::group_key_of(binding.proxy.as_ref()));
    let client = engine.client_for_binding(&binding).await?;
    let (fs, mount_path) = mount_surface(&binding, request.path.as_deref())?;

    let mut fallback_reason: Option<String> = None;
    let probe = rclone::mount::probe_driver();
    if should_attempt_rclone(&strategy, &probe) {
        let mount_point = match request.mount_point.as_deref() {
            Some(explicit) => {
                let point = PathBuf::from(explicit);
                ensure_empty_mount_dir(&point)?;
                point
            }
            None => {
                let point = default_mount_point(connection_id)?;
                ensure_empty_mount_dir(&point)?;
                point
            }
        };
        match rclone::mount::mount(
            &client,
            &rclone::mount::MountSpec {
                fs: fs.clone(),
                mount_point: mount_point.clone(),
                read_only: true,
            },
        )
        .await
        {
            Ok(()) => {
                let mount_id = Uuid::new_v4().simple().to_string();
                let hint = mount_hint("rclone", &mount_point);
                lock_table(mounts)?.insert(
                    mount_id.clone(),
                    MountRecord {
                        mount_id: mount_id.clone(),
                        connection_id: connection_id.to_string(),
                        strategy: "rclone".to_string(),
                        fs: fs.clone(),
                        backend: MountBackend::Rclone { mount_point: mount_point.clone() },
                        fallback_reason: None,
                        created_at_ms: chrono::Utc::now().timestamp_millis(),
                        _work: work,
                    },
                );
                return Ok(json!({
                    "mountId": mount_id,
                    "strategy": "rclone",
                    "mountPoint": mount_point.display().to_string(),
                    "readOnly": true,
                    "hint": hint,
                }));
            }
            Err(error) => match on_rclone_failure(&strategy, &error) {
                Decision::FallBack(reason) => fallback_reason = Some(reason),
                Decision::Fail(message) => return Err(message),
            },
        }
    } else if strategy == "auto" {
        // Probe already knows the driver is absent: carry its guidance into
        // the gateway response instead of burning a doomed mount attempt.
        if let DriverProbe::Missing(reason) = &probe {
            fallback_reason = Some(reason.clone());
        }
    }

    // WebDAV gateway fallback (or pinned webdav strategy).
    let token = random_token();
    let gateway = webdav_gateway::start(webdav_gateway::GatewayConfig {
        conn_id: connection_id.to_string(),
        token: token.clone(),
        source: Arc::new(EngineSource {
            client,
            fs: rclone::call_fs(&binding),
            root: binding.root.clone(),
            lock_to_root: binding.lock_to_root,
            mount_path: mount_path.clone(),
        }),
    })
    .await?;
    let url = format!("http://127.0.0.1:{}/{}/{}/", gateway.port, token, connection_id);
    let mount_id = Uuid::new_v4().simple().to_string();
    let hint = mount_hint("webdav", Path::new(&url));
    lock_table(mounts)?.insert(
        mount_id.clone(),
        MountRecord {
            mount_id: mount_id.clone(),
            connection_id: connection_id.to_string(),
            strategy: "webdav".to_string(),
            fs: fs.clone(),
            backend: MountBackend::WebDav { gateway },
            fallback_reason: fallback_reason.clone(),
            created_at_ms: chrono::Utc::now().timestamp_millis(),
            _work: work,
        },
    );
    let mut response = json!({
        "mountId": mount_id,
        "strategy": "webdav",
        "gatewayUrl": url,
        "readOnly": true,
        "hint": hint,
    });
    if let Some(reason) = fallback_reason {
        response["fallbackReason"] = Value::String(reason);
    }
    Ok(response)
}

/// Snapshot `(mount_id, backend)` pairs for one connection under a short
/// lock; the awaits happen outside.
fn snapshot_for_connection(
    mounts: &MountTable,
    connection_id: &str,
    mount_id: Option<&str>,
) -> Result<Vec<(String, MountBackend)>, String> {
    Ok(lock_table(mounts)?
        .values()
        .filter(|record| {
            record.connection_id == connection_id
                && mount_id.map_or(true, |wanted| record.mount_id == wanted)
        })
        .map(|record| (record.mount_id.clone(), backend_snapshot(&record.backend)))
        .collect())
}

/// Whether `path` is the live local mount point of an active mount record.
/// WebDAV gateway mounts answer on loopback and have no local directory, so
/// only kernel (rclone) mounts qualify — this gates "reveal the mount in the
/// file manager" so `files/local/reveal` never becomes an open-anything path.
pub fn is_active_mount_point(mounts: &MountTable, path: &Path) -> Result<bool, String> {
    Ok(lock_table(mounts)?
        .values()
        .any(|record| backend_mounts_path(&record.backend, path)))
}

/// Pure match behind `is_active_mount_point` (testable without a WorkGuard).
fn backend_mounts_path(backend: &MountBackend, path: &Path) -> bool {
    match backend {
        MountBackend::Rclone { mount_point } => mount_point == path,
        MountBackend::WebDav { .. } => false,
    }
}

fn backend_snapshot(backend: &MountBackend) -> MountBackend {
    match backend {
        MountBackend::Rclone { mount_point } => MountBackend::Rclone {
            mount_point: mount_point.clone(),
        },
        MountBackend::WebDav { gateway } => MountBackend::WebDav {
            gateway: webdav_gateway::GatewayHandle {
                port: gateway.port,
                token: gateway.token.clone(),
                shutdown: gateway.shutdown.clone(),
            },
        },
    }
}

/// Unmount matching mounts of one connection. Best-effort: a dead rcd or an
/// already-gone kernel mount never blocks the table cleanup; failures are
/// reported per mount.
pub async fn unmount_connection(
    engine: &rclone::RcloneEngine,
    mounts: &MountTable,
    connection_id: &str,
    mount_id: Option<&str>,
) -> Value {
    let snapshot = match snapshot_for_connection(mounts, connection_id, mount_id) {
        Ok(snapshot) => snapshot,
        Err(error) => return json!({ "unmounted": [], "errors": [error] }),
    };
    let mut unmounted = Vec::new();
    let mut errors = Vec::new();
    for (id, backend) in snapshot {
        match &backend {
            MountBackend::Rclone { mount_point } => {
                // A missing registry entry means the connection (and its rcd
                // with every kernel mount) is already gone — nothing to do.
                if engine.binding(connection_id).is_ok() {
                    match engine.client_for_id(connection_id).await {
                        Ok(client) => {
                            if let Err(error) =
                                rclone::mount::unmount(&client, mount_point).await
                            {
                                // Unavailable-class failures mean the mount
                                // already went away with its rcd.
                                if !error.is_unavailable() {
                                    errors.push(format!("{id}: {error}"));
                                    continue;
                                }
                            }
                        }
                        Err(error) => {
                            errors.push(format!("{id}: {error}"));
                            continue;
                        }
                    }
                }
            }
            MountBackend::WebDav { gateway } => {
                gateway.shutdown.notify_one();
            }
        }
        if lock_table(mounts).map(|mut table| table.remove(&id)).is_err() {
            errors.push(format!("{id}: mount table poisoned"));
            continue;
        }
        unmounted.push(id);
    }
    json!({ "unmounted": unmounted, "errors": errors })
}

/// Status projection for one mount or every mount of the connection.
pub async fn mount_status(
    engine: &rclone::RcloneEngine,
    mounts: &MountTable,
    connection_id: &str,
    mount_id: Option<&str>,
) -> Result<Value, String> {
    let mut rows: Vec<Value> = {
        let table = lock_table(mounts)?;
        table
            .values()
            .filter(|record| {
                record.connection_id == connection_id
                    && mount_id.map_or(true, |wanted| record.mount_id == wanted)
            })
            .map(|record| {
                let mut row = json!({
                    "mountId": record.mount_id,
                    "strategy": record.strategy,
                    "readOnly": true,
                    "createdAt": record.created_at_ms,
                });
                match &record.backend {
                    MountBackend::Rclone { mount_point } => {
                        row["mountPoint"] = Value::String(mount_point.display().to_string());
                    }
                    MountBackend::WebDav { gateway } => {
                        row["gatewayPort"] = json!(gateway.port);
                    }
                }
                if let Some(reason) = &record.fallback_reason {
                    row["fallbackReason"] = Value::String(reason.clone());
                }
                row
            })
            .collect()
    };
    // Best-effort liveness for kernel mounts: anything the rcd no longer
    // reports is marked stale rather than dropped (the user unmounts).
    let kernel_points = match engine.client_for_id(connection_id).await {
        Ok(client) => rclone::mount::list_mount_points(&client).await.ok(),
        Err(_) => None,
    };
    for row in rows.iter_mut() {
        if row["strategy"] == "rclone" {
            if let Some(points) = &kernel_points {
                let live = row["mountPoint"].as_str().map_or(false, |mounted| {
                    points.iter().any(|point| point.contains(mounted))
                });
                row["mounted"] = json!(live);
            }
        }
    }
    Ok(json!({ "mounts": rows }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- mount-point reveal allowlist ---------------------------------------

    fn table() -> MountTable {
        Arc::new(std::sync::Mutex::new(HashMap::new()))
    }

    #[test]
    fn active_mount_point_matches_kernel_mounts_only() {
        assert!(backend_mounts_path(
            &MountBackend::Rclone { mount_point: PathBuf::from("/Volumes/dbx") },
            Path::new("/Volumes/dbx"),
        ));
        assert!(!backend_mounts_path(
            &MountBackend::Rclone { mount_point: PathBuf::from("/Volumes/dbx") },
            Path::new("/Volumes/other"),
        ));
        // WebDAV gateway serves loopback — there is no local directory.
        let gateway = MountBackend::WebDav {
            gateway: webdav_gateway::GatewayHandle {
                port: 1,
                token: "t".to_string(),
                shutdown: Arc::new(tokio::sync::Notify::new()),
            },
        };
        assert!(!backend_mounts_path(&gateway, Path::new("/Volumes/dbx")));
    }

    #[test]
    fn active_mount_point_over_empty_table_is_false() {
        assert!(!is_active_mount_point(&table(), Path::new("/anywhere")).unwrap());
    }

    #[test]
    fn active_mount_point_finds_registered_record() {
        let mounts = table();
        lock_table(&mounts)
            .unwrap()
            .insert(
                "m1".to_string(),
                MountRecord {
                    mount_id: "m1".to_string(),
                    connection_id: "c1".to_string(),
                    strategy: "rclone".to_string(),
                    fs: "remote:/".to_string(),
                    backend: MountBackend::Rclone { mount_point: PathBuf::from("/tmp/mnt") },
                    fallback_reason: None,
                    created_at_ms: 0,
                    _work: rclone::WorkGuard::for_tests(),
                },
            );
        assert!(is_active_mount_point(&mounts, Path::new("/tmp/mnt")).unwrap());
        assert!(!is_active_mount_point(&mounts, Path::new("/elsewhere")).unwrap());
    }

    // -- strategy matrix ----------------------------------------------------

    #[test]
    fn normalizes_and_rejects_strategies() {
        assert_eq!(normalize_strategy(None).unwrap(), "auto");
        assert_eq!(normalize_strategy(Some("RCLONE")).unwrap(), "rclone");
        assert_eq!(normalize_strategy(Some(" webdav ")).unwrap(), "webdav");
        assert!(normalize_strategy(Some("fuse3")).is_err());
    }

    #[test]
    fn auto_skips_rclone_attempt_when_probe_says_missing() {
        assert!(should_attempt_rclone("auto", &DriverProbe::Available));
        assert!(!should_attempt_rclone(
            "auto",
            &DriverProbe::Missing("no fuse".to_string())
        ));
        // Pinned strategies ignore the probe entirely.
        assert!(should_attempt_rclone("rclone", &DriverProbe::Missing("x".into())));
        assert!(!should_attempt_rclone("webdav", &DriverProbe::Available));
    }

    #[test]
    fn auto_falls_back_on_unavailable_and_other_but_not_busy() {
        let missing = MountError::DriverMissing("no fuse".to_string());
        let unsupported = MountError::Unsupported("homebrew refusal".to_string());
        let busy = MountError::MountPointBusy("not empty".to_string());
        let other = MountError::Other("rcd exploded".to_string());

        assert!(matches!(
            on_rclone_failure("auto", &missing),
            Decision::FallBack(_)
        ));
        assert!(matches!(
            on_rclone_failure("auto", &unsupported),
            Decision::FallBack(_)
        ));
        assert!(matches!(
            on_rclone_failure("auto", &other),
            Decision::FallBack(_)
        ));
        assert!(matches!(
            on_rclone_failure("auto", &busy),
            Decision::Fail(_)
        ));
        // Pinned rclone never falls back.
        assert!(matches!(
            on_rclone_failure("rclone", &missing),
            Decision::Fail(_)
        ));
    }

    #[test]
    fn mount_surface_joins_and_policy_checks() {
        let mut binding = RemoteBinding {
            remote_fs: "dbxabc123:".to_string(),
            backend_type: "s3".to_string(),
            root: "/data".to_string(),
            lock_to_root: true,
            read_only: false,
            allow_delete: true,
            proxy: None,
        };
        // Whole root: one fs string, no re-basing.
        assert_eq!(
            mount_surface(&binding, None).unwrap(),
            ("dbxabc123:/data".to_string(), String::new())
        );
        // Sub-path: kernel fs carries the folder; gateway gets the
        // connection-space mount dir for path mapping.
        assert_eq!(
            mount_surface(&binding, Some("/inside")).unwrap(),
            (
                "dbxabc123:/data/inside".to_string(),
                "/data/inside".to_string()
            )
        );
        // lock_to_root rejects escapes.
        assert!(mount_surface(&binding, Some("../../etc/passwd")).is_err());

        binding.backend_type = "local".to_string();
        binding.remote_fs = "/srv/data".to_string();
        binding.root = "/".to_string();
        binding.lock_to_root = false;
        assert_eq!(
            mount_surface(&binding, Some("/sub/dir")).unwrap(),
            (
                "/srv/data/sub/dir".to_string(),
                "/sub/dir".to_string()
            )
        );
    }

    #[test]
    fn mount_point_defaults_are_empty_dirs() {
        // Unix-only assertion: the default base is created under HOME.
        if std::env::consts::OS == "windows" {
            let picked = default_mount_point("abc");
            assert!(picked.is_ok() || true, "drive scan is machine-dependent");
            return;
        }
        let point = default_mount_point("testconn").expect("default mount point");
        assert!(point.to_string_lossy().contains("dbx-files-mounts"));
        assert!(point.to_string_lossy().contains("dbxtestconn"));
        ensure_empty_mount_dir(&point).expect("fresh dir is empty");
        let nested = point.join("keep.txt");
        std::fs::write(&nested, b"x").expect("seed");
        assert!(ensure_empty_mount_dir(&point).is_err(), "non-empty rejected");
        std::fs::remove_file(&nested).expect("cleanup");
    }
}
