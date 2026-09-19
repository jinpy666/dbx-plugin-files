//! rclone rcd engine foundation (F-RCLONE phase A).
//!
//! Storage runs through a managed `rclone rcd`
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
//! auth and bucket enumeration all come for free, with no custom adapters.

pub mod archive;
pub mod bytes_channel;
pub mod ops;
pub mod proc;
pub mod rc;
pub mod registry;
pub mod sync;
#[allow(dead_code)] // several pub helpers exist for the module's own tests
pub mod tunnel;

pub use proc::{RcdEnv, RcdHandle, RcdSupervisor, MIN_RCLONE_VERSION};
pub use rc::{RcClient, RcError};
pub use tunnel::{TunnelSpec, rewrite_endpoint};

use std::collections::HashMap;

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
    /// In-flight marker for the connection's proxy group: dropped with the
    /// task on finish/cancel, releasing the idle-group teardown hold.
    pub(crate) _work: WorkGuard,
}

/// Phase B download pump slot (the `transfers::DownloadSlot` twin): pump
/// coordination flags plus the save_to_local `.part`
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
    /// In-flight marker for the connection's proxy group: dropped with the
    /// slot on pump exit, releasing the idle-group teardown hold.
    pub(crate) _work: WorkGuard,
}

/// Reserved `connectionId` for the sidecar-local filesystem (dual-pane left
/// column, "local files" side). Never present in the host connection table:
/// [`RcloneEngine::binding`] folds it in on demand without registration —
/// local fs stays config-free, so a rcd respawn cannot orphan it.
pub const LOCAL_CONNECTION_ID: &str = "__local__";

/// Synthesized connection record behind [`LOCAL_CONNECTION_ID`]: an fs
/// connection rooted at `/` with default gates (writable, deletable,
/// unlocked root); policy and quick-path behavior are uniform with every
/// other fs connection.
pub fn local_connection() -> crate::model::StoredConnection {
    crate::model::StoredConnection::from_lifecycle_params(&serde_json::json!({
        "connection": {
            "id": LOCAL_CONNECTION_ID,
            "name": "Local",
            "external_config": { "protocol": "fs", "root": "/" }
        }
    }))
    .expect("local connection record is a constant shape")
}

/// Engine facade held by the plugin: the rcd supervisor plus the
/// connection→remote registry. `main.rs` routes every storage method here
/// (docs/IMPL_PLAN_RCLONE.zh-CN.md §2).
pub struct RcloneEngine {
    pub supervisor: tokio::sync::Mutex<RcdSupervisor>,
    pub registry: registry::Registry,
    /// Phase B upload staging sinks keyed by taskId (`files/upload/start`
    /// inserts, `finish`/`cancel`/append-failure remove). `handle_binary`
    /// reads membership FIRST — a miss falls through to the download-map
    /// check, so this map is also the frame-routing check.
    /// std Mutex: every hold is a short sync section; nothing awaits under
    /// the lock.
    pub uploads: std::sync::Mutex<std::collections::HashMap<String, UploadTask>>,
    /// Phase B download pump slots keyed by taskId.
    pub downloads: std::sync::Mutex<std::collections::HashMap<String, DownloadTask>>,
    /// Phase B single-file job records with the full `TransferJob` payload
    /// (progress events, terminal finish replay). Kept after terminal states
    /// so a late finish replays the stored outcome instead of reporting
    /// not-found.
    pub jobs: std::sync::Mutex<std::collections::HashMap<String, crate::transfers::TransferJob>>,
    /// Phase D transfers-history persistence: hydrated `Option<Arc<Store>>`
    /// (set once by `Plugin::new`). Terminal single-file jobs are appended to
    /// the shared `transfers.json`, so the reveal/open local-download
    /// whitelist and the restart-safe panel history keep working. Dir jobs
    /// (syncDir/copyDir) stay memory-only: `TransferRecord.kind` is
    /// upload|download only, and the wire-visible DirJob shape has no
    /// persisted counterpart. Concurrency: writes are serialized by
    /// [`crate::history_write_lock`] (each write is an atomic tmp+rename, so
    /// a race costs at most one dropped history line, never a corrupt file).
    pub history: std::sync::Mutex<Option<std::sync::Arc<crate::store::Store>>>,
    /// Local `ssh -N -L` forwarders keyed by connection id (tunnel channel:
    /// `prepare` establishes and rewrites the endpoint, `release_tunnel`
    /// tears down on disconnect).
    pub tunnels: tokio::sync::Mutex<tunnel::TunnelSupervisor>,
    /// Per-proxy-group in-flight async work (uploads / download pumps / dir
    /// jobs). `WorkGuard` guards keep the counts exact on every terminal
    /// path; the idle-group teardown and the keepalive reaper never kill an
    /// rcd while a group's counter is non-zero.
    work: std::sync::Arc<std::sync::Mutex<HashMap<String, u64>>>,
}

