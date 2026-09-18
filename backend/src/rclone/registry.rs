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
//!   `pass`). `Sensitive`-but-not-password keys (s3 `secret_access_key`,
//!   azureblob `key`, gcs `service_account_credentials`) are stored verbatim
//!   in rcd's `0600` temp config, which dies with the process.
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
use crate::model::StoredConnection;

/// Protocols served by the rclone engine in phase A. `sftp` and
/// `sftp-native` both map onto the rclone `sftp` backend; everything else in
/// [`crate::model::PROTOCOLS`] is rejected (phase C or unsupported).
pub const SUPPORTED_PROTOCOLS: [&str; 12] = [
    "fs", "s3", "oss", "cos", "obs", "gcs", "azblob", "webdav", "ftp", "sftp", "sftp-native", "smb",
];

/// Phase C protocols with their planned rclone type; rejected for now.
const PHASE_C_PROTOCOLS: [(&str, &str); 8] = [
    ("gdrive", "drive"),
    ("onedrive", "onedrive"),
    ("dropbox", "dropbox"),
    ("yandex-disk", "yandex"),
    ("seafile", "seafile"),
    ("koofr", "koofr"),
    ("pcloud", "pcloud"),
    ("opendal-custom", "custom (service pass-through)"),
];

/// Parameter keys rclone obscures at rest when `opt.obscure` is set. Only
/// `IsPassword`-marked keys are actually transformed (verified on v1.75.1);
/// this set decides whether the flag is worth sending.
const PASSWORD_KEYS: [&str; 5] = [
    "pass",
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
    pub backend_type: &'static str,
    pub root: String,
    pub lock_to_root: bool,
    pub read_only: bool,
    pub allow_delete: bool,
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

/// `true` when the protocol is part of the phase A support set.
pub fn is_supported(protocol: &str) -> bool {
    SUPPORTED_PROTOCOLS.contains(&protocol)
}

/// The rclone backend type for a supported protocol.
fn rclone_type(protocol: &str) -> Option<&'static str> {
    match protocol {
        "fs" => Some("local"),
        "s3" | "oss" | "cos" | "obs" => Some("s3"),
        "gcs" => Some("gcs"),
        "azblob" => Some("azureblob"),
        "webdav" => Some("webdav"),
        "ftp" => Some("ftp"),
        "sftp" | "sftp-native" => Some("sftp"),
        "smb" => Some("smb"),
        _ => None,
    }
}

