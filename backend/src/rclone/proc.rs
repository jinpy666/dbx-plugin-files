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
    /// Spawn-time proxy env (see [`RcdEnv`]). Kept only so the streaming
    /// listing (`list_stream.rs`) can re-apply the SAME overrides to its
    /// short-lived `lsjson` child — a proxy-group connection must never
    /// bypass the user's proxy. Values may embed credentials: never log.
    env: Option<RcdEnv>,
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
            env: env.cloned(),
        })
    }

    pub fn endpoint(&self) -> &RcdEndpoint {
        &self.endpoint
    }

    /// Path of this group's private config file (`0600`, credentials land
    /// here via `config/create`). The streaming listing reuses it so a
    /// short-lived `lsjson` child sees exactly the remotes the rcd holds —
    /// never a second copy of the credentials.
    pub fn config_path(&self) -> PathBuf {
        self.temp_dir.join("rclone.conf")
    }

    /// Spawn-time proxy overrides (`None` = inherit sidecar env).
    pub fn env(&self) -> Option<&RcdEnv> {
        self.env.as_ref()
    }

    /// OS pid of the rcd child (`None` once it has exited and been reaped).
    /// Diagnostics and the unix-only orphan regression tests.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
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
    /// Every in-process exit path (stdin-EOF `serve()` return → `Plugin`
    /// drop chain, request-handler panic unwind, `connection/disconnect`
    /// and idle-group teardown) lands here, so the rcd child and its temp
    /// config die with the handle. The one leak path that remains is an
    /// external `SIGKILL` of the sidecar itself: no macOS process can
    /// intercept its own SIGKILL (Linux's `prctl(PR_SET_PDEATHSIG)` has no
    /// equivalent) and `rclone rcd` has no idle-exit flag to self-reap
    /// (only per-connection `--rc-server-{read,write}-timeout`). Orphaned
    /// rcds are inert (they hold no children of their own) but keep their
    /// loopback rc endpoint until reboot; a POSIX watchdog exec-chain
    /// remains the fallback if field reports ever make it worth the spawn
    /// complexity.
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

/// Everything a consumer needs to spawn a short-lived child that shares a
/// group rcd's identity: the same binary, the same private config (so the
/// child resolves the same remotes — no second credential copy) and the same
/// proxy env (a proxy-group connection must never bypass the user's proxy).
/// `Debug` skips every field on purpose: `env` may embed proxy credentials.
pub struct RcdSpawnInfo {
    pub binary: PathBuf,
    pub config_path: PathBuf,
    pub env: Option<RcdEnv>,
}

impl std::fmt::Debug for RcdSpawnInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RcdSpawnInfo")
            .field("binary", &self.binary)
            .field("config_path", &self.config_path)
            .field("env", &self.env.is_some())
            .finish()
    }
}

impl RcdSpawnInfo {
    /// Applies the group's proxy overrides to a std command (wrap into the
    /// tokio command afterwards, same pattern as [`RcdHandle::start`]).
    pub fn apply_env_to(&self, command: &mut std::process::Command) {
        if let Some(env) = &self.env {
            env.apply_to(command);
        }
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
pub struct RcdSupervisor {
    binary: Option<PathBuf>,
    handles: std::collections::HashMap<String, RcdHandle>,
    /// Pre-teardown callback (installed once by the engine owner): runs with
    /// the group key BEFORE the rcd handle is dropped, so dependents can
    /// kill their children first — the streaming listing kills in-flight
    /// `lsjson` children here, otherwise one could still be reading the
    /// half-deleted temp config while the rcd is being torn down.
    teardown_hook: Option<std::sync::Arc<dyn Fn(&str) + Send + Sync>>,
}

impl std::fmt::Debug for RcdSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RcdSupervisor")
            .field("binary", &self.binary)
            .field("handles", &self.handles.keys())
            .finish_non_exhaustive()
    }
}

impl Default for RcdSupervisor {
    fn default() -> Self {
        Self {
            binary: None,
            handles: std::collections::HashMap::new(),
            teardown_hook: None,
        }
    }
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

