//! Request/response model and lifecycle parsing for `io.dbx.files`.
//!
//! Lifecycle params follow the M0 common contract (`IMPL_PLAN_M0_COMMON.zh-CN.md`
//! §3.1): the host sends `provider`, `connection` (with `external_config` and
//! `connection_secrets`), optional `runtime`, and `operationId` (Host API 1.1+).
//! Manifest field `binding: config` lands in `connection.external_config.<key>`,
//! `binding: secret` lands in `connection.connection_secrets.<key>`.
//!
//! All protocol payloads are camelCase. Secret values only ever live inside
//! [`StoredConnection`]; the [`redact`] helpers keep them out of logs/errors.

// `redact` and a few parsing helpers are consumed by the policy/ops layers
// that land with F-B.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Binary transfer chunk size (256 KiB), aligned with the ssh-sftp plugin.
pub const TRANSFER_CHUNK_SIZE: usize = 256 * 1024;
/// `files/read` preview upper bound (2 MiB).
pub const MAX_PREVIEW_BYTES: usize = 2 * 1024 * 1024;
/// `files/write` inline payload upper bound (4 MiB); larger uploads must use
/// the binary channel.
pub const MAX_INLINE_WRITE_BYTES: usize = 4 * 1024 * 1024;
/// Finished-transfer history cap for `transfers.json` (ring overwrite).
pub const TRANSFER_HISTORY_LIMIT: usize = 200;
/// `files/transfer/progress` throttle: emit at most every 200 ms or 1% change.
pub const PROGRESS_INTERVAL_MS: u64 = 200;
pub const PROGRESS_MIN_DELTA: f64 = 0.01;
/// JSON+base64 fallback chunk (1 MiB raw) for hosts without `host.binary`.
pub const JSON_CHUNK_BYTES: usize = 1024 * 1024;

/// Protocols understood by the engine. The quick protocols map onto a
/// fixed OpenDAL scheme (`smb` via the custom `engine::smb` adapter,
/// `sftp-native` via the custom `engine::sftp_native` adapter); the
/// generic pass-through is `opendal-custom`.
pub const PROTOCOLS: [&str; 40] = [
    "fs",
    "s3",
    "oss",
    "cos",
    "webdav",
    "ftp",
    "sftp",
    "smb",
    "sftp-native",
    "opendal-custom",
    "aliyun-drive",
    "alluxio",
    "azdls",
    "azfile",
    "b2",
    "compfs",
    "dbfs",
    "dropbox",
    "gdrive",
    "ghac",
    "github",
    "goosefs",
    "hdfs",
    "hdfs-native",
    "http",
    "ipfs",
    "ipmfs",
    "koofr",
    "lakefs",
    "monoiofs",
    "onedrive",
    "pcloud",
    "seafile",
    "swift",
    "tos",
    "upyun",
    "vercel-artifacts",
    "vercel-blob",
    "webhdfs",
    "yandex-disk",
];

/// A validated connection parsed from lifecycle params. Secret fields
/// (`password`, `secret_access_key`, `secret_id`, `secret_key`,
/// `security_token`) are kept in memory only.
#[derive(Debug, Clone)]
pub struct StoredConnection {
    pub id: String,
    pub name: String,
    /// Quick protocol name; one of [`PROTOCOLS`].
    pub protocol: String,
    /// OpenDAL root prefix (`external_config.root`); empty means default root.
    pub root: String,
    /// Reject any path escaping `root`.
    pub lock_to_root: bool,
    /// `opendal-custom` service name (e.g. `gcs`, `memory`).
    pub service: String,
    /// `opendal-custom` config JSON object (verbatim Builder kv source).
    pub custom_config: Value,
    /// Protocol-specific config fields not shared by the legacy quick
    /// protocols. These are forwarded verbatim to the OpenDAL builder for
    /// the independent file-service protocols.
    pub extra_config: serde_json::Map<String, Value>,
    /// Secret-bound fields not covered by the legacy quick protocol fields.
    pub extra_secrets: HashMap<String, String>,
    // --- s3 ---
    pub bucket: String,
    pub endpoint: String,
    pub region: String,
    pub access_key_id: String,
    /// Secret (`connection_secrets.secret_access_key`).
    pub secret_access_key: String,
    // --- cos ---
    /// Secret (`connection_secrets.secret_id`).
    pub secret_id: String,
    /// Secret (`connection_secrets.secret_key`).
    pub secret_key: String,
    /// Optional STS session token (`connection_secrets.security_token`).
    pub security_token: String,
    /// s3 only: address the bucket as `bucket.host` instead of a path-style
    /// URL (`external_config.enable_virtual_host_style`).
    pub enable_virtual_host_style: bool,
    // --- webdav / ftp / sftp ---
    pub username: String,
    pub user: String,
    /// Secret (`connection_secrets.password`).
    pub password: String,
    // --- sftp ---
    /// Private key content or path (`connection_secrets.key`; legacy config fallback).
    pub key: String,
    pub known_hosts_strategy: String,
    // --- smb ---
    /// Optional share name (`external_config.share`). Empty enables SMB
    /// server-level share discovery; a path's first component then selects
    /// the tree connect target.
    pub share: String,
    /// NTLM domain / workgroup; optional (`external_config.domain`).
    pub domain: String,
    // --- gating / network ---
    pub read_only: bool,
    pub allow_delete: bool,
    /// Per-operation timeout in seconds (default 30).
    pub timeout_secs: u64,
    /// DBX transport dial endpoint (`runtime`), for `direct` dial semantics.
    pub runtime_host: String,
    pub runtime_port: u16,
}

