//! OpenDAL engine: Operator construction and the in-memory connection table.
//!
//! Design points (implementation doc §5.1/§5.4/§6.2):
//! - Credentials are only ever held in process memory: the Builder kv is
//!   assembled from `external_config` + `connection_secrets`, handed to
//!   OpenDAL, and dropped. Nothing is logged, persisted, or exposed.
//! - `Operator` is `Clone + Send + Sync`, so the connection table stores one
//!   Operator per connection behind a plain mutex.
//! - `connection/test` builds a throwaway Operator and runs `check()`; it
//!   leaves no state behind.
//! - `opendal 0.57` exposes the universal `Operator::via_iter(scheme, kv)`
//!   constructor (registry-based); quick protocols and `opendal-custom` share
//!   the same path, differing only in how the kv map is assembled.

// Fully implemented (F-A); the ops layer that lands with F-B will consume
// `timeout`/`connection` helpers, so silence dead-code until then.
#![allow(dead_code)]

pub mod ops;
// F-C: streaming transfer primitives (Writer/Reader slots + dir traversal)
// consumed by `crate::transfers`. One-line addition to the F-A module list.
pub mod transfer;
// F5-SMB: custom OpenDAL Access adapter for the `smb` quick protocol
// (`smb2 =0.20.1` behind `opendal::raw::Access`; IMPL_PLAN_SMB §1/§2).
pub mod smb;
// sftp-native (dual-stack decision 2026-08-31): russh + russh-sftp Access
// adapter for the `sftp-native` quick protocol — password auth the OpenDAL
// 0.57 sftp service cannot do. Additive; OpenDAL `sftp` stays available.
pub mod sftp_native;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opendal::Operator;

use crate::model::StoredConnection;

/// One connected storage: the parsed lifecycle record (for read_only /
/// allow_delete / lock_to_root gates and timeouts) plus its Operator.
#[derive(Clone)]
pub struct OperatorEntry {
    pub connection: StoredConnection,
    pub operator: Operator,
}

/// Reserved `connectionId` for the sidecar-local filesystem (dual-pane left
/// column, "local files" side). Never present in the host connection table:
/// [`Engine::entry`] synthesizes it on demand so every `files/*` method
/// (browse/read/write/copy/move/uploads) works against the local disk without
/// a dedicated protocol surface.
pub const LOCAL_CONNECTION_ID: &str = "__local__";

/// Synthesized connection record behind [`LOCAL_CONNECTION_ID`]: an fs
/// connection rooted at `/` with default gates (writable, deletable,
/// unlocked root) — identical to a user-created "local filesystem"
/// connection, so policy/quick-path behavior stays uniform.
pub fn local_connection() -> StoredConnection {
    StoredConnection::from_lifecycle_params(&serde_json::json!({
        "connection": {
            "id": LOCAL_CONNECTION_ID,
            "name": "Local",
            "external_config": { "protocol": "fs", "root": "/" }
        }
    }))
    .expect("local connection record is a constant shape")
}

/// `connectionId -> OperatorEntry`. Handler concurrency (one request per task
/// on the worker pool) requires every access to hold the mutex; entries are
/// replaced wholesale on reconnect (M0 §3.2 idempotent connect).
pub type ConnectionTable = Arc<Mutex<HashMap<String, OperatorEntry>>>;

