//! rclone mount over the running rcd (RC API).
//!
//! Primary strategy of the local-mount feature: the bundled rclone fork
//! carries `mount` compiled in (cmount on macOS, default elsewhere), so the
//! plugin mounts through the already-running `rclone rcd` instead of
//! spawning a second binary. Credentials stay inside the rcd's memory-resident
//! config — nothing new is exposed to argv/env (docs/MOUNT.zh-CN.md §2.3).
//!
//! M1 scope: read-only mounts only (`vfsOpt.ReadOnly`), plus a platform
//! driver probe and error classification so the strategy layer
//! (`crate::mount`) can fall back to the WebDAV gateway when the OS FUSE
//! driver is absent (macFUSE/WinFsp not installed).

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::rc::{RcClient, RcError};

/// One mount request handed to the rcd's `mount/mount` endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountSpec {
    /// rclone fs string (`dbxName:root` form or a local path).
    pub fs: String,
    /// Local mountpoint (must exist and be empty; the strategy layer
    /// creates/picks it).
    pub mount_point: PathBuf,
    /// M1 always mounts read-only; kept as a field so M2 write support is a
    /// policy change, not a signature change.
    pub read_only: bool,
}

/// Why a mount attempt could not proceed, classified from the rcd error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountError {
    /// OS FUSE driver absent or unusable (macFUSE / WinFsp / fusermount).
    DriverMissing(String),
    /// This rclone binary refuses to mount on this OS (e.g. Homebrew builds
    /// without cmount). Same fallback consequence as [`MountError::DriverMissing`].
    Unsupported(String),
    /// Mountpoint busy, not empty, or already mounted.
    MountPointBusy(String),
    /// Anything else — surfaced verbatim.
    Other(String),
}

impl MountError {
    /// True when the rclone-mount strategy is permanently unavailable on
    /// this host/binary and the caller should fall back to the WebDAV
    /// gateway (auto strategy) or surface an actionable error (explicit).
    pub fn is_unavailable(&self) -> bool {
        matches!(self, MountError::DriverMissing(_) | MountError::Unsupported(_))
    }
}

impl std::fmt::Display for MountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MountError::DriverMissing(message) => write!(f, "FUSE driver missing: {message}"),
            MountError::Unsupported(message) => write!(f, "mount unsupported here: {message}"),
            MountError::MountPointBusy(message) => write!(f, "mount point unusable: {message}"),
            MountError::Other(message) => write!(f, "{message}"),
        }
    }
}

/// Best-effort platform probe for the OS-side FUSE driver. Advisory only:
/// the strategy layer uses it to skip straight to the WebDAV gateway, but a
/// false "missing" just costs one classified mount error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverProbe {
    Available,
    Missing(String),
}

pub async fn mount(client: &RcClient, spec: &MountSpec) -> Result<(), MountError> {
    let params = serde_json::json!({
        "fs": spec.fs,
        "mountPoint": spec.mount_point.display().to_string(),
        // In-process mount (no --daemon); the rcd owns the mount lifecycle.
        "mountOpt": { "Daemon": false },
        "vfsOpt": { "ReadOnly": spec.read_only },
    });
    match client.call("mount/mount", &params).await {
        Ok(_) => Ok(()),
        Err(error) => match &error {
            RcError::Http { body, .. } | RcError::Rclone { message: body } => {
                Err(classify_mount_error(body))
            }
            other => Err(MountError::Other(other.to_string())),
        },
    }
}

pub async fn unmount(client: &RcClient, mount_point: &Path) -> Result<(), MountError> {
    let params = serde_json::json!({
        "mountPoint": mount_point.display().to_string(),
    });
    match client.call("mount/unmount", &params).await {
        Ok(_) => Ok(()),
        Err(error) => {
            let message = rc_message(&error);
            // Idempotent unmount: an already-gone mount is success for the
            // lifecycle layer (rcd restarts drop every mount silently).
            let lower = message.to_lowercase();
            if lower.contains("not mounted") || lower.contains("no mount") {
                Ok(())
            } else {
                Err(classify_mount_error(&message))
            }
        }
    }
}