/// Per-group in-flight work guard: incremented at job start, decremented
/// exactly once on drop or explicit [`WorkGuard::done`] — clones share the
/// done flag, so cloned handles never double-decrement.
#[derive(Clone)]
pub(crate) struct WorkGuard {
    inner: std::sync::Arc<WorkGuardInner>,
}

struct WorkGuardInner {
    work: std::sync::Arc<std::sync::Mutex<HashMap<String, u64>>>,
    key: String,
    done: std::sync::atomic::AtomicBool,
}

impl WorkGuardInner {
    /// Settles exactly once: releases the group's in-flight hold. Later
    /// calls (including the Drop) are no-ops.
    fn settle(&self) {
        if self
            .done
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        if let Ok(mut work) = self.work.lock() {
            if let Some(count) = work.get_mut(&self.key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    work.remove(&self.key);
                }
            }
        }
    }
}

impl WorkGuard {
    /// Settles the guard early (job reached a terminal state while its
    /// record is kept for late-finish replay). Idempotent.
    pub(crate) fn done(&self) {
        self.inner.settle();
    }
}

impl Drop for WorkGuardInner {
    fn drop(&mut self) {
        self.settle();
    }
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
            tunnels: tokio::sync::Mutex::new(tunnel::TunnelSupervisor::new()),
            work: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }


    /// Live rc client, spawning or respawning rcd as needed.
    pub async fn client(&self) -> Result<RcClient, String> {
        self.supervisor.lock().await.client().await
    }

    /// Connection-id → binding lookup shared by the workbench route and the
    /// MCP route. Folds in the built-in `__local__` connection (the
    /// dual-pane local column): the synthesized root-`/` fs connection. The
    /// binding is never registered — local fs stays config-free, so a rcd
    /// respawn cannot orphan it.
    pub fn binding(&self, connection_id: &str) -> Result<registry::RemoteBinding, String> {
        if connection_id == LOCAL_CONNECTION_ID {
            return registry::binding_for(&local_connection());
        }
        self.registry
            .get(connection_id)
            .ok_or_else(|| "Connection is not connected (rclone engine)".to_string())
    }

    /// Routed rc client for an already-resolved binding: connections that
    /// carry a proxy spec land in their own rcd group (proxy env on the
    /// child process covers the HTTP-family backends), everything else
    /// shares the inherited-env "direct" rcd.
    pub async fn client_for_binding(
        &self,
        binding: &registry::RemoteBinding,
    ) -> Result<RcClient, String> {
        let (key, env) = proxy_route(binding.proxy.as_ref());
        let (client, respawned) = self
            .supervisor
            .lock()
            .await
            .client_for(&key, env.as_ref())
            .await?;
        if respawned {
            self.replay_group_registrations(&client, &key).await?;
        }
        Ok(client)
    }

    /// Routed rc client by connection id (registry lookup). The synthesized
    /// `__local__` binding has no proxy and routes to the direct group.
    pub async fn client_for_id(&self, connection_id: &str) -> Result<RcClient, String> {
        let binding = self.binding(connection_id)?;
        self.client_for_binding(&binding).await
    }

    /// Routed rc client for a not-yet-registered connection — the
    /// `connection/test` and `connection/connect` arms route by the parsed
    /// params because the registry entry does not exist yet.
    pub async fn client_for(
        &self,
        connection: &crate::model::StoredConnection,
    ) -> Result<RcClient, String> {
        let (key, env) = proxy_route(connection.proxy.as_ref());
        let (client, respawned) = self
            .supervisor
            .lock()
            .await
            .client_for(&key, env.as_ref())
            .await?;
        if respawned {
            self.replay_group_registrations(&client, &key).await?;
        }
        Ok(client)
    }

    /// Replays every connection's `config/create` into a freshly spawned
    /// group rcd — a respawn starts from an empty temp config, so without
    /// this every remote in the group would break until manually
    /// reconnected. The fresh config holds nothing; the delete is
    /// best-effort hygiene against half-written sections.
    async fn replay_group_registrations(
        &self,
        client: &RcClient,
        key: &str,
    ) -> Result<(), String> {
        for registration in self.registry.registrations_in_group(key) {
            let _ = client.config_delete(&registration.name).await;
            client
                .config_create(
                    &registration.name,
                    &registration.backend_type,
                    registration.parameters,
                    registration.obscure,
                )
                .await
                .map_err(|error| {
                    format!(
                        "failed to re-register remote '{}' after an rcd respawn: {error}",
                        registration.name
                    )
                })?;
        }
        Ok(())
    }

    /// Starts the keepalive watchdog: periodic sweeps that (a) respawn and
    /// re-register every connected group whose rcd crashed — the first user
    /// request after a crash then just works instead of paying spawn latency
    /// or failing on a stale remote — and (b) reap proxy groups that lost
    /// every connection and have no in-flight work. Interval via
    /// `DBX_FILES_RCLONE_KEEPALIVE_SECS` (default 30; 0 disables). Call once
    /// with the shared engine handle after construction.
    pub fn start_keepalive(self: &std::sync::Arc<Self>) {
        let secs = std::env::var("DBX_FILES_RCLONE_KEEPALIVE_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(30);
        if secs == 0 {
            return;
        }
        let engine = std::sync::Arc::clone(self);
        // Own single-thread runtime: Plugin::new runs before the sidecar's
        // main tokio runtime exists, so a bare tokio::spawn here panics.
        std::thread::Builder::new()
            .name("rclone-keepalive".to_string())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("keepalive runtime");
                runtime.block_on(async move {
                    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(secs));
                    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        ticker.tick().await;
                        engine.keepalive_sweep().await;
                    }
                });
            })
            .expect("keepalive thread");
    }

    /// One keepalive sweep (see [`Self::start_keepalive`]).
    pub async fn keepalive_sweep(&self) {
        for key in self.registry.live_group_keys() {
            let proxy = self.registry.proxy_in_group(&key);
            let (route_key, env) = proxy_route(proxy.as_ref());
            let (client, respawned) = match self
                .supervisor
                .lock()
                .await
                .client_for(&route_key, env.as_ref())
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    eprintln!("rclone keepalive: group respawn failed: {error}");
                    continue;
                }
            };
            if respawned {
                if let Err(error) = self.replay_group_registrations(&client, &route_key).await {
                    eprintln!("rclone keepalive: {error}");
                }
            }
        }
        self.reap_idle_groups().await;
    }

    /// Kills rcds of proxy groups that lost every connection and have no
    /// in-flight work. The shared `"direct"` group is skipped: local-pane
    /// traffic and `__local__` respawn it lazily, and one idle process
    /// costs less than respawning it per operation.
    async fn reap_idle_groups(&self) {
        let reaped: Vec<String> = self
            .supervisor
            .lock()
            .await
            .group_keys()
            .into_iter()
            .filter(|key| key != "direct")
            .filter(|key| self.registry.count_in_group(key) == 0 && self.active_work(key) == 0)
            .collect();
        if reaped.is_empty() {
            return;
        }
        let mut supervisor = self.supervisor.lock().await;
        for key in reaped {
            supervisor.shutdown_group(&key);
        }
    }

    /// Stops a proxy group's rcd when its last connection is gone and no
    /// async work references it (`true` when the rcd was stopped). Called
    /// from `connection/disconnect`; groups still draining async work are
    /// reaped by the keepalive sweep once the work settles.
    pub async fn shutdown_group_if_idle(&self, key: &str) -> bool {
        if self.registry.count_in_group(key) > 0 || self.active_work(key) > 0 {
            return false;
        }
        self.supervisor.lock().await.shutdown_group(key)
    }

    /// Marks one unit of in-flight work for a proxy group. The returned
    /// guard decrements on drop (or [`WorkGuard::done`]).
    pub(crate) fn start_work(&self, key: &str) -> WorkGuard {
        if let Ok(mut work) = self.work.lock() {
            *work.entry(key.to_string()).or_insert(0) += 1;
        }
        WorkGuard {
            inner: std::sync::Arc::new(WorkGuardInner {
                work: std::sync::Arc::clone(&self.work),
                key: key.to_string(),
                done: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    /// In-flight work count for a proxy group.
    pub(crate) fn active_work(&self, key: &str) -> u64 {
        self.work
            .lock()
            .ok()
            .and_then(|work| work.get(key).copied())
            .unwrap_or(0)
    }

    /// Tunnel channel: when the connection carries a tunnel spec, ensure
    /// the `ssh -N -L` forwarder for `key` is up and return the connection
    /// with its endpoint rewritten to `127.0.0.1:<local_port>` so the
    /// backend params dial through the tunnel. Without a tunnel spec this
    /// is an identity clone. `key` is normally the connection id; the test
    /// arm passes a `::test`-suffixed key so a probe never disrupts a live
    /// forwarder on the same connection.
    pub async fn prepare(
        &self,
        key: &str,
        connection: &crate::model::StoredConnection,
    ) -> Result<crate::model::StoredConnection, String> {
        let Some(tunnel) = connection.tunnel.as_ref() else {
            return Ok(connection.clone());
        };
        let (target_host, target_port) = tunnel_target(connection)?;
        let spec = TunnelSpec {
            hops: tunnel
                .jump_hosts
                .iter()
                .map(|hop| tunnel::Hop {
                    host: hop.host.clone(),
                    port: hop.port,
                    username: hop.username.clone(),
                })
                .collect(),
            target_host,
            target_port,
            identity_file: tunnel.identity_file.clone(),
        };
        let mut tunnels = self.tunnels.lock().await;
        let local_port = tunnels.establish(key, &spec).await?;
        drop(tunnels);
        let mut prepared = connection.clone();
        prepared.endpoint = rewrite_endpoint(&connection.endpoint, local_port);
        Ok(prepared)
    }

    /// Tear down the forwarder(s) of a connection: the live key plus the
    /// test-probe key (`::test` suffix) a `connection/test` may have left.
    pub async fn release_tunnel(&self, connection_id: &str) {
        let mut tunnels = self.tunnels.lock().await;
        tunnels.release(connection_id);
        tunnels.release(&format!("{connection_id}::test"));
    }
}

/// Forward destination for a connection's tunnel: the endpoint's host and
/// port as reachable from the last jump hop. The endpoint may carry a
/// `scheme://` prefix and an optional `user@` prefix; the host may be a
/// bracketed IPv6 literal (`[::1]:8443`). A missing port falls back to the
/// scheme default (https/http), then the protocol default (ftp/sftp/smb);
/// otherwise the endpoint must carry an explicit port.
fn tunnel_target(
    connection: &crate::model::StoredConnection,
) -> Result<(String, u16), String> {
    let endpoint = connection.endpoint.trim();
    let missing = |what: &str| {
        format!("connection '{}' endpoint '{endpoint}' {what}", connection.id)
    };
    if endpoint.is_empty() {
        return Err(missing("configures a tunnel but the connection has no endpoint"));
    }
    let (scheme, authority) = match endpoint.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest.trim_start_matches('/')),
        None => (String::new(), endpoint),
    };
    // Drop an optional userinfo component (`user@host[:port]`).
    let authority = authority
        .rsplit_once('@')
        .map(|(_, rest)| rest)
        .unwrap_or(authority);
    let (host, port_raw) = if let Some(rest) = authority.strip_prefix('[') {
        let (inner, tail) = rest
            .split_once(']')
            .ok_or_else(|| missing("has an unterminated IPv6 bracket"))?;
        (inner, tail.strip_prefix(':').map(str::to_owned))
    } else {
        match authority.rsplit_once(':') {
            Some((head, tail)) => (head, (!tail.is_empty()).then(|| tail.to_owned())),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return Err(missing("has no host"));
    }
    let port = match port_raw.as_deref() {
        Some(raw) => raw
            .parse::<u16>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| missing("has an invalid port"))?,
        None => match scheme.as_str() {
            "https" => 443,
            "http" => 80,
            "ftp" => 21,
            "sftp" => 22,
            "smb" => 445,
            _ => match connection.protocol.as_str() {
                "ftp" => 21,
                "sftp" | "sftp-native" => 22,
                "smb" => 445,
                _ => {
                    return Err(format!(
                        "connection '{}' endpoint '{endpoint}' must carry an explicit port \
                         when a tunnel is configured (no default for scheme '{scheme}', \
                         protocol '{}')",
                        connection.id, connection.protocol
                    ))
                }
            },
        },
    };
    Ok((host.to_string(), port))
}

#[cfg(test)]
mod tunnel_target_tests {
    use super::*;
    use crate::model::StoredConnection;
    use serde_json::json;

    fn connection(endpoint: &str, protocol: &str) -> crate::model::StoredConnection {
        StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "c",
                "external_config": { "protocol": protocol, "endpoint": endpoint }
            }
        }))
        .expect("connection parses")
    }

    #[test]
    fn scheme_and_explicit_port_win() {
        assert_eq!(
            tunnel_target(&connection("https://s3.internal:8443", "s3")).unwrap(),
            ("s3.internal".to_string(), 8443)
        );
        assert_eq!(
            tunnel_target(&connection("ftp://files.local:2121", "ftp")).unwrap(),
            ("files.local".to_string(), 2121)
        );
    }

    #[test]
    fn scheme_and_protocol_defaults_apply_without_port() {
        assert_eq!(
            tunnel_target(&connection("https://s3.internal", "s3")).unwrap(),
            ("s3.internal".to_string(), 443)
        );
        assert_eq!(
            tunnel_target(&connection("127.0.0.1", "sftp")).unwrap(),
            ("127.0.0.1".to_string(), 22)
        );
        assert_eq!(
            tunnel_target(&connection("nas.local", "smb")).unwrap(),
            ("nas.local".to_string(), 445)
        );
    }

    #[test]
    fn no_default_means_explicit_port_required() {
        let error = tunnel_target(&connection("s3.internal", "s3")).unwrap_err();
        assert!(error.contains("explicit port"), "{error}");
        let error = tunnel_target(&connection("", "s3")).unwrap_err();
        assert!(error.contains("no endpoint"), "{error}");
    }

    #[test]
    fn userinfo_prefix_is_dropped() {
        assert_eq!(
            tunnel_target(&connection("ops@j2.corp:22", "s3")).unwrap(),
            ("j2.corp".to_string(), 22)
        );
    }

    #[test]
    fn bracketed_ipv6_parses_with_and_without_port() {
        assert_eq!(
            tunnel_target(&connection("[::1]:8443", "s3")).unwrap(),
            ("::1".to_string(), 8443)
        );
        assert_eq!(
            tunnel_target(&connection("https://[2001:db8::9]", "s3")).unwrap(),
            ("2001:db8::9".to_string(), 443)
        );
    }
}