/// Engine facade used by `main.rs`.
pub struct Engine {
    connections: ConnectionTable,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        // Registry init is idempotent; the facade's ctor already ran for the
        // normal path, this covers embedded/test harnesses.
        opendal::init_default_registry();
        Self {
            connections: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// `connection/test`: builds a temporary Operator and probes it with
    /// `check()`. No state is stored; a failure surfaces as a business error.
    pub async fn test(&self, connection: &StoredConnection) -> Result<(), String> {
        let operator = build_operator(connection)?;
        operator
            .check()
            .await
            .map_err(|error| format!("Storage check failed: {error}"))
    }

    /// `connection/connect`: idempotent — an existing entry for the same id is
    /// dropped before the new Operator is inserted.
    pub fn connect(&self, connection: StoredConnection) -> Result<(), String> {
        // The reserved local id always resolves to the synthesized local
        // entry; refuse the shadow attempt explicitly instead of silently
        // making the host connection unreachable.
        if connection.id == LOCAL_CONNECTION_ID {
            return Err(format!(
                "connectionId '{LOCAL_CONNECTION_ID}' is reserved for the built-in local filesystem"
            ));
        }
        // Build first so a bad config never evicts a good entry.
        let operator = build_operator(&connection)?;
        let mut table = self
            .connections
            .lock()
            .map_err(|_| "Connection table lock is poisoned".to_string())?;
        table.insert(
            connection.id.clone(),
            OperatorEntry {
                connection,
                operator,
            },
        );
        Ok(())
    }

    /// `connection/disconnect`: drops the entry; idempotent (unknown ids are
    /// a success). Job cancellation for the connection is handled by the
    /// transfer layer (`transfers::cancel_connection_jobs`).
    pub fn disconnect(&self, connection_id: &str) -> Result<(), String> {
        let mut table = self
            .connections
            .lock()
            .map_err(|_| "Connection table lock is poisoned".to_string())?;
        table.remove(connection_id);
        Ok(())
    }

    /// Clones the Operator out of the table for a request.
    pub fn operator(&self, connection_id: &str) -> Result<Operator, String> {
        Ok(self.entry(connection_id)?.operator)
    }

    /// Clones the stored connection record (for gating decisions).
    pub fn connection(&self, connection_id: &str) -> Result<StoredConnection, String> {
        Ok(self.entry(connection_id)?.connection)
    }

    fn entry(&self, connection_id: &str) -> Result<OperatorEntry, String> {
        if connection_id == LOCAL_CONNECTION_ID {
            // Pure config construction (no I/O); the fs Operator dials
            // nothing and each call goes straight to the local filesystem.
            let connection = local_connection();
            return Ok(OperatorEntry {
                operator: build_operator(&connection)?,
                connection,
            });
        }
        let table = self
            .connections
            .lock()
            .map_err(|_| "Connection table lock is poisoned".to_string())?;
        table
            .get(connection_id)
            .cloned()
            .ok_or_else(|| format!("Unknown connectionId '{connection_id}'; connect first"))
    }

    /// Ids of all live connections (diagnostics/tests only; no config echoed).
    #[allow(dead_code)]
    pub fn connection_ids(&self) -> Vec<String> {
        self.connections
            .lock()
            .map(|table| table.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Default per-operation timeout derived from `timeout_secs`.
    pub fn timeout(entry: &OperatorEntry) -> Duration {
        Duration::from_secs(entry.connection.timeout_secs.max(1))
    }
}

/// Builds an Operator from a connection record.
///
/// - `smb` is NOT an OpenDAL service: it goes through `Operator::new`
///   static dispatch on the custom [`smb::SmbBuilder`] (IMPL_PLAN_SMB §1).
/// - `sftp-native` likewise goes through `Operator::new` on the custom
///   [`sftp_native::SftpNativeBuilder`] (russh + russh-sftp; dual-stack
///   decision 2026-08-31).
/// - Everything else funnels through `Operator::via_iter`:
///   - quick protocol (`fs`/`s3`/`webdav`/`ftp`/`sftp`): scheme is the
///     protocol name, kv assembled from the manifest-mapped OpenDAL keys
///     (doc §4 table).
///   - `opendal-custom`: scheme is `service`, kv is the `config` JSON object
///     (the generic `root` field is merged in, JSON keys win).
///
/// Unknown extra keys are ignored by OpenDAL's config deserializer, so
/// passing e.g. `password` to sftp (which has no password option and uses
/// key auth only) is harmless.
pub fn build_operator(connection: &StoredConnection) -> Result<Operator, String> {
    if connection.protocol == "smb" {
        return build_smb_operator(connection);
    }
    if connection.protocol == "sftp-native" {
        return build_sftp_native_operator(connection);
    }
    let (scheme, kv) = protocol_kv(connection)?;
    Operator::via_iter(scheme, kv)
        .map_err(|error| format!("Failed to build storage operator: {error}"))
}

/// Builds the native SFTP Operator via static dispatch on
/// [`sftp_native::SftpNativeBuilder`]. Credentials pass through the builder
/// in memory only; building does not touch the network (the SSH handshake
/// dials lazily on the first operation).
fn build_sftp_native_operator(connection: &StoredConnection) -> Result<Operator, String> {
    // Endpoint hygiene first so a bad endpoint fails with the scheme-level
    // message instead of surfacing as a dial error.
    validate_endpoints(connection)?;
    let builder = sftp_native::SftpNativeBuilder::new()
        .endpoint(&connection.endpoint)
        .user(&connection.user)
        .username(&connection.username)
        .password(&connection.password)
        .key(&connection.key)
        .known_hosts_strategy(&connection.known_hosts_strategy)
        .root(&connection.root);
    Operator::new(builder)
        .map(|builder| builder.finish())
        .map_err(|error| format!("Failed to build storage operator: {error}"))
}

/// Builds the SMB Operator via static dispatch on [`smb::SmbBuilder`].
/// Credentials pass through the builder in memory only; building does not
/// touch the network (the adapter dials lazily on the first operation).
fn build_smb_operator(connection: &StoredConnection) -> Result<Operator, String> {
    // Endpoint hygiene first so a bad endpoint fails with the scheme-level
    // message instead of surfacing as a build/dial error.
    validate_endpoints(connection)?;
    let builder = smb::SmbBuilder::new()
        .endpoint(&connection.endpoint)
        .share(&connection.share)
        .username(&connection.username)
        .password(&connection.password)
        .domain(&connection.domain)
        .root(&connection.root);
    Operator::new(builder)
        .map(|builder| builder.finish())
        .map_err(|error| format!("Failed to build storage operator: {error}"))
}

/// Resolves the OpenDAL scheme and Builder kv for a connection.
/// Public for tests; callers must not log the returned kv (secrets inside).
pub fn protocol_kv(
    connection: &StoredConnection,
) -> Result<(String, Vec<(String, String)>), String> {
    let mut kv: Vec<(String, String)> = Vec::new();
    fn push(kv: &mut Vec<(String, String)>, key: &str, value: &str) {
        if !value.is_empty() {
            kv.push((key.to_string(), value.to_string()));
        }
    }

    let scheme: String = match connection.protocol.as_str() {
        "fs" => {
            push(&mut kv, "root", &connection.root);
            "fs".to_string()
        }
        "s3" => {
            push(&mut kv, "root", &connection.root);
            push(&mut kv, "bucket", &connection.bucket);
            push(&mut kv, "endpoint", &connection.endpoint);
            push(&mut kv, "region", &connection.region);
            push(&mut kv, "access_key_id", &connection.access_key_id);
            push(&mut kv, "secret_access_key", &connection.secret_access_key);
            "s3".to_string()
        }
        "oss" => {
            // Alibaba OSS (services-oss). Reuses the s3-shaped connection
            // fields; the secret maps to OpenDAL's `access_key_secret` key.
            push(&mut kv, "root", &connection.root);
            push(&mut kv, "bucket", &connection.bucket);
            push(&mut kv, "endpoint", &connection.endpoint);
            push(&mut kv, "access_key_id", &connection.access_key_id);
            push(&mut kv, "access_key_secret", &connection.secret_access_key);
            "oss".to_string()
        }
        "webdav" => {
            push(&mut kv, "root", &connection.root);
            push(&mut kv, "endpoint", &connection.endpoint);
            push(&mut kv, "username", &connection.username);
            push(&mut kv, "password", &connection.password);
            "webdav".to_string()
        }
        "ftp" => {
            push(&mut kv, "root", &connection.root);
            push(&mut kv, "endpoint", &connection.endpoint);
            push(&mut kv, "user", &connection.user);
            push(&mut kv, "password", &connection.password);
            "ftp".to_string()
        }
        "sftp" => {
            push(&mut kv, "root", &connection.root);
            push(&mut kv, "endpoint", &connection.endpoint);
            push(&mut kv, "user", &connection.user);
            push(&mut kv, "key", &connection.key);
            push(&mut kv, "known_hosts_strategy", &connection.known_hosts_strategy);
            // Note: OpenDAL's sftp service is key-based; `password` has no
            // matching option in 0.57 and is deliberately not forwarded.
            "sftp".to_string()
        }
        "smb" => {
            // Documents the manifest → adapter field mapping; the Operator
            // itself is built via `Operator::new(SmbBuilder)` in
            // `build_smb_operator` (via_iter has no "smb" service registered).
            push(&mut kv, "root", &connection.root);
            push(&mut kv, "endpoint", &connection.endpoint);
            push(&mut kv, "share", &connection.share);
            push(&mut kv, "username", &connection.username);
            push(&mut kv, "password", &connection.password);
            push(&mut kv, "domain", &connection.domain);
            "smb".to_string()
        }
        "sftp-native" => {
            // Documents the manifest → adapter field mapping; the Operator
            // itself is built via `Operator::new(SftpNativeBuilder)` in
            // `build_sftp_native_operator` (via_iter has no "sftp-native"
            // service registered). `password` is a secret-bound field.
            push(&mut kv, "root", &connection.root);
            push(&mut kv, "endpoint", &connection.endpoint);
            push(&mut kv, "user", &connection.user);
            push(&mut kv, "username", &connection.username);
            push(&mut kv, "password", &connection.password);
            push(&mut kv, "key", &connection.key);
            push(&mut kv, "known_hosts_strategy", &connection.known_hosts_strategy);
            "sftp-native".to_string()
        }
        "opendal-custom" => {
            let service = connection.service.trim();
            if service.is_empty() {
                return Err("opendal-custom requires 'service'".to_string());
            }
            // Reject anything that is not a plain identifier so the service
            // field can never smuggle URI syntax into via_iter.
            if !service
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(format!("Invalid opendal-custom service '{service}'"));
            }
            kv_from_custom_config(connection, &mut kv);
            service.to_string()
        }
        other => return Err(format!("Unsupported protocol '{other}'")),
    };

    validate_endpoints(connection)?;
    Ok((scheme, kv))
}

/// Flattens the custom config JSON object into kv pairs, with the generic
/// `root` field merged underneath (JSON keys take precedence).
fn kv_from_custom_config(connection: &StoredConnection, kv: &mut Vec<(String, String)>) {
    if !connection.root.is_empty() {
        kv.push(("root".to_string(), connection.root.clone()));
    }
    if let Some(map) = connection.custom_config.as_object() {
        for (key, value) in map {
            let text = match value {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            // Replace the generic root when the JSON config defines its own.
            if key == "root" {
                kv.retain(|(existing, _)| existing != "root");
            }
            kv.push((key.clone(), text));
        }
    }
}

/// Light endpoint hygiene (doc §6.1.1/§6.1.2): URL-shaped endpoint fields must
/// use schemes appropriate to the backend (HTTP-class: http/https; ftp: ftp/ftps;
/// sftp: ssh). Covers both the quick-protocol `endpoint` field and the
/// `endpoint` key inside an opendal-custom `config` JSON (where obs/cos/…
/// live). A full policy module (private address handling, redirect rules)
/// lands with the policy layer; loopback/private http endpoints stay allowed
/// for local storage workflows (MinIO smoke, dev S3).
fn validate_endpoints(connection: &StoredConnection) -> Result<(), String> {
    let http_class = matches!(
        connection.protocol.as_str(),
        "s3" | "oss" | "webdav" | "opendal-custom"
    );
    if http_class && !connection.endpoint.is_empty() {
        check_http_scheme(&connection.endpoint)?;
    }
    // smb: bare `host[:port]` (same shape as ftp) or `smb://host[:port]`;
    // every other scheme is rejected (IMPL_PLAN_SMB §2.3).
    if connection.protocol == "smb" && !connection.endpoint.is_empty() {
        check_smb_endpoint(&connection.endpoint)?;
    }
    // sftp-native: bare `host[:port]` or `ssh://[user@]host[:port]`; any
    // other scheme is rejected at validation time (same fail-fast shape as
    // smb — the adapter's parser re-checks at build).
    if connection.protocol == "sftp-native" && !connection.endpoint.is_empty() {
        sftp_native::parse_sftp_native_endpoint(&connection.endpoint)
            .map_err(|error| format!("Invalid endpoint for {}: {error}", connection.protocol))?;
    }
    // opendal-custom: the config JSON carries the endpoint for the cloud
    // services (s3/oss/obs/cos/...); the quick `endpoint` field is empty.
    if connection.protocol == "opendal-custom" {
        if let Some(map) = connection.custom_config.as_object() {
            let service = connection.service.as_str();
            for key in map.keys() {
                if !key.eq_ignore_ascii_case("endpoint") {
                    continue;
                }
                if let Some(value) = map[key].as_str() {
                    if value.is_empty() {
                        continue;
                    }
                    match allowed_endpoint_schemes(service) {
                        // Known service class: enforce its scheme set.
                        Some(schemes) => check_url_scheme(value, schemes, service)?,
                        // Unknown service: only reject schemes that are never
                        // legitimate for a network storage endpoint.
                        None => reject_never_valid_schemes(value)?,
                    }
                }
            }
        }
    }
    Ok(())
}

/// Schemes a service's `endpoint` may use; `None` = unknown service class.
fn allowed_endpoint_schemes(service: &str) -> Option<&'static [&'static str]> {
    match service {
        "s3" | "gcs" | "azblob" | "oss" | "obs" | "cos" | "webdav" | "memory" => {
            Some(&["http", "https"])
        }
        "ftp" => Some(&["ftp", "ftps"]),
        "sftp" => Some(&["ssh"]),
        _ => None,
    }
}

fn check_url_scheme(endpoint: &str, schemes: &[&str], service: &str) -> Result<(), String> {
    let lower = endpoint.to_ascii_lowercase();
    if schemes
        .iter()
        .any(|scheme| lower.starts_with(&format!("{scheme}://")))
    {
        Ok(())
    } else {
        Err(format!(
            "opendal-custom service '{service}' endpoint must use {} (got '{}')",
            schemes
                .iter()
                .map(|scheme| format!("{scheme}://"))
                .collect::<Vec<_>>()
                .join(" or "),
            redact_url(endpoint)
        ))
    }
}

fn reject_never_valid_schemes(endpoint: &str) -> Result<(), String> {
    let lower = endpoint.to_ascii_lowercase();
    if lower.starts_with("file://") {
        Err(format!(
            "Endpoint must not use file:// for a network storage service (got '{}')",
            redact_url(endpoint)
        ))
    } else {
        Ok(())
    }
}

fn check_http_scheme(endpoint: &str) -> Result<(), String> {
    let lower = endpoint.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        Ok(())
    } else {
        Err(format!(
            "Endpoint must use http:// or https:// (got '{}')",
            redact_url(endpoint)
        ))
    }
}

/// `smb` endpoint hygiene (IMPL_PLAN_SMB §2.3): accepts bare `host[:port]`
/// (same shape as ftp) or `smb://host[:port]`; rejects `file://`, `http://`
/// and every other scheme. Port/emptiness details are enforced by
/// `smb::parse_smb_endpoint` at build time.
fn check_smb_endpoint(endpoint: &str) -> Result<(), String> {
    let text = endpoint.trim();
    let body = match text.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("smb") => rest,
        Some((scheme, _)) => {
            return Err(format!(
                "SMB endpoint must be a bare host[:port] or smb://host[:port] (got scheme '{scheme}://')"
            ))
        }
        None => text,
    };
    let host = body
        .trim_end_matches('/')
        .rsplit(':')
        .next()
        .unwrap_or_default();
    if host.is_empty() {
        return Err(format!(
            "SMB endpoint host must not be empty (got '{endpoint}')"
        ));
    }
    Ok(())
}

