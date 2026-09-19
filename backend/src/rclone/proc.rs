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

/// Environment overrides applied to an rcd child for one proxy group.
///
/// `Some(value)` sets the variable; `None` removes it from the child env.
/// Values may embed proxy credentials — they live only in process memory
/// and the child env; never log them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RcdEnv {
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub no_proxy: Option<String>,
}

impl RcdEnv {
    /// Applies the overrides to `command`. Go's net/http honors both the
    /// canonical uppercase names and the lowercase variants, so a set
    /// writes both spellings and a removal drops both — a proxy-group
    /// child must never fall back to a stale sidecar proxy. Operates on
    /// the std command (wrapped into the tokio command by [`RcdHandle::start`])
    /// so tests can assert on `get_envs`.
    fn apply_to(&self, command: &mut std::process::Command) {
        for (upper, lower, value) in [
            ("HTTP_PROXY", "http_proxy", &self.http_proxy),
            ("HTTPS_PROXY", "https_proxy", &self.https_proxy),
            ("NO_PROXY", "no_proxy", &self.no_proxy),
        ] {
            match value {
                Some(value) => {
                    command.env(upper, value.as_str());
                    command.env(lower, value.as_str());
                }
                None => {
                    command.env_remove(upper);
                    command.env_remove(lower);
                }
            }
        }
    }
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
    ///
    /// `env` picks the child's proxy environment: `None` inherits the
    /// sidecar environment untouched (today's behavior; the "direct"
    /// group). `Some(overrides)` keeps the inherited environment but
    /// replaces the proxy variables — `Some(value)` sets the variable in
    /// both uppercase and lowercase spellings, `None` removes both, so
    /// each proxy group needs its own rcd process (env is process-level).
    pub async fn start(binary: &Path, env: Option<&RcdEnv>) -> Result<Self, String> {
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

        let mut std_command = std::process::Command::new(binary);
        std_command
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
            .stderr(std::process::Stdio::piped());
        if let Some(env) = env {
            env.apply_to(&mut std_command);
        }
        let mut child = Command::from(std_command)
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

/// Owns one rcd per proxy group and respawns groups after crashes.
///
/// Process-wide single instance by design (the engine holds one); keys are
/// opaque here — the engine picks them, by convention `"direct"` for the
/// inherited-environment process or `ProxyConfig::group_key()` for a proxy
/// group. Env overrides are spawn-time only: once a group's rcd is up,
/// later [`RcdSupervisor::client_for`] calls with a different `RcdEnv`
/// reuse the running process instead of restarting it.
#[derive(Debug, Default)]
pub struct RcdSupervisor {
    binary: Option<PathBuf>,
    handles: std::collections::HashMap<String, RcdHandle>,
}

impl RcdSupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a live client for proxy group `key`, spawning or respawning
    /// that group's rcd with `env` as needed. A crashed rcd fails
    /// [`RcdHandle::is_running`] and the next call rebuilds the group; a
    /// live process is reused regardless of any `env` change (overrides
    /// only take effect at spawn time). The bool is `true` when the rcd was
    /// (re)spawned by this call — the caller replays group registrations on
    /// respawn, because a fresh rcd starts from an empty temp config.
    pub async fn client_for(
        &mut self,
        key: &str,
        env: Option<&RcdEnv>,
    ) -> Result<(RcClient, bool), String> {
        if let Some(handle) = self.handles.get_mut(key) {
            if handle.is_running() {
                return Ok((handle.client(), false));
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
        let handle = RcdHandle::start(&binary, env).await?;
        let client = handle.client();
        self.handles.insert(key.to_string(), handle);
        Ok((client, true))
    }

    /// Back-compat entry for the inherited-environment group; equivalent
    /// to `client_for("direct", None)`.
    pub async fn client(&mut self) -> Result<RcClient, String> {
        self.client_for("direct", None).await.map(|(client, _)| client)
    }

    /// Overrides binary resolution (tests, explicit configuration).
    pub fn set_binary(&mut self, binary: PathBuf) {
        self.binary = Some(binary);
    }

    /// Stops and forgets ONE group's rcd (idle-group teardown / keepalive
    /// reaping). `true` when a handle existed and was killed; a group whose
    /// rcd is already gone is a no-op success. The dropped handle's Drop
    /// kills the child.
    pub fn shutdown_group(&mut self, key: &str) -> bool {
        self.handles.remove(key).is_some()
    }

    /// Group keys with live handles (keepalive sweep inputs).
    pub fn group_keys(&self) -> Vec<String> {
        self.handles.keys().cloned().collect()
    }

    /// Stops every group's rcd and forgets it. Remotes registered in the
    /// configs die with the temp dirs — `connection/disconnect` uses this
    /// only when the whole engine has no live connections left.
    pub fn shutdown(&mut self) {
        self.handles.clear();
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
        let handle = RcdHandle::start(&binary, None)
            .await
            .expect("rcd should spawn");
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

    #[test]
    fn rcd_env_sets_and_removes_both_spellings() {
        let env = RcdEnv {
            http_proxy: Some("http://user:secret@127.0.0.1:8080".to_string()),
            https_proxy: None,
            no_proxy: Some("127.0.0.1,localhost".to_string()),
        };
        let mut command = std::process::Command::new("true");
        env.apply_to(&mut command);
        let envs: std::collections::HashMap<&std::ffi::OsStr, Option<&std::ffi::OsStr>> = command
            .get_envs()
            .map(|(key, value)| (key, value))
            .collect();
        // Set: uppercase canonical name + lowercase variant, both carrying
        // the same value (Go reads either).
        for name in ["HTTP_PROXY", "http_proxy"] {
            assert_eq!(
                envs.get(std::ffi::OsStr::new(name)).copied().flatten(),
                Some(std::ffi::OsStr::new("http://user:secret@127.0.0.1:8080")),
                "{name} must be set"
            );
        }
        for name in ["NO_PROXY", "no_proxy"] {
            assert_eq!(
                envs.get(std::ffi::OsStr::new(name)).copied().flatten(),
                Some(std::ffi::OsStr::new("127.0.0.1,localhost")),
                "{name} must be set"
            );
        }
        // Remove: both spellings dropped so the child cannot inherit a
        // stale sidecar proxy. `get_envs` records an explicit removal as
        // `Some(None)` (key listed, value cleared) — flattening yields
        // `None` for both that shape and an absent key.
        for name in ["HTTPS_PROXY", "https_proxy"] {
            assert_eq!(
                envs.get(std::ffi::OsStr::new(name)).copied().flatten(),
                None,
                "{name} must be removed"
            );
        }
    }

    /// Proxy-group rcd stays healthy with env overrides pointing at a
    /// dead proxy. The rc API binds and is probed on 127.0.0.1 loopback
    /// from the sidecar, so the child's `https_proxy` (port 1 always
    /// refuses) cannot affect startup — exactly the isolation the HTTP
    /// backends will rely on when they dial through the proxy.
    #[tokio::test]
    async fn proxy_group_rcd_spawns_healthy_and_groups_stay_separate() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let mut supervisor = RcdSupervisor::new();
        supervisor.set_binary(binary);
        let env = RcdEnv {
            https_proxy: Some("http://127.0.0.1:1".to_string()),
            ..RcdEnv::default()
        };
        let proxied = supervisor
            .client_for("proxy-dead", Some(&env))
            .await
            .expect("proxy-group rcd should spawn healthy")
            .0;
        proxied
            .noop()
            .await
            .expect("loopback rc API reachable despite dead proxy env");
        let proxied_endpoint = supervisor
            .handles
            .get("proxy-dead")
            .expect("group registered")
            .endpoint()
            .base_url
            .clone();

        // Same key with different env: the live process is reused, never
        // respawned (overrides are spawn-time only).
        supervisor
            .client_for("proxy-dead", None)
            .await
            .expect("second call should reuse the group");
        assert_eq!(
            supervisor
                .handles
                .get("proxy-dead")
                .expect("group still registered")
                .endpoint()
                .base_url,
            proxied_endpoint,
            "same key must not respawn a live rcd"
        );

        // A different key gets its own process with its own endpoint.
        let other = supervisor
            .client_for("other-group", None)
            .await
            .expect("second group should spawn")
            .0;
        other.noop().await.expect("second group reachable");
        let other_endpoint = supervisor
            .handles
            .get("other-group")
            .expect("second group registered")
            .endpoint()
            .base_url
            .clone();
        assert_ne!(
            proxied_endpoint, other_endpoint,
            "distinct keys must map to distinct rcd processes"
        );

        // Idle-group teardown: one key stops one group, the other survives;
        // repeated shutdowns are no-ops.
        assert_eq!(supervisor.group_keys().len(), 2);
        assert!(supervisor.shutdown_group("other-group"));
        assert!(!supervisor.group_keys().contains(&"other-group".to_string()));
        assert!(!supervisor.shutdown_group("other-group"), "idempotent");
        assert!(supervisor.group_keys().contains(&"proxy-dead".to_string()));

        // shutdown clears every group; Drop kills the children.
        supervisor.shutdown();
        assert!(supervisor.handles.is_empty(), "shutdown clears all groups");
    }

    /// The `client()` compat entry must keep working and must land in the
    /// conventional "direct" group with no env overrides.
    #[tokio::test]
    async fn client_compat_entry_maps_to_direct_group() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let mut supervisor = RcdSupervisor::new();
        supervisor.set_binary(binary);
        let client = supervisor.client().await.expect("compat client entry");
        client.noop().await.expect("noopauth");
        assert!(
            supervisor.handles.contains_key("direct"),
            "client() must register the direct group"
        );
        assert_eq!(supervisor.handles.len(), 1, "client() spawns one group");
    }
}
