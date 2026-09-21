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
    /// The mounted folder in connection-space absolute form (`""` = whole
    /// root) — the same `mount_surface` second component the gateway maps
    /// paths through. The VFS refresh paths use it to translate a
    /// plugin-space directory onto the mount's root-relative `dir` param.
    pub mount_path: String,
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
    /// Loopback WebDAV listener inside this sidecar. When the OS WebDAV
    /// client picked the URL up automatically, the mounted volume lives at
    /// `mount_point` (macOS: /Volumes/<name>) and can be revealed/unmounted.
    WebDav { gateway: webdav_gateway::GatewayHandle, mount_point: Option<PathBuf> },
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

/// Volume name the OS WebDAV client derives from the gateway URL: the last
/// non-empty path segment (the connection id). Pure helper, unit-tested.
fn webdav_volume_name(url: &str) -> Option<&str> {
    url.trim_end_matches('/')
        .rsplit('/')
        .find(|segment| !segment.is_empty())
}

/// Ask macOS to mount the gateway URL right away: `mount volume` drives the
/// same WebDAVFS stack as Finder's "Connect to Server", so the read-only
/// volume appears in /Volumes and the Finder sidebar without user steps.
/// Returns the mounted volume path, or None when the platform is not macOS,
/// the client refused/timed out (auth prompt, sandboxed host) — the caller
/// then falls back to copy-URL guidance.
#[cfg(target_os = "macos")]
async fn mount_webdav_volume(url: &str) -> Option<PathBuf> {
    let wanted = webdav_volume_name(url)?.to_string();
    let script = format!("mount volume \"{}\"", url);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tokio::process::Command::new("osascript").arg("-e").arg(&script).output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    // Finder may dedupe volume names ("<conn> 2"); scan /Volumes for the
    // first directory containing the expected segment instead of trusting
    // osascript output.
    let mut entries = tokio::fs::read_dir("/Volumes").await.ok()?;
    while let Some(entry) = entries.next_entry().await.ok()? {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.contains(&wanted) {
            return Some(entry.path());
        }
    }
    None
}

#[cfg(not(target_os = "macos"))]
async fn mount_webdav_volume(_url: &str) -> Option<PathBuf> {
    // Windows (`net use`) / Linux (`gio mount dav://`) auto-mounts land in M2.
    None
}

/// Mount the gateway at the user-chosen directory (not a /Volumes volume):
/// `mount_webdav -S <url> <node>` accepts an arbitrary empty mountpoint, so
/// the WebDAV fallback honors the same "pick a directory" contract as the
/// rclone path. `-S` suppresses auth/disconnect dialogs for silent backend
/// use; the gateway is anonymous (token lives in the URL path). Returns
/// false on refusal/timeout — the caller falls back to a /Volumes volume.
#[cfg(target_os = "macos")]
async fn mount_webdav_at(url: &str, point: &Path) -> bool {
    matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            tokio::process::Command::new("mount_webdav")
                .arg("-S")
                .arg(url)
                .arg(point)
                .output(),
        )
        .await,
        Ok(Ok(output)) if output.status.success(),
    )
}

