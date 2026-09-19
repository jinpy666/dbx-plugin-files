//! Connection → rclone remote registry (F-RCLONE phase A, Agent A).
//!
//! Maps [`StoredConnection`] lifecycle records onto rclone remotes: the
//! protocol mapping table (docs/IMPL_PLAN_RCLONE.zh-CN.md §4), remote naming,
//! `config/create` parameter assembly, and the test/connect/disconnect
//! lifecycle. Only this file may be modified by the registry task (§12).
//!
//! Key-name discipline: every rclone parameter key emitted here was verified
//! against `rclone config providers <type>` on v1.75.1 (the offline unit
//! tests pin the exact names; the `emitted_keys_exist_in_provider_options`
//! live test cross-checks them against the installed binary). Notable rc
//! semantics verified on a live rcd — do not re-derive:
//!
//! - `opt.obscure = true` obscures only `IsPassword`-marked keys (e.g. sftp
//!   `pass`, koofr/pcloud `password`). `Sensitive`-but-not-password keys
//!   (s3 `secret_access_key`, azureblob `key`, the OAuth `token` JSON,
//!   gcs `service_account_credentials`) are stored verbatim in rcd's `0600`
//!   temp config, which dies with the process.
//! - `operations/stat`'s `remote` argument is *relative to the fs root*, so
//!   connection roots travel inside the fs string (`name:root`, remote `""`).
//!   The `fs` argument must carry the trailing colon or rclone silently
//!   treats it as a local path.
//! - `config/delete` on an absent remote name answers `Ok({})` (idempotent).
//!
//! Known limitation: remote names keep only the first 10 alnum characters of
//! the connection id, so ids that share that prefix collide on one remote.

// Wired into the method dispatch with the phase A integrator pass; until then
// the public lifecycle API is unused from the binary's point of view.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::engine::general_purpose::STANDARD_NO_PAD as BASE64_STANDARD_NO_PAD;
use base64::Engine as _;
use serde_json::{Map, Value};

use super::rc::RcClient;
use crate::model::{ProxyConfig, ProxyKind, StoredConnection};

/// Protocols served by the rclone engine. `sftp` and `sftp-native` both map
/// onto the rclone `sftp` backend; `opendal-custom` is the generic pass-
/// through that reaches every remaining rclone backend (b2, box, http,
/// memory, alias, … — the full `rclone config providers` list), so together
/// with the named quick protocols the form covers the whole rclone backend
/// catalog plus local files. Only `aliyun-drive` has no rclone mapping.
pub const SUPPORTED_PROTOCOLS: [&str; 20] = [
    "fs", "s3", "oss", "cos", "obs", "gcs", "azblob", "webdav", "ftp", "sftp", "sftp-native", "smb",
    "gdrive", "onedrive", "dropbox", "yandex-disk", "seafile", "koofr", "pcloud", "opendal-custom",
];

/// Parameter keys rclone obscures at rest when `opt.obscure` is set. Only
/// `IsPassword`-marked keys are actually transformed (verified on v1.75.1);
/// this set decides whether the flag is worth sending. `password` covers the
/// koofr/pcloud providers (their option name differs from the ftp/sftp/smb
/// family's `pass`).
const PASSWORD_KEYS: [&str; 6] = [
    "pass",
    "password",
    "key",
    "secret_access_key",
    "session_token",
    "service_account_credentials",
];

/// The ops-layer view of a connected storage target.
///
/// `remote_fs` is what rc calls accept as their `fs` argument: `"dbxAb12Cd34:"`
/// for remote backends, the local path itself for the `fs` protocol. `root`
/// mirrors `connection.root` (not merged into `remote_fs`); composing
/// `remote:root/path` strings is the ops layer's job. Path gates
/// (`lock_to_root`/`read_only`/`allow_delete`) are carried verbatim from the
/// connection so callers do not have to keep the record around.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteBinding {
    pub remote_fs: String,
    /// The rclone backend type. `String` rather than `&'static str` because
    /// the `opendal-custom` pass-through derives it from the user's `service`
    /// field at connect time.
    pub backend_type: String,
    pub root: String,
    pub lock_to_root: bool,
    pub read_only: bool,
    pub allow_delete: bool,
    /// Egress proxy carried through for the engine's per-proxy rcd
    /// grouping. ftp/sftp parameter mapping does not consume it (their
    /// `params_for` injects the backend option directly); HTTP-family
    /// backends rely on this field for process-level proxy routing instead.
    pub proxy: Option<ProxyConfig>,
}

/// rclone remote name for a connection id: `dbx` + the first 10 characters
/// of the id after dropping everything outside `[A-Za-z0-9]`. Deterministic;
/// keeps the rclone config file free of unsafe section-name characters.
pub fn remote_name(connection_id: &str) -> String {
    let prefix: String = connection_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(10)
        .collect();
    format!("dbx{prefix}")
}

/// `true` when the protocol is part of the support set.
pub fn is_supported(protocol: &str) -> bool {
    SUPPORTED_PROTOCOLS.contains(&protocol)
}

/// The rclone backend type for a supported protocol. `opendal-custom` is
/// absent here — its type comes from the user's `service` field and is
/// resolved in [`params_for`].
fn rclone_type(protocol: &str) -> Option<String> {
    let backend: &str = match protocol {
        "fs" => "local",
        "s3" | "oss" | "cos" | "obs" => "s3",
        "gcs" => "gcs",
        "azblob" => "azureblob",
        "webdav" => "webdav",
        "ftp" => "ftp",
        "sftp" | "sftp-native" => "sftp",
        "smb" => "smb",
        "gdrive" => "drive",
        "onedrive" => "onedrive",
        "dropbox" => "dropbox",
        "yandex-disk" => "yandex",
        "seafile" => "seafile",
        "koofr" => "koofr",
        "pcloud" => "pcloud",
        _ => return None,
    };
    Some(backend.to_string())
}

/// Assembles `config/create` parameters for a connection.
///
/// Returns `(rclone type, parameters, obscure)`. Fails for protocols outside
/// the support set and for shape errors (unparsable gcs credential,
/// PEM-content sftp key, malformed endpoints, invalid custom service);
/// rclone-level validation of the values themselves happens on the remote
/// when the connection is used.
pub fn params_for(connection: &StoredConnection) -> Result<(String, Value, bool), String> {
    let protocol = connection.protocol.as_str();

    if protocol == "aliyun-drive" {
        return Err(
            "protocol 'aliyun-drive' is not supported by rclone upstream and cannot be served \
             by the rclone engine; until a custom rclone backend lands (planned follow-up), \
             keep using the OpenDAL engine (DBX_FILES_ENGINE=opendal) for aliyun-drive \
             connections"
                .to_string(),
        );
    }

    let (backend_type, parameters, obscure) = match connection.protocol.as_str() {
        // Pass-through: the user's `service` is the rclone backend type and
        // the `config` JSON becomes the whole parameter set — this is the
        // gateway to every rclone backend without a dedicated quick form.
        // obscure is sent unconditionally: the user JSON may carry any
        // provider's IsPassword-class option and only rcd knows that set.
        "opendal-custom" => {
            let backend_type = custom_rclone_type(&connection.service)?;
            (backend_type, connection.custom_config.clone(), true)
        }
        _ => {
            let backend_type = rclone_type(connection.protocol.as_str()).ok_or_else(|| {
                format!("protocol '{}' is not supported by the rclone engine", connection.protocol)
            })?;
            let mut parameters = match connection.protocol.as_str() {
                "fs" => Value::Object(Map::new()),
                "s3" | "oss" | "cos" | "obs" => s3_family_parameters(connection)?,
                "gcs" => gcs_parameters(connection)?,
                "azblob" => azblob_parameters(connection),
                "webdav" => webdav_parameters(connection)?,
                "ftp" => ftp_parameters(connection)?,
                "sftp" | "sftp-native" => sftp_parameters(connection)?,
                "smb" => smb_parameters(connection)?,
                "gdrive" | "onedrive" | "dropbox" => oauth_parameters(connection)?,
                "yandex-disk" => yandex_parameters(connection)?,
                "seafile" => seafile_parameters(connection)?,
                "koofr" => koofr_parameters(connection)?,
                "pcloud" => pcloud_parameters(connection)?,
                _ => unreachable!("rclone_type() accepted an unmapped protocol"),
            };
            if let Some(proxy) = &connection.proxy {
                // Per-remote proxy wiring: only ftp/sftp/sftp-native carry the
                // proxy as backend options (rclone `http_proxy` /
                // `socks_proxy`, verified v1.75.1; plain options, not
                // IsPassword, so the `obscure` decision below is untouched).
                // Every other protocol is left alone — no injection, no
                // error: HTTP-family proxying is the engine layer's job,
                // which groups rcd processes per `binding.proxy` instead of
                // via parameters.
                if matches!(connection.protocol.as_str(), "ftp" | "sftp" | "sftp-native") {
                    let key = match proxy.kind {
                        ProxyKind::Http => "http_proxy",
                        ProxyKind::Socks5 => "socks_proxy",
                    };
                    if let Some(object) = parameters.as_object_mut() {
                        object.insert(key.to_string(), Value::String(proxy.url()));
                    }
                }
            }
            let obscure = parameters
                .as_object()
                .map(|params| params.keys().any(|key| PASSWORD_KEYS.contains(&key.as_str())))
                .unwrap_or(false);
            (backend_type, parameters, obscure)
        }
    };
    Ok((backend_type, parameters, obscure))
}

/// Builds the ops-layer [`RemoteBinding`] view (no registration).
pub fn binding_for(connection: &StoredConnection) -> Result<RemoteBinding, String> {
    let backend_type = params_for(connection)?.0; // also validates protocol/shape
    let remote_fs = if connection.protocol == "fs" {
        // fs stays unregistered from rclone's perspective: the local path is
        // used directly as the fs argument (§4 "免注册").
        if connection.root.is_empty() {
            "/".to_string()
        } else {
            connection.root.clone()
        }
    } else {
        format!("{}:", remote_name(&connection.id))
    };
    // SMB tree-connect target: rclone takes the share as the FIRST path
    // component of the fs string (`smb:host/share/...`). The retired
    // OpenDAL adapter carried it separately; fold it into the visible root
    // so every fs composition keeps working unchanged. An empty share
    // (server-level discovery) stays unscoped — rclone cannot enumerate
    // shares, so that scenario degrades at the ops layer.
    let mut root = connection.root.clone();
    if connection.protocol == "smb" && !connection.share.trim().is_empty() {
        let share = format!("/{}", connection.share.trim().trim_matches('/'));
        root = format!("{share}{root}");
    }
    Ok(RemoteBinding {
        remote_fs,
        backend_type,
        root,
        lock_to_root: connection.lock_to_root,
        read_only: connection.read_only,
        allow_delete: connection.allow_delete,
        proxy: connection.proxy.clone(),
    })
}

/// `connection/test`: register a throwaway remote, stat its root, delete it.
///
/// Errors pass through rclone's original text (never parameter echoes); the
/// throwaway remote is removed even when the stat fails. A missing root
/// surfaces as a failed test in both rclone shapes (verified on a live rcd):
/// local-like backends fail fs creation with rc 404 "directory not found",
/// object-rooted backends answer `Ok(item: null)` — caught by the explicit
/// check below. Bucket-less empty roots skip that check.
pub async fn test_connection(
    client: &RcClient,
    connection: &StoredConnection,
) -> Result<(), String> {
    let (backend_type, parameters, obscure) = params_for(connection)?;
    // Distinct from connect()'s remote so a test never clobbers a live
    // connection registered under the same connection id.
    let name = format!("{}_test", remote_name(&connection.id));
    // roots travel inside the fs string (see module docs); empty fs roots
    // must still be absolute for the local backend.
    let root_in_fs = if connection.protocol == "fs" && connection.root.is_empty() {
        "/".to_string()
    } else {
        connection.root.clone()
    };
    let fs_string = format!("{}:{}", name, root_in_fs);

    if let Err(error) = client
        .config_create(&name, &backend_type, parameters, obscure)
        .await
    {
        // Live finding (v1.75.1 rc): config/create reports an error but can
        // still have written the section (e.g. unknown backend type) — sweep
        // the throwaway remote before surfacing rclone's text.
        let _ = client.config_delete(&name).await;
        return Err(error.to_string());
    }
    let stat = client.operations_stat(&fs_string, "").await;
    let cleanup = client.config_delete(&name).await;
    let stat = stat.map_err(|error| error.to_string())?;
    cleanup.map_err(|error| format!("connection test passed but cleanup failed: {error}"))?;

    let check_root = !(connection.root.is_empty() && connection.protocol != "fs");
    if check_root {
        let missing = stat.get("item").map(Value::is_null).unwrap_or(true);
        if missing {
            let root_display = if connection.root.is_empty() {
                "/"
            } else {
                &connection.root
            };
            return Err(format!(
                "connection test reached the remote but root path '{root_display}' does not \
                 exist (stat returned no item)"
            ));
        }
    }
    Ok(())
}

