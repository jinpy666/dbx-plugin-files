//! SSH tunnel local port forwarding for tunnel-protected connections.
//!
//! Connections that must ride an SSH jump chain get a sidecar-managed
//! `ssh -N -L` child, mirroring the rcd-per-group lifecycle of
//! [`super::proc`]: the supervisor spawns one forwarder per connection key
//! on an ephemeral `127.0.0.1` port and the caller rewrites the connection
//! endpoint to it ([`rewrite_endpoint`]).
//!
//! DBX `jumpHosts` semantics map onto ssh as follows: the LAST hop of the
//! chain is the ssh login target, the hops before it become the `-J`
//! ProxyJump chain, and the `-L` forward destination (`target_host`) must
//! be reachable from the last hop's network view.
//!
//! Security posture:
//! - Key-only authentication. `BatchMode=yes` makes ssh fail instead of
//!   prompting; password-authenticated specs are rejected at the parsing
//!   layer, never here. Credentials never enter argv — the identity is a
//!   key file path (`-i`) or the agent/default keys.
//! - The child's stderr may contain host names and ssh diagnostics but
//!   never credentials; the error tails surfaced to callers are exactly
//!   that text.
//! - TLS/SNI semantics after the rewrite: the client dials `127.0.0.1`, so
//!   the remote certificate is validated against `127.0.0.1` and SNI
//!   carries `127.0.0.1`. Intranet deployments fronting a private CA are
//!   the target scenario; public-CA hosts behind a tunnel would need an
//!   explicit CA/verification override on the backend side.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::time::sleep;

/// How long [`TunnelSupervisor::establish`] health-polls a fresh forwarder
/// before reporting it as up anyway (while the child is still alive).
const SPAWN_HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
const SPAWN_HEALTH_INTERVAL: Duration = Duration::from_millis(100);

/// Default ssh resolution: no override configured → `"ssh"` from PATH.
const DEFAULT_SSH_BINARY: &str = "ssh";

/// One hop of the jump chain. `username` empty means ssh defaults; `port` 0
/// means ssh's default port (22) and is omitted from argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop {
    pub host: String,
    pub port: u16,
    pub username: String,
}

/// Forward spec: ordered jump chain plus the destination as reachable from
/// the LAST hop. The last hop is the ssh login target; the hops before it
/// form the `-J` ProxyJump chain. `identity_file` empty means the agent or
/// ssh's default keys. Key-only by design — passwords are rejected at the
/// parsing layer and never reach this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelSpec {
    /// Non-empty; an empty chain is an `establish` error.
    pub hops: Vec<Hop>,
    /// Non-empty; the `-L` destination host.
    pub target_host: String,
    pub target_port: u16,
    /// Empty = ssh default keys/agent.
    pub identity_file: String,
}

/// Rewrites a connection endpoint to the local forwarder.
///
/// A leading `scheme://` is preserved as-is (detection is
/// case-insensitive, the original casing is kept) and the authority is
/// replaced with `127.0.0.1:<local_port>`; an endpoint without a scheme is
/// replaced outright. The port is always present in the output. Note the
/// TLS/SNI consequence documented at the module level: after the rewrite
/// the remote certificate is validated against `127.0.0.1`.
pub fn rewrite_endpoint(endpoint: &str, local_port: u16) -> String {
    let rewritten = format!("127.0.0.1:{local_port}");
    match endpoint.split_once("://") {
        Some((scheme, _)) if is_scheme(scheme) => format!("{scheme}://{rewritten}"),
        _ => rewritten,
    }
}