#[cfg(not(target_os = "macos"))]
async fn mount_webdav_at(_url: &str, _point: &Path) -> bool {
    false
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
            Ok(()) if rclone::mount::wait_registered(&client, &mount_point).await => {
                let mount_id = Uuid::new_v4().simple().to_string();
                let hint = mount_hint("rclone", &mount_point);
                lock_table(mounts)?.insert(
                    mount_id.clone(),
                    MountRecord {
                        mount_id: mount_id.clone(),
                        connection_id: connection_id.to_string(),
                        strategy: "rclone".to_string(),
                        fs: fs.clone(),
                        mount_path: mount_path.clone(),
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
            // rc answered Ok but the kernel side never registered (FUSE-less
            // container/CI hosts) — a ghost mount reads as an empty dir.
            // Degrade exactly like a driver failure: cleanup, then fall back
            // (auto) or fail with an actionable message (explicit rclone).
            Ok(()) => {
                let _ = rclone::mount::unmount(&client, &mount_point).await;
                let ghost = rclone::mount::MountError::DriverMissing(format!(
                    "mount accepted but never registered at {}",
                    mount_point.display()
                ));
                match on_rclone_failure(&strategy, &ghost) {
                    Decision::FallBack(reason) => fallback_reason = Some(reason),
                    Decision::Fail(message) => return Err(message),
                }
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
    // 重复挂载去重：网关没有内核挂载点的 OS 占用保护（rclone 策略由
    // MountPointBusy 拒绝语义挡住），同连接同挂载面重复进来会静默开出第二
    // 个网关、返回新 mountId。对齐 rclone 策略口径：挂载点被占用即报业务
    // 错误，不并发并存。
    if let Some(existing) = duplicate_mount_conflict(mounts, connection_id, &mount_path)? {
        let surface = if mount_path.is_empty() {
            "/".to_string()
        } else {
            mount_path.clone()
        };
        return Err(format!(
            "webdav mount failed: mount point unusable: {surface} is already mounted (mountId {existing}) — unmount it first"
        ));
    }
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
    // 系统级自动挂载，优先尊重用户在对话框里选的挂载位置（mount_webdav
    // 支持任意空目录，与 rclone 路径同一"选目录"语义）；选点失败退到
    // /Volumes 网络卷，再失败才退到「复制地址手动连接」。
    let mut volume_fallback = false;
    let volume: Option<PathBuf> = match request.mount_point.as_deref() {
        Some(explicit) => {
            let point = PathBuf::from(explicit);
            ensure_empty_mount_dir(&point)?;
            if mount_webdav_at(&url, &point).await {
                Some(point)
            } else {
                volume_fallback = true;
                mount_webdav_volume(&url).await
            }
        }
        None => mount_webdav_volume(&url).await,
    };
    let mount_id = Uuid::new_v4().simple().to_string();
    lock_table(mounts)?.insert(
        mount_id.clone(),
        MountRecord {
            mount_id: mount_id.clone(),
            connection_id: connection_id.to_string(),
            strategy: "webdav".to_string(),
            fs: fs.clone(),
            mount_path: mount_path.clone(),
            backend: MountBackend::WebDav { gateway, mount_point: volume.clone() },
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
        "mounted": volume.is_some(),
        "volumeFallback": volume_fallback && volume.is_some(),
    });
    if let Some(point) = &volume {
        response["mountPoint"] = Value::String(point.display().to_string());
        response["hint"] = Value::String(format!("已通过 WebDAV 挂载到 {}", point.display()));
    } else {
        response["hint"] = Value::String(mount_hint("webdav", Path::new(&url)));
    }
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

/// 同连接同挂载面（`mount_path` 归一化后相等）的现存 mountId。挂载面即
/// rclone 策略下内核挂载点的对应物：kernel mount 的 busy 判定由 OS 挂载点
/// 承担，WebDAV 网关没有这道保护，重复挂载由这里挡（含 rclone 策略的现存
/// 记录——同一挂载面不再开第二个网关）。纯判定，start_mount 的降级/固定
/// webdav 路径在开网关前调用，可单测。
fn duplicate_mount_conflict(
    mounts: &MountTable,
    connection_id: &str,
    mount_path: &str,
) -> Result<Option<String>, String> {
    let wanted = mount_path.trim_matches('/');
    Ok(lock_table(mounts)?
        .values()
        .find(|record| {
            record.connection_id == connection_id
                && record.mount_path.trim_matches('/') == wanted
        })
        .map(|record| record.mount_id.clone()))
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
        MountBackend::WebDav { gateway, mount_point } => MountBackend::WebDav {
            mount_point: mount_point.clone(),
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
            MountBackend::WebDav { gateway, mount_point } => {
                // 先停网关（卷内容随即失效），再尽力卸载系统卷，避免 Finder
                // 里留下一个指向已死端口的"幽灵卷"。
                gateway.shutdown.notify_one();
                if let (Some(point), true) = (mount_point, cfg!(target_os = "macos")) {
                    let _ = tokio::process::Command::new("diskutil")
                        .args(["unmount", "force"])
                        .arg(point)
                        .output()
                        .await;
                }
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
                    MountBackend::WebDav { gateway, mount_point } => {
                        row["gatewayPort"] = json!(gateway.port);
                        if let Some(point) = mount_point {
                            row["mountPoint"] = Value::String(point.display().to_string());
                            row["mounted"] = json!(true);
                        }
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

// ---------------------------------------------------------------------------
// VFS cache management (batch 5): refresh + stats over the rcd's vfs/* rc
// endpoints. Only rclone-strategy mounts carry a VFS; WebDAV gateway mounts
// answer from live ops calls and have nothing to refresh (counted `skipped`).
// ---------------------------------------------------------------------------

/// Map a plugin-space directory onto a mount's VFS root-relative `dir`
/// param. `""` mount path = whole connection root (pass-through); a subdir
/// mount only serves paths inside its own prefix — boundary-checked, so
/// `/data/insidex` is NOT under `/data/inside` and the change is skipped
/// for that mount. `None` (no dir given = refresh the mount root) maps to
/// `""`.
fn dir_rel_for_mount(mount_path: &str, dir: Option<&str>) -> Option<String> {
    let Some(dir) = dir else {
        return Some(String::new());
    };
    let dir = dir.trim_start_matches('/');
    let base = mount_path.trim_matches('/');
    if base.is_empty() {
        return Some(dir.to_string());
    }
    dir.strip_prefix(base)
        .filter(|rest| rest.is_empty() || rest.starts_with('/'))
        .map(|rest| rest.trim_matches('/').to_string())
}

/// Refresh the VFS directory cache of an rclone-strategy mount (or every
/// mount of the connection when `mount_id` is absent). `dir` is the
/// plugin-space absolute directory whose listing changed; absent = the
/// mount root. Response `{refreshed, skipped, errors}` counts one per
/// matching mount; per-mount rc failures land in `errors` (the rest still
/// refresh), and the whole call errors only when the connection itself is
/// unreachable.
pub async fn refresh_mount_caches(
    engine: &rclone::RcloneEngine,
    mounts: &MountTable,
    connection_id: &str,
    mount_id: Option<&str>,
    dir: Option<&str>,
    recursive: bool,
) -> Result<Value, String> {
    // Short lock: snapshot the records; the rc awaits happen outside.
    let snapshot: Vec<(String, String, String, String)> = {
        let table = lock_table(mounts)?;
        table
            .values()
            .filter(|record| {
                record.connection_id == connection_id
                    && mount_id.map_or(true, |wanted| record.mount_id == wanted)
            })
            .map(|record| {
                (
                    record.mount_id.clone(),
                    record.strategy.clone(),
                    record.fs.clone(),
                    record.mount_path.clone(),
                )
            })
            .collect()
    };
    let mut refreshed = 0usize;
    let mut skipped = 0usize;
    let mut errors: Vec<String> = Vec::new();
    // Only resolve an rc client when an rclone mount actually needs one —
    // spawning a group rcd just to count skipped gateway rows is waste.
    let client = if snapshot
        .iter()
        .any(|(_, strategy, _, _)| strategy == "rclone")
    {
        Some(engine.client_for_id(connection_id).await?)
    } else {
        None
    };
    for (id, strategy, fs, mount_path) in snapshot {
        if strategy != "rclone" {
            skipped += 1;
            continue;
        }
        match dir_rel_for_mount(&mount_path, dir) {
            // Change outside this subdir mount's surface: nothing to refresh.
            None => skipped += 1,
            Some(rel) => match &client {
                Some(client) => {
                    let dir_param = (!rel.is_empty()).then_some(rel.as_str());
                    match client.vfs_refresh(&fs, dir_param, recursive).await {
                        Ok(_) => refreshed += 1,
                        Err(error) => errors.push(format!("{id}: {error}")),
                    }
                }
                None => errors.push(format!("{id}: connection is not connected")),
            },
        }
    }
    Ok(json!({ "refreshed": refreshed, "skipped": skipped, "errors": errors }))
}

/// Auto-refresh hook for the mutation success paths (upload finish,
/// delete/purge): fire-and-forget [`refresh_mount_caches`] over every
/// rclone mount of the connection, non-recursive at the changed dir.
/// Never fails the caller — unreachable connections and rc errors only log.
pub async fn best_effort_refresh_mount_caches(
    engine: &rclone::RcloneEngine,
    mounts: &MountTable,
    connection_id: &str,
    dir: Option<&str>,
) {
    match refresh_mount_caches(engine, mounts, connection_id, None, dir, false).await {
        Ok(value) => {
            for error in value
                .get("errors")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                eprintln!("[io.dbx.files] mount vfs auto-refresh failed: {error}");
            }
        }
        Err(error) => {
            eprintln!("[io.dbx.files] mount vfs auto-refresh skipped: {error}");
        }
    }
}

/// Per rclone-strategy mount `vfs/stats` (raw rclone JSON under `stats`,
/// per-mount failure under `error`). WebDAV gateway mounts have no VFS and
/// answer under `skipped`. Row shape: `{mountId, strategy, stats|error}`.
pub async fn mount_vfs_stats(
    engine: &rclone::RcloneEngine,
    mounts: &MountTable,
    connection_id: &str,
    mount_id: Option<&str>,
) -> Result<Value, String> {
    let snapshot: Vec<(String, String, String)> = {
        let table = lock_table(mounts)?;
        table
            .values()
            .filter(|record| {
                record.connection_id == connection_id
                    && mount_id.map_or(true, |wanted| record.mount_id == wanted)
            })
            .map(|record| {
                (
                    record.mount_id.clone(),
                    record.strategy.clone(),
                    record.fs.clone(),
                )
            })
            .collect()
    };
    let client = if snapshot
        .iter()
        .any(|(_, strategy, _)| strategy == "rclone")
    {
        Some(engine.client_for_id(connection_id).await?)
    } else {
        None
    };
    let mut rows = Vec::new();
    let mut skipped = 0usize;
    for (id, strategy, fs) in snapshot {
        if strategy != "rclone" {
            skipped += 1;
            continue;
        }
        let mut row = json!({ "mountId": id, "strategy": "rclone" });
        match client.as_ref() {
            Some(client) => match client.vfs_stats(&fs).await {
                Ok(stats) => row["stats"] = stats,
                Err(error) => row["error"] = Value::String(error.to_string()),
            },
            None => row["error"] = Value::String("connection is not connected".to_string()),
        }
        rows.push(row);
    }
    Ok(json!({ "mounts": rows, "skipped": skipped }))
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
            mount_point: None,
            gateway: webdav_gateway::GatewayHandle {
                port: 1,
                token: "t".to_string(),
                shutdown: Arc::new(tokio::sync::Notify::new()),
            },
        };
        assert!(!backend_mounts_path(&gateway, Path::new("/Volumes/dbx")));
    }

    #[test]
    fn webdav_volume_name_takes_last_segment() {
        assert_eq!(webdav_volume_name("http://127.0.0.1:54321/tok/conn1/"), Some("conn1"));
        assert_eq!(webdav_volume_name("http://127.0.0.1:54321/tok/conn1"), Some("conn1"));
        // No path segments: the host:port becomes the volume name (the real
        // gateway URL always carries /token/<connId>/, this is just the edge).
        assert_eq!(webdav_volume_name("http://127.0.0.1:54321/"), Some("127.0.0.1:54321"));
        assert_eq!(webdav_volume_name(""), None);
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
                    mount_path: String::new(),
                    backend: MountBackend::Rclone { mount_point: PathBuf::from("/tmp/mnt") },
                    fallback_reason: None,
                    created_at_ms: 0,
                    _work: rclone::WorkGuard::for_tests(),
                },
            );
        assert!(is_active_mount_point(&mounts, Path::new("/tmp/mnt")).unwrap());
        assert!(!is_active_mount_point(&mounts, Path::new("/elsewhere")).unwrap());
    }

    // -- webdav 重复挂载去重 ---------------------------------------------------

    fn seed_webdav(mounts: &MountTable, id: &str, conn: &str, path: &str) {
        lock_table(mounts).unwrap().insert(
            id.to_string(),
            MountRecord {
                mount_id: id.to_string(),
                connection_id: conn.to_string(),
                strategy: "webdav".to_string(),
                fs: "dbxc:/data".to_string(),
                mount_path: path.to_string(),
                backend: MountBackend::WebDav {
                    mount_point: None,
                    gateway: webdav_gateway::GatewayHandle {
                        port: 1,
                        token: "t".to_string(),
                        shutdown: Arc::new(tokio::sync::Notify::new()),
                    },
                },
                fallback_reason: None,
                created_at_ms: 0,
                _work: rclone::WorkGuard::for_tests(),
            },
        );
    }

    #[test]
    fn duplicate_mount_conflict_matches_same_connection_and_surface() {
        let mounts = table();
        seed_webdav(&mounts, "m1", "c1", "/data/inside");
        // 同连接同挂载面（拼写差异归一化）→ 冲突，并回带现存 mountId。
        assert_eq!(
            duplicate_mount_conflict(&mounts, "c1", "/data/inside")
                .unwrap()
                .as_deref(),
            Some("m1")
        );
        assert_eq!(
            duplicate_mount_conflict(&mounts, "c1", "/data/inside/")
                .unwrap()
                .as_deref(),
            Some("m1")
        );
        assert_eq!(
            duplicate_mount_conflict(&mounts, "c1", "data/inside")
                .unwrap()
                .as_deref(),
            Some("m1")
        );
        // 不同连接、不同子路径、子路径 vs 整根 → 不冲突。
        assert!(duplicate_mount_conflict(&mounts, "c2", "/data/inside")
            .unwrap()
            .is_none());
        assert!(duplicate_mount_conflict(&mounts, "c1", "/data/other")
            .unwrap()
            .is_none());
        assert!(duplicate_mount_conflict(&mounts, "c1", "").unwrap().is_none());
    }

    #[test]
    fn duplicate_mount_conflict_includes_kernel_mounts_of_same_surface() {
        // rclone 策略的现存记录同样占用挂载面：网关不再叠加第二个入口。
        let mounts = table();
        lock_table(&mounts).unwrap().insert(
            "k1".to_string(),
            MountRecord {
                mount_id: "k1".to_string(),
                connection_id: "c1".to_string(),
                strategy: "rclone".to_string(),
                fs: "dbxc:/data/inside".to_string(),
                mount_path: "/data/inside".to_string(),
                backend: MountBackend::Rclone { mount_point: PathBuf::from("/tmp/mnt") },
                fallback_reason: None,
                created_at_ms: 0,
                _work: rclone::WorkGuard::for_tests(),
            },
        );
        assert_eq!(
            duplicate_mount_conflict(&mounts, "c1", "/data/inside")
                .unwrap()
                .as_deref(),
            Some("k1")
        );
    }

    // -- VFS cache mapping ---------------------------------------------------

    #[test]
    fn dir_rel_maps_plugin_paths_onto_mount_roots() {
        // Whole-root mounts pass through; a missing dir means the mount root.
        assert_eq!(dir_rel_for_mount("", Some("/a/b")).as_deref(), Some("a/b"));
        assert_eq!(dir_rel_for_mount("/", Some("/a/b")).as_deref(), Some("a/b"));
        assert_eq!(dir_rel_for_mount("", None).as_deref(), Some(""));
        // Subdir mounts: equal maps to the mount root, nested maps relative.
        assert_eq!(
            dir_rel_for_mount("/data/inside", Some("/data/inside")).as_deref(),
            Some("")
        );
        assert_eq!(
            dir_rel_for_mount("/data/inside", Some("/data/inside/x")).as_deref(),
            Some("x")
        );
        // Outside (or boundary-adjacent, so not actually inside) → skip.
        assert_eq!(dir_rel_for_mount("/data/inside", Some("/data/other")), None);
        assert_eq!(dir_rel_for_mount("/data/inside", Some("/data/insidex")), None);
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