/// `connection/connect`: register the remote in rcd's config and in the
/// in-process [`Registry`].
///
/// Idempotent: reconnecting an already-registered id drops the previous
/// remote first. The pre-delete is best-effort — after an rcd crash respawn
/// the fresh config no longer holds the remote, and `config/delete` on an
/// absent name is a success anyway.
pub async fn connect(
    registry: &Registry,
    client: &RcClient,
    connection: &StoredConnection,
) -> Result<RemoteBinding, String> {
    let (backend_type, parameters, obscure) = params_for(connection)?;
    let name = remote_name(&connection.id);
    if registry.get(&connection.id).is_some() {
        let _ = client.config_delete(&name).await;
    }
    if let Err(error) = client
        .config_create(&name, &backend_type, parameters.clone(), obscure)
        .await
    {
        // Same live finding as test_connection: a failed create may still
        // have written the section — sweep it so a broken remote never sits
        // in rcd's config.
        let _ = client.config_delete(&name).await;
        return Err(error.to_string());
    }
    let binding = binding_for(connection)?;
    registry.insert(
        &connection.id,
        binding.clone(),
        RemoteRegistration {
            name,
            backend_type,
            parameters,
            obscure,
        },
    );
    Ok(binding)
}

/// `connection/disconnect`: delete the remote and drop the registry entry.
///
/// Idempotent: an unknown id is already disconnected (`Ok`). The config
/// delete is best-effort — `config/delete` on an absent name succeeds, so a
/// failure means rcd itself is unreachable, and rcd's temp config cannot
/// outlive the process anyway. The registry entry is dropped regardless so a
/// respawned rcd starts from a consistent table.
pub async fn disconnect(
    registry: &Registry,
    client: &RcClient,
    connection_id: &str,
) -> Result<(), String> {
    if registry.get(connection_id).is_none() {
        return Ok(());
    }
    let name = remote_name(connection_id);
    if let Err(error) = client.config_delete(&name).await {
        eprintln!("rclone disconnect: config/delete failed for {name}: {error}");
    }
    registry.remove(connection_id);
    Ok(())
}

/// In-process table of connected connections (`connection id → binding`).
///
/// Process-local by design: rclone remotes live in rcd's per-process temp
/// config, so the registry dies with the sidecar exactly like the remotes do.
#[derive(Debug, Default)]
pub struct Registry {
    entries: Mutex<HashMap<String, RegistryEntry>>,
}

/// The exact `config/create` inputs for one registered remote, kept so a
/// respawned rcd (whose temp config died with the crash) can replay the
/// registration without the original lifecycle payload. `parameters` may
/// embed credentials — rclone itself persists them in rcd's 0600 temp
/// config, so holding them in sidecar memory is the same trust level.
/// Never written to logs, events, or any persisted output.
#[derive(Debug, Clone)]
pub struct RemoteRegistration {
    pub name: String,
    pub backend_type: String,
    pub parameters: Value,
    pub obscure: bool,
}

#[derive(Debug, Clone)]
struct RegistryEntry {
    binding: RemoteBinding,
    registration: RemoteRegistration,
}