/// RFC 3986 scheme shape: ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ).
/// Rejects empty or digit-leading prefixes so a bare `://…` falls through
/// to the outright replacement.
fn is_scheme(candidate: &str) -> bool {
    let mut chars = candidate.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// `[u@]host` or `[u@]host:port` for one hop (port 0 omitted, see [`Hop`]).
fn hop_destination(hop: &Hop) -> String {
    let mut destination = String::new();
    if !hop.username.trim().is_empty() {
        destination.push_str(hop.username.trim());
        destination.push('@');
    }
    destination.push_str(&hop.host);
    if hop.port != 0 {
        destination.push(':');
        destination.push_str(&hop.port.to_string());
    }
    destination
}

/// `[u@]host` only — the ssh login destination form (its port travels via
/// `-p`; a `host:port` destination would be resolved as one hostname).
fn hop_destination_no_port(hop: &Hop) -> String {
    let mut destination = String::new();
    if !hop.username.trim().is_empty() {
        destination.push_str(hop.username.trim());
        destination.push('@');
    }
    destination.push_str(&hop.host);
    destination
}

/// Builds the ssh command (without stdio wiring — [`TunnelSupervisor`]
/// owns that). Arg shape (stable):
///
/// ```text
/// <binary> -N -T -o BatchMode=yes -o ExitOnForwardFailure=yes
///  -o StrictHostKeyChecking=accept-new -o ServerAliveInterval=30
///  [-i <identity_file>]
///  [-J u1@h1:p1,u2@h2:p2]      (only when hops.len() > 1: all but the last)
///  -L 127.0.0.1:<local_port>:<target_host>:<target_port>
///  [u@]last_host[:last_port]
/// ```
///
/// Key-only auth: `BatchMode=yes` disables every password prompt, so argv
/// never needs (and never carries) a credential. `ExitOnForwardFailure=yes`
/// turns a refused forward into a child exit the supervisor can observe;
/// `accept-new` trusts unknown first-seen host keys (the tunnel is operator
/// configured) while still rejecting changed keys.
pub fn ssh_command(binary: &Path, spec: &TunnelSpec, local_port: u16) -> std::process::Command {
    let mut command = std::process::Command::new(binary);
    command
        .arg("-N")
        .arg("-T")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-o")
        .arg("ServerAliveInterval=30");
    if !spec.identity_file.trim().is_empty() {
        command.arg("-i").arg(&spec.identity_file);
    }
    let last = spec.hops.last().expect("caller validated non-empty");
    if spec.hops.len() > 1 {
        let jumps: Vec<String> = spec.hops[..spec.hops.len() - 1]
            .iter()
            .map(hop_destination)
            .collect();
        command.arg("-J").arg(jumps.join(","));
    }
    // The login destination carries its port via `-p`: OpenSSH parses
    // `user@host:port` in a destination argument as one hostname and hangs
    // in resolution (fake-IP DNS makes that a silent hang, not an error).
    // The `-J` entries above are the exception — that option's grammar IS
    // `[user@]host:port`.
    if last.port != 0 {
        command.arg("-p").arg(last.port.to_string());
    }
    command.arg("-L").arg(format!(
        "127.0.0.1:{local_port}:{}:{}",
        spec.target_host, spec.target_port
    ));
    command.arg(hop_destination_no_port(last));
    command
}

/// A running `ssh -N -L` forwarder for one connection key. Dropping kills
/// the child (explicit `start_kill` plus `kill_on_drop`), so a dropped
/// supervisor cannot leak live tunnels.
struct TunnelHandle {
    /// std Mutex: only non-async operations (`try_wait`/`start_kill`) touch
    /// the child, and `is_running` must work from `&self`.
    child: std::sync::Mutex<Child>,
    local_port: u16,
    binary: PathBuf,
}

impl TunnelHandle {
    /// True while the child has not exited. A crashed forwarder fails this;
    /// callers re-establish to respawn.
    fn is_running(&self) -> bool {
        let mut child = match self.child.lock() {
            Ok(child) => child,
            Err(poisoned) => poisoned.into_inner(),
        };
        matches!(child.try_wait(), Ok(None))
    }

    fn kill(&self) {
        let mut child = match self.child.lock() {
            Ok(child) => child,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = child.start_kill();
    }
}

impl Drop for TunnelHandle {
    fn drop(&mut self) {
        self.kill();
    }
}

impl std::fmt::Debug for TunnelHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelHandle")
            .field("local_port", &self.local_port)
            .field("binary", &self.binary)
            .finish_non_exhaustive()
    }
}

/// Owns one ssh forwarder per connection key, mirroring
/// [`super::proc::RcdSupervisor`]. Keys are opaque here — the engine picks
/// them (by convention the connection id). Re-establishing an existing key
/// replaces the old child first: the spec may have changed and a stale
/// forwarder must not keep serving.
#[derive(Debug, Default)]
pub struct TunnelSupervisor {
    binary: Option<PathBuf>,
    handles: std::collections::HashMap<String, TunnelHandle>,
}