    /// Installs the pre-teardown hook (see the field docs). At most one
    /// hook: a later call replaces the previous one.
    pub fn set_teardown_hook(&mut self, hook: std::sync::Arc<dyn Fn(&str) + Send + Sync>) {
        self.teardown_hook = Some(hook);
    }

    fn run_teardown_hook(&self, key: &str) {
        if let Some(hook) = &self.teardown_hook {
            hook(key);
        }
    }

    /// Spawn identity of a live group (`None` for an unknown/tearing-down
    /// key): binary + private config + proxy env, for children that must
    /// share the rcd's view of the world.
    pub fn spawn_details(&self, key: &str) -> Option<RcdSpawnInfo> {
        let handle = self.handles.get(key)?;
        Some(RcdSpawnInfo {
            binary: handle.binary.clone(),
            config_path: handle.config_path(),
            env: handle.env.clone(),
        })
    }

    /// Stops and forgets ONE group's rcd (idle-group teardown / keepalive
    /// reaping). `true` when a handle existed and was killed; a group whose
    /// rcd is already gone is a no-op success. The dropped handle's Drop
    /// kills the child. The teardown hook runs FIRST so dependents (the
    /// streaming listing) can SIGKILL their children before the temp config
    /// disappears under them.
    pub fn shutdown_group(&mut self, key: &str) -> bool {
        if self.handles.contains_key(key) {
            self.run_teardown_hook(key);
        }
        self.handles.remove(key).is_some()
    }

    /// Group keys with live handles (keepalive sweep inputs).
    pub fn group_keys(&self) -> Vec<String> {
        self.handles.keys().cloned().collect()
    }

    /// `(group, client)` pairs for every LIVE group rcd — process-wide
    /// settings (bandwidth limit) applied to each running process. Dead
    /// groups are skipped on purpose: their respawn path replays the
    /// setting through `replay_group_registrations`, and spawning here
    /// would use the wrong proxy env.
    pub async fn live_clients(&mut self) -> Vec<(String, RcClient)> {
        let mut pairs = Vec::new();
        for (key, handle) in self.handles.iter_mut() {
            if handle.is_running() {
                pairs.push((key.clone(), handle.client()));
            }
        }
        pairs
    }

    /// Stops every group's rcd and forgets it. Remotes registered in the
    /// configs die with the temp dirs — `connection/disconnect` uses this
    /// only when the whole engine has no live connections left. Same
    /// hook-first discipline as [`Self::shutdown_group`].
    pub fn shutdown(&mut self) {
        for key in self.handles.keys() {
            self.run_teardown_hook(key);
        }
        self.handles.clear();
    }
}

/// Resolve the rclone binary: explicit env → bundled next to our own exe →
/// PATH. Returns the first candidate whose version parses high enough.
pub fn resolve_binary() -> Option<PathBuf> {
    resolve_binary_with(|key| std::env::var_os(key))
}

/// Env-injectable core of [`resolve_binary`] (unit-testable without touching
/// the real environment). A `None` lookup for `PATH` skips the PATH scan.
pub fn resolve_binary_with(
    lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(env_binary) = lookup("DBX_FILES_RCLONE_BIN") {
        let env_binary = env_binary.to_string_lossy().to_string();
        if !env_binary.trim().is_empty() {
            candidates.push(PathBuf::from(env_binary));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(rclone_binary_name()));
        }
    }
    if let Some(path_var) = lookup("PATH") {
        for dir in std::env::split_paths(&path_var) {
            candidates.push(dir.join(rclone_binary_name()));
        }
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file() && probe_version(candidate).is_ok())
}

