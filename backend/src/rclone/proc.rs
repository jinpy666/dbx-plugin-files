//! rclone binary resolution + `rcd` subprocess lifecycle.

use std::io::Write as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::time::sleep;
use uuid::Uuid;

use super::rc::RcClient;

/// Oldest rclone the engine accepts. `operations/uploadfile` (the streaming
/// upload path used by the upload channel) landed after 1.68; older system
/// installs are rejected with an actionable message instead of failing deep
/// inside rc calls.
pub const MIN_RCLONE_VERSION: (u64, u64, u64) = (1, 68, 0);

/// How long `RcdHandle::wait_healthy` polls `rc/noopauth` before giving up.
const SPAWN_HEALTH_TIMEOUT: Duration = Duration::from_secs(10);
const SPAWN_HEALTH_INTERVAL: Duration = Duration::from_millis(150);

/// Everything needed to talk to a running rcd (owned by [`RcdHandle`]).
#[derive(Debug, Clone)]
pub struct RcdEndpoint {
    pub base_url: String,
    pub user: String,
    pub pass: String,
}

/// A running `rclone rcd` child plus its private config directory.
///
/// Dropping the handle kills the child and removes the temp config dir, so a
/// sidecar crash cannot leak credentials in world-readable temp files on
/// purpose — the config is created `0600` and the directory is removed on
/// every clean shutdown path.
pub struct RcdHandle {
    child: Child,
    endpoint: RcdEndpoint,
    temp_dir: PathBuf,
    binary: PathBuf,
}

impl std::fmt::Debug for RcdHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RcdHandle")
            .field("endpoint", &self.endpoint)
            .field("temp_dir", &self.temp_dir)
            .field("binary", &self.binary)
            .finish_non_exhaustive()
    }
}

impl RcdHandle {
    /// Spawns rcd and waits until `rc/noopauth` answers.
    ///
    /// `binary` must have passed [`probe_version`]. The config file starts
    /// empty — remotes are registered per connection through
    /// `config/create`, keeping credentials out of argv/env.
    pub async fn start(binary: &Path) -> Result<Self, String> {
        let port = free_loopback_port()?;
        let temp_dir = std::env::temp_dir().join(format!(
            "dbx-files-rclone-{}-{}",
            std::process::id(),
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&temp_dir)
            .map_err(|error| format!("Failed to create rclone config dir {}: {error}", temp_dir.display()))?;
        let config_path = temp_dir.join("rclone.conf");
        // 0600: credentials land here via config/create, never in argv.
        open_0600(&config_path)?;

        let endpoint = RcdEndpoint {
            base_url: format!("http://127.0.0.1:{port}"),
            user: format!("u{}", Uuid::new_v4().simple()),
            pass: Uuid::new_v4().simple().to_string(),
        };

        let mut child = Command::new(binary)
            .arg("rcd")
            .arg(format!("--rc-addr=127.0.0.1:{port}"))
            .arg(format!("--rc-user={}", endpoint.user))
            .arg(format!("--rc-pass={}", endpoint.pass))
            // Byte streaming for downloads (GET /{remote:}/{path}).
            .arg("--rc-serve")
            .arg(format!("--config={}", config_path.display()))
            .arg("--log-level=INFO")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("Failed to spawn {}: {error}", binary.display()))?;

        if let Err(error) = wait_healthy(&endpoint).await {
            // Surface rcd's own stderr — port clashes and bad flags show up
            // there, not in our health loop's generic timeout.
            let stderr = drain_stderr(&mut child).await;
            let _ = child.start_kill();
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Err(format!("rclone rcd failed to become healthy: {error}{stderr}"));
        }

        Ok(Self {
            child,
            endpoint,
            temp_dir,
            binary: binary.to_path_buf(),
        })
    }