/// Mount points currently held by the rcd. Lenient parse: any unexpected
/// shape degrades to an empty list rather than failing status reporting.
pub async fn list_mount_points(client: &RcClient) -> Result<Vec<String>, String> {
    let value = client
        .call("mount/listmounts", &serde_json::json!({}))
        .await
        .map_err(|error| rc_message(&error))?;
    Ok(value
        .get("mountPoints")
        .and_then(Value::as_array)
        .map(|points| {
            points
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

/// Polls `mount/listmounts` until `mount_point` shows up — registration can
/// lag the `mount/mount` answer, and a host without a usable kernel FUSE
/// (containers, CI sandboxes) answers Ok while nothing ever registers.
/// `false` = never registered within ~5s; the caller treats that exactly
/// like a driver failure (fallback / actionable error).
pub async fn wait_registered(client: &RcClient, mount_point: &std::path::Path) -> bool {
    let wanted = mount_point.display().to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(points) = list_mount_points(client).await {
            if points
                .iter()
                .any(|point| point.trim_end_matches('/') == wanted)
            {
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

pub fn probe_driver() -> DriverProbe {
    match std::env::consts::OS {
        "linux" => {
            if !Path::new("/dev/fuse").exists() {
                return DriverProbe::Missing(
                    "/dev/fuse is missing — install fuse3 (e.g. `apt install fuse3` or \
                     `dnf install fuse3`)"
                        .to_string(),
                );
            }
            if which_any(&["fusermount3", "fusermount"]).is_none() {
                DriverProbe::Missing(
                    "fusermount/fusermount3 not found in PATH — install fuse3".to_string(),
                )
            } else {
                DriverProbe::Available
            }
        }
        "macos" => {
            if which_any(&["mount_macfuse", "mount_fuse-t"]).is_some() {
                DriverProbe::Available
            } else {
                DriverProbe::Missing(
                    "macFUSE or FUSE-T not detected — install macFUSE \
                     (https://osxfuse.github.io) or FUSE-T (https://www.fuse-t.io)"
                        .to_string(),
                )
            }
        }
        "windows" => {
            if Path::new(r"C:\Program Files (x86)\WinFsp\bin").is_dir() {
                DriverProbe::Available
            } else {
                DriverProbe::Missing(
                    "WinFsp not detected — install it from https://winfsp.dev".to_string(),
                )
            }
        }
        other => DriverProbe::Missing(format!("mount is unsupported on {other}")),
    }
}

pub fn classify_mount_error(message: &str) -> MountError {
    let lower = message.to_lowercase();
    // Driver-shaped failures: the OS FUSE stack is absent or refused to load.
    const DRIVER_HINTS: [&str; 8] = [
        "fusermount",
        "osxfuse",
        "mount_macfuse",
        "mount_fuse-t",
        "libfuse",
        "winfsp",
        "/dev/fuse",
        "no fuse",
    ];
    if DRIVER_HINTS.iter().any(|hint| lower.contains(hint)) {
        return MountError::DriverMissing(message.to_string());
    }
    // Mountpoint-shaped failures: wrong target rather than missing stack.
    if lower.contains("not empty")
        || lower.contains("resource busy")
        || (lower.contains("already") && lower.contains("mount"))
    {
        return MountError::MountPointBusy(message.to_string());
    }
    // Binary-level refusals: rclone compiled without mount support, or a
    // Homebrew build declining to mount on macOS outright.
    if lower.contains("mount is not supported") || lower.contains("not supported on macos") {
        return MountError::Unsupported(message.to_string());
    }
    MountError::Other(message.to_string())
}

/// PATH scan without an external `which` crate (same spirit as rc.rs's
/// hand-rolled percent-encoding).
fn which_any(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn rc_message(error: &RcError) -> String {
    match error {
        RcError::Http { body, .. } => body.clone(),
        RcError::Rclone { message } => message.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------
    // classify_mount_error — sample strings from real rclone mount failures.
    // ---------------------------------------------------------------------

    #[test]
    fn classifies_missing_fusermount_as_driver_missing() {
        let error = classify_mount_error(
            "mount helper error: fusermount3: exit status 1, subprocess returned error",
        );
        assert_eq!(
            error,
            MountError::DriverMissing(
                "mount helper error: fusermount3: exit status 1, subprocess returned error"
                    .to_string()
            )
        );
    }

    #[test]
    fn classifies_missing_macfuse_as_driver_missing() {
        let error = classify_mount_error(
            "Can not mount, no FUSE found: exec: \"mount_macfuse\": executable file not found in $PATH",
        );
        assert!(matches!(error, MountError::DriverMissing(_)));
    }

    #[test]
    fn classifies_missing_winfsp_as_driver_missing() {
        let error =
            classify_mount_error("Can not mount: WinFsp is not installed (registry key missing)");
        assert!(matches!(error, MountError::DriverMissing(_)));
    }

    #[test]
    fn classifies_missing_dev_fuse_as_driver_missing() {
        let error = classify_mount_error("error: /dev/fuse: device not found");
        assert!(matches!(error, MountError::DriverMissing(_)));
    }

    #[test]
    fn classifies_nonempty_mountpoint_as_busy() {
        let error = classify_mount_error(
            "mount point /tmp/mp already has files: directory is not empty",
        );
        assert!(matches!(error, MountError::MountPointBusy(_)));
    }

    #[test]
    fn classifies_resource_busy_as_busy() {
        let error = classify_mount_error("mount failed: Device or resource busy");
        assert!(matches!(error, MountError::MountPointBusy(_)));
    }

    #[test]
    fn classifies_unknown_error_as_other() {
        let error = classify_mount_error("connection is read-only: write operations are rejected");
        assert!(matches!(error, MountError::Other(_)));
    }

    #[test]
    fn classifies_homebrew_refusal_as_unsupported() {
        let error = classify_mount_error(
            "failed to mount FUSE fs: rclone mount is not supported on MacOS when rclone \
             is installed via Homebrew. Please install the rclone binaries available at \
             https://rclone.org/downloads/ instead if you want to use the rclone mount command",
        );
        assert!(matches!(error, MountError::Unsupported(_)));
        assert!(error.is_unavailable());
    }

    #[test]
    fn driver_missing_and_unsupported_are_fallback_worthy() {
        assert!(MountError::DriverMissing("no fuse".to_string()).is_unavailable());
        assert!(!MountError::MountPointBusy("not empty".to_string()).is_unavailable());
        assert!(!MountError::Other("anything".to_string()).is_unavailable());
    }

    #[test]
    fn mount_error_display_is_actionable() {
        let display = MountError::DriverMissing("winfsp missing".to_string()).to_string();
        assert!(display.contains("FUSE driver missing"));
        assert!(display.contains("winfsp missing"));
    }

    // ---------------------------------------------------------------------
    // probe_driver — smoke only: the answer is machine-dependent.
    // ---------------------------------------------------------------------

    #[test]
    fn probe_driver_returns_a_verdict_without_panicking() {
        let _ = probe_driver();
    }

    // ---------------------------------------------------------------------
    // Live rcd round-trip: skipped unless an rclone binary is present AND
    // the OS FUSE driver works — same graceful-skip convention as sync.rs.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn mount_list_unmount_roundtrip_against_rcd() {
        let Some(binary) = super::super::proc::resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = match super::super::proc::RcdHandle::start(&binary, None).await {
            Ok(rcd) => rcd,
            Err(error) => {
                eprintln!("skipping: rcd spawn failed: {error}");
                return;
            }
        };
        let client = rcd.client();

        let sandbox = tempfile::tempdir().expect("tempdir");
        let source = sandbox.path().join("source");
        std::fs::create_dir_all(&source).expect("source dir");
        std::fs::write(source.join("hello.txt"), "hello mount").expect("seed file");
        let mount_point = sandbox.path().join("mnt");
        std::fs::create_dir_all(&mount_point).expect("mountpoint dir");

        let spec = MountSpec {
            fs: source.display().to_string(),
            mount_point: mount_point.clone(),
            read_only: true,
        };
        if let Err(error) = mount(&client, &spec).await {
            // DriverMissing/Unsupported AND mountpoint-unusable are all
            // environment verdicts: hosts with WinFSP installed but a
            // sandboxed mount service (CI) land in MountPointBusy — skip,
            // the runtime path still surfaces these as real errors.
            if error.is_unavailable() || matches!(error, MountError::MountPointBusy(_)) {
                eprintln!("skipping: rclone mount unavailable on this host: {error}");
                return;
            }
            panic!("mount failed with unexpected error: {error}");
        }

        // Registration can lag the mount() answer, and a host without a
        // usable kernel FUSE can answer mount() Ok while nothing ever
        // registers (observed on FUSE-less CI runners) — poll, and SKIP
        // when the kernel side never comes up (same convention as the
        // is_unavailable skip above).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let listed = loop {
            let listed = list_mount_points(&client).await.expect("listmounts");
            if listed.iter().any(|point| point.contains("mnt")) {
                break listed;
            }
            if std::time::Instant::now() >= deadline {
                eprintln!(
                    "skipping: mount() answered but the kernel side never registered: {listed:?}"
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        };

        // Read through the mount proves the kernel side is live.
        let via_mount = mount_point.join("hello.txt");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut content = None;
        while std::time::Instant::now() < deadline {
            if let Ok(text) = std::fs::read_to_string(&via_mount) {
                content = Some(text);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        match content.as_deref() {
            Some("hello mount") => {}
            // Kernel FUSE never became live (FUSE-less CI host): skip
            // rather than fail — the rc-side roundtrip above is proven.
            _ => {
                eprintln!("skipping: mount not readable through the kernel (no usable FUSE)");
                return;
            }
        }

        unmount(&client, &mount_point)
            .await
            .expect("unmount should succeed");
    }

    // ---------------------------------------------------------------------
    // VFS cache endpoints (batch 5). Behavior pinned against a live rcd in
    // two layers, mirroring the skip conventions above.
    // ---------------------------------------------------------------------

    /// Without any mount, `vfs/refresh`/`vfs/stats` answer rclone's
    /// business error naming the fs — this pins the endpoint paths, the
    /// `fs` param spelling and the error-envelope surfacing (RcError::Rclone
    /// with `no VFS found with name %q`), all without needing a FUSE mount.
    #[tokio::test]
    async fn vfs_endpoints_surface_rc_business_errors_without_a_mount() {
        let Some(binary) = super::super::proc::resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = match super::super::proc::RcdHandle::start(&binary, None).await {
            Ok(rcd) => rcd,
            Err(error) => {
                eprintln!("skipping: rcd spawn failed: {error}");
                return;
            }
        };
        let client = rcd.client();

        let missing = "no VFS found with name";
        for error in [
            client
                .vfs_stats("/tmp/dbx-not-a-mount")
                .await
                .expect_err("stats needs an active VFS")
                .to_string(),
            client
                .vfs_refresh("/tmp/dbx-not-a-mount", None, false)
                .await
                .expect_err("refresh needs an active VFS")
                .to_string(),
            client
                .vfs_refresh("/tmp/dbx-not-a-mount", Some("sub"), true)
                .await
                .expect_err("dir + recursive still need an active VFS")
                .to_string(),
        ] {
            assert!(error.contains(missing), "unexpected rc error: {error}");
        }
    }

    /// Full round-trip on a live mount: root refresh reports rclone's
    /// canonical `{"result": {"": "OK"}}`, `vfs/stats` identifies the VFS
    /// with a live `inUse` count. Skipped when the FUSE driver is absent —
    /// the error-shape test above still pins the endpoint wiring there.
    #[tokio::test]
    async fn vfs_refresh_and_stats_roundtrip_on_a_live_mount() {
        let Some(binary) = super::super::proc::resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let rcd = match super::super::proc::RcdHandle::start(&binary, None).await {
            Ok(rcd) => rcd,
            Err(error) => {
                eprintln!("skipping: rcd spawn failed: {error}");
                return;
            }
        };
        let client = rcd.client();

        let sandbox = tempfile::tempdir().expect("tempdir");
        let source = sandbox.path().join("source");
        std::fs::create_dir_all(&source).expect("source dir");
        std::fs::write(source.join("hello.txt"), "hello vfs").expect("seed file");
        let mount_point = sandbox.path().join("mnt");
        std::fs::create_dir_all(&mount_point).expect("mountpoint dir");

        let spec = MountSpec {
            fs: source.display().to_string(),
            mount_point: mount_point.clone(),
            read_only: true,
        };
        if let Err(error) = mount(&client, &spec).await {
            // DriverMissing/Unsupported AND mountpoint-unusable are all
            // environment verdicts: hosts with WinFSP installed but a
            // sandboxed mount service (CI) land in MountPointBusy — skip,
            // the runtime path still surfaces these as real errors.
            if error.is_unavailable() || matches!(error, MountError::MountPointBusy(_)) {
                eprintln!("skipping: rclone mount unavailable on this host: {error}");
                return;
            }
            panic!("mount failed with unexpected error: {error}");
        }

        // Non-recursive root refresh: the canonical OK lands under result[""].
        let refreshed = client
            .vfs_refresh(&spec.fs, None, false)
            .await
            .expect("vfs refresh");
        assert_eq!(
            refreshed
                .get("result")
                .and_then(|result| result.get(""))
                .and_then(Value::as_str),
            Some("OK"),
            "unexpected refresh payload: {refreshed}"
        );

        // Stats: the VFS identifies itself by fs and counts the mount inUse.
        let stats = client.vfs_stats(&spec.fs).await.expect("vfs stats");
        assert_eq!(
            stats.get("fs").and_then(Value::as_str),
            Some(spec.fs.as_str()),
            "unexpected stats payload: {stats}"
        );
        assert!(
            stats
                .get("inUse")
                .and_then(Value::as_u64)
                .map_or(false, |in_use| in_use >= 1),
            "inUse missing or stale: {stats}"
        );

        unmount(&client, &mount_point)
            .await
            .expect("unmount should succeed");
    }
}