impl TunnelSupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Overrides ssh binary resolution (tests). Default: `"ssh"` from PATH.
    pub fn set_binary(&mut self, binary: PathBuf) {
        self.binary = Some(binary);
    }

    /// Spawns (or replaces) the forwarder for `key` and returns the local
    /// loopback port it listens on.
    ///
    /// A free port is reserved by binding `TcpListener` on `127.0.0.1:0`
    /// and dropping it, the ssh child spawns with `kill_on_drop`, null
    /// stdin/stdout and piped stderr, then health polls up to ~5s:
    ///
    /// - child exited → `Err` carrying the stderr tail (ssh diagnostics,
    ///   host names but never credentials);
    /// - port accepts → `Ok(port)`;
    /// - deadline with the child still alive → `Ok(port)` (slow jump chains
    ///   must not fail establishment; the forward is the caller's endpoint).
    ///
    /// Same key re-establish kills the old child first (the spec may have
    /// changed). The reservation window between dropping the probe listener
    /// and ssh binding the port is tiny; a collision there is accepted as
    /// best-effort, like every ephemeral-port scheme.
    pub async fn establish(&mut self, key: &str, spec: &TunnelSpec) -> Result<u16, String> {
        // Validate before touching the running state: a bad spec must not
        // kill a live tunnel.
        if spec.hops.is_empty() {
            return Err(
                "tunnel spec has no hops; at least one jump/login host is required".to_string(),
            );
        }
        if spec.target_host.trim().is_empty() {
            return Err(
                "tunnel spec has no target host; the -L forward destination is required"
                    .to_string(),
            );
        }
        if let Some(old) = self.handles.remove(key) {
            old.kill();
        }

        let local_port = free_loopback_port()?;
        let binary = self
            .binary
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_SSH_BINARY));
        let mut std_command = ssh_command(&binary, spec, local_port);
        std_command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        let mut child = Command::from(std_command)
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("Failed to spawn {}: {error}", binary.display()))?;

        let deadline = tokio::time::Instant::now() + SPAWN_HEALTH_TIMEOUT;
        let outcome = loop {
            if let Ok(Some(_)) = child.try_wait() {
                break Err(drain_stderr_tail(&mut child).await);
            }
            if tokio::net::TcpStream::connect(("127.0.0.1", local_port))
                .await
                .is_ok()
            {
                break Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                if matches!(child.try_wait(), Ok(None)) {
                    break Ok(());
                }
                break Err(drain_stderr_tail(&mut child).await);
            }
            sleep(SPAWN_HEALTH_INTERVAL).await;
        };

        match outcome {
            Err(stderr_tail) => {
                let _ = child.start_kill();
                Err(format!(
                    "ssh tunnel exited while establishing 127.0.0.1:{local_port}{stderr_tail}"
                ))
            }
            Ok(()) => {
                self.handles.insert(
                    key.to_string(),
                    TunnelHandle {
                        child: std::sync::Mutex::new(child),
                        local_port,
                        binary: binary.clone(),
                    },
                );
                Ok(local_port)
            }
        }
    }

    /// Kill and forget the forwarder (idempotent on unknown key). Callers
    /// must rewrite any cached endpoint before the next dial.
    pub fn release(&mut self, key: &str) {
        if let Some(handle) = self.handles.remove(key) {
            handle.kill();
        }
    }

    /// True while a forwarder is registered for `key` and its child has not
    /// exited. A crashed forwarder fails this; re-establish respawns.
    pub fn is_running(&self, key: &str) -> bool {
        self.handles.get(key).is_some_and(TunnelHandle::is_running)
    }

    /// The loopback port handed out by [`TunnelSupervisor::establish`].
    pub fn local_port(&self, key: &str) -> Option<u16> {
        self.handles.get(key).map(|handle| handle.local_port)
    }

    pub fn len(&self) -> usize {
        self.handles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }
}

/// Ephemeral loopback reservation: bind `127.0.0.1:0`, read the port, drop.
fn free_loopback_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("cannot bind loopback for tunnel port: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("cannot read loopback port: {error}"))?
        .port();
    drop(listener);
    Ok(port)
}