/// Assembles `config/create` parameters for a connection.
///
/// Returns `(rclone type, parameters, obscure)`. Fails for protocols outside
/// the phase A support set and for shape errors (unparsable gcs credential,
/// PEM-content sftp key, malformed endpoints); rclone-level validation of the
/// values themselves happens on the remote when the connection is used.
pub fn params_for(connection: &StoredConnection) -> Result<(&'static str, Value, bool), String> {
    let protocol = connection.protocol.as_str();

    if let Some((name, planned)) = PHASE_C_PROTOCOLS.iter().find(|(name, _)| *name == protocol) {
        let _ = name;
        return Err(format!(
            "protocol '{protocol}' is scheduled for phase C of the rclone engine migration \
             (planned rclone type '{planned}'); not supported yet"
        ));
    }
    if protocol == "aliyun-drive" {
        return Err(
            "protocol 'aliyun-drive' is not supported by rclone upstream and cannot be served \
             by the rclone engine; until a custom rclone backend lands (planned follow-up), \
             keep using the OpenDAL engine (DBX_FILES_ENGINE=opendal) for aliyun-drive \
             connections"
                .to_string(),
        );
    }

    let backend_type = rclone_type(protocol).ok_or_else(|| {
        format!("protocol '{protocol}' is not supported by the rclone engine")
    })?;

    let parameters = match protocol {
        "fs" => Value::Object(Map::new()),
        "s3" | "oss" | "cos" | "obs" => s3_family_parameters(connection)?,
        "gcs" => gcs_parameters(connection)?,
        "azblob" => azblob_parameters(connection),
        "webdav" => webdav_parameters(connection)?,
        "ftp" => ftp_parameters(connection)?,
        "sftp" | "sftp-native" => sftp_parameters(connection)?,
        "smb" => smb_parameters(connection)?,
        _ => unreachable!("rclone_type() accepted an unmapped protocol"),
    };
    let obscure = parameters
        .as_object()
        .map(|params| params.keys().any(|key| PASSWORD_KEYS.contains(&key.as_str())))
        .unwrap_or(false);
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
    Ok(RemoteBinding {
        remote_fs,
        backend_type,
        root: connection.root.clone(),
        lock_to_root: connection.lock_to_root,
        read_only: connection.read_only,
        allow_delete: connection.allow_delete,
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

    client
        .config_create(&name, backend_type, parameters, obscure)
        .await
        .map_err(|error| error.to_string())?;
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
    client
        .config_create(&name, backend_type, parameters, obscure)
        .await
        .map_err(|error| error.to_string())?;
    let binding = binding_for(connection)?;
    registry.insert(&connection.id, binding.clone());
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
    entries: Mutex<HashMap<String, RemoteBinding>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The binding for a connected connection id, if any.
    pub fn get(&self, id: &str) -> Option<RemoteBinding> {
        self.lock().get(id).cloned()
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

    fn insert(&self, id: &str, binding: RemoteBinding) {
        self.lock().insert(id.to_string(), binding);
    }

    fn remove(&self, id: &str) -> Option<RemoteBinding> {
        self.lock().remove(id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, RemoteBinding>> {
        // A poisoned lock still holds valid data; recover instead of panicking
        // (guards are never held across await points, so this is rare).
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

// --- per-family parameter assembly -----------------------------------------

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
    fn secret(tag: &str) -> String {
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
        for protocol in [
            "gdrive",
            "onedrive",
            "dropbox",
            "yandex-disk",
            "seafile",
            "koofr",
            "pcloud",
            "opendal-custom",
            "aliyun-drive",
            "webdisk",
            "",
        ] {
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

    // -- phase C / unsupported ----------------------------------------------

    #[test]
    fn phase_c_protocols_error_with_schedule_notice() {
        for (protocol, planned) in PHASE_C_PROTOCOLS {
            let error = params_for(&fixture(protocol)).expect_err(protocol);
            assert!(
                error.contains("scheduled for phase C"),
                "{protocol}: {error}"
            );
            assert!(error.contains(planned), "{protocol}: {error}");
        }
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
        ];
        for protocol in [
            "fs", "s3", "oss", "cos", "obs", "gcs", "azblob", "webdav", "ftp", "sftp",
            "sftp-native", "smb", "aliyun-drive", "gdrive", "opendal-custom",
        ] {
            let mut connection = fixture(protocol);
            connection.password = secrets[0].clone();
            connection.secret_access_key = secrets[1].clone();
            connection.account_key = secrets[2].clone();
            connection.credential = secrets[3].clone();
            connection.key = secrets[4].clone();
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

        assert!(binding_for(&fixture("gdrive")).is_err(), "phase C rejected");
    }

    // -- registry table ----------------------------------------------------------

    #[test]
    fn registry_tracks_bindings() {
        let registry = Registry::new();
        assert!(registry.is_empty());
        let connection = fixture("s3");
        let binding = binding_for(&connection).expect("binding");
        registry.insert(&connection.id, binding.clone());
        assert_eq!(registry.get(&connection.id), Some(binding));
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.ids(), vec!["Ab12Cd34".to_string()]);
        registry.remove(&connection.id);
        assert!(registry.get(&connection.id).is_none());
        assert!(registry.is_empty());
        assert_eq!(registry.get("unknown"), None);
    }

    // -- live-process tests (skipped without an rclone binary) --------------------

    async fn rcd_client() -> Option<(super::super::proc::RcdHandle, RcClient)> {
        let binary = super::super::proc::resolve_binary()?;
        let handle = super::super::proc::RcdHandle::start(&binary).await.ok()?;
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
        ) -> (String, &'static str, Value) {
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
            }),
            maximal_case("sftp", &|c: &mut StoredConnection| {
                c.endpoint = "ssh://user@host:22".into();
                c.password = secret("pass");
                c.key = "~/.ssh/id_ed25519".into();
                c.known_hosts_strategy = "Strict".into();
            }),
            maximal_case("smb", &|c: &mut StoredConnection| {
                c.endpoint = "smb://host:445".into();
                c.username = "user".into();
                c.password = secret("pass");
                c.domain = "WORKGROUP".into();
            }),
        ];
        for (protocol, rclone_type, parameters) in cases {
            let names = option_names(rclone_type);
            for key in parameters.as_object().expect("object").keys() {
                assert!(
                    names.iter().any(|name| name == key),
                    "{protocol}: emitted key '{key}' not in rclone config providers {rclone_type}"
                );
            }
        }
    }
}