/// rcd group key + child env for a connection's proxy spec. `None` → the
/// "direct" group with no overrides (today's inherit-everything behavior,
/// so ambient `HTTP_PROXY`/`HTTPS_PROXY` keep working). Socks5 values must
/// carry the `socks5://` scheme for Go's `net/http`; the bare form used by
/// the per-remote ftp/sftp backend options lives in
/// `registry::params_for` instead.
fn proxy_route(
    proxy: Option<&crate::model::ProxyConfig>,
) -> (String, Option<RcdEnv>) {
    let key = registry::group_key_of(proxy);
    match proxy {
        None => (key, None),
        Some(proxy) => {
            let url = match proxy.kind {
                crate::model::ProxyKind::Http => proxy.url(),
                crate::model::ProxyKind::Socks5 => format!("socks5://{}", proxy.url()),
            };
            (
                key,
                Some(RcdEnv {
                    http_proxy: Some(url.clone()),
                    https_proxy: Some(url),
                    // An inherited NO_PROXY could bypass the proxy for the
                    // target host — a proxied group always proxies.
                    no_proxy: None,
                }),
            )
        }
    }
}

/// Server-side rclone transfers (`operations/copy`, `sync/copy`) run both
/// fs strings inside one rcd, so source and target must live in the same
/// proxy group. Cross-group pairs cannot be served without streaming the
/// bytes through the sidecar — refused with an actionable message.
pub fn ensure_same_proxy_group(
    source: &registry::RemoteBinding,
    target: &registry::RemoteBinding,
) -> Result<(), String> {
    let key_of = |binding: &registry::RemoteBinding| {
        binding
            .proxy
            .as_ref()
            .map(|proxy| proxy.group_key())
            .unwrap_or_else(|| "direct".to_string())
    };
    if key_of(source) != key_of(target) {
        return Err(
            "source and target connections use different proxy settings; server-side \
             transfer requires one shared proxy group — align the proxy settings or \
             transfer through a local intermediate"
                .to_string(),
        );
    }
    Ok(())
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

    #[test]
    fn work_guard_counts_are_exact() {
        let engine = RcloneEngine::new();
        let g1 = engine.start_work("direct");
        assert_eq!(engine.active_work("direct"), 1);
        let g2 = engine.start_work("direct");
        assert_eq!(engine.active_work("direct"), 2);
        let g3 = g2.clone();
        drop(g2);
        assert_eq!(engine.active_work("direct"), 2, "clone keeps the guard alive");
        g3.done();
        assert_eq!(engine.active_work("direct"), 1, "done() settles exactly once");
        drop(g1);
        assert_eq!(engine.active_work("direct"), 0);
        drop(g3);
        assert_eq!(engine.active_work("direct"), 0, "settled clone never re-decrements");
    }

    fn binding(backend_type: &str, remote_fs: &str, root: &str) -> registry::RemoteBinding {
        registry::RemoteBinding {
            remote_fs: remote_fs.to_string(),
            backend_type: backend_type.to_string(),
            root: root.to_string(),
            lock_to_root: false,
            read_only: false,
            allow_delete: true,
            proxy: None,
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
            .binding(LOCAL_CONNECTION_ID)
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

        let handle = RcdHandle::start(&binary, None).await.expect("rcd spawn");
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