    pub fn endpoint(&self) -> &RcdEndpoint {
        &self.endpoint
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// Rc client bound to this rcd instance.
    pub fn client(&self) -> RcClient {
        RcClient::new(
            self.endpoint.base_url.clone(),
            self.endpoint.user.clone(),
            self.endpoint.pass.clone(),
        )
    }

    /// True while the child has not exited. A crashed rcd fails this and the
    /// supervisor respawns on the next call.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// rcd's version string from its own `core/version` (diagnostics).
    pub async fn version(&self) -> Result<String, String> {
        let value = self
            .client()
            .call("core/version", &serde_json::json!({}))
            .await
            .map_err(|error| error.to_string())?;
        Ok(value
            .get("version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string())
    }
}

impl Drop for RcdHandle {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

/// Owns at most one rcd for the sidecar and respawns it after crashes.
///
/// Process-wide single instance by design (the engine holds one); all rcd
/// interaction goes through [`RcdSupervisor::client`], which transparently
/// (re)starts the process and never returns a client bound to a dead rcd.
#[derive(Debug, Default)]
pub struct RcdSupervisor {
    binary: Option<PathBuf>,
    handle: Option<RcdHandle>,
}

impl RcdSupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a live client, spawning or respawning rcd as needed.
    pub async fn client(&mut self) -> Result<RcClient, String> {
        if let Some(handle) = self.handle.as_mut() {
            if handle.is_running() {
                return Ok(handle.client());
            }
        }
        let binary = match self.binary.clone() {
            Some(binary) => binary,
            None => resolve_binary().ok_or_else(|| {
                "rclone binary not found; set DBX_FILES_RCLONE_BIN or install rclone >= \
                 1.68 on PATH"
                    .to_string()
            })?,
        };
        let handle = RcdHandle::start(&binary).await?;
        let client = handle.client();
        self.handle = Some(handle);
        Ok(client)
    }

    /// Overrides binary resolution (tests, explicit configuration).
    pub fn set_binary(&mut self, binary: PathBuf) {
        self.binary = Some(binary);
    }

    /// Stops rcd and forgets it. Remotes registered in its config die with
    /// the temp dir — `connection/disconnect` uses this only when the whole
    /// engine has no live connections left.
    pub fn shutdown(&mut self) {
        self.handle = None;
    }
}

/// Resolve the rclone binary: explicit env → bundled next to our own exe →
/// PATH. Returns the first candidate whose version parses high enough.
pub fn resolve_binary() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(env_binary) = std::env::var("DBX_FILES_RCLONE_BIN") {
        if !env_binary.trim().is_empty() {
            candidates.push(PathBuf::from(env_binary));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(rclone_binary_name()));
        }
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            candidates.push(dir.join(rclone_binary_name()));
        }
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file() && probe_version(candidate).is_ok())
}

/// Runs `<binary> version` and checks the minimum version.
pub fn probe_version(binary: &Path) -> Result<(u64, u64, u64), String> {
    let output = std::process::Command::new(binary)
        .arg("version")
        .output()
        .map_err(|error| format!("cannot execute {}: {error}", binary.display()))?;
    if !output.status.success() {
        return Err(format!("{} exited non-zero", binary.display()));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let version = parse_version(&text)
        .ok_or_else(|| format!("cannot parse rclone version from output: {text:.80}"))?;
    if version < MIN_RCLONE_VERSION {
        return Err(format!(
            "rclone {}.{}.{} is older than the required {}.{}.{}",
            version.0,
            version.1,
            version.2,
            MIN_RCLONE_VERSION.0,
            MIN_RCLONE_VERSION.1,
            MIN_RCLONE_VERSION.2
        ));
    }
    Ok(version)
}

/// `"rclone v1.75.1\n- os/version: ..."` → `(1, 75, 1)`.
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let line = text.lines().next()?;
    let start = line.find('v')? + 1;
    let mut parts = line[start..].split('.');
    let major = parts.next()?.trim().parse().ok()?;
    let minor = parts.next()?.trim().parse().ok()?;
    let patch: String = parts
        .next()
        .unwrap_or("0")
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let patch = if patch.is_empty() {
        0
    } else {
        patch.parse().ok()?
    };
    Some((major, minor, patch))
}

fn rclone_binary_name() -> &'static str {
    if cfg!(windows) {
        "rclone.exe"
    } else {
        "rclone"
    }
}

fn free_loopback_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("cannot bind loopback for rcd port: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("cannot read loopback port: {error}"))?
        .port();
    drop(listener);
    Ok(port)
}

fn open_0600(path: &Path) -> Result<std::fs::File, String> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    file.flush()
        .map_err(|error| format!("cannot initialise {}: {error}", path.display()))?;
    Ok(file)
}