impl StoredConnection {
    /// Parses lifecycle params (M0 §3.1 shape) into a connection record.
    ///
    /// Tolerant by design: only `connection.id` and a valid `protocol` are
    /// mandatory; every protocol-specific field is optional because each
    /// backend enforces its own required keys when the Operator is built.
    pub fn from_lifecycle_params(params: &Value) -> Result<Self, String> {
        let connection = params
            .get("connection")
            .and_then(Value::as_object)
            .ok_or("Missing connection payload")?;
        let id = string_field(connection, "id")?;
        let name = optional_string(Some(connection), "name");

        let external_config = connection.get("external_config").and_then(Value::as_object);
        let connection_secrets = connection
            .get("connection_secrets")
            .and_then(Value::as_object);

        let protocol = optional_string(external_config, "protocol");
        if protocol.is_empty() {
            return Err("Missing protocol in external_config".to_string());
        }
        if !PROTOCOLS.contains(&protocol.as_str()) {
            return Err(format!(
                "Unsupported protocol '{protocol}'; expected one of {}",
                PROTOCOLS.join(", ")
            ));
        }

        // opendal-custom accepts either a JSON object or a JSON string in the
        // textarea field; anything else must parse to an object.
        let custom_value = if protocol == "opendal-custom" {
            external_config.and_then(|config| config.get("config"))
        } else {
            None // An inactive custom-service draft must not break another protocol.
        };
        let custom_config = match custom_value {
            None | Some(Value::Null) => Value::Object(serde_json::Map::new()),
            Some(Value::Object(map)) => Value::Object(map.clone()),
            Some(Value::String(text)) => {
                // A cleared textarea arrives as an empty (or whitespace-only)
                // string: that means "no service config", not broken JSON.
                if text.trim().is_empty() {
                    Value::Object(serde_json::Map::new())
                } else {
                    let parsed: Value = serde_json::from_str(text)
                        .map_err(|error| format!("Invalid service config JSON: {error}"))?;
                    if !parsed.is_object() {
                        return Err("Service config JSON must be an object".to_string());
                    }
                    parsed
                }
            }
            Some(_) => return Err("Service config must be a JSON object".to_string()),
        };

        // Preserve every non-lifecycle config key so newly exposed OpenDAL
        // services can use their native Builder names without another fixed
        // field added to StoredConnection. The legacy quick protocols still
        // use their typed fields below; this map is consumed only by the
        // independent service branch in engine::protocol_kv.
        let mut extra_config = external_config.cloned().unwrap_or_default();
        for key in [
            "protocol",
            "service",
            "config",
            "read_only",
            "allow_delete",
            "lock_to_root",
            "timeout_secs",
        ] {
            extra_config.remove(key);
        }
        let extra_secrets = connection_secrets
            .map(|secrets| {
                secrets
                    .iter()
                    .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        let runtime = params.get("runtime").and_then(Value::as_object);
        let runtime_host = optional_string(runtime, "host");
        let runtime_port = runtime
            .and_then(|value| value.get("port"))
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .filter(|value| *value > 0)
            .unwrap_or(0);

        Ok(Self {
            id,
            name,
            protocol,
            root: optional_string(external_config, "root"),
            lock_to_root: bool_field(external_config, "lock_to_root", false),
            service: optional_string(external_config, "service"),
            custom_config,
            extra_config,
            extra_secrets,
            bucket: optional_string(external_config, "bucket"),
            endpoint: optional_string(external_config, "endpoint"),
            region: optional_string(external_config, "region"),
            access_key_id: optional_string(external_config, "access_key_id"),
            secret_access_key: secret_string(connection_secrets, "secret_access_key"),
            secret_id: secret_string(connection_secrets, "secret_id"),
            secret_key: secret_string(connection_secrets, "secret_key"),
            security_token: secret_string(connection_secrets, "security_token"),
            enable_virtual_host_style: bool_field(
                external_config,
                "enable_virtual_host_style",
                false,
            ),
            username: optional_string(external_config, "username"),
            user: optional_string(external_config, "user"),
            password: secret_string(connection_secrets, "password"),
            key: if connection_secrets.is_some_and(|secrets| secrets.contains_key("key")) {
                secret_string(connection_secrets, "key")
            } else {
                optional_string(external_config, "key")
            },
            known_hosts_strategy: optional_string(external_config, "known_hosts_strategy"),
            share: optional_string(external_config, "share"),
            domain: optional_string(external_config, "domain"),
            // 只读门禁收敛：连接表单 read_only（插件特定配置项）∥ 宿主标准
            // read_only（ConnectionConfig 通用设置）。
            read_only: bool_field(external_config, "read_only", false)
                || bool_field(Some(connection), "read_only", false),
            allow_delete: bool_field(external_config, "allow_delete", true),
            timeout_secs: external_config
                .and_then(|config| config.get("timeout_secs"))
                .and_then(Value::as_u64)
                .filter(|value| *value > 0)
                .unwrap_or(30),
            runtime_host,
            runtime_port,
        })
    }

    /// `true` when the protocol is the generic pass-through service form.
    pub fn is_custom(&self) -> bool {
        self.protocol == "opendal-custom"
    }
}

/// One directory entry as returned by `files/list` / `files/listPaged`.
/// `kind` is `"file"` or `"dir"`; `size`/`modifiedAt` are omitted when the
/// backend does not expose them (e.g. dirs on some services).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Unix epoch milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<u64>,
}

/// Capability report for `files/capabilities`; mirrors `info().full_capability()`
/// projections the frontend uses to show/hide actions.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub scheme: String,
    pub list: bool,
    pub write: bool,
    pub read: bool,
    pub stat: bool,
    pub delete: bool,
    pub create_dir: bool,
    pub copy: bool,
    pub rename: bool,
    pub presign: bool,
}