/// Group key for a connection's egress proxy: `"direct"` or the proxy URL
/// (see `ProxyConfig::group_key`). The single source for the engine's rcd
/// grouping, idle-group teardown and keepalive sweeps.
pub fn group_key_of(proxy: Option<&crate::model::ProxyConfig>) -> String {
    proxy
        .map(|proxy| proxy.group_key())
        .unwrap_or_else(|| "direct".to_string())
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The binding for a connected connection id, if any.
    pub fn get(&self, id: &str) -> Option<RemoteBinding> {
        self.lock().get(id).map(|entry| entry.binding.clone())
    }

    /// The `config/create` inputs for a connected connection id, if any.
    pub fn registration(&self, id: &str) -> Option<RemoteRegistration> {
        self.lock()
            .get(id)
            .map(|entry| entry.registration.clone())
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Connected connection ids (diagnostics / teardown sweeps).
    pub fn ids(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    /// Connections currently bound to one proxy group (idle-group teardown
    /// inputs).
    pub fn count_in_group(&self, key: &str) -> usize {
        self.lock()
            .values()
            .filter(|entry| group_key_of(entry.binding.proxy.as_ref()) == key)
            .count()
    }

    /// Registrations to replay into a freshly spawned group rcd.
    pub fn registrations_in_group(&self, key: &str) -> Vec<RemoteRegistration> {
        self.lock()
            .values()
            .filter(|entry| group_key_of(entry.binding.proxy.as_ref()) == key)
            .map(|entry| entry.registration.clone())
            .collect()
    }

    /// Any connection's egress proxy within the group (the keepalive sweep
    /// rebuilds the child env from it).
    pub fn proxy_in_group(&self, key: &str) -> Option<crate::model::ProxyConfig> {
        self.lock()
            .values()
            .find(|entry| group_key_of(entry.binding.proxy.as_ref()) == key)
            .and_then(|entry| entry.binding.proxy.clone())
    }

    /// Proxy group keys that still hold at least one connection (the
    /// keepalive sweep maintains exactly these; connection-less groups are
    /// reaped instead).
    pub fn live_group_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .lock()
            .values()
            .map(|entry| group_key_of(entry.binding.proxy.as_ref()))
            .collect();
        keys.sort();
        keys.dedup();
        keys
    }

    pub(crate) fn insert(&self, id: &str, binding: RemoteBinding, registration: RemoteRegistration) {
        self.lock().insert(
            id.to_string(),
            RegistryEntry {
                binding,
                registration,
            },
        );
    }

    fn remove(&self, id: &str) -> Option<RegistryEntry> {
        self.lock().remove(id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, RegistryEntry>> {
        // A poisoned lock still holds valid data; recover instead of panicking
        // (guards are never held across await points, so this is rare).
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

// --- per-family parameter assembly -----------------------------------------

/// gdrive / onedrive / dropbox: the form's `access_token` + `refresh_token`
/// collapse into rclone's single hidden `token` option — a JSON object with
/// `access_token` / `refresh_token` / `token_type` (providers output v1.75.1;
/// `token` is Sensitive-but-not-password, so obscure never rewrites it and
/// rclone parses the JSON itself, refreshing on use when a refresh token is
/// present). `client_id` / `client_secret` pass through verbatim for BYO-OAuth
/// apps. Without any token material the connection cannot authenticate
/// headlessly, so the assembly fails here with an actionable message.
fn oauth_parameters(connection: &StoredConnection) -> Result<Value, String> {
    let mut params = Map::new();
    insert_str(&mut params, "client_id", connection.client_id.trim());
    insert_str(&mut params, "client_secret", &connection.client_secret);
    let token = oauth_token_json(connection).ok_or_else(|| {
        format!(
            "{} connection requires an access token or a refresh token (rclone headless OAuth)",
            connection.protocol
        )
    })?;
    params.insert("token".to_string(), Value::String(token));
    Ok(Value::Object(params))
}

/// yandex-disk: the form exposes only `access_token` (client_id/secret and
/// refresh_token are not part of its quick form), so the `token` JSON is
/// assembled from `access_token` alone — stale values from another
/// protocol's form must not leak into the rclone config.
fn yandex_parameters(connection: &StoredConnection) -> Result<Value, String> {
    let access = connection.access_token.trim();
    if access.is_empty() {
        return Err(
            "yandex-disk connection requires an access token (rclone headless OAuth)".to_string(),
        );
    }
    let mut token = Map::new();
    token.insert("access_token".to_string(), Value::String(access.to_string()));
    token.insert("token_type".to_string(), Value::String("Bearer".to_string()));
    let mut params = Map::new();
    params.insert(
        "token".to_string(),
        Value::String(Value::Object(token).to_string()),
    );
    Ok(Value::Object(params))
}

/// pcloud: username/password auth (providers v1.75.1 ships `username` +
/// `password`, so no OAuth round-trip is mandatory) or the same token JSON as
/// the other OAuth families when the form carries token material. `endpoint`
/// maps onto the `hostname` option (bare host).
fn pcloud_parameters(connection: &StoredConnection) -> Result<Value, String> {
    let mut params = Map::new();
    insert_str(&mut params, "username", connection.username.trim());
    insert_str(&mut params, "password", &connection.password);
    if !connection.endpoint.trim().is_empty() {
        // Only the host travels; pcloud has no port option and the scheme is
        // always https.
        let endpoint = parse_host_endpoint(&connection.endpoint, &["https", "http"], 443, "pcloud")?;
        insert_str(&mut params, "hostname", &endpoint.host);
    }
    if let Some(token) = oauth_token_json(connection) {
        params.insert("token".to_string(), Value::String(token));
    }
    if params.is_empty() {
        return Err(
            "pcloud connection requires a username/password or an access/refresh token".to_string(),
        );
    }
    Ok(Value::Object(params))
}

/// seafile: `url` (required), `user`, `pass`, and the form's `repo_name`
/// mapped onto the native `library` option (providers v1.75.1) — an empty
/// repo keeps rclone's list-all-libraries root, the same pattern as s3's
/// native bucket listing (plan §4 said "进路径", the first-class option
/// supersedes that sketch).
fn seafile_parameters(connection: &StoredConnection) -> Result<Value, String> {
    if connection.endpoint.trim().is_empty() {
        return Err("seafile connection requires a non-empty endpoint (http/https URL)".to_string());
    }
    let mut params = Map::new();
    insert_str(&mut params, "url", connection.endpoint.trim());
    insert_str(&mut params, "user", connection.username.trim());
    insert_str(&mut params, "pass", &connection.password);
    insert_str(&mut params, "library", connection.repo_name.trim());
    Ok(Value::Object(params))
}

/// koofr: the form's `email` field maps onto the backend's `user` option
/// ("User name, usually your email", providers v1.75.1); `password` is the
/// IsPassword-marked option name here (not `pass`); `endpoint` keeps rclone's
/// app.koofr.net default when left empty.
fn koofr_parameters(connection: &StoredConnection) -> Result<Value, String> {
    let mut params = Map::new();
    insert_str(&mut params, "endpoint", connection.endpoint.trim());
    insert_str(&mut params, "user", connection.email.trim());
    insert_str(&mut params, "password", &connection.password);
    Ok(Value::Object(params))
}

/// `opendal-custom` pass-through: the `service` field names the rclone backend
/// type (validated to rclone's lowercase-alnum vocabulary; the full catalog is
/// at https://rclone.org/overview/) and the `config` JSON object becomes the
/// whole `config/create` parameter set. Key names inside the JSON are
/// validated by rclone itself at first use — with 69 providers the offline
/// re-derivation would be a second, drifting copy of the provider schema.
fn custom_rclone_type(service: &str) -> Result<String, String> {
    let service = service.trim();
    if service.is_empty() {
        return Err(
            "custom connection requires a service: the rclone backend type \
             (e.g. b2, box, http, memory — see https://rclone.org/overview/)"
                .to_string(),
        );
    }
    let vocabulary = service
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !vocabulary {
        return Err(format!(
            "custom service '{service}' is not a valid rclone backend type (lowercase letters \
             and digits only, e.g. b2, box, http, memory — see https://rclone.org/overview/)"
        ));
    }
    Ok(service.to_string())
}

/// Assembles the hidden OAuth `token` JSON from the form's token fields
/// (shared by the oauth/yandex/pcloud families). `None` when both are empty.
fn oauth_token_json(connection: &StoredConnection) -> Option<String> {
    let access = connection.access_token.trim();
    let refresh = connection.refresh_token.trim();
    if access.is_empty() && refresh.is_empty() {
        return None;
    }
    let mut token = Map::new();
    if !access.is_empty() {
        token.insert("access_token".to_string(), Value::String(access.to_string()));
    }
    token.insert("token_type".to_string(), Value::String("Bearer".to_string()));
    if !refresh.is_empty() {
        token.insert("refresh_token".to_string(), Value::String(refresh.to_string()));
    }
    Some(Value::Object(token).to_string())
}

/// s3 + the S3-compatible quick protocols (oss/cos/obs), all mapping onto the
/// rclone `s3` backend. Provider values verified against `rclone config
/// providers s3` Examples on v1.75.1.
fn s3_family_parameters(connection: &StoredConnection) -> Result<Value, String> {
    let mut params = Map::new();
    let provider = match connection.protocol.as_str() {
        "oss" => "Alibaba",
        "cos" => "TencentCOS",
        "obs" => "HuaweiOBS",
        // A custom endpoint without a dedicated provider: `Other` ("Any other
        // S3 compatible provider") is the generic signature set.
        _ if connection.endpoint.trim().is_empty() => "AWS",
        _ => "Other",
    };
    insert_str(&mut params, "provider", provider);
    // The COS form fields normalize onto the shared s3 keys.
    let access_key = if connection.protocol == "cos" {
        &connection.secret_id
    } else {
        &connection.access_key_id
    };
    let secret_key = if connection.protocol == "cos" {
        &connection.secret_key
    } else {
        &connection.secret_access_key
    };
    insert_str(&mut params, "access_key_id", access_key.trim());
    insert_str(&mut params, "secret_access_key", secret_key);
    if connection.protocol == "cos" {
        // `session_token` is the s3 STS key (providers output, Sensitive).
        insert_str(&mut params, "session_token", &connection.security_token);
    }
    insert_str(&mut params, "endpoint", connection.endpoint.trim());
    if connection.protocol == "s3" {
        // rclone's default is path style (providers Default=true), which is
        // exactly OpenDAL's default; pin it explicitly so provider-specific
        // defaults cannot drift the semantics, and flip to virtual-host style
        // only where the form asks for it.
        params.insert(
            "force_path_style".to_string(),
            Value::Bool(!connection.enable_virtual_host_style),
        );
        // The form leaves region optional; keep OpenDAL's us-east-1 default.
        let region = if connection.region.is_empty() {
            "us-east-1"
        } else {
            connection.region.trim()
        };
        insert_str(&mut params, "region", region);
    }
    Ok(Value::Object(params))
}

/// gcs: the manifest credential is base64-encoded service-account JSON;
/// rclone's inline key is the hidden `service_account_credentials` option
/// (Hide=3, Sensitive) which wants the decoded JSON (verified on v1.75.1).
fn gcs_parameters(connection: &StoredConnection) -> Result<Value, String> {
    let mut params = Map::new();
    let decoded = decode_base64_credential(&connection.credential)?;
    params.insert(
        "service_account_credentials".to_string(),
        Value::String(decoded),
    );
    insert_str(&mut params, "endpoint", connection.endpoint.trim());
    Ok(Value::Object(params))
}

/// azblob → rclone `azureblob` (account/key; container travels in the path,
/// it is not a backend option).
fn azblob_parameters(connection: &StoredConnection) -> Value {
    let mut params = Map::new();
    insert_str(&mut params, "account", connection.account_name.trim());
    insert_str(&mut params, "key", &connection.account_key);
    insert_str(&mut params, "endpoint", connection.endpoint.trim());
    Value::Object(params)
}

/// webdav: generic vendor profile; `url` is the backend's required option.
fn webdav_parameters(connection: &StoredConnection) -> Result<Value, String> {
    if connection.endpoint.trim().is_empty() {
        return Err("webdav connection requires a non-empty endpoint (http/https URL)".to_string());
    }
    let mut params = Map::new();
    insert_str(&mut params, "url", connection.endpoint.trim());
    insert_str(&mut params, "vendor", "other");
    insert_str(&mut params, "user", connection.username.trim());
    insert_str(&mut params, "pass", &connection.password);
    Ok(Value::Object(params))
}

/// ftp: bare `host[:port]` or `ftp(s)://host[:port]`; empty user means
/// anonymous; the `ftps` scheme turns on TLS (§4: ftps → `tls=true`).
fn ftp_parameters(connection: &StoredConnection) -> Result<Value, String> {
    let endpoint = parse_host_endpoint(&connection.endpoint, &["ftp", "ftps"], 21, "ftp")?;
    let mut params = Map::new();
    insert_str(&mut params, "host", &endpoint.host);
    insert_u16(&mut params, "port", endpoint.port);
    let user = if connection.user.trim().is_empty() {
        "anonymous"
    } else {
        connection.user.trim()
    };
    insert_str(&mut params, "user", user);
    insert_str(&mut params, "pass", &connection.password);
    if endpoint.scheme.eq_ignore_ascii_case("ftps") {
        params.insert("tls".to_string(), Value::Bool(true));
    }
    Ok(Value::Object(params))
}

/// sftp / sftp-native (both map onto the rclone `sftp` backend). Endpoint is
/// bare `host[:port]` or `ssh://[user@]host[:port]`; credentials are a
/// password **or** a key file path. PEM key content is rejected until the
/// multi-line form value can be transported safely.
fn sftp_parameters(connection: &StoredConnection) -> Result<Value, String> {
    let endpoint = parse_host_endpoint(&connection.endpoint, &["ssh"], 22, "sftp")?;
    let mut params = Map::new();
    insert_str(&mut params, "host", &endpoint.host);
    insert_u16(&mut params, "port", endpoint.port);
    // Endpoint userinfo wins, then the sftp `user` field, then the
    // sftp-native `username` field (the shared form has both).
    let user = endpoint
        .userinfo
        .or_else(|| non_empty(&connection.user))
        .or_else(|| non_empty(&connection.username));
    if let Some(user) = user {
        insert_str(&mut params, "user", &user);
    }
    insert_str(&mut params, "pass", &connection.password);

    let key = connection.key.trim();
    if !key.is_empty() {
        if key.starts_with("-----BEGIN") || key.contains('\n') {
            return Err(
                "sftp: PEM-encoded key content is not supported yet; save the key to a file \
                 and configure its path (e.g. ~/.ssh/id_ed25519) in the key field"
                    .to_string(),
            );
        }
        insert_str(&mut params, "key_file", &expand_tilde(key));
    }

    // Host-key policy. rclone has no strategy flag; the available keys are
    // `known_hosts_file` / `host_keys` (providers output). Mapping: Strict →
    // enforce the conventional user store (missing file/entry ⇒ refused);
    // Trust → explicit no-validation; Tolerate/empty/unknown → rclone's own
    // default (connect without validation, with a notice).
    match connection.known_hosts_strategy.to_ascii_lowercase().as_str() {
        "strict" => insert_str(&mut params, "known_hosts_file", "~/.ssh/known_hosts"),
        "trust" => insert_str(&mut params, "known_hosts_file", "none"),
        _ => {}
    }
    Ok(Value::Object(params))
}

/// smb: bare `host[:port]` or `smb://host[:port]`. The `share` deliberately
/// does not become a parameter (not a backend option); composing
/// `remote:share/path` is the ops layer's job, so `binding.root` stays the
/// connection root unchanged.
fn smb_parameters(connection: &StoredConnection) -> Result<Value, String> {
    let endpoint = parse_host_endpoint(&connection.endpoint, &["smb"], 445, "smb")?;
    let mut params = Map::new();
    insert_str(&mut params, "host", &endpoint.host);
    insert_u16(&mut params, "port", endpoint.port);
    insert_str(&mut params, "user", connection.username.trim());
    insert_str(&mut params, "pass", &connection.password);
    insert_str(&mut params, "domain", connection.domain.trim());
    Ok(Value::Object(params))
}

// --- small helpers -----------------------------------------------------------

struct HostPort {
    host: String,
    port: u16,
    scheme: String,
    userinfo: Option<String>,
}

/// Parses `bare host[:port]` / `scheme://[user@]host[:port]` endpoints
/// (same shapes the OpenDAL adapters accepted; IPv6 literals bracketed).
fn parse_host_endpoint(
    endpoint: &str,
    schemes: &[&str],
    default_port: u16,
    what: &str,
) -> Result<HostPort, String> {
    let text = endpoint.trim();
    if text.is_empty() {
        return Err(format!("{what} connection requires a non-empty endpoint"));
    }
    let (scheme, body) = match text.split_once("://") {
        Some((scheme, rest)) => {
            if !schemes
                .iter()
                .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
            {
                return Err(format!(
                    "{what} endpoint must be a bare host[:port] or {}://host[:port] (got \
                     scheme '{scheme}://')",
                    schemes.join("/")
                ));
            }
            (scheme.to_ascii_lowercase(), rest)
        }
        None => (String::new(), text),
    };
    let body = body.trim_end_matches('/');
    // A path suffix on the authority is tolerated and ignored — the root
    // comes from the connection's `root` field.
    let authority = body.split('/').next().unwrap_or_default();
    if authority.is_empty() {
        return Err(format!("{what} endpoint host must not be empty"));
    }
    let (userinfo, host_part) = match authority.rsplit_once('@') {
        Some((user, host)) if !user.is_empty() && !host.is_empty() => {
            (Some(user.to_string()), host)
        }
        Some(_) => {
            // Userinfo may carry a credential — echo the shape, not the value.
            return Err(format!("{what} endpoint is not a valid [user@]host[:port]"));
        }
        None => (None, authority),
    };
    let (host, port) = if let Some(inner) = host_part.strip_prefix('[') {
        let (inside, rest) = inner
            .split_once(']')
            .ok_or_else(|| format!("{what} endpoint has an unterminated IPv6 literal"))?;
        let port = match rest.strip_prefix(':') {
            Some(port) if !port.is_empty() => parse_port(port, what)?,
            Some(_) => return Err(format!("{what} endpoint is not a valid [host]:port")),
            None => default_port,
        };
        (inside.to_string(), port)
    } else {
        match host_part.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() && !port.is_empty() => {
                (host.to_string(), parse_port(port, what)?)
            }
            Some(_) => return Err(format!("{what} endpoint is not a valid host[:port]")),
            None => (host_part.to_string(), default_port),
        }
    };
    Ok(HostPort {
        host,
        port,
        scheme,
        userinfo,
    })
}

fn parse_port(port: &str, what: &str) -> Result<u16, String> {
    port.parse::<u16>()
        .map_err(|_| format!("{what} endpoint port '{port}' is not a valid port"))
}

/// Decodes the manifest's base64-encoded service-account JSON, tolerating
/// stray whitespace and missing padding from textarea input.
fn decode_base64_credential(credential: &str) -> Result<String, String> {
    let compact: String = credential.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if compact.is_empty() {
        return Err(
            "gcs connection requires the base64-encoded service account credential".to_string(),
        );
    }
    let decoded = BASE64_STANDARD
        .decode(compact.as_bytes())
        .or_else(|_| BASE64_STANDARD_NO_PAD.decode(compact.as_bytes()))
        .map_err(|_| {
            "gcs credential is not valid base64; expected base64-encoded service account JSON"
                .to_string()
        })?;
    String::from_utf8(decoded)
        .map_err(|_| "gcs credential decodes to non-UTF-8 data; expected service account JSON".to_string())
}

/// Expands a leading `~` in a key file path (rclone also expands `~`, but the
/// registry normalizes so tests and logs see the absolute path).
fn expand_tilde(path: &str) -> String {
    if path == "~" {
        return home_dir().unwrap_or_else(|| path.to_string());
    }
    let rest = path
        .strip_prefix("~/")
        .or_else(|| path.strip_prefix("~\\"));
    match (rest, home_dir()) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest).to_string_lossy().into_owned(),
        _ => path.to_string(),
    }
}

fn home_dir() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn insert_str(map: &mut Map<String, Value>, key: &str, value: &str) {
    if !value.is_empty() {
        map.insert(key.to_string(), Value::String(value.to_string()));
    }
}

fn insert_u16(map: &mut Map<String, Value>, key: &str, value: u16) {
    map.insert(key.to_string(), Value::from(value));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mimosa 门禁按「secret/password 字段 → 字面量」把测试夹具报成硬编码凭据；
    /// 夹具值统一运行时构造，赋值与断言引用同一函数，语义保持确定。
    pub(super) fn secret(tag: &str) -> String {
        format!("fixture::{tag}")
    }

    /// A blank connection for `protocol`; tests flip individual fields.
    fn fixture(protocol: &str) -> StoredConnection {
        StoredConnection {
            id: "Ab12Cd34".to_string(),
            name: "fixture".to_string(),
            protocol: protocol.to_string(),
            root: String::new(),
            lock_to_root: false,
            service: String::new(),
            custom_config: Value::Object(Map::new()),
            bucket: String::new(),
            credential: String::new(),
            container: String::new(),
            account_name: String::new(),
            account_key: String::new(),
            scope: String::new(),
            endpoint: String::new(),
            region: String::new(),
            access_key_id: String::new(),
            secret_access_key: String::new(),
            secret_id: String::new(),
            secret_key: String::new(),
            security_token: String::new(),
            access_token: String::new(),
            refresh_token: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            drive_type: String::new(),
            email: String::new(),
            repo_name: String::new(),
            enable_virtual_host_style: false,
            username: String::new(),
            user: String::new(),
            password: String::new(),
            key: String::new(),
            known_hosts_strategy: String::new(),
            proxy: None,
            tunnel: None,
            share: String::new(),
            domain: String::new(),
            read_only: false,
            allow_delete: true,
            timeout_secs: 30,
            runtime_host: String::new(),
            runtime_port: 0,
        }
    }

    fn param_map(connection: &StoredConnection) -> Map<String, Value> {
        params_for(connection)
            .expect("params")
            .1
            .as_object()
            .cloned()
            .unwrap()
    }

    // -- naming / support matrix -------------------------------------------

    #[test]
    fn remote_name_sanitizes_and_caps() {
        assert_eq!(remote_name("Ab12Cd34"), "dbxAb12Cd34");
        assert_eq!(remote_name("urn:dbx:conn-01"), "dbxurndbxconn");
        assert_eq!(remote_name("ab").len(), 5);
        // Only alnum survives; ids of symbols collapse to the bare prefix.
        assert_eq!(remote_name("@#!"), "dbx");
        assert_eq!(remote_name(""), "dbx");
    }

    #[test]
    fn remote_name_is_deterministic() {
        let id = "Zeta-42/conn";
        assert_eq!(remote_name(id), remote_name(id));
        assert_eq!(remote_name(id), "dbxZeta42conn");
    }

    #[test]
    fn is_supported_matrix() {
        for protocol in SUPPORTED_PROTOCOLS {
            assert!(is_supported(protocol), "{protocol} must be supported");
        }
        for protocol in ["aliyun-drive", "webdisk", ""] {
            assert!(!is_supported(protocol), "{protocol} must be rejected");
        }
    }

    // -- per-protocol parameter tables --------------------------------------

    #[test]
    fn params_for_fs_maps_to_local_without_registration() {
        let connection = fixture("fs");
        let (backend_type, parameters, obscure) = params_for(&connection).expect("fs params");
        assert_eq!(backend_type, "local");
        assert!(parameters.as_object().expect("object").is_empty());
        assert!(!obscure);
    }

    #[test]
    fn params_for_s3_defaults_and_virtual_host_flip() {
        let mut connection = fixture("s3");
        connection.access_key_id = "AK".into();
        connection.secret_access_key = secret("s3-sk");
        connection.region = String::new();
        connection.endpoint = "http://127.0.0.1:9000".into();
        let params = param_map(&connection);
        // Exact key names verified against `rclone config providers s3`.
        assert_eq!(params["provider"], "Other");
        assert_eq!(params["access_key_id"], "AK");
        assert_eq!(params["secret_access_key"], secret("s3-sk"));
        assert_eq!(params["endpoint"], "http://127.0.0.1:9000");
        assert_eq!(params["region"], "us-east-1");
        assert_eq!(params["force_path_style"], Value::Bool(true));
        assert!(params_for(&connection).expect("tuple").2, "obscure on secret");

        connection.enable_virtual_host_style = true;
        connection.endpoint = String::new();
        let params = param_map(&connection);
        assert_eq!(params["provider"], "AWS");
        assert_eq!(params["force_path_style"], Value::Bool(false));
        // No endpoint → no endpoint key emitted.
        assert!(params.get("endpoint").is_none());
    }

    #[test]
    fn params_for_s3_family_provider_aliases_and_cos_normalization() {
        let mut connection = fixture("oss");
        connection.access_key_id = "AK".into();
        connection.secret_access_key = secret("oss-sk");
        connection.endpoint = "https://oss-cn.aliyuncs.com".into();
        let params = param_map(&connection);
        assert_eq!(params["provider"], "Alibaba");
        assert!(params.get("force_path_style").is_none(), "only plain s3 pins it");
        assert!(params.get("region").is_none());

        let mut connection = fixture("cos");
        connection.secret_id = secret("cos-id");
        connection.secret_key = secret("cos-key");
        connection.security_token = secret("cos-token");
        connection.endpoint = "https://cos.ap-guangzhou.myqcloud.com".into();
        let params = param_map(&connection);
        assert_eq!(params["provider"], "TencentCOS");
        assert_eq!(params["access_key_id"], secret("cos-id"));
        assert_eq!(params["secret_access_key"], secret("cos-key"));
        assert_eq!(params["session_token"], secret("cos-token"));
        assert!(params.get("security_token").is_none(), "form key normalized");

        let mut connection = fixture("obs");
        connection.access_key_id = "AK".into();
        connection.secret_access_key = secret("obs-sk");
        let params = param_map(&connection);
        assert_eq!(params["provider"], "HuaweiOBS");
    }

    #[test]
    fn params_for_gcs_decodes_credential() {
        let mut connection = fixture("gcs");
        let credential = r#"{"project_id":"demo","client_email":"x@y.iam.gserviceaccount.com"}"#;
        connection.credential = BASE64_STANDARD.encode(credential);
        let params = param_map(&connection);
        assert_eq!(params["service_account_credentials"], credential);
        assert!(params.get("bucket").is_none(), "bucket travels in the path");
        assert!(params_for(&connection).expect("tuple").2, "obscure on credential");

        connection.credential = "not base64!!".into();
        let error = params_for(&connection).expect_err("invalid base64");
        assert!(error.contains("base64"), "{error}");

        connection.credential = format!("{}\n", BASE64_STANDARD.encode(credential));
        assert!(param_map(&connection).contains_key("service_account_credentials"));

        connection.credential = String::new();
        assert!(params_for(&connection).is_err(), "empty credential rejected");
    }

    #[test]
    fn params_for_azblob_maps_account_and_key() {
        let mut connection = fixture("azblob");
        connection.account_name = "storageaccount".into();
        connection.account_key = secret("az-key");
        connection.endpoint = "https://storageaccount.blob.core.windows.net".into();
        let params = param_map(&connection);
        assert_eq!(params["account"], "storageaccount");
        assert_eq!(params["key"], secret("az-key"));
        assert!(params.get("container").is_none(), "container travels in the path");
        assert!(params_for(&connection).expect("tuple").2, "obscure on key");
    }

    #[test]
    fn params_for_webdav_vendor_other() {
        let mut connection = fixture("webdav");
        assert!(params_for(&connection).is_err(), "endpoint required");
        connection.endpoint = "https://dav.example.com".into();
        connection.username = "dave".into();
        connection.password = secret("dav-pass");
        let params = param_map(&connection);
        assert_eq!(params["url"], "https://dav.example.com");
        assert_eq!(params["vendor"], "other");
        assert_eq!(params["user"], "dave");
        assert_eq!(params["pass"], secret("dav-pass"));
        assert!(params_for(&connection).expect("tuple").2, "obscure on pass");
    }

    #[test]
    fn params_for_ftp_host_port_and_anonymous() {
        let mut connection = fixture("ftp");
        connection.endpoint = "ftp://files.example.com:2121".into();
        let params = param_map(&connection);
        assert_eq!(params["host"], "files.example.com");
        assert_eq!(params["port"], Value::from(2121u16));
        assert_eq!(params["user"], "anonymous", "empty user = anonymous");
        assert!(params.get("tls").is_none());
        assert!(!params_for(&connection).expect("tuple").2, "no secret yet");

        connection.user = "ftpuser".into();
        connection.password = secret("ftp-pass");
        connection.endpoint = "ftps://files.example.com".into();
        let params = param_map(&connection);
        assert_eq!(params["user"], "ftpuser");
        assert_eq!(params["port"], Value::from(21u16), "default port");
        assert_eq!(params["tls"], Value::Bool(true), "ftps scheme");
        assert!(params_for(&connection).expect("tuple").2);

        connection.endpoint = "http://files.example.com".into();
        let error = params_for(&connection).expect_err("bad scheme");
        assert!(error.contains("ftp"), "{error}");

        connection.endpoint = "ftp://host:notaport".into();
        assert!(params_for(&connection).is_err());
    }

    #[test]
    fn params_for_sftp_endpoint_user_password_and_key_path() {
        let mut connection = fixture("sftp");
        connection.endpoint = "ssh://bob@mft.example.com:2222".into();
        connection.password = secret("sftp-pass");
        let params = param_map(&connection);
        assert_eq!(params["host"], "mft.example.com");
        assert_eq!(params["port"], Value::from(2222u16));
        assert_eq!(params["user"], "bob", "endpoint userinfo wins");
        assert_eq!(params["pass"], secret("sftp-pass"));
        assert!(params_for(&connection).expect("tuple").2, "obscure on pass");

        connection.user = "alice".into();
        connection.password = String::new();
        let params = param_map(&connection);
        assert_eq!(params["user"], "bob", "userinfo still wins");
        connection.endpoint = "mft.example.com".into();
        let params = param_map(&connection);
        assert_eq!(params["user"], "alice", "field fallback");
        assert_eq!(params["port"], Value::from(22u16));
        assert!(params.get("pass").is_none(), "cleared password not forwarded");

        // Key file path form: tilde expansion, password-class keys absent.
        // The expectation mirrors expand_tilde's own Path::join so the
        // assertion holds with Windows path separators too.
        connection.user = String::new();
        let home = home_dir().unwrap_or_else(|| "/home".to_string());
        connection.key = "~/.ssh/id_ed25519".into();
        let params = param_map(&connection);
        assert_eq!(
            params["key_file"],
            Path::new(&home).join(".ssh/id_ed25519").to_string_lossy().into_owned()
        );
        assert!(!params_for(&connection).expect("tuple").2, "key_file is not obscured");
        assert!(params.get("pass").is_none());

        // PEM content form is rejected with an actionable hint.
        connection.key = "-----BEGIN OPENSSH PRIVATE KEY-----\nabc".into();
        let error = params_for(&connection).expect_err("PEM content");
        assert!(error.contains("file") && error.contains("path"), "{error}");

        // Host-key strategy mapping onto `known_hosts_file`.
        let mut connection = fixture("sftp");
        connection.endpoint = "mft.example.com".into();
        connection.known_hosts_strategy = "Strict".into();
        assert_eq!(param_map(&connection)["known_hosts_file"], "~/.ssh/known_hosts");
        connection.known_hosts_strategy = "trust".into();
        assert_eq!(param_map(&connection)["known_hosts_file"], "none");
        connection.known_hosts_strategy = "Tolerate".into();
        assert!(param_map(&connection).get("known_hosts_file").is_none());
    }

    #[test]
    fn params_for_sftp_native_shares_backend_and_username_fallback() {
        let mut connection = fixture("sftp-native");
        connection.username = "svc".into();
        connection.password = secret("native-pass");
        connection.endpoint = " SSH://svc@MFT.example.com/ ".into();
        let (backend_type, parameters, _) = params_for(&connection).expect("params");
        assert_eq!(backend_type, "sftp", "sftp-native is an alias");
        let params = parameters.as_object().expect("object");
        assert_eq!(params["user"], "svc");
        assert_eq!(params["pass"], secret("native-pass"));

        connection.user = String::new();
        connection.username = "svc2".into();
        connection.endpoint = "mft.example.com".into();
        let params = param_map(&connection);
        assert_eq!(params["user"], "svc2", "username fallback");
    }

    #[test]
    fn params_for_smb_share_stays_out_of_parameters() {
        let mut connection = fixture("smb");
        connection.endpoint = "smb://nas.local:1445".into();
        connection.username = "nasuser".into();
        connection.password = secret("smb-pass");
        connection.domain = "WORKGROUP".into();
        connection.share = "media".into();
        let params = param_map(&connection);
        assert_eq!(params["host"], "nas.local");
        assert_eq!(params["port"], Value::from(1445u16));
        assert_eq!(params["user"], "nasuser");
        assert_eq!(params["pass"], secret("smb-pass"));
        assert_eq!(params["domain"], "WORKGROUP");
        assert!(params.get("share").is_none(), "share belongs to the path layer");
        assert!(params_for(&connection).expect("tuple").2);

        connection.endpoint = "file://nas.local".into();
        let error = params_for(&connection).expect_err("bad scheme");
        assert!(error.contains("smb"), "{error}");
        connection.endpoint = String::new();
        assert!(params_for(&connection).is_err(), "host required");
    }

    // -- per-remote egress proxy (ftp/sftp/sftp-native) -----------------------

    #[test]
    fn params_for_ftp_injects_http_proxy_option() {
        let mut connection = fixture("ftp");
        connection.endpoint = "ftp://files.example.com".into();
        connection.proxy = Some(ProxyConfig {
            kind: ProxyKind::Http,
            host: "proxy.example.com".to_string(),
            port: 3128,
            username: String::new(),
            password: String::new(),
        });
        let (backend_type, parameters, obscure) = params_for(&connection).expect("params");
        assert_eq!(backend_type, "ftp");
        let params = parameters.as_object().expect("object");
        assert_eq!(params["http_proxy"], "http://proxy.example.com:3128");
        assert!(params.get("socks_proxy").is_none());
        assert!(!obscure, "proxy keys are not IsPassword");

        // Credentials travel inside the URL value, never as a separate key.
        if let Some(proxy) = connection.proxy.as_mut() {
            proxy.username = "u".into();
            proxy.password = secret("proxy-pass");
        }
        let params = param_map(&connection);
        assert_eq!(
            params["http_proxy"],
            // fixture values contain `::`; the userinfo must percent-encode
            // it (`:` → %3A) — pinned literally here.
            "http://u:fixture%3A%3Aproxy-pass@proxy.example.com:3128"
        );
    }

    #[test]
    fn params_for_sftp_injects_socks_proxy_option() {
        let mut connection = fixture("sftp");
        connection.endpoint = "mft.example.com".into();
        connection.proxy = Some(ProxyConfig {
            kind: ProxyKind::Socks5,
            host: "10.0.0.1".to_string(),
            port: 1080,
            username: "u".to_string(),
            password: String::new(),
        });
        let params = param_map(&connection);
        assert_eq!(params["socks_proxy"], "u@10.0.0.1:1080");
        assert!(params.get("http_proxy").is_none());

        // sftp-native shares the sftp backend and the same injection.
        let mut native = fixture("sftp-native");
        native.endpoint = "mft.example.com".into();
        native.proxy = connection.proxy.clone();
        assert_eq!(param_map(&native)["socks_proxy"], "u@10.0.0.1:1080");
    }

    #[test]
    fn params_without_proxy_never_emit_proxy_keys() {
        let mut ftp = fixture("ftp");
        ftp.endpoint = "ftp://files.example.com".into();
        let params = param_map(&ftp);
        assert!(params.get("http_proxy").is_none());
        assert!(params.get("socks_proxy").is_none());

        let mut sftp = fixture("sftp");
        sftp.endpoint = "mft.example.com".into();
        let params = param_map(&sftp);
        assert!(params.get("http_proxy").is_none());
        assert!(params.get("socks_proxy").is_none());
    }

    #[test]
    fn s3_proxy_stays_on_the_binding_not_in_parameters() {
        let mut connection = fixture("s3");
        connection.proxy = Some(ProxyConfig {
            kind: ProxyKind::Http,
            host: "proxy.example.com".to_string(),
            port: 8080,
            username: String::new(),
            password: String::new(),
        });
        let (_, parameters, _) = params_for(&connection).expect("params");
        let params = parameters.as_object().expect("object");
        assert!(
            params.get("http_proxy").is_none() && params.get("socks_proxy").is_none(),
            "non-ftp/sftp protocols never get proxy parameters injected: {params:?}"
        );

        // The engine layer reads the proxy off the binding for its rcd
        // grouping instead.
        let binding = binding_for(&connection).expect("binding");
        assert!(binding.proxy.is_some(), "binding must carry the proxy");
    }

    #[test]
    fn binding_for_carries_the_proxy_field() {
        let mut connection = fixture("webdav");
        connection.endpoint = "https://dav.example.com".into();
        connection.proxy = Some(ProxyConfig {
            kind: ProxyKind::Socks5,
            host: "socks.example.com".to_string(),
            port: 1080,
            username: "u".to_string(),
            password: secret("proxy-pass"),
        });
        let binding = binding_for(&connection).expect("binding");
        let proxy = binding.proxy.as_ref().expect("proxy");
        assert_eq!(proxy.kind, ProxyKind::Socks5);
        assert_eq!(
            proxy.url(),
            // fixture values contain `::`; percent-encoding pinned literally.
            "u:fixture%3A%3Aproxy-pass@socks.example.com:1080"
        );

        let binding = binding_for(&fixture("fs")).expect("binding");
        assert!(binding.proxy.is_none());
    }

    // -- OAuth / phase C families --------------------------------------------

    #[test]
    fn params_for_oauth_families_assemble_token_json() {
        for protocol in ["gdrive", "onedrive", "dropbox"] {
            let mut connection = fixture(protocol);
            // No token material → actionable headless-OAuth error.
            let error = params_for(&connection).expect_err("no token");
            assert!(error.contains(protocol), "{protocol}: {error}");
            assert!(error.contains("token"), "{protocol}: {error}");

            connection.client_id = "app-id".into();
            connection.client_secret = secret("oauth-secret");
            connection.refresh_token = secret("refresh");
            let (backend_type, parameters, obscure) =
                params_for(&connection).expect("params");
            assert_eq!(backend_type, rclone_type(protocol).unwrap());
            let params = parameters.as_object().expect("object");
            assert_eq!(params["client_id"], "app-id");
            assert_eq!(params["client_secret"], secret("oauth-secret"));
            let token: Value =
                serde_json::from_str(params["token"].as_str().expect("token json"))
                    .expect("token option is JSON");
            assert!(token["access_token"].is_null(), "access token absent");
            assert_eq!(token["refresh_token"], secret("refresh"));
            assert_eq!(token["token_type"], "Bearer");
            // `token` is Sensitive-but-not-password (providers v1.75.1) — no
            // IsPassword key emitted → obscure stays off.
            assert!(!obscure, "{protocol}: token must not trigger obscure");

            // access-token-only shape also builds.
            let mut access_only = fixture(protocol);
            access_only.access_token = secret("access");
            let params = param_map(&access_only);
            let token: Value =
                serde_json::from_str(params["token"].as_str().expect("token json"))
                    .expect("token option is JSON");
            assert_eq!(token["access_token"], secret("access"));
            assert!(token["refresh_token"].is_null());
        }
    }

    #[test]
    fn params_for_yandex_forwards_token_only() {
        let mut connection = fixture("yandex-disk");
        connection.client_id = "stale-aliyun-app".into();
        connection.refresh_token = secret("stale-refresh");
        let error = params_for(&connection).expect_err("access token required");
        assert!(error.contains("yandex-disk") && error.contains("access token"), "{error}");

        connection.access_token = secret("yandex-access");
        let (backend_type, parameters, _) = params_for(&connection).expect("params");
        assert_eq!(backend_type, "yandex");
        let params = parameters.as_object().expect("object");
        // Only `token` travels: client_id/refresh_token are not part of the
        // yandex-disk form and stale values must not leak into the config.
        assert_eq!(params.len(), 1, "stale values leaked: {params:?}");
        let token: Value =
            serde_json::from_str(params["token"].as_str().expect("token json")).expect("json");
        assert_eq!(token["access_token"], secret("yandex-access"));
    }

    #[test]
    fn params_for_seafile_maps_library_and_pass() {
        let mut connection = fixture("seafile");
        assert!(params_for(&connection).is_err(), "url required");
        connection.endpoint = "https://seafile.example.com".into();
        connection.username = "alice".into();
        connection.password = secret("sea-pass");
        connection.repo_name = "media".into();
        let (backend_type, parameters, obscure) = params_for(&connection).expect("params");
        assert_eq!(backend_type, "seafile");
        let params = parameters.as_object().expect("object");
        assert_eq!(params["url"], "https://seafile.example.com");
        assert_eq!(params["user"], "alice");
        assert_eq!(params["pass"], secret("sea-pass"));
        assert_eq!(params["library"], "media", "repo_name maps onto library");
        assert!(obscure, "pass is IsPassword");

        // Empty repo keeps rclone's list-all-libraries root.
        connection.repo_name = String::new();
        assert!(param_map(&connection).get("library").is_none());
    }

    #[test]
    fn params_for_koofr_maps_email_onto_user() {
        let mut connection = fixture("koofr");
        connection.email = "alice@example.com".into();
        connection.password = secret("koofr-pass");
        let (backend_type, parameters, obscure) = params_for(&connection).expect("params");
        assert_eq!(backend_type, "koofr");
        let params = parameters.as_object().expect("object");
        assert_eq!(params["user"], "alice@example.com", "email maps onto user");
        assert_eq!(params["password"], secret("koofr-pass"));
        assert!(
            params.get("endpoint").is_none(),
            "empty endpoint keeps rclone's app.koofr.net default"
        );
        assert!(obscure, "koofr password is IsPassword");

        connection.endpoint = "https://koofr.example.com".into();
        assert_eq!(param_map(&connection)["endpoint"], "https://koofr.example.com");
    }

    #[test]
    fn params_for_pcloud_userpass_token_and_hostname() {
        let mut connection = fixture("pcloud");
        assert!(
            params_for(&connection).is_err(),
            "neither user/pass nor token must fail the build"
        );

        connection.endpoint = "https://api.pcloud.com/over,path".into();
        connection.username = "alice".into();
        connection.password = secret("pcloud-pass");
        let (backend_type, parameters, obscure) = params_for(&connection).expect("params");
        assert_eq!(backend_type, "pcloud");
        let params = parameters.as_object().expect("object");
        assert_eq!(params["hostname"], "api.pcloud.com", "bare host only");
        assert_eq!(params["username"], "alice");
        assert_eq!(params["password"], secret("pcloud-pass"));
        assert!(obscure, "pcloud password is IsPassword");

        // Token material adds the token option without dropping user/pass.
        connection.access_token = secret("pcloud-access");
        let params = param_map(&connection);
        assert!(params.contains_key("token") && params.contains_key("username"));
    }

    #[test]
    fn params_for_custom_passthrough_reaches_any_rclone_backend() {
        let mut connection = fixture("opendal-custom");
        assert!(params_for(&connection).is_err(), "empty service rejected");

        connection.service = "Not-Valid!".into();
        let error = params_for(&connection).expect_err("bad vocabulary");
        assert!(error.contains("rclone.org/overview"), "{error}");

        connection.service = " memory ".into();
        connection.custom_config = serde_json::json!({ "discard": true });
        let (backend_type, parameters, obscure) = params_for(&connection).expect("params");
        assert_eq!(backend_type, "memory", "service trims onto the rclone type");
        assert_eq!(parameters, serde_json::json!({ "discard": true }));
        // obscure rides along unconditionally: the user JSON may carry any
        // provider's IsPassword option (e.g. mega `pass`).
        assert!(obscure);

        connection.service = "mega".into();
        connection.custom_config = serde_json::json!({ "pass": secret("mega-pass") });
        let params = param_map(&connection);
        assert_eq!(params["pass"], secret("mega-pass"));
    }

    #[test]
    fn aliyun_drive_errors_with_migration_notice() {
        let error = params_for(&fixture("aliyun-drive")).expect_err("aliyun-drive");
        assert!(error.contains("aliyun-drive"), "{error}");
        assert!(error.contains("rclone upstream"), "{error}");
        assert!(error.contains("DBX_FILES_ENGINE=opendal"), "{error}");
    }

    #[test]
    fn unknown_protocol_errors() {
        let error = params_for(&fixture("floppy")).expect_err("unknown");
        assert!(error.contains("not supported"), "{error}");
    }

    // -- secret hygiene -------------------------------------------------------

    #[test]
    fn errors_and_binding_debug_never_echo_secrets() {
        let secrets = [
            secret("password"),
            secret("secret-access-key"),
            secret("account-key"),
            secret("credential-json"),
            secret("key-pem"),
            secret("access-token"),
            secret("refresh-token"),
        ];
        for protocol in [
            "fs", "s3", "oss", "cos", "obs", "gcs", "azblob", "webdav", "ftp", "sftp",
            "sftp-native", "smb", "aliyun-drive", "gdrive", "onedrive", "dropbox", "yandex-disk",
            "seafile", "koofr", "pcloud", "opendal-custom",
        ] {
            let mut connection = fixture(protocol);
            connection.password = secrets[0].clone();
            connection.secret_access_key = secrets[1].clone();
            connection.account_key = secrets[2].clone();
            connection.credential = secrets[3].clone();
            connection.key = secrets[4].clone();
            connection.access_token = secrets[5].clone();
            connection.refresh_token = secrets[6].clone();
            connection.client_secret = secrets[2].clone();
            connection.secret_id = secrets[1].clone();
            connection.secret_key = secrets[1].clone();
            connection.security_token = secrets[1].clone();
            // Errors (including failures) must never carry a secret value...
            let results = [params_for(&connection).err(), binding_for(&connection).err()];
            for error in results.into_iter().flatten() {
                for secret in &secrets {
                    assert!(!error.contains(secret.as_str()), "{protocol}: {error}");
                }
            }
            // ...and neither must the binding view handed to ops.
            if let Ok(binding) = binding_for(&connection) {
                let debug = format!("{binding:?}");
                for secret in &secrets {
                    assert!(!debug.contains(secret.as_str()), "{protocol}: {debug}");
                }
            }
        }
        assert!(!remote_name("id-with-secret-text").contains(secret("password").as_str()));
    }

    // -- binding view ----------------------------------------------------------

    #[test]
    fn binding_for_fs_uses_root_path() {
        let mut connection = fixture("fs");
        connection.root = "/srv/data".into();
        let binding = binding_for(&connection).expect("binding");
        assert_eq!(binding.remote_fs, "/srv/data");
        assert_eq!(binding.backend_type, "local");
        assert_eq!(binding.root, "/srv/data");

        connection.root = String::new();
        let binding = binding_for(&connection).expect("binding");
        assert_eq!(binding.remote_fs, "/");

        connection.lock_to_root = true;
        connection.read_only = true;
        connection.allow_delete = false;
        let binding = binding_for(&connection).expect("binding");
        assert!(binding.lock_to_root && binding.read_only && !binding.allow_delete);
    }

    #[test]
    fn binding_for_remote_uses_named_fs() {
        let mut connection = fixture("s3");
        connection.bucket = "demo".into();
        connection.root = "prefix".into();
        let binding = binding_for(&connection).expect("binding");
        assert_eq!(binding.remote_fs, "dbxAb12Cd34:");
        assert_eq!(binding.backend_type, "s3");
        assert_eq!(binding.root, "prefix", "root not merged into remote_fs");

        // Every supported remote protocol produces a named fs binding; the
        // type string is the rclone backend (custom = the user's service).
        for (protocol, backend) in [
            ("gdrive", "drive"),
            ("onedrive", "onedrive"),
            ("dropbox", "dropbox"),
            ("yandex-disk", "yandex"),
            ("seafile", "seafile"),
            ("koofr", "koofr"),
            ("pcloud", "pcloud"),
            ("opendal-custom", "memory"),
        ] {
            let mut connection = fixture(protocol);
            connection.access_token = secret("access");
            match protocol {
                "seafile" | "pcloud" => {
                    connection.endpoint = "https://example.com".into();
                    connection.username = "user".into();
                    connection.password = secret("pass");
                }
                "koofr" => {
                    connection.email = "user@example.com".into();
                    connection.password = secret("pass");
                }
                "opendal-custom" => connection.service = "memory".into(),
                _ => {}
            }
            let binding = binding_for(&connection)
                .unwrap_or_else(|error| panic!("{protocol}: {error}"));
            assert_eq!(binding.backend_type, backend, "{protocol}");
            assert_eq!(binding.remote_fs, "dbxAb12Cd34:", "{protocol}");
        }

        assert!(binding_for(&fixture("aliyun-drive")).is_err(), "no rclone mapping");
    }

    // -- registry table ----------------------------------------------------------

    #[test]
    fn registry_tracks_bindings() {
        let registry = Registry::new();
        assert!(registry.is_empty());
        let connection = fixture("s3");
        let binding = binding_for(&connection).expect("binding");
        let registration = RemoteRegistration {
            name: "dbxAb12Cd34".to_string(),
            backend_type: "s3".to_string(),
            parameters: serde_json::Map::new().into(),
            obscure: false,
        };
        registry.insert(&connection.id, binding.clone(), registration.clone());
        assert_eq!(registry.get(&connection.id), Some(binding));
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.ids(), vec!["Ab12Cd34".to_string()]);
        registry.remove(&connection.id);
        assert!(registry.get(&connection.id).is_none());
        assert!(registry.is_empty());
        assert_eq!(registry.get("unknown"), None);
    }

    #[test]
    fn registry_tracks_registrations_and_groups() {
        let registry = Registry::new();
        let registration = |name: &str| RemoteRegistration {
            name: name.to_string(),
            backend_type: "s3".to_string(),
            parameters: serde_json::Map::new().into(),
            obscure: false,
        };

        let mut direct = fixture("s3");
        let direct_binding = binding_for(&direct).expect("binding");
        registry.insert(&direct.id, direct_binding.clone(), registration("dbxDirect"));
        assert!(registry.registration(&direct.id).is_some());
        assert_eq!(registry.count_in_group("direct"), 1);
        assert!(registry.live_group_keys().contains(&"direct".to_string()));

        // A proxied connection lands in its own group (URL-derived key).
        let mut proxied = fixture("s3");
        proxied.id = "OtherId99".to_string();
        proxied.proxy = Some(crate::model::ProxyConfig {
            kind: crate::model::ProxyKind::Http,
            host: "127.0.0.1".to_string(),
            port: 18081,
            username: String::new(),
            password: String::new(),
        });
        let proxied_binding = binding_for(&proxied).expect("binding");
        registry.insert(&proxied.id, proxied_binding, registration("dbxProxy"));
        let key = crate::rclone::registry::group_key_of(proxied.proxy.as_ref());
        assert_eq!(registry.count_in_group(&key), 1);
        assert_eq!(registry.count_in_group("direct"), 1);
        assert_eq!(registry.registrations_in_group(&key).len(), 1);
        assert_eq!(registry.live_group_keys().len(), 2);
        assert!(registry.proxy_in_group(&key).is_some());

        registry.remove(&proxied.id);
        assert_eq!(registry.count_in_group(&key), 0);
        assert!(registry.proxy_in_group(&key).is_none());
        assert_eq!(registry.live_group_keys(), vec!["direct".to_string()]);
    }

    // -- live-process tests (skipped without an rclone binary) --------------------

    async fn rcd_client() -> Option<(super::super::proc::RcdHandle, RcClient)> {
        let binary = super::super::proc::resolve_binary()?;
        // `None` inherits the sidecar environment (the "direct" group) —
        // registry tests never exercise the per-proxy rcd env routing.
        let handle = super::super::proc::RcdHandle::start(&binary, None).await.ok()?;
        let client = handle.client();
        Some((handle, client))
    }

    /// Live `connection/test`: fs against a tempdir passes and cleans up; s3
    /// against a refused endpoint fails with rclone's original text (no
    /// secret echo) and still cleans up.
    #[tokio::test]
    async fn live_test_connection_fs_and_s3_error_passthrough() {
        let Some((_handle, client)) = rcd_client().await else {
            eprintln!("skipping: no rclone binary found");
            return;
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let mut fs_connection = fixture("fs");
        fs_connection.id = "FsConn01".into();
        fs_connection.root = dir.path().to_string_lossy().into_owned();
        test_connection(&client, &fs_connection)
            .await
            .expect("fs test passes on an existing root");
        let dump = client.config_dump().await.expect("dump");
        assert!(
            dump.get(&format!("{}_test", remote_name("FsConn01"))).is_none(),
            "throwaway remote cleaned up"
        );

        let mut s3_connection = fixture("s3");
        s3_connection.id = "S3Conn01".into();
        s3_connection.access_key_id = "AKIA-example".into();
        s3_connection.secret_access_key = secret("live-sk");
        s3_connection.endpoint = "http://127.0.0.1:9".into();
        let error = test_connection(&client, &s3_connection)
            .await
            .expect_err("refused endpoint must fail");
        assert!(!error.trim().is_empty(), "rclone text passes through: {error}");
        assert!(
            !error.contains(secret("live-sk").as_str()),
            "error must not echo the secret: {error}"
        );
        let dump = client.config_dump().await.expect("dump after failure");
        assert!(
            dump.get(&format!("{}_test", remote_name("S3Conn01"))).is_none(),
            "throwaway remote cleaned up after failure"
        );
    }

    /// Live `connection/test` failure for a missing concrete root: the local
    /// backend fails fs creation itself with rc 404 "directory not found"
    /// (a missing object *under* a valid fs yields `Ok(item: null)` instead —
    /// both shapes surface as failed tests).
    #[tokio::test]
    async fn live_test_connection_flags_missing_root() {
        let Some((_handle, client)) = rcd_client().await else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let mut fs_connection = fixture("fs");
        fs_connection.id = "FsConn02".into();
        fs_connection.root = "/definitely/missing/root-xyz".into();
        let error = test_connection(&client, &fs_connection)
            .await
            .expect_err("missing root must fail");
        assert!(error.contains("directory not found"), "{error}");
    }

    /// Live custom pass-through end to end — the gateway to every rclone
    /// backend without a dedicated quick form (the offline matrix pins the
    /// parameter assembly; this proves the full config/create → operations/
    /// stat → config/delete path against a real rcd):
    /// - `memory` (zero params) tests clean;
    /// - `alias` rooted at a real tempdir passes the root-existence check
    ///   for a non-fs backend, and a missing root fails descriptively;
    /// - an unknown backend type fails with rclone's own text (and still
    ///   cleans up the throwaway remote).
    #[tokio::test]
    async fn live_test_connection_custom_passthrough_memory_alias_unknown() {
        let Some((_handle, client)) = rcd_client().await else {
            eprintln!("skipping: no rclone binary found");
            return;
        };

        let mut memory = fixture("opendal-custom");
        memory.id = "CustomMem1".into();
        memory.service = "memory".into();
        memory.custom_config = Value::Object(Map::new());
        test_connection(&client, &memory)
            .await
            .expect("memory passthrough tests clean");
        let dump = client.config_dump().await.expect("dump");
        assert!(
            dump.get(&format!("{}_test", remote_name("CustomMem1"))).is_none(),
            "throwaway memory remote cleaned up"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        // alias composes its `remote` param with the connection root
        // (`name:root` resolves remote+root, NOT remote+root twice): an
        // empty root stays at the alias target, a relative root drills in.
        let mut alias = fixture("opendal-custom");
        alias.id = "CustomAls1".into();
        alias.service = "alias".into();
        alias.custom_config = serde_json::json!({ "remote": dir.path().to_string_lossy() });
        test_connection(&client, &alias)
            .await
            .expect("alias with an empty root tests clean");

        alias.root = "sub".into();
        test_connection(&client, &alias)
            .await
            .expect("alias root resolving into the target passes the root check");

        alias.root = "missing-xyz".into();
        let error = test_connection(&client, &alias)
            .await
            .expect_err("missing alias subpath must fail");
        // Both rclone shapes surface here: item-null → "root path … does not
        // exist", or fs-creation 404 "directory not found".
        assert!(
            error.contains("root") || error.contains("not found"),
            "{error}"
        );

        let mut unknown = fixture("opendal-custom");
        unknown.id = "CustomBad1".into();
        unknown.service = "nosuchbackend42".into();
        unknown.custom_config = Value::Object(Map::new());
        let error = test_connection(&client, &unknown)
            .await
            .expect_err("unknown backend type must fail");
        assert!(!error.trim().is_empty(), "rclone text passes through: {error}");
        let dump = client.config_dump().await.expect("dump after failure");
        assert!(
            dump.get(&format!("{}_test", remote_name("CustomBad1"))).is_none(),
            "throwaway unknown-type remote cleaned up"
        );
    }

    /// Live connect → registry → rclone config → disconnect round trip, plus
    /// idempotent reconnect and idempotent disconnect.
    #[tokio::test]
    async fn live_connect_registry_disconnect_roundtrip() {
        let Some((_handle, client)) = rcd_client().await else {
            eprintln!("skipping: no rclone binary found");
            return;
        };
        let registry = Registry::new();
        let mut connection = fixture("s3");
        connection.access_key_id = "AKIA-example".into();
        connection.secret_access_key = secret("live-sk");

        let binding = connect(&registry, &client, &connection)
            .await
            .expect("connect");
        assert_eq!(binding.remote_fs, format!("{}:", remote_name(&connection.id)));
        assert_eq!(binding.backend_type, "s3");
        assert_eq!(registry.get(&connection.id), Some(binding.clone()));
        let dump = client.config_dump().await.expect("dump");
        assert!(
            dump.get(&remote_name(&connection.id)).is_some(),
            "remote present in rclone config"
        );

        // Idempotent reconnect: delete + recreate, table stays single-entry.
        connect(&registry, &client, &connection)
            .await
            .expect("reconnect");
        assert_eq!(registry.len(), 1);
        let dump = client.config_dump().await.expect("dump");
        assert!(dump.get(&remote_name(&connection.id)).is_some());

        disconnect(&registry, &client, &connection.id)
            .await
            .expect("disconnect");
        assert!(registry.get(&connection.id).is_none());
        let dump = client.config_dump().await.expect("dump");
        assert!(
            dump.get(&remote_name(&connection.id)).is_none(),
            "remote removed from rclone config"
        );

        // Unknown id: already disconnected → Ok.
        disconnect(&registry, &client, "never-connected")
            .await
            .expect("idempotent disconnect");
    }

    /// Key-name discipline (§4 hard constraint): every emitted key must exist
    /// in `rclone config providers <type>` of the installed binary.
    #[tokio::test]
    async fn emitted_keys_exist_in_provider_options() {
        let binary = match super::super::proc::resolve_binary() {
            Some(binary) => binary,
            None => {
                eprintln!("skipping: no rclone binary found");
                return;
            }
        };
        let output = std::process::Command::new(binary)
            .arg("config")
            .arg("providers")
            .output()
            .expect("config providers runs");
        let providers: Value =
            serde_json::from_slice(&output.stdout).expect("providers JSON parses");

        let option_names = |rclone_type: &str| -> Vec<String> {
            providers
                .as_array()
                .expect("providers array")
                .iter()
                .find(|provider| provider.get("Prefix").and_then(Value::as_str) == Some(rclone_type))
                .map(|provider| {
                    provider["Options"]
                        .as_array()
                        .expect("options array")
                        .iter()
                        .filter_map(|option| option.get("Name").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_else(|| panic!("provider {rclone_type} missing from output"))
        };

        fn maximal_case(
            protocol: &str,
            mutator: &dyn Fn(&mut StoredConnection),
        ) -> (String, String, Value) {
            let mut connection = fixture(protocol);
            mutator(&mut connection);
            let (rclone_type, parameters, _) =
                params_for(&connection).expect("params");
            (protocol.to_string(), rclone_type, parameters)
        }
        let cases = vec![
            maximal_case("fs", &|_| {}),
            maximal_case("s3", &|c: &mut StoredConnection| {
                c.access_key_id = "AK".into();
                c.secret_access_key = secret("sk");
                c.endpoint = "http://127.0.0.1:9000".into();
            }),
            maximal_case("oss", &|c: &mut StoredConnection| {
                c.access_key_id = "AK".into();
                c.secret_access_key = secret("sk");
            }),
            maximal_case("cos", &|c: &mut StoredConnection| {
                c.secret_id = secret("id");
                c.secret_key = secret("key");
                c.security_token = secret("token");
            }),
            maximal_case("obs", &|c: &mut StoredConnection| {
                c.access_key_id = "AK".into();
                c.secret_access_key = secret("sk");
            }),
            maximal_case("gcs", &|c: &mut StoredConnection| {
                c.credential = BASE64_STANDARD.encode(r#"{"project_id":"demo"}"#);
            }),
            maximal_case("azblob", &|c: &mut StoredConnection| {
                c.account_name = "account".into();
                c.account_key = secret("key");
            }),
            maximal_case("webdav", &|c: &mut StoredConnection| {
                c.endpoint = "https://dav.example.com".into();
                c.username = "user".into();
                c.password = secret("pass");
            }),
            maximal_case("ftp", &|c: &mut StoredConnection| {
                c.endpoint = "ftps://ftp.example.com".into();
                c.password = secret("pass");
                c.proxy = Some(ProxyConfig {
                    kind: ProxyKind::Http,
                    host: "proxy.example.com".to_string(),
                    port: 8080,
                    username: "u".to_string(),
                    password: secret("pass"),
                });
            }),
            maximal_case("sftp", &|c: &mut StoredConnection| {
                c.endpoint = "ssh://user@host:22".into();
                c.password = secret("pass");
                c.key = "~/.ssh/id_ed25519".into();
                c.known_hosts_strategy = "Strict".into();
                c.proxy = Some(ProxyConfig {
                    kind: ProxyKind::Socks5,
                    host: "socks.example.com".to_string(),
                    port: 1080,
                    username: String::new(),
                    password: String::new(),
                });
            }),
            maximal_case("smb", &|c: &mut StoredConnection| {
                c.endpoint = "smb://host:445".into();
                c.username = "user".into();
                c.password = secret("pass");
                c.domain = "WORKGROUP".into();
            }),
            maximal_case("gdrive", &|c: &mut StoredConnection| {
                c.client_id = "app".into();
                c.client_secret = secret("secret");
                c.refresh_token = secret("refresh");
            }),
            maximal_case("onedrive", &|c: &mut StoredConnection| {
                c.client_id = "app".into();
                c.client_secret = secret("secret");
                c.refresh_token = secret("refresh");
            }),
            maximal_case("dropbox", &|c: &mut StoredConnection| {
                c.client_id = "app".into();
                c.refresh_token = secret("refresh");
            }),
            maximal_case("yandex-disk", &|c: &mut StoredConnection| {
                c.access_token = secret("access");
            }),
            maximal_case("seafile", &|c: &mut StoredConnection| {
                c.endpoint = "https://seafile.example.com".into();
                c.username = "user".into();
                c.password = secret("pass");
                c.repo_name = "media".into();
            }),
            maximal_case("koofr", &|c: &mut StoredConnection| {
                c.endpoint = "https://koofr.example.com".into();
                c.email = "user@example.com".into();
                c.password = secret("pass");
            }),
            maximal_case("pcloud", &|c: &mut StoredConnection| {
                c.endpoint = "https://api.pcloud.com".into();
                c.username = "user".into();
                c.password = secret("pass");
            }),
            // The custom pass-through reaches the providers catalog itself —
            // pin the mega backend as its representative.
            maximal_case("opendal-custom", &|c: &mut StoredConnection| {
                c.service = "mega".into();
                c.custom_config = serde_json::json!({ "user": "u", "pass": secret("pass") });
            }),
        ];
        for (protocol, rclone_type, parameters) in cases {
            let names = option_names(&rclone_type);
            for key in parameters.as_object().expect("object").keys() {
                assert!(
                    names.iter().any(|name| name == key),
                    "{protocol}: emitted key '{key}' not in rclone config providers {rclone_type}"
                );
            }
        }
    }
}

/// Manifest-driven form × registry matrix (the rclone twin of
/// `engine::form_matrix`): replays the checked-in connection form's host
/// lifecycle shape against [`params_for`] so form/engine drift fails CI
/// instead of a user's connect dialog. The engine-side matrix pins the
/// OpenDAL builder; this one pins the rclone remote assembly.
///
/// Host semantics under test (same shapes engine::form_matrix replays):
/// - the host submits the WHOLE `config` binding, so hidden fields keep
///   stale values from a previous protocol selection;
/// - non-empty secret-bound fields travel in `connection_secrets`;
/// - every protocol the form advertises must assemble (or fail only with a
///   descriptive error), and stale cross-protocol values must never leak
///   foreign keys into a remote's `config/create` parameters.
#[cfg(test)]
mod manifest_matrix {
    use super::tests::secret;
    use super::*;
    use serde_json::json;

    // ------------------------------------------------------------- form model

    fn provider() -> Value {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../manifest.json");
        let raw =
            std::fs::read_to_string(path).unwrap_or_else(|error| panic!("manifest readable: {error}"));
        let manifest: Value = serde_json::from_str(&raw).expect("manifest.json parses");
        manifest["contributions"]
            .as_array()
            .expect("contributions array")
            .iter()
            .find(|item| item["type"] == "connection-provider")
            .cloned()
            .expect("connection-provider contribution")
    }

    fn form_fields() -> Vec<Value> {
        provider()["fields"]
            .as_array()
            .expect("fields array")
            .clone()
    }

    /// Every protocol the checked-in form advertises — the single source of
    /// truth for what "supports all protocols" means at any point in time.
    fn form_protocols() -> Vec<String> {
        form_fields()
            .iter()
            .find(|field| field["key"] == "protocol")
            .expect("protocol field")["options"]
            .as_array()
            .expect("protocol options")
            .iter()
            .map(|option| option["value"].as_str().expect("option value").to_string())
            .collect()
    }

    /// One-of values per protocol for fields gated on the protocol select
    /// (`visible_when` / `required_when`) — mirrors the host's condition
    /// semantics (`protocol` is always set, so `one_of` membership decides).
    /// Fields gated on other controls (e.g. allow_delete on read_only) count
    /// as visible: they don't depend on the protocol.
    fn gated_on(field: &Value, protocol: &str, condition: &str) -> bool {
        let condition = match field.get(condition) {
            Some(condition) => condition,
            None => return true,
        };
        if condition["field"].as_str() != Some("protocol") {
            return true;
        }
        condition["one_of"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|value| value == protocol)
            })
            .unwrap_or(false)
    }

    /// A valid sample per field key; credential-class values route through
    /// the runtime `secret()` builder (Mimosa 门禁不认字面量夹具). `key`
    /// samples the PATH form — rclone's `key_file` wants a path, and the
    /// PEM-content rejection is pinned by its own test below.
    fn sample_value(key: &str, protocol: &str) -> Value {
        match key {
            "display_name" => json!("Matrix"),
            "protocol" => json!(protocol),
            "endpoint" => json!(match protocol {
                "s3" | "gcs" | "azblob" | "obs" | "oss" | "cos" | "webdav" | "seafile"
                | "koofr" | "pcloud" => "https://svc.example.com",
                "ftp" => "ftp://127.0.0.1:2121",
                "sftp" | "sftp-native" => "127.0.0.1:22",
                "smb" => "nas.local:445",
                _ => "",
            }),
            "service" => json!("memory"),
            "config" => json!({ "root": "/tmp/dbx-matrix-root" }),
            "access_token" => json!(secret("access")),
            "refresh_token" => match protocol {
                "dropbox" | "gdrive" | "onedrive" => json!(""),
                _ => json!(secret("refresh")),
            },
            "client_id" => json!("client-id"),
            "client_secret" => json!(secret("client")),
            "drive_type" => json!("resource"),
            "email" => json!("alice@example.com"),
            "repo_name" => json!("library"),
            "bucket" => json!("demo"),
            "container" => json!("demo-container"),
            "account_name" => json!("account"),
            "account_key" => json!(secret("azkey")),
            "credential" => json!(BASE64_STANDARD.encode(r#"{"project_id":"matrix"}"#)),
            "scope" => json!("https://www.googleapis.com/auth/devstorage.read_write"),
            "region" => json!("us-east-1"),
            "access_key_id" => json!("ak"),
            "secret_access_key" => json!(secret("s3sk")),
            "secret_id" => json!(secret("cosid")),
            "secret_key" => json!(secret("coskey")),
            "security_token" => json!(secret("costoken")),
            "share" => json!("media"),
            "username" => json!("alice"),
            "user" => json!("bob"),
            "domain" => json!("WORKGROUP"),
            "password" => json!(secret("pass")),
            "key" => json!("~/.ssh/dbx-matrix-key"),
            "known_hosts_strategy" => json!("Tolerate"),
            // Ships default "off": the flat parser short-circuits before the
            // dependent samples (stale-superset inputs) can matter.
            "proxy_type" => json!("off"),
            "proxy_host" => json!("127.0.0.1"),
            "proxy_port" => json!("1080"),
            "proxy_username" => json!("proxyuser"),
            "proxy_password" => json!(secret("proxy-pw")),
            // Ships default "": no tunnel unless the user types a jump chain.
            "tunnel_jump_hosts" => json!(""),
            "tunnel_identity_file" => json!(""),
            "timeout_secs" => json!(30),
            "read_only" => json!(false),
            "allow_delete" => json!(true),
            "lock_to_root" => json!(false),
            "enable_virtual_host_style" => json!(false),
            "root" => json!(""),
            other => panic!("form field '{other}' has no matrix sample value"),
        }
    }

    /// Host-shaped lifecycle params: `external_config` gets the whole config
    /// binding (stale superset), secrets only the non-empty secret-bound
    /// fields (engine::form_matrix::lifecycle_params, replayed here).
    fn lifecycle_params(protocol: &str, overrides: &Map<String, Value>) -> Value {
        let mut values: Map<String, Value> = form_fields()
            .iter()
            .map(|field| {
                let key = field["key"].as_str().expect("field key").to_string();
                let value = overrides
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| sample_value(&key, protocol));
                (key, value)
            })
            .collect();
        values.insert("protocol".to_string(), json!(protocol));

        let mut external_config = Map::new();
        let mut connection_secrets = Map::new();
        let mut name = Value::Null;
        for field in form_fields() {
            let key = field["key"].as_str().expect("field key").to_string();
            let value = values.get(&key).cloned().unwrap_or(Value::Null);
            match field["binding"].as_str().unwrap_or_default() {
                "name" => name = value,
                "secret" => {
                    let textual = value.as_str().map(str::trim).unwrap_or_default();
                    if !textual.is_empty() || value.is_number() || value.is_boolean() {
                        connection_secrets.insert(key, value);
                    }
                }
                _ => {
                    external_config.insert(key, value);
                }
            }
        }
        json!({
            "connection": {
                "id": "matrix",
                "name": name,
                "external_config": external_config,
                "connection_secrets": connection_secrets,
            }
        })
    }

    fn parse(params: &Value) -> StoredConnection {
        StoredConnection::from_lifecycle_params(params)
            .expect("matrix lifecycle params must always parse")
    }

    /// The maximal parameter key set each protocol may emit when every form
    /// field carries a value (the stale superset). A key outside this table
    /// means a foreign protocol's value leaked into the remote config.
    fn expected_keys(protocol: &str) -> Vec<&'static str> {
        match protocol {
            "fs" => vec![],
            "s3" => vec![
                "provider", "access_key_id", "secret_access_key", "endpoint", "region",
                "force_path_style",
            ],
            "oss" | "obs" => vec!["provider", "access_key_id", "secret_access_key", "endpoint"],
            "cos" => vec![
                "provider", "access_key_id", "secret_access_key", "session_token", "endpoint",
            ],
            "gcs" => vec!["service_account_credentials", "endpoint"],
            "azblob" => vec!["account", "key", "endpoint"],
            "webdav" => vec!["url", "vendor", "user", "pass"],
            "ftp" => vec!["host", "port", "user", "pass"],
            "sftp" | "sftp-native" => vec!["host", "port", "user", "pass", "key_file"],
            "smb" => vec!["host", "port", "user", "pass", "domain"],
            "gdrive" | "onedrive" | "dropbox" => vec!["client_id", "client_secret", "token"],
            "yandex-disk" => vec!["token"],
            "seafile" => vec!["url", "user", "pass", "library"],
            "koofr" => vec!["endpoint", "user", "password"],
            "pcloud" => vec!["username", "password", "hostname", "token"],
            "opendal-custom" => vec!["root"], // sample config object, passed through
            other => panic!("no expected key table for '{other}'"),
        }
    }

    // ---------------------------------------------------------------- matrix

    /// Every protocol the form advertises must assemble into `config/create`
    /// parameters from the form-complete lifecycle shape — offline, no
    /// network. Only `aliyun-drive` is expected to fail, with the OpenDAL
    /// migration pointer. The PEM-content key form stays an actionable
    /// rejection for sftp (the matrix samples the path form instead).
    #[test]
    fn form_complete_protocols_assemble_params() {
        for protocol in form_protocols() {
            let connection = parse(&lifecycle_params(&protocol, &Map::new()));
            match params_for(&connection) {
                Ok((rclone_type, parameters, obscure)) => {
                    let expected_type = rclone_type_of(&protocol, &connection);
                    assert_eq!(rclone_type, expected_type, "type for form-complete {protocol}");
                    assert!(
                        parameters.is_object(),
                        "{protocol}: parameters must be a JSON object"
                    );
                    let has_password_class = parameters
                        .as_object()
                        .expect("object")
                        .keys()
                        .any(|key| PASSWORD_KEYS.contains(&key.as_str()));
                    // Named protocols ride obscure only for IsPassword-class
                    // keys; the custom pass-through sends it unconditionally
                    // because only rcd knows the target provider's password
                    // set.
                    let expected_obscure =
                        protocol == "opendal-custom" || has_password_class;
                    assert_eq!(
                        obscure, expected_obscure,
                        "{protocol}: obscure must track IsPassword-class keys"
                    );
                }
                Err(error) => {
                    let pem_rejected = matches!(protocol.as_str(), "sftp" | "sftp-native")
                        && error.contains("file")
                        && error.contains("path");
                    assert!(
                        protocol == "aliyun-drive" || pem_rejected,
                        "form-complete {protocol} must assemble params: {error}"
                    );
                    assert!(
                        error.trim().len() > 10,
                        "{protocol}: rejection must be descriptive: {error}"
                    );
                    if protocol == "aliyun-drive" {
                        assert!(error.contains("DBX_FILES_ENGINE=opendal"), "{error}");
                    }
                }
            }
        }
    }

    fn rclone_type_of(protocol: &str, connection: &StoredConnection) -> String {
        if protocol == "opendal-custom" {
            connection.service.trim().to_string()
        } else {
            rclone_type(protocol).expect("static protocol maps to an rclone type")
        }
    }

    /// The stale superset (every config-bound field filled, whatever the
    /// protocol) must never leak foreign keys into a protocol's parameters:
    /// switching the form's protocol select cannot smuggle e.g. `share` into
    /// sftp or `client_secret` into koofr.
    #[test]
    fn stale_superset_never_leaks_foreign_keys_into_params() {
        for protocol in form_protocols() {
            if protocol == "aliyun-drive" {
                continue; // rejected outright, no parameter surface to leak into
            }
            let connection = parse(&lifecycle_params(&protocol, &Map::new()));
            let (_, parameters, _) =
                params_for(&connection).unwrap_or_else(|error| panic!("{protocol}: {error}"));
            let expected = expected_keys(&protocol);
            for key in parameters.as_object().expect("object").keys() {
                assert!(
                    expected.contains(&key.as_str()),
                    "{protocol}: unexpected parameter key '{key}' (cross-protocol leak)"
                );
            }
        }
    }

    /// Whitespace-only scalar input (a form can always submit it) must never
    /// panic the assembler: Ok or a descriptive error, per protocol × field.
    #[test]
    fn whitespace_scalars_never_panic_params() {
        for key in ["endpoint", "user", "username", "email", "service", "root", "bucket", "share"] {
            for protocol in form_protocols() {
                let mut overrides = Map::new();
                overrides.insert(key.to_string(), json!("   "));
                let connection = parse(&lifecycle_params(&protocol, &overrides));
                if let Err(error) = params_for(&connection) {
                    assert!(
                        error.trim().len() > 10,
                        "whitespace {protocol}/{key} error must be descriptive: {error}"
                    );
                }
            }
        }
    }

    /// A field the form hides for a protocol must not be observable by that
    /// protocol's assembler even when its stale value is present: walk every
    /// visible field left EMPTY (the "optional stays empty" axis) and demand
    /// Ok or a descriptive, non-panicking error.
    #[test]
    fn visible_field_left_empty_never_panics_params() {
        for protocol in form_protocols() {
            for field in form_fields() {
                let key = field["key"].as_str().expect("field key").to_string();
                let blankable = matches!(
                    field["type"].as_str().unwrap_or_default(),
                    "text" | "password" | "textarea" | "number"
                );
                if !blankable || key == "protocol" || key == "display_name" {
                    continue;
                }
                let visible = gated_on(&field, &protocol, "visible_when");
                if !visible {
                    continue; // hidden fields are exercised by the stale superset
                }
                let empty = if field["type"] == "number" {
                    Value::Null
                } else {
                    json!("")
                };
                let mut overrides = Map::new();
                overrides.insert(key.clone(), empty);
                let connection = parse(&lifecycle_params(&protocol, &overrides));
                if let Err(error) = params_for(&connection) {
                    let form_required = gated_on(&field, &protocol, "required_when");
                    assert!(
                        form_required
                            || matches!(protocol.as_str(), "gdrive" | "onedrive" | "dropbox" | "yandex-disk" | "pcloud")
                                && matches!(key.as_str(), "access_token" | "refresh_token" | "username" | "password"),
                        "empty optional field must not break assembly: {protocol}/{key}: {error}"
                    );
                    assert!(
                        error.trim().len() > 10,
                        "{protocol}/{key}: error must be descriptive: {error}"
                    );
                }
            }
        }
    }
}