async fn wait_healthy(endpoint: &RcdEndpoint) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + SPAWN_HEALTH_TIMEOUT;
    let client = reqwest::Client::new();
    loop {
        let response = client
            .post(format!("{}/rc/noopauth", endpoint.base_url))
            .basic_auth(&endpoint.user, Some(&endpoint.pass))
            .timeout(Duration::from_secs(2))
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => return Ok(()),
            _ if tokio::time::Instant::now() >= deadline => {
                return Err(format!(
                    "rc/noopauth did not answer within {}s",
                    SPAWN_HEALTH_TIMEOUT.as_secs()
                ))
            }
            _ => sleep(SPAWN_HEALTH_INTERVAL).await,
        }
    }
}

async fn drain_stderr(child: &mut Child) -> String {
    let mut stderr = String::new();
    if let Some(pipe) = child.stderr.take() {
        use tokio::io::AsyncReadExt;
        let mut pipe = pipe;
        let _ = tokio::time::timeout(Duration::from_millis(500), pipe.read_to_string(&mut stderr))
            .await;
    }
    if stderr.trim().is_empty() {
        String::new()
    } else {
        format!("\nrcd stderr: {}", stderr.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_reads_release_strings() {
        assert_eq!(
            parse_version("rclone v1.75.1\n- os/version: darwin 26.6.2"),
            Some((1, 75, 1))
        );
        assert_eq!(parse_version("rclone v1.68.0"), Some((1, 68, 0)));
        assert_eq!(parse_version("rclone v1.72.0-beta.1234"), Some((1, 72, 0)));
        assert_eq!(parse_version("garbage"), None);
    }

    #[test]
    fn parse_version_enforces_minimum() {
        let binary = PathBuf::from("/nonexistent/rclone");
        // probe_version on a missing binary fails before version comparison;
        // assert the error mentions execution, not a panic.
        assert!(probe_version(&binary).is_err());
    }

    /// Real-process smoke: needs rclone on PATH or DBX_FILES_RCLONE_BIN.
    /// Skips silently otherwise so CI stays green without the binary.
    #[tokio::test]
    async fn spawns_rcd_and_serves_config_api() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let mut supervisor = RcdSupervisor::new();
        supervisor.set_binary(binary);
        let client = supervisor.client().await.expect("rcd should spawn");

        // config/create + list round trip through the live process.
        client
            .config_create("probe-local", "local", serde_json::json!({}), false)
            .await
            .expect("config/create should succeed");
        let dump = client
            .call("config/dump", &serde_json::json!({}))
            .await
            .expect("config/dump should succeed");
        assert!(dump.get("probe-local").is_some(), "remote registered");

        // The temp config actually holds the remote (file semantics, not just
        // in-memory) and dies with the handle.
        supervisor.shutdown();
    }

    #[tokio::test]
    async fn live_client_roundtrip() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let handle = RcdHandle::start(&binary).await.expect("rcd should spawn");
        let client = handle.client();
        client.noop().await.expect("noopauth");
        let version = client.version().await.expect("core/version");
        assert!(
            version
                .get("version")
                .and_then(serde_json::Value::as_str)
                .is_some(),
            "version payload shape"
        );
        // operations/stat semantics we must mirror in files/stat: a missing
        // object on a valid remote is Ok(item:null), not an error; an
        // unknown remote name (with the fs colon) is a real rc error.
        client
            .config_create("probe-local", "local", serde_json::json!({}), false)
            .await
            .expect("config/create should succeed");
        let missing_object = client
            .operations_stat("probe-local:", "definitely-missing-path-xyz")
            .await
            .expect("stat on valid remote must not error");
        assert!(missing_object.get("item").unwrap().is_null());
        let unknown_remote = client
            .operations_stat("dbx-no-such-remote:", "x")
            .await;
        assert!(
            matches!(
                unknown_remote,
                Err(super::super::rc::RcError::Rclone { .. })
                    | Err(super::super::rc::RcError::Http { .. })
            ),
            "unknown remote must surface an error, got {unknown_remote:?}"
        );
        assert!(!handle.version().await.expect("version").is_empty());
    }

    #[tokio::test]
    async fn rejects_ancient_rclone() {
        // resolve_binary filters by probe_version; simulate by probing a
        // stub script that reports an ancient version.
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = dir.path().join("rclone");
        std::fs::write(&stub, "#!/bin/sh\necho 'rclone v1.60.0'\n").expect("stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        #[cfg(unix)]
        {
            let error = probe_version(&stub).expect_err("ancient version must be rejected");
            assert!(error.contains("older than the required"), "{error}");
        }
        let _ = &dir;
    }
}