// ---------------------------------------------------------------------------
// Request payloads (§8) — camelCase, `connectionId` mandatory outside lifecycle.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListRequest {
    pub connection_id: String,
    pub path: String,
    #[serde(default)]
    pub recurse: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListPagedRequest {
    pub connection_id: String,
    pub path: String,
    pub page: u64,
    pub page_size: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathRequest {
    pub connection_id: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicLinkRequest {
    pub connection_id: String,
    pub path: String,
    #[serde(default)]
    pub expire_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadRequest {
    pub connection_id: String,
    pub path: String,
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteRequest {
    pub connection_id: String,
    pub path: String,
    pub data_base64: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyMoveRequest {
    pub connection_id: String,
    /// Defaults to `connectionId` (same-connection op).
    #[serde(default)]
    pub source_connection_id: Option<String>,
    pub source_path: String,
    /// Defaults to `connectionId` (same-connection op).
    #[serde(default)]
    pub target_connection_id: Option<String>,
    pub target_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameRequest {
    pub connection_id: String,
    pub path: String,
    pub new_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadStartRequest {
    pub connection_id: String,
    pub remote_path: String,
    pub size: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadStartRequest {
    pub connection_id: String,
    pub remote_path: String,
    /// 桌面端 sidecar 本机落盘（写入下载目录并随 finish 返回 localPath）。
    #[serde(default)]
    pub save_to_local: bool,
    /// 工作台「保存到」偏好的目录覆盖；空/缺省用系统下载目录。
    #[serde(default)]
    pub download_dir: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRequest {
    pub task_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirJobRequest {
    pub source_connection_id: String,
    pub source_path: String,
    pub target_connection_id: String,
    pub target_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransfersListRequest {
    #[serde(default)]
    pub connection_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRequest {
    pub job_id: String,
}

/// `files/archiveList` (B-ARCHIVE route): `page`/`pageSize` are optional —
/// defaults are applied by the handler (page 1, pageSize 200, clamp ≤ 1000).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveListRequest {
    pub connection_id: String,
    pub path: String,
    #[serde(default)]
    pub page: Option<u64>,
    #[serde(default)]
    pub page_size: Option<u64>,
}

/// `files/extract` (B-ARCHIVE route): `target_path` is a directory (created
/// when missing).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractRequest {
    pub connection_id: String,
    pub path: String,
    pub target_path: String,
}

/// `files/compress` (P-FILES round 13): packs same-connection `paths`
/// (files and/or directories) into one tar / tar.gz archive. The format is
/// derived from the `target_path` suffix (`.tar` / `.tar.gz` / `.tgz`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompressRequest {
    pub connection_id: String,
    pub paths: Vec<String>,
    pub target_path: String,
}

/// Extracts the mandatory `connectionId` from a generic params object
/// (mirrors the ssh-sftp helper of the same name).
pub fn connection_id_param(params: &Value) -> Result<&str, String> {
    params
        .get("connectionId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Missing connectionId".to_string())
}

/// Returns a copy of `value` with known credential-ish keys masked. Used for
/// any debug/error formatting that might carry external_config content.
pub fn redact(value: &Value) -> Value {
    const SECRET_KEY_FRAGMENTS: [&str; 7] = [
        "password",
        "secret",
        "key",
        "token",
        "credential",
        "signature",
        "connection_secrets",
    ];
    fn is_secret_key(key: &str) -> bool {
        let lower = key.to_ascii_lowercase();
        SECRET_KEY_FRAGMENTS
            .iter()
            .any(|fragment| lower.contains(fragment))
    }
    fn walk(value: &Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, item)| {
                        if is_secret_key(key) {
                            (key.clone(), Value::String("***".to_string()))
                        } else {
                            (key.clone(), walk(item))
                        }
                    })
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(walk).collect()),
            other => other.clone(),
        }
    }
    walk(value)
}

fn string_field(object: &serde_json::Map<String, Value>, key: &str) -> Result<String, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("Missing connection {key}"))
}

fn optional_string(object: Option<&serde_json::Map<String, Value>>, key: &str) -> String {
    object
        .and_then(|object| object.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Passwords and key material are opaque: whitespace may be intentional.
fn secret_string(object: Option<&serde_json::Map<String, Value>>, key: &str) -> String {
    object
        .and_then(|object| object.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn bool_field(object: Option<&serde_json::Map<String, Value>>, key: &str, default: bool) -> bool {
    match object.and_then(|object| object.get(key)) {
        None | Some(Value::Null) => default,
        Some(Value::Bool(value)) => *value,
        // Quoted booleans from LLM-built inline connections ("true"/"false")
        // must honor the caller's intent, never silently flip it; an
        // unparseable string keeps the field default.
        Some(Value::String(text)) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => true,
            "false" | "0" | "no" | "off" => false,
            _ => default,
        },
        Some(_) => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_minio_lifecycle_shape() {
        let params = json!({
            "provider": { "id": "io.dbx.files.connection", "databaseType": "storage" },
            "connection": {
                "id": "conn-1",
                "name": "minio",
                "external_config": {
                    "protocol": "s3",
                    "bucket": "demo",
                    "endpoint": "http://127.0.0.1:9000",
                    "region": "us-east-1",
                    "access_key_id": "minioadmin",
                    "root": "/data",
                    "timeout_secs": 45,
                    "read_only": true
                },
                "connection_secrets": { "secret_access_key": "minioadmin" }
            },
            "runtime": { "host": "127.0.0.1", "port": 9000 },
            "operationId": "op-42"
        });

        let connection = StoredConnection::from_lifecycle_params(&params).unwrap();
        assert_eq!(connection.id, "conn-1");
        assert_eq!(connection.protocol, "s3");
        assert_eq!(connection.bucket, "demo");
        assert_eq!(connection.endpoint, "http://127.0.0.1:9000");
        assert_eq!(connection.secret_access_key, "minioadmin");
        assert_eq!(connection.root, "/data");
        assert!(connection.read_only);
        assert!(connection.allow_delete, "allow_delete defaults to true");
        assert_eq!(connection.timeout_secs, 45);
        assert_eq!(connection.runtime_host, "127.0.0.1");
        assert_eq!(connection.runtime_port, 9000);
        assert!(
            !connection.enable_virtual_host_style,
            "virtual-host style defaults to off"
        );
    }

    #[test]
    fn parses_s3_virtual_host_style_flag() {
        let connection = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "conn-vhs",
                "external_config": {
                    "protocol": "s3",
                    "bucket": "demo",
                    "enable_virtual_host_style": true
                }
            }
        }))
        .unwrap();
        assert!(connection.enable_virtual_host_style);
    }

    #[test]
    fn parses_cos_credentials_only_from_connection_secrets() {
        let connection = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "conn-cos",
                "external_config": {
                    "protocol": "cos",
                    "bucket": "demo-1250000000",
                    "endpoint": "https://cos.ap-guangzhou.myqcloud.com",
                    "secret_id": "must-not-be-config",
                    "secret_key": "must-not-be-config"
                },
                "connection_secrets": {
                    "secret_id": "throwaway-secret-id",
                    "secret_key": "throwaway-secret-key",
                    "security_token": "throwaway-security-token"
                }
            }
        }))
        .unwrap();
        assert_eq!(connection.protocol, "cos");
        assert_eq!(connection.secret_id, "throwaway-secret-id");
        assert_eq!(connection.secret_key, "throwaway-secret-key");
        assert_eq!(connection.security_token, "throwaway-security-token");
        assert_eq!(connection.secret_access_key, "");
    }

    #[test]
    fn parses_webdav_with_secret_password_and_defaults() {
        let connection = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "conn-2",
                "external_config": {
                    "protocol": "webdav",
                    "endpoint": "https://dav.example.com",
                    "username": "alice"
                },
                "connection_secrets": { "password": "wonderland" }
            }
        }))
        .unwrap();
        assert_eq!(connection.protocol, "webdav");
        assert_eq!(connection.password, "wonderland");
        assert_eq!(connection.timeout_secs, 30, "default timeout");
        assert!(!connection.read_only);
        assert!(connection.allow_delete);
        assert_eq!(connection.runtime_port, 0, "runtime absent");
    }

    #[test]
    fn read_only_flags_from_host_and_form_force_read_only_gate() {
        // 宿主标准 read_only（ConnectionConfig.read_only，通用连接设置）→ 只读门禁。
        let host_read_only = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "conn-ro",
                "read_only": true,
                "external_config": { "protocol": "fs", "root": "/data" }
            }
        }))
        .unwrap();
        assert!(
            host_read_only.read_only,
            "host read_only must force the gate"
        );

        // 连接表单 read_only（插件特定配置项）同样生效。
        let form_read_only = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "conn-form",
                "external_config": { "protocol": "fs", "root": "/data", "read_only": true }
            }
        }))
        .unwrap();
        assert!(
            form_read_only.read_only,
            "form read_only must force the gate"
        );

        // 表单可写 + 宿主可写 → 门禁不误伤。
        let writable = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "conn-rw",
                "read_only": false,
                "external_config": { "protocol": "fs", "root": "/data" }
            }
        }))
        .unwrap();
        assert!(!writable.read_only);
    }

    /// Boolean form fields tolerate the quoted variants LLM-built inline
    /// connections emit (`"true"`/`"false"`); an unparseable string keeps the
    /// field default instead of silently flipping the caller's intent (a
    /// string-typed `read_only: "true"` must never yield a writable gate).
    #[test]
    fn bool_fields_tolerate_string_variants() {
        let quoted = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "conn-quoted",
                "external_config": {
                    "protocol": "fs",
                    "read_only": "true",
                    "allow_delete": "false",
                    "lock_to_root": "yes",
                }
            }
        }))
        .unwrap();
        assert!(quoted.read_only, "quoted read_only must force the gate");
        assert!(!quoted.allow_delete, "quoted allow_delete=false must bind");
        assert!(quoted.lock_to_root);

        let junk = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "conn-junk",
                "external_config": { "protocol": "fs", "allow_delete": "maybe" }
            }
        }))
        .unwrap();
        assert!(junk.allow_delete, "unparseable string keeps the default");
    }

    #[test]
    fn parses_custom_config_from_object_and_string() {
        let from_object = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "c",
                "external_config": {
                    "protocol": "opendal-custom",
                    "service": "memory",
                    "config": { "root": "/x" }
                }
            }
        }))
        .unwrap();
        assert_eq!(from_object.custom_config["root"], "/x");

        let from_string = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "c",
                "external_config": {
                    "protocol": "opendal-custom",
                    "service": "memory",
                    "config": "{\"root\":\"/y\"}"
                }
            }
        }))
        .unwrap();
        assert_eq!(from_string.custom_config["root"], "/y");

        let invalid = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "c",
                "external_config": {
                    "protocol": "opendal-custom",
                    "service": "memory",
                    "config": "not json"
                }
            }
        }));
        assert!(invalid.is_err());
    }

    #[test]
    fn rejects_unknown_protocol_and_missing_id() {
        let unknown = StoredConnection::from_lifecycle_params(&json!({
            "connection": { "id": "c", "external_config": { "protocol": "gopher" } }
        }))
        .unwrap_err();
        assert!(unknown.contains("Unsupported protocol"), "{unknown}");

        let no_id = StoredConnection::from_lifecycle_params(&json!({
            "connection": { "external_config": { "protocol": "fs" } }
        }))
        .unwrap_err();
        assert!(no_id.contains("id"), "{no_id}");

        let no_protocol = StoredConnection::from_lifecycle_params(&json!({
            "connection": { "id": "c" }
        }))
        .unwrap_err();
        assert!(no_protocol.contains("protocol"), "{no_protocol}");
    }

    #[test]
    fn ignores_inactive_custom_config_and_unimplemented_legacy_tunnel_fields() {
        let connection = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "c",
                "external_config": {
                    "protocol": "fs",
                    "config": "unfinished JSON {",
                    "connection_mode": "via-dbx-ssh",
                    "dbx_ssh_connection": ""
                }
            }
        }))
        .unwrap();
        assert_eq!(connection.custom_config, json!({}));
    }

    #[test]
    fn secret_key_migrates_without_losing_legacy_keys_or_password_whitespace() {
        let mut params = json!({
            "connection": {
                "id": "c",
                "external_config": {
                    "protocol": "sftp-native",
                    "key": "/legacy/key"
                },
                "connection_secrets": { "password": "  meaningful spaces  " }
            }
        });
        let legacy = StoredConnection::from_lifecycle_params(&params).unwrap();
        assert_eq!(legacy.key, "/legacy/key");
        assert_eq!(legacy.password, "  meaningful spaces  ");
        params["connection"]["connection_secrets"]["key"] = json!("PEM\nkey\n");
        assert_eq!(
            StoredConnection::from_lifecycle_params(&params)
                .unwrap()
                .key,
            "PEM\nkey\n"
        );
        params["connection"]["connection_secrets"]["key"] = json!("");
        assert_eq!(
            StoredConnection::from_lifecycle_params(&params)
                .unwrap()
                .key,
            ""
        );
    }

    #[test]
    fn parses_smb_with_share_domain_and_secret_password() {
        let connection = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "conn-smb",
                "external_config": {
                    "protocol": "smb",
                    "endpoint": "nas.local:445",
                    "share": "media",
                    "username": "alice",
                    "domain": "WORKGROUP",
                    "root": "/archive"
                },
                "connection_secrets": { "password": "wonderland" }
            }
        }))
        .unwrap();
        assert_eq!(connection.protocol, "smb");
        assert_eq!(connection.endpoint, "nas.local:445");
        assert_eq!(connection.share, "media");
        assert_eq!(connection.username, "alice");
        assert_eq!(connection.domain, "WORKGROUP");
        assert_eq!(
            connection.password, "wonderland",
            "secret stays memory-only"
        );
        assert_eq!(connection.root, "/archive");
        // share/domain default to empty when the form omits them.
        let minimal = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "conn-smb2",
                "external_config": { "protocol": "smb" }
            }
        }))
        .unwrap();
        assert_eq!(minimal.share, "");
        assert_eq!(minimal.domain, "");
    }

    #[test]
    fn entry_serializes_camel_case_with_optional_fields() {
        let entry = FileEntry {
            name: "a.txt".into(),
            path: "/data/a.txt".into(),
            kind: "file",
            size: Some(3),
            modified_at: Some(1_700_000_000_000),
        };
        let text = serde_json::to_string(&entry).unwrap();
        assert!(text.contains("\"modifiedAt\":1700000000000"), "{text}");
        assert!(text.contains("\"path\":\"/data/a.txt\""), "{text}");

        let sparse = FileEntry {
            name: "d".into(),
            path: "/d".into(),
            kind: "dir",
            size: None,
            modified_at: None,
        };
        let text = serde_json::to_string(&sparse).unwrap();
        assert!(!text.contains("size"), "{text}");
    }

    #[test]
    fn connection_id_param_requires_value() {
        assert!(connection_id_param(&json!({})).is_err());
        assert!(connection_id_param(&json!({ "connectionId": "" })).is_err());
        assert_eq!(
            connection_id_param(&json!({ "connectionId": "c1" })).unwrap(),
            "c1"
        );
    }

    #[test]
    fn redact_masks_nested_secret_keys() {
        let value = json!({
            "external_config": {
                "endpoint": "http://ok",
                "private_key": "BEGIN RSA",
                "nested": { "access_token": "t0p" }
            },
            "connection_secrets": { "password": "hunter2" },
            "keep": [1, { "secret_access_key": "k" }]
        });
        let masked = redact(&value);
        assert_eq!(masked["external_config"]["endpoint"], "http://ok");
        assert_eq!(masked["external_config"]["private_key"], "***");
        assert_eq!(masked["external_config"]["nested"]["access_token"], "***");
        // The whole secrets object is masked (its key matches the fragment
        // list), which is the conservative behavior we want.
        assert_eq!(masked["connection_secrets"], "***");
        assert_eq!(masked["keep"][1]["secret_access_key"], "***");
        // The original must remain untouched.
        assert_eq!(value["connection_secrets"]["password"], "hunter2");
    }

    // -- manifest contract（connection-provider 字段显隐/必填搭配）-------------

    /// Loads `../manifest.json` relative to the crate (backend/ → plugin root).
    fn manifest_value() -> Value {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../manifest.json");
        let raw = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("manifest.json readable at {path}: {error}"));
        serde_json::from_str(&raw).expect("manifest.json parses as JSON")
    }

    fn connection_provider_fields<'a>(manifest: &'a Value) -> Vec<&'a Value> {
        manifest["contributions"]
            .as_array()
            .expect("manifest.contributions array")
            .iter()
            .find(|item| item["type"] == "connection-provider")
            .expect("connection-provider contribution")["fields"]
            .as_array()
            .expect("fields array")
            .iter()
            .collect()
    }

    #[test]
    fn manifest_connection_fields_are_self_consistent() {
        let manifest = manifest_value();
        let fields = connection_provider_fields(&manifest);

        // Keys are unique and bindings are one of the host-known kinds.
        let mut keys: Vec<&str> = fields
            .iter()
            .map(|field| field["key"].as_str().expect("field key"))
            .collect();
        let key_count = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), key_count, "duplicate manifest field keys");

        let field = |name: &str| {
            fields
                .iter()
                .find(|field| field["key"] == name)
                .unwrap_or_else(|| panic!("manifest field '{name}' missing"))
        };
        for item in &fields {
            let binding = item["binding"].as_str().expect("field binding");
            assert!(
                matches!(binding, "name" | "config" | "secret"),
                "unknown binding '{binding}' on {}",
                item["key"]
            );
            if binding == "secret" {
                assert!(
                    matches!(item["type"].as_str(), Some("password" | "textarea")),
                    "secret-bound field {} must support secret input",
                    item["key"]
                );
            }
        }

        // The protocol select mirrors PROTOCOLS exactly, so a host form can
        // never offer a value the sidecar engine would reject.
        let protocol_options: Vec<&str> = field("protocol")["options"]
            .as_array()
            .expect("protocol options")
            .iter()
            .map(|option| option["value"].as_str().expect("option value"))
            .collect();
        let mut sorted_options = protocol_options.clone();
        sorted_options.sort();
        let mut sorted_protocols = PROTOCOLS.to_vec();
        sorted_protocols.sort_unstable();
        assert!(
            sorted_options.iter().all(|option| sorted_protocols.contains(option)),
            "manifest protocol options must be understood by PROTOCOLS"
        );

        // visible_when references existing fields and only offers values the
        // referenced field actually accepts (its options, when defined).
        for item in &fields {
            let Some(visible_when) = item.get("visible_when") else {
                continue;
            };
            let target = visible_when["field"].as_str().expect("visible_when.field");
            assert!(
                keys.contains(&target),
                "visible_when.field '{target}' unknown"
            );
            let one_of = visible_when["one_of"]
                .as_array()
                .expect("visible_when.one_of");
            assert!(
                !one_of.is_empty(),
                "visible_when.one_of empty on {}",
                item["key"]
            );
            let allowed: Option<Vec<&str>> = field(target)["options"].as_array().map(|options| {
                options
                    .iter()
                    .map(|option| option["value"].as_str().expect("option value"))
                    .collect()
            });
            for value in one_of {
                let value = value.as_str().expect("one_of value");
                if let Some(allowed) = &allowed {
                    assert!(
                        allowed.contains(&value),
                        "{} visible_when offers '{value}' which {} does not accept",
                        item["key"],
                        target
                    );
                }
            }
        }

        // Protocol-gated field matrix: each protocol only surfaces its own
        // fields, global fields stay ungated.
        let expects: &[(&str, &[&str])] = &[
            ("bucket", &["s3", "oss", "cos", "b2", "tos", "upyun"]),
            ("region", &["s3", "tos"]),
            ("access_key_id", &["s3", "oss", "tos"]),
            ("secret_access_key", &["s3", "oss", "tos"]),
            ("enable_virtual_host_style", &["s3"]),
            (
                "endpoint",
                &[
                    "s3", "oss", "cos", "webdav", "ftp", "sftp", "smb", "sftp-native",
                    "alluxio", "azdls", "azfile", "dbfs", "ghac", "http", "ipfs", "ipmfs",
                    "koofr", "lakefs", "pcloud", "seafile", "swift", "tos", "webhdfs",
                ],
            ),
            ("username", &["webdav", "smb", "http", "lakefs", "pcloud", "seafile"]),
            ("user", &["ftp", "sftp", "sftp-native", "hdfs"]),
            ("share", &["smb"]),
            ("domain", &["smb"]),
            // password deliberately excludes `sftp`: the OpenDAL sftp service
            // is key-only (the backend never forwards a password), password
            // accounts belong to `sftp-native`.
            (
                "password",
                &[
                    "webdav", "ftp", "smb", "sftp-native", "http", "koofr", "lakefs", "pcloud", "seafile", "upyun",
                ],
            ),
            ("key", &["sftp", "sftp-native"]),
            ("known_hosts_strategy", &["sftp", "sftp-native"]),
            ("service", &["opendal-custom"]),
            ("config", &["opendal-custom"]),
            ("secret_id", &["cos"]),
            ("secret_key", &["cos"]),
            ("security_token", &["cos", "tos"]),
        ];
        for (name, protocols) in expects {
            let one_of: Vec<&str> = field(name)["visible_when"]["one_of"]
                .as_array()
                .unwrap_or_else(|| panic!("field '{name}' must be protocol-gated"))
                .iter()
                .map(|value| value.as_str().expect("one_of value"))
                .collect();
            assert_eq!(&one_of, protocols, "visible_when mismatch on '{name}'");
        }
        for name in [
            "display_name",
            "protocol",
            "root",
            "lock_to_root",
            "read_only",
            "timeout_secs",
        ] {
            assert!(
                field(name).get("visible_when").is_none(),
                "global field '{name}' must not be protocol-gated"
            );
        }

        assert_eq!(
            field("allow_delete")["visible_when"],
            json!({"field":"read_only", "one_of":["false"]})
        );
        assert_eq!(field("key")["binding"], "secret");
        assert!(!keys.contains(&"connection_mode"));
        assert!(!keys.contains(&"dbx_ssh_connection"));
    }

    #[test]
    fn manifest_required_fields_have_validation_worthy_shape() {
        let manifest = manifest_value();
        let fields = connection_provider_fields(&manifest);
        // required is only meaningful on user-facing inputs; anything marked
        // required must be a text/password/select field with a binding.
        for item in &fields {
            if item.get("required").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let key = item["key"].as_str().expect("field key");
            assert!(
                matches!(item["type"].as_str(), Some("text" | "password" | "select")),
                "required field '{key}' has non-input type {}",
                item["type"]
            );
        }
        // Connection identity stays statically required. Protocol-specific
        // essentials must be conditionally required: the host validates static
        // `required` unconditionally and never evaluates `visible_when`, so a
        // static `required` on an s3/smb-only field would reject every
        // fs/webdav/ftp/sftp connection with "Plugin connection field '…' is
        // required". `required_when` keeps the form-level enforcement on the
        // matching protocol; the engine builders remain the runtime
        // enforcement point with their own clear errors. SMB share is
        // intentionally optional because an empty share opens server-level
        // discovery.
        let field_of = |name: &str| {
            fields
                .iter()
                .find(|item| item["key"] == name)
                .unwrap_or_else(|| panic!("manifest field '{name}' missing"))
        };
        let required = |name: &str| {
            field_of(name)
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        assert!(required("display_name"));
        assert!(required("protocol"));

        let conditionally_required = ["bucket", "access_key_id", "secret_access_key", "service"];
        for key in conditionally_required {
            let item = field_of(key);
            assert!(
                !required(key),
                "field '{key}' must not be statically required: the host cannot scope static required to visible_when and would block every other protocol"
            );
            let visible_when = item
                .get("visible_when")
                .unwrap_or_else(|| panic!("field '{key}' lacks visible_when"));
            let required_when = item.get("required_when").unwrap_or_else(|| {
                panic!("field '{key}' must pair visible_when with required_when")
            });
            let visible_values = visible_when["one_of"]
                .as_array()
                .expect("visible_when.one_of");
            let required_values = required_when["one_of"]
                .as_array()
                .expect("required_when.one_of");
            assert!(
                required_values.iter().all(|value| visible_values.contains(value)),
                "field '{key}' required_when must stay inside its visible_when"
            );
        }

        // Every conditional requirement must be satisfiable by a visible
        // input. Global fields such as root may omit visible_when because
        // they are visible for every protocol.
        for item in fields.iter() {
            let Some(required_when) = item.get("required_when") else { continue };
            let Some(visible_when) = item.get("visible_when") else { continue };
            let visible_values = visible_when["one_of"].as_array().expect("visible_when.one_of");
            for value in required_when["one_of"].as_array().expect("required_when.one_of") {
                assert!(visible_values.contains(value), "required field {} is hidden", item["key"]);
            }
        }
        let share = field_of("share");
        assert!(!required("share"));
        assert!(share.get("required_when").is_none());

        // endpoint is conditionally required on a SUBSET of its visible
        // protocols: oss/webdav/ftp/sftp/smb/sftp-native need it (the OSS
        // service has no default endpoint), s3 stays optional (the AWS
        // default endpoint applies when unset). The host evaluates
        // required_when independently of visible_when, so the subset must
        // never leave the required scope hidden — asserted here by keeping
        // it inside the endpoint visible list.
        let endpoint = field_of("endpoint");
        assert!(
            required("endpoint") == false,
            "endpoint must not be statically required"
        );
        let endpoint_visible: Vec<&str> = endpoint["visible_when"]["one_of"]
            .as_array()
            .expect("endpoint visible_when")
            .iter()
            .map(|value| value.as_str().expect("one_of value"))
            .collect();
        let endpoint_required: Vec<&str> = endpoint
            .get("required_when")
            .expect("endpoint must carry required_when")
            .get("one_of")
            .and_then(Value::as_array)
            .expect("required_when.one_of")
            .iter()
            .map(|value| value.as_str().expect("one_of value"))
            .collect();
        assert!(
            endpoint_required
                .iter()
                .all(|value| endpoint_visible.contains(value)),
            "endpoint required_when must stay inside its visible_when"
        );
        for required_protocol in ["oss", "webdav", "ftp", "sftp", "smb", "sftp-native"] {
            assert!(
                endpoint_required.contains(&required_protocol),
                "endpoint must be conditionally required for '{required_protocol}'"
            );
        }
        assert!(
            !endpoint_required.contains(&"s3"),
            "endpoint stays optional for s3 (AWS default endpoint)"
        );
    }

    #[test]
    fn manifest_localizations_cover_the_same_field_key_sets() {
        let manifest = manifest_value();
        let en_keys: Vec<&str> = connection_provider_fields(&manifest)
            .iter()
            .map(|field| field["key"].as_str().expect("field key"))
            .collect();
        let localizations = manifest["localizations"]
            .as_object()
            .expect("localizations object");
        assert!(
            localizations.len() >= 7,
            "seven locales expected, got {}",
            localizations.len()
        );
        for (locale, body) in localizations {
            let localized = body["contributions"]["io.dbx.files.connection"]["fields"]
                .as_object()
                .unwrap_or_else(|| panic!("locale {locale} lacks connection field localizations"));
            // Newly added service fields may use the English manifest label
            // until translated strings land; reject only orphaned locale
            // entries so localized metadata can never point at a removed
            // field.
            for key in localized.keys() {
                assert!(
                    en_keys.contains(&key.as_str()),
                    "locale {locale} localizes unknown field '{key}'"
                );
            }
        }
    }
}