/// Reads the child's remaining stderr and keeps only the tail — ssh banners
/// and diagnostics can be long, and the actionable lines (auth refusals,
/// DNS failures) are usually last. May contain host names, never credentials.
async fn drain_stderr_tail(child: &mut Child) -> String {
    let mut text = String::new();
    if let Some(pipe) = child.stderr.take() {
        use tokio::io::AsyncReadExt;
        let mut pipe = pipe;
        let _ =
            tokio::time::timeout(Duration::from_millis(500), pipe.read_to_string(&mut text)).await;
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let tail: String = trimmed
        .chars()
        .rev()
        .take(1000)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("\nssh stderr: {tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop(host: &str) -> Hop {
        Hop {
            host: host.to_string(),
            port: 0,
            username: String::new(),
        }
    }

    fn user_hop(username: &str, host: &str, port: u16) -> Hop {
        Hop {
            host: host.to_string(),
            port,
            username: username.to_string(),
        }
    }

    fn spec(hops: Vec<Hop>, target_host: &str, target_port: u16) -> TunnelSpec {
        TunnelSpec {
            hops,
            target_host: target_host.to_string(),
            target_port,
            identity_file: String::new(),
        }
    }

    fn args_of(command: &std::process::Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn single_hop_shape_has_no_proxyjump_and_carries_batchmode() {
        let command = ssh_command(
            Path::new("ssh"),
            &spec(vec![user_hop("ops", "bastion.corp", 22)], "svc.internal", 443),
            9000,
        );
        let args = args_of(&command);
        assert_eq!(
            args,
            vec![
                "-N",
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "ServerAliveInterval=30",
                "-p",
                "22",
                "-L",
                "127.0.0.1:9000:svc.internal:443",
                "ops@bastion.corp",
            ]
        );
        // Key-only design marker: BatchMode guarantees ssh never prompts, so
        // argv can never carry password-type content (the struct holds none).
        assert!(
            args.windows(2).any(|w| w[0] == "-o" && w[1] == "BatchMode=yes"),
            "BatchMode must be present"
        );
    }

    #[test]
    fn multi_hop_routes_earlier_hops_through_proxyjump() {
        let hops = vec![
            user_hop("u1", "j1.corp", 2222),
            hop("j2.corp"),
            user_hop("admin", "target.corp", 2200),
        ];
        let command = ssh_command(Path::new("ssh"), &spec(hops, "nas.lan", 445), 9100);
        let args = args_of(&command);
        let jump_at = args
            .iter()
            .position(|arg| arg == "-J")
            .expect("-J present for multi-hop");
        assert_eq!(args[jump_at + 1], "u1@j1.corp:2222,j2.corp");
        assert_eq!(
            args.last().map(String::as_str),
            Some("admin@target.corp"),
            "last hop is the login target"
        );
        // The login port travels via `-p`, never inside the destination —
        // `user@host:port` there would be resolved as one hostname.
        let p_at = args.iter().position(|arg| arg == "-p").expect("-p present");
        assert_eq!(args[p_at + 1], "2200");
    }

    #[test]
    fn default_username_and_port_stay_implicit() {
        let command = ssh_command(
            Path::new("ssh"),
            &spec(vec![hop("bastion.corp")], "svc.internal", 443),
            9000,
        );
        let args = args_of(&command);
        assert_eq!(args.last().map(String::as_str), Some("bastion.corp"));
        assert!(
            !args.iter().any(|arg| arg == "-J"),
            "single hop never gets -J"
        );
        assert!(
            !args.iter().any(|arg| arg == "-i"),
            "empty identity_file means agent/default keys"
        );
    }

    #[test]
    fn identity_file_maps_to_dash_i() {
        let spec = TunnelSpec {
            identity_file: "/home/u/.ssh/id_ed25519".to_string(),
            ..spec(vec![hop("bastion.corp")], "svc.internal", 443)
        };
        let args = args_of(&ssh_command(Path::new("ssh"), &spec, 9000));
        let identity_at = args
            .iter()
            .position(|arg| arg == "-i")
            .expect("-i present");
        assert_eq!(args[identity_at + 1], "/home/u/.ssh/id_ed25519");
    }

    #[test]
    fn rewrite_endpoint_keeps_scheme_and_replaces_authority() {
        assert_eq!(
            rewrite_endpoint("https://s3.internal:443", 9000),
            "https://127.0.0.1:9000"
        );
        assert_eq!(rewrite_endpoint("http://x", 9000), "http://127.0.0.1:9000");
        assert_eq!(rewrite_endpoint("nas.local:445", 9000), "127.0.0.1:9000");
        assert_eq!(rewrite_endpoint("s3.internal", 9000), "127.0.0.1:9000");
        // Scheme detection is case-insensitive; the original casing is kept.
        assert_eq!(
            rewrite_endpoint("HTTPS://S3.Internal", 9000),
            "HTTPS://127.0.0.1:9000"
        );
    }

    #[cfg(unix)]
    fn write_stub(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write stub");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
        path
    }

    /// Live-process stub: ssh exits immediately → establish fails with the
    /// child's stderr tail and registers nothing.
    #[tokio::test]
    #[cfg(unix)]
    async fn establish_surfaces_child_stderr_when_ssh_exits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = write_stub(dir.path(), "ssh-die", "echo bad-key >&2\nexit 255");
        let mut supervisor = TunnelSupervisor::new();
        supervisor.set_binary(stub);
        let error = supervisor
            .establish(
                "conn",
                &spec(vec![hop("bastion.corp")], "svc.internal", 443),
            )
            .await
            .expect_err("immediately exiting ssh must fail establish");
        assert!(error.contains("bad-key"), "stderr tail must surface: {error}");
        assert!(!supervisor.is_running("conn"));
        assert!(
            supervisor.is_empty(),
            "failed establish must not register a handle"
        );
    }

    /// Live-process stub: ssh stays alive through the health window (the
    /// deadline path reports the reserved port), same-key re-establish
    /// replaces the old child, release forgets the key, unknown keys are
    /// no-ops.
    #[tokio::test]
    #[cfg(unix)]
    async fn living_forwarder_is_replaced_on_same_key_and_released() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = write_stub(dir.path(), "ssh-sleep", "sleep 30");
        let mut supervisor = TunnelSupervisor::new();
        supervisor.set_binary(stub);

        let port = supervisor
            .establish(
                "conn",
                &spec(vec![hop("bastion.corp")], "svc.internal", 443),
            )
            .await
            .expect("living child survives the health window");
        assert!(supervisor.is_running("conn"));
        assert_eq!(supervisor.local_port("conn"), Some(port));
        assert_eq!(supervisor.len(), 1);

        let replaced = supervisor
            .establish("conn", &spec(vec![hop("edge.corp")], "nas.local", 445))
            .await
            .expect("replacement establish");
        assert_eq!(
            supervisor.len(),
            1,
            "same key replaces, never accumulates"
        );
        assert_eq!(supervisor.local_port("conn"), Some(replaced));

        supervisor.release("conn");
        assert!(!supervisor.is_running("conn"));
        assert!(supervisor.is_empty());
        // Idempotent on unknown keys.
        supervisor.release("no-such-key");
        assert_eq!(supervisor.len(), 0);
    }

    /// Live-process stub: distinct keys hold concurrent forwarders on
    /// distinct ports.
    #[tokio::test]
    #[cfg(unix)]
    async fn distinct_keys_hold_distinct_live_forwarders() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = write_stub(dir.path(), "ssh-sleep", "sleep 30");
        let mut supervisor = TunnelSupervisor::new();
        supervisor.set_binary(stub);
        let first = supervisor
            .establish(
                "a",
                &spec(vec![hop("bastion.corp")], "svc.internal", 443),
            )
            .await
            .expect("first forwarder");
        let second = supervisor
            .establish(
                "b",
                &spec(vec![hop("bastion.corp")], "svc.internal", 443),
            )
            .await
            .expect("second forwarder");
        assert_ne!(first, second, "distinct keys must reserve distinct ports");
        assert_eq!(supervisor.len(), 2);
        assert!(supervisor.is_running("a") && supervisor.is_running("b"));
        supervisor.release("a");
        supervisor.release("b");
        assert!(supervisor.is_empty());
    }

    #[tokio::test]
    async fn establish_rejects_empty_chain_and_empty_target() {
        let mut supervisor = TunnelSupervisor::new();
        let error = supervisor
            .establish("k", &spec(Vec::new(), "svc.internal", 443))
            .await
            .expect_err("empty hop chain must be rejected");
        assert!(error.contains("no hops"), "{error}");
        assert!(supervisor.is_empty());
        let error = supervisor
            .establish("k", &spec(vec![hop("bastion.corp")], " ", 443))
            .await
            .expect_err("blank target host must be rejected");
        assert!(error.contains("target host"), "{error}");
        assert!(supervisor.is_empty());
    }
}