/// Strips userinfo/query from an endpoint before echoing it in errors.
fn redact_url(endpoint: &str) -> String {
    match endpoint.split_once("://") {
        Some((scheme, rest)) => {
            let authority = rest.split(['/', '?']).next().unwrap_or(rest);
            let host = match authority.rsplit_once('@') {
                Some((_userinfo, host)) => host,
                None => authority,
            };
            format!("{scheme}://{host}")
        }
        None => "<endpoint>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn connection(protocol: &str) -> StoredConnection {
        StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "c1",
                "external_config": { "protocol": protocol }
            }
        }))
        .unwrap()
    }

    #[test]
    fn memory_custom_operator_checks_and_roundtrips() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut custom = connection("opendal-custom");
            custom.service = "memory".to_string();
            custom.custom_config = json!({});
            let operator = build_operator(&custom).unwrap();
            operator
                .check()
                .await
                .expect("memory operator must check");

            operator.write("hello.txt", "world").await.unwrap();
            let data = operator.read("hello.txt").await.unwrap();
            assert_eq!(data.to_vec(), b"world");
        });
    }

    #[test]
    fn cloud_custom_service_operators_build_with_official_config_keys() {
        // 文件/云存储范围（2026-08-29 产品决策）：obs/cos 必须能用 schema
        // 收录的官方 builder 键真实构建 Operator（构建不联网；真实连通性由
        // connection/test 在用户配置后验证）。memory 已移出配置面但后端仍可建。
        let mut custom = connection("opendal-custom");

        custom.service = "obs".to_string();
        custom.custom_config = json!({
            "bucket": "demo",
            "endpoint": "https://obs.cn-north-4.myhuaweicloud.com",
            "access_key_id": "ak",
            "secret_access_key": "sk",
        });
        build_operator(&custom).expect("obs operator must build from schema keys");

        custom.service = "cos".to_string();
        custom.custom_config = json!({
            "bucket": "demo-1250000000",
            "endpoint": "https://cos.ap-guangzhou.myqcloud.com",
            "secret_id": "id",
            "secret_key": "key",
        });
        build_operator(&custom).expect("cos operator must build from schema keys");

        custom.service = "oss".to_string();
        custom.custom_config = json!({
            "bucket": "demo",
            "endpoint": "https://oss-cn-hangzhou.aliyuncs.com",
            "access_key_id": "ak",
            "access_key_secret": "sk",
        });
        build_operator(&custom).expect("oss operator must build from schema keys");
    }

    #[test]
    fn quick_protocol_kv_maps_opendal_keys() {
        let mut s3 = connection("s3");
        s3.bucket = "demo".into();
        s3.endpoint = "http://127.0.0.1:9000".into();
        s3.access_key_id = "minioadmin".into();
        s3.secret_access_key = "minioadmin".into();
        let (scheme, kv) = protocol_kv(&s3).unwrap();
        assert_eq!(scheme, "s3");
        let map: HashMap<String, String> = kv.into_iter().collect();
        assert_eq!(map["bucket"], "demo");
        assert_eq!(map["endpoint"], "http://127.0.0.1:9000");
        assert_eq!(map["secret_access_key"], "minioadmin");
        assert!(!map.contains_key("password"));

        let mut ftp = connection("ftp");
        ftp.endpoint = "ftp://127.0.0.1:2121".into();
        ftp.user = "anonymous".into();
        let (scheme, kv) = protocol_kv(&ftp).unwrap();
        assert_eq!(scheme, "ftp");
        let map: HashMap<String, String> = kv.into_iter().collect();
        assert_eq!(map["user"], "anonymous");
    }

    #[test]
    fn quick_protocol_oss_maps_s3_shaped_fields_to_oss_keys() {
        let mut oss = connection("oss");
        oss.bucket = "demo".into();
        oss.endpoint = "https://oss-cn-hangzhou.aliyuncs.com".into();
        oss.access_key_id = "ak".into();
        oss.secret_access_key = "sk".into();
        let (scheme, kv) = protocol_kv(&oss).unwrap();
        assert_eq!(scheme, "oss");
        let map: HashMap<String, String> = kv.into_iter().collect();
        assert_eq!(map["bucket"], "demo");
        assert_eq!(map["endpoint"], "https://oss-cn-hangzhou.aliyuncs.com");
        assert_eq!(map["access_key_id"], "ak");
        // The secret reuses the s3-shaped binding but lands on the OpenDAL
        // oss service key.
        assert_eq!(map["access_key_secret"], "sk");
        assert!(!map.contains_key("secret_access_key"));
        assert!(!map.contains_key("region"));

        // Empty optionals are dropped; root passes through when set.
        let mut bare = connection("oss");
        bare.root = "/sub".into();
        let (scheme, kv) = protocol_kv(&bare).unwrap();
        assert_eq!(scheme, "oss");
        let map: HashMap<String, String> = kv.into_iter().collect();
        assert_eq!(map["root"], "/sub");
        assert_eq!(map.len(), 1, "only root survives when everything else is empty");
    }

    #[test]
    fn oss_endpoints_reject_non_http_schemes() {
        let mut oss = connection("oss");
        oss.endpoint = "file:///etc/passwd".into();
        assert!(protocol_kv(&oss).is_err(), "file:// must be rejected for oss");

        oss.endpoint = "https://oss-cn-hangzhou.aliyuncs.com".into();
        assert!(protocol_kv(&oss).is_ok());
    }

    #[test]
    fn custom_kv_merges_root_under_json() {
        let mut custom = connection("opendal-custom");
        custom.service = "memory".into();
        custom.root = "/generic".into();
        custom.custom_config = json!({ "root": "/from-json" });
        let (_, kv) = protocol_kv(&custom).unwrap();
        let map: HashMap<String, String> = kv.into_iter().collect();
        assert_eq!(map["root"], "/from-json", "JSON root wins");
    }

    #[test]
    fn custom_requires_service_and_plain_identifier() {
        let mut no_service = connection("opendal-custom");
        assert!(protocol_kv(&no_service).is_err());

        no_service.service = "bad scheme".into();
        assert!(protocol_kv(&no_service).is_err());

        no_service.service = "memory".into();
        assert!(protocol_kv(&no_service).is_ok());
    }

    #[test]
    fn http_endpoints_reject_non_http_schemes() {
        let mut s3 = connection("s3");
        s3.endpoint = "ftp://10.0.0.1:9000".into();
        assert!(protocol_kv(&s3).is_err());

        s3.endpoint = "http://10.0.0.1:9000".into();
        assert!(protocol_kv(&s3).is_ok());

        // ftp/sftp endpoints are not URL-shaped and skip the check.
        let mut ftp = connection("ftp");
        ftp.endpoint = "127.0.0.1:2121".into();
        assert!(protocol_kv(&ftp).is_ok());
    }

    #[test]
    fn custom_config_endpoints_are_scheme_gated_per_service() {
        // 第 3 轮遗留项收口：opendal-custom 的 endpoint 在 config JSON 里，
        // 之前未进护栏。HTTP 类云服务拒绝 file:// 等 scheme；ftp/sftp 各自
        // 允许集生效；未知服务仅拒绝 file://（最小惊讶）。
        let mut custom = connection("opendal-custom");
        custom.service = "obs".to_string();
        custom.custom_config = json!({
            "bucket": "demo",
            "endpoint": "file:///etc/passwd",
            "access_key_id": "ak",
            "secret_access_key": "sk",
        });
        assert!(protocol_kv(&custom).is_err(), "file:// must be rejected");

        custom.custom_config = json!({
            "bucket": "demo",
            "endpoint": "ftp://mirror.example.com",
            "access_key_id": "ak",
            "secret_access_key": "sk",
        });
        assert!(protocol_kv(&custom).is_err(), "ftp scheme must be rejected for obs");

        custom.custom_config = json!({
            "bucket": "demo",
            "endpoint": "https://obs.cn-north-4.myhuaweicloud.com",
            "access_key_id": "ak",
            "secret_access_key": "sk",
        });
        assert!(protocol_kv(&custom).is_ok(), "https stays allowed");

        custom.service = "sftp".to_string();
        custom.custom_config = json!({
            "endpoint": "ssh://user@host:22",
            "key": "k",
        });
        assert!(protocol_kv(&custom).is_ok(), "ssh scheme stays allowed for sftp");

        custom.service = "totally-new-service".to_string();
        custom.custom_config = json!({ "endpoint": "file:///x" });
        assert!(protocol_kv(&custom).is_err(), "file:// rejected for unknown services");
        custom.custom_config = json!({ "endpoint": "weird://x" });
        assert!(protocol_kv(&custom).is_ok(), "unknown schemes tolerated for unknown services");
    }

    #[test]
    fn engine_connect_disconnect_is_idempotent() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let engine = Engine::new();
            let mut custom = connection("opendal-custom");
            custom.service = "memory".into();

            engine.connect(custom.clone()).unwrap();
            engine.connect(custom.clone()).unwrap();
            assert_eq!(engine.connection_ids().len(), 1);

            engine.operator("c1").unwrap();
            engine.disconnect("c1").unwrap();
            engine.disconnect("c1").unwrap(); // idempotent
            assert!(engine.operator("c1").is_err());
        });
    }

    #[test]
    fn test_leaves_no_state_behind() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let engine = Engine::new();
            let mut custom = connection("opendal-custom");
            custom.service = "memory".into();
            engine.test(&custom).await.unwrap();
            assert!(engine.connection_ids().is_empty());
        });
    }

    #[test]
    fn unknown_scheme_fails_to_build_operator() {
        let mut custom = connection("opendal-custom");
        custom.service = "definitely-not-a-service".into();
        assert!(build_operator(&custom).is_err());
    }

    // -- smb protocol branch (F5-SMB) ----------------------------------------

    fn smb_connection() -> StoredConnection {
        let mut smb = connection("smb");
        smb.endpoint = "nas.local:445".into();
        smb.share = "media".into();
        smb.username = "alice".into();
        smb.password = "wonderland".into();
        smb.domain = "WORKGROUP".into();
        smb
    }

    #[test]
    fn smb_endpoint_validation_table() {
        let mut smb = smb_connection();

        for ok in [
            "nas.local",
            "nas.local:445",
            "smb://nas.local",
            "smb://nas.local:1445",
        ] {
            smb.endpoint = ok.into();
            assert!(protocol_kv(&smb).is_ok(), "endpoint '{ok}' must pass");
        }
        for bad in [
            "file:///etc/passwd",
            "http://nas.local",
            "https://nas.local",
        ] {
            smb.endpoint = bad.into();
            let error = protocol_kv(&smb).unwrap_err();
            assert!(
                error.contains("smb://") || error.contains("bare"),
                "{bad}: {error}"
            );
        }
    }

    #[test]
    fn smb_kv_documents_manifest_mapping() {
        let (scheme, kv) = protocol_kv(&smb_connection()).unwrap();
        assert_eq!(scheme, "smb");
        let map: HashMap<String, String> = kv.into_iter().collect();
        assert_eq!(map["endpoint"], "nas.local:445");
        assert_eq!(map["share"], "media");
        assert_eq!(map["username"], "alice");
        assert_eq!(map["password"], "wonderland", "secret stays in-memory only");
        assert_eq!(map["domain"], "WORKGROUP");
    }

    #[test]
    fn smb_operator_builds_via_custom_access_with_capabilities() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            // Building must not touch the network (lazy dial); capabilities
            // are declared by the adapter, copy/presign deliberately false.
            let operator = build_operator(&smb_connection()).unwrap();
            let capability = operator.info().full_capability();
            assert_eq!(operator.info().scheme(), "smb");
            assert!(capability.list && capability.read && capability.write);
            assert!(capability.rename && capability.create_dir && capability.delete);
            assert!(!capability.copy, "copy degrades to the read→write job");
            assert!(!capability.presign_read, "publicLink reports unsupported");
        });
    }

    #[test]
    fn smb_operator_rejects_missing_share_and_bad_endpoints() {
        let mut smb = smb_connection();
        smb.share = "".into();
        assert!(build_operator(&smb).is_err(), "share is required");

        let mut smb = smb_connection();
        smb.endpoint = "http://nas.local".into();
        assert!(build_operator(&smb).is_err(), "scheme hygiene applies");

        let mut smb = smb_connection();
        smb.endpoint = "nas.local:notaport".into();
        assert!(build_operator(&smb).is_err(), "port must be numeric");
    }

    // -- built-in local filesystem connection (dual-pane left column) --------

    #[test]
    fn local_connection_resolves_as_rooted_fs_entry() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let engine = Engine::new();
            // Not in the connection table, yet resolvable on demand.
            let connection = engine
                .connection(LOCAL_CONNECTION_ID)
                .expect("local connection resolves without connect");
            assert_eq!(connection.protocol, "fs");
            assert_eq!(connection.root, "/");
            assert!(!connection.read_only);
            assert!(connection.allow_delete);
            assert!(!connection.lock_to_root);
            let operator = engine.operator(LOCAL_CONNECTION_ID).unwrap();
            assert_eq!(operator.info().scheme(), "fs");
            // check() runs a backend probe — the local fs must be there.
            operator.check().await.expect("local fs checks");
        });
    }

    #[test]
    fn local_connection_id_is_reserved_against_connect() {
        let engine = Engine::new();
        let mut local = local_connection();
        // A host connection attempting to shadow the reserved id is refused.
        assert!(engine.connect(local.clone()).is_err());
        // The reserved entry still resolves to the built-in fs operator.
        assert_eq!(engine.operator(LOCAL_CONNECTION_ID).unwrap().info().scheme(), "fs");

        local.id = "host-fs".into();
        engine.connect(local).unwrap();
        assert_eq!(engine.connection_ids().len(), 1);
    }
}