/// One-line engine state for the startup log (issue #16): which rclone the
/// engine will use, or the actionable "not found" guidance. The storage
/// surface degrades to structured errors when this reports a missing binary;
/// the line exists so users see the reason in the plugin log instead of
/// discovering it on the first failed operation.
pub fn startup_diagnostic() -> String {
    match resolve_binary() {
        Some(binary) => match probe_version(&binary) {
            Ok((major, minor, patch)) => format!(
                "rclone engine ready: {} v{major}.{minor}.{patch}",
                binary.display()
            ),
            Err(error) => format!(
                "rclone engine probe failed for {}: {error}",
                binary.display()
            ),
        },
        None => "rclone binary not found; storage calls will fail until \
                 DBX_FILES_RCLONE_BIN is set or rclone >= 1.68 is on PATH"
            .to_string(),
    }
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

    /// Issue #16 regression (resolution half): with no env override and no
    /// PATH, resolution must return None — the structured "binary not found"
    /// error branch — instead of panicking or guessing. The exe-sibling
    /// candidate stays in play, but the cargo test dir never ships an rclone,
    /// so None is deterministic.
    #[test]
    fn resolve_without_env_or_path_finds_nothing() {
        let found = resolve_binary_with(|_| None);
        assert_eq!(found, None, "unexpected rclone: {found:?}");
    }

    /// DBX_FILES_RCLONE_BIN wins over everything (bundled exe sibling and
    /// PATH) and must pass its version probe. unix-only: the version stub is
    /// a shell script.
    #[cfg(unix)]
    #[test]
    fn resolve_prefers_dbx_files_rclone_bin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = dir.path().join("rclone");
        std::fs::write(&stub, "#!/bin/sh\necho 'rclone v1.75.1'\n").expect("stub");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let found = resolve_binary_with(|key| {
            if key == "DBX_FILES_RCLONE_BIN" {
                Some(std::ffi::OsString::from(stub.as_os_str()))
            } else {
                None
            }
        })
        .expect("env override must resolve");
        assert_eq!(found, stub);
    }

    /// The startup diagnostic is always a complete, actionable line: either
    /// the engine's binary+version, or the explicit not-found guidance. This
    /// is the line users paste when reporting a failed initialization
    /// (issue #16), so "empty or partial" would be a bug.
    #[test]
    fn startup_diagnostic_is_always_actionable() {
        let text = startup_diagnostic();
        assert!(text.contains("rclone"), "{text}");
        assert!(
            text.contains("engine ready") || text.contains("not found") || text.contains("failed"),
            "{text}"
        );
    }

    /// Issue #16 regression (call half): when the engine process cannot be
    /// started at all, the storage call answers the structured error string —
    /// never a panic. The sidecar stays alive and the host keeps its side of
    /// the stdio pipe.
    #[tokio::test]
    async fn engine_spawn_failure_yields_structured_error_not_panic() {
        let mut supervisor = RcdSupervisor::new();
        supervisor.set_binary(PathBuf::from("/nonexistent/dbx-rclone"));
        let error = supervisor
            .client_for("direct", None)
            .await
            .expect_err("spawn must fail for a missing binary");
        assert!(error.contains("Failed to spawn"), "{error}");
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
        // Windows env names are case-insensitive: both `env()` spellings of
        // one variable collapse into a single entry, so assert on an
        // uppercased projection. Unix keeps both spellings as distinct keys.
        let mut normalized: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        for (key, value) in command.get_envs() {
            normalized.insert(
                key.to_string_lossy().to_ascii_uppercase(),
                value.map(|value| value.to_string_lossy().into_owned()),
            );
        }
        assert_eq!(
            normalized.get("HTTP_PROXY").and_then(|value| value.as_deref()),
            Some("http://user:secret@127.0.0.1:8080"),
            "http_proxy must be set"
        );
        assert_eq!(
            normalized.get("NO_PROXY").and_then(|value| value.as_deref()),
            Some("127.0.0.1,localhost"),
            "no_proxy must be set"
        );
        // Remove: both spellings dropped so the child cannot inherit a
        // stale sidecar proxy. `get_envs` records an explicit removal as
        // `Some(None)` (key listed, value cleared) — flattening yields
        // `None` for both that shape and an absent key.
        assert_eq!(
            normalized.get("HTTPS_PROXY").and_then(|value| value.as_deref()),
            None,
            "https_proxy must be removed"
        );
        #[cfg(unix)]
        {
            // Unix distinguishes the spellings: both must carry the value.
            let envs: std::collections::HashMap<
                &std::ffi::OsStr,
                Option<&std::ffi::OsStr>,
            > = command
                .get_envs()
                .map(|(key, value)| (key, value))
                .collect();
            for name in ["HTTP_PROXY", "http_proxy"] {
                assert_eq!(
                    envs.get(std::ffi::OsStr::new(name)).copied().flatten(),
                    Some(std::ffi::OsStr::new("http://user:secret@127.0.0.1:8080")),
                    "{name} must be set"
                );
            }
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

    /// The teardown hook fires with the group key BEFORE the handle is
    /// removed (`list_stream` kills its in-flight children there — the
    /// ordering is the whole point). Ghost keys never fire; a removed group
    /// never fires twice.
    #[tokio::test]
    async fn teardown_hook_fires_before_group_removal() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let mut supervisor = RcdSupervisor::new();
        supervisor.set_binary(binary);
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&calls);
        supervisor.set_teardown_hook(std::sync::Arc::new(move |key: &str| {
            sink.lock().unwrap().push(key.to_string());
        }));
        // No such group: no fire.
        assert!(!supervisor.shutdown_group("ghost"));
        assert!(calls.lock().unwrap().is_empty());
        supervisor.client_for("hook-group", None).await.expect("group");
        assert!(supervisor.shutdown_group("hook-group"));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["hook-group".to_string()]
        );
        // Idempotent: the removed group does not fire again.
        assert!(!supervisor.shutdown_group("hook-group"));
        assert_eq!(calls.lock().unwrap().len(), 1);
        supervisor.shutdown();
        assert_eq!(calls.lock().unwrap().len(), 1);
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

    // -- 孤儿 rcd 回归（正常路径 OS 级验证） ---------------------------------
    //
    // 黑盒验证轮在本机留下多个 PPID=1 的 rcd 孤儿，根因是 sidecar 被 SIGKILL
    // （Drop 链无从运行）；下面两条测试锁住"正常路径必须杀干净"的另一半：
    // drop handle / supervisor shutdown 后，rcd 进程必须从 OS 层面消失。
    // unix-only：用 `ps` 断言；Windows CI 上由 `kill_on_drop` 语义与上面
    // 的进程存活行为测试覆盖（进程死了后续 rc 调用必然失败）。

    /// `ps` reports the pid alive and not a zombie. Fully-reaped pids make
    /// `ps` exit non-zero; zombies are already SIGKILLed (reaping races are
    /// fine) so both count as dead.
    #[cfg(unix)]
    fn unix_process_alive(pid: u32) -> bool {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .expect("ps must exist on unix");
        if !output.status.success() {
            return false; // pid fully reaped: ps finds nothing
        }
        let stat = String::from_utf8_lossy(&output.stdout).trim().to_string();
        !stat.is_empty() && !stat.starts_with('Z')
    }

    #[cfg(unix)]
    async fn assert_process_gone(pid: u32) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while unix_process_alive(pid) && tokio::time::Instant::now() < deadline {
            sleep(Duration::from_millis(100)).await;
        }
        assert!(!unix_process_alive(pid), "rcd pid {pid} still alive");
    }

    /// Drop of a live handle must SIGKILL the rcd child for real — the OS
    /// process disappears, not just the in-process handle (orphan-rcd
    /// investigation regression, normal-path half).
    #[cfg(unix)]
    #[tokio::test]
    async fn dropped_rcd_handle_kills_child_process() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let handle = RcdHandle::start(&binary, None)
            .await
            .expect("rcd should spawn");
        let pid = handle.pid().expect("live child pid");
        assert!(unix_process_alive(pid), "spawned pid must be alive");
        drop(handle);
        assert_process_gone(pid).await;
    }

    /// `shutdown()` (whole-engine teardown; the same Drop path backs
    /// `shutdown_group` and the EOF drop chain) must kill every group's rcd
    /// process at the OS level.
    #[cfg(unix)]
    #[tokio::test]
    async fn supervisor_shutdown_kills_all_group_processes() {
        let Some(binary) = resolve_binary() else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let mut supervisor = RcdSupervisor::new();
        supervisor.set_binary(binary);
        supervisor.client_for("direct", None).await.expect("group 1");
        supervisor.client_for("second", None).await.expect("group 2");
        let pids: Vec<u32> = supervisor
            .handles
            .values()
            .filter_map(|handle| handle.pid())
            .collect();
        assert_eq!(pids.len(), 2, "two groups, two pids");
        for pid in &pids {
            assert!(unix_process_alive(*pid));
        }
        supervisor.shutdown();
        for pid in pids {
            assert_process_gone(pid).await;
        }
    }
}
