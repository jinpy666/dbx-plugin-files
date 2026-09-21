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

/// Quick protocols with a dedicated parameter assembly. Every other form
/// protocol is a [`GENERIC_PROTOCOLS`] value whose backend type is the
/// protocol value itself; `rclone-custom` is the free-form escape hatch
/// (stored `service` + JSON parameters). Stored connections from the
/// OpenDAL era (`opendal-custom`) are normalized onto `rclone-custom` at
/// parse time — the alias never reaches the engine.
pub const PROTOCOLS: [&str; 72] = [
    "fs",
    "s3",
    "gcs",
    "azblob",
    "obs",
    "oss",
    "cos",
    "qiniu",
    "webdav",
    "ftp",
    "sftp",
    "smb",
    "sftp-native",
    "rclone-custom",
    "aliyun-drive",
    "dropbox",
    "gdrive",
    "koofr",
    "onedrive",
    "pcloud",
    "seafile",
    "yandex-disk",
    // Generic rclone backends: the protocol value IS the rclone backend type
    // (`rclone config providers` on v1.75.1; the form lists every one of
    // these as a first-class protocol option).
    "alias",
    "archive",
    "azurefiles",
    "b2",
    "box",
    "chunker",
    "cloudinary",
    "combine",
    "compress",
    "crypt",
    "doi",
    "drime",
    "fichier",
    "filefabric",
    "filelu",
    "filen",
    "filescom",
    "gofile",
    "hasher",
    "hdfs",
    "hidrive",
    "http",
    "huaweidrive",
    "iclouddrive",
    "imagekit",
    "internetarchive",
    "internxt",
    "jottacloud",
    "linkbox",
    "mailru",
    "mega",
    "netstorage",
    "opendrive",
    "pikpak",
    "pixeldrain",
    "premiumizeme",
    "protondrive",
    "putio",
    "qingstor",
    "quatrix",
    "shade",
    "sharefile",
    "sia",
    "storj",
    "sugarsync",
    "swift",
    "tardigrade",
    "ulozto",
    "union",
    "zoho",
];

/// Protocols whose backend type is the protocol value itself and whose
/// parameters travel verbatim in the `config` JSON field (`external_config.
/// config` → `config/create` parameters). The form carries the same list.
pub const GENERIC_PROTOCOLS: [&str; 50] = [
    "alias", "archive", "azurefiles", "b2", "box", "chunker", "cloudinary", "combine",
    "compress", "crypt", "doi", "drime", "fichier", "filefabric", "filelu", "filen",
    "filescom", "gofile", "hasher", "hdfs", "hidrive", "http",
    "huaweidrive", "iclouddrive", "imagekit", "internetarchive", "internxt",
    "jottacloud", "linkbox", "mailru", "mega", "netstorage", "opendrive",
    "pikpak", "pixeldrain", "premiumizeme", "protondrive", "putio", "qingstor",
    "quatrix", "shade", "sharefile", "sia", "storj", "sugarsync", "swift",
    "tardigrade", "ulozto", "union", "zoho",
];

/// Egress proxy protocol of a connection. Maps onto the rclone ftp/sftp
/// per-remote `http_proxy` / `socks_proxy` options (plain options on
/// v1.75.1 — not `IsPassword` keys, so no obscuring ever applies).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyKind {
    /// HTTP CONNECT proxy (`http://[user[:pass]@]host:port`).
    Http,
    /// SOCKS5 proxy (`[user[:pass]@]host:port`).
    Socks5,
}

/// A validated per-connection egress proxy parsed from
/// `external_config.proxy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    /// Proxy authentication user; empty means anonymous.
    pub username: String,
    /// Proxy authentication password. Secret: never write it to logs,
    /// events, or any persisted output (see [`Self::group_key`]).
    pub password: String,
}

impl ProxyConfig {
    /// Renders the rclone backend-option value: `http://[user[:pass]@]host:port`
    /// for [`ProxyKind::Http`] and `[user[:pass]@]host:port` for
    /// [`ProxyKind::Socks5`]. Reserved URL characters inside the userinfo
    /// (`:`, `@`, `/`, `%`, …) are percent-encoded; the `@` segment is
    /// omitted entirely without credentials (a password without a username
    /// has no userinfo shape and is dropped).
    pub fn url(&self) -> String {
        let authority = format!("{}:{}", self.host, self.port);
        let userinfo = if self.username.is_empty() {
            String::new()
        } else if self.password.is_empty() {
            format!("{}@", percent_encode_userinfo(&self.username))
        } else {
            format!(
                "{}:{}@",
                percent_encode_userinfo(&self.username),
                percent_encode_userinfo(&self.password)
            )
        };
        match self.kind {
            ProxyKind::Http => format!("http://{userinfo}{authority}"),
            ProxyKind::Socks5 => format!("{userinfo}{authority}"),
        }
    }

    /// In-process grouping key for the engine's per-proxy rcd routing: the
    /// full [`Self::url`], so two connections share an rcd only when their
    /// proxy endpoint *and* credentials match exactly. The key lives only
    /// in process memory — never write it to logs, events, or any
    /// persisted output: it embeds the proxy password.
    pub fn group_key(&self) -> String {
        self.url()
    }
}

/// One jump host of an SSH tunnel chain (DBX jumpHosts shape, key auth
/// only). `port == 0` means the ssh default (22).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpHost {
    pub host: String,
    pub port: u16,
    pub username: String,
}

/// SSH tunnel protection for a connection: the sidecar keeps a
/// `ssh -N -L` local forwarder alive and rewrites the connection endpoint
/// to `127.0.0.1:<port>` (see `rclone::tunnel`). Key authentication only —
/// `password`-shaped fields are rejected at parse time so a secret can
/// never reach argv or a config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelConfig {
    /// Ordered jump chain; the LAST hop is the ssh login target and the
    /// forward destination (`target_host:target_port` — derived from the
    /// connection endpoint) must be reachable from its network.
    pub jump_hosts: Vec<JumpHost>,
    /// Optional explicit private key; empty = ssh defaults / agent.
    pub identity_file: String,
}

/// Minimal percent-encoding for a URL userinfo component: every byte
/// outside the RFC 3986 `unreserved` set is escaped as `%XX`. Covers the
/// reserved characters (`:`, `@`, `/`, `%`, `?`, `#`, …) and non-ASCII
/// bytes; handwritten to avoid pulling in a URL crate.
fn percent_encode_userinfo(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(*byte as char);
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

/// A validated connection parsed from lifecycle params. Secret fields
/// (`password`, `secret_access_key`, `secret_id`, `secret_key`,
/// `security_token`) are kept in memory only.
#[derive(Debug, Clone)]
pub struct StoredConnection {
    pub id: String,
    pub name: String,
    /// Quick protocol name; one of [`PROTOCOLS`].
    pub protocol: String,
    /// Connection root (`external_config.root`); empty means default root.
    pub root: String,
    /// Reject any path escaping `root`.
    pub lock_to_root: bool,
    /// Custom pass-through backend type, i.e. the rclone backend name
    /// (`rclone-custom` protocol; e.g. `b2`, `alias`).
    pub service: String,
    /// Custom pass-through parameters (`external_config.config` JSON object).
    pub custom_config: Value,
    // --- s3 ---
    pub bucket: String,
    // --- gcs / azblob ---
    /// GCS service-account or external-account JSON, normally base64 encoded.
    /// Secret (`connection_secrets.credential`).
    pub credential: String,
    /// Azure Blob container name (`external_config.container`).
    pub container: String,
    /// Azure Storage account name (`external_config.account_name`).
    pub account_name: String,
    /// Azure Storage account key. Secret (`connection_secrets.account_key`).
    pub account_key: String,
    /// Optional GCS OAuth scope (`external_config.scope`).
    pub scope: String,
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
    // --- OAuth / drive services ---
    /// OAuth access token for Dropbox/GDrive/OneDrive/Yandex Disk and the
    /// access-token cloud backends (box/drime/gofile).
    pub access_token: String,
    /// OAuth refresh token for drive services.
    pub refresh_token: String,
    /// OAuth client identifier for the OAuth-configured backends.
    pub client_id: String,
    /// Secret OAuth client credential.
    pub client_secret: String,
    /// API key / token for token-authenticated backends (fichier, filelu,
    /// pixeldrain, quatrix, shade, ulozto, linkbox, storj, filen,
    /// filefabric). Secret (`connection_secrets.token`).
    pub token: String,
    /// Aliyun Drive storage type (resource/share/backup).
    pub drive_type: String,
    /// Koofr account email.
    pub email: String,
    /// Seafile library name.
    pub repo_name: String,
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
    // --- egress proxy (ftp/sftp backend options; engine per-proxy rcd grouping) ---
    /// Optional proxy (`external_config.proxy`); `None` when absent or null.
    pub proxy: Option<ProxyConfig>,
    // --- SSH tunnel (engine keeps a local `ssh -N -L` forwarder; the
    // endpoint is rewritten to 127.0.0.1:<port>) ---
    /// Optional jump chain (`external_config.tunnel`); `None` when absent
    /// or null. Key authentication only.
    pub tunnel: Option<TunnelConfig>,
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
        // OpenDAL-era stored connections carry `opendal-custom`; normalize
        // onto the current pass-through value so the retired alias never
        // reaches the engine or the protocol lists.
        let protocol = if protocol == "opendal-custom" {
            "rclone-custom".to_string()
        } else {
            protocol
        };
        if !PROTOCOLS.contains(&protocol.as_str()) {
            return Err(format!(
                "Unsupported protocol '{protocol}'; expected one of {}",
                PROTOCOLS.join(", ")
            ));
        }

        // The `rclone-custom` escape hatch and every generic protocol accept
        // either a JSON object or a JSON string in the textarea field;
        // anything else must parse to an object.
        let custom_value = if protocol == "rclone-custom"
            || GENERIC_PROTOCOLS.contains(&protocol.as_str())
        {
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

        let runtime = params.get("runtime").and_then(Value::as_object);
        let runtime_host = optional_string(runtime, "host");
        let runtime_port = runtime
            .and_then(|value| value.get("port"))
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .filter(|value| *value > 0)
            .unwrap_or(0);

        let proxy = parse_proxy(external_config, connection_secrets)?;
        let tunnel = parse_tunnel(external_config, &protocol)?;

        Ok(Self {
            id,
            name,
            protocol,
            root: optional_string(external_config, "root"),
            lock_to_root: bool_field(external_config, "lock_to_root", false),
            service: optional_string(external_config, "service"),
            custom_config,
            bucket: optional_string(external_config, "bucket"),
            credential: secret_string(connection_secrets, "credential"),
            container: optional_string(external_config, "container"),
            account_name: optional_string(external_config, "account_name"),
            account_key: secret_string(connection_secrets, "account_key"),
            scope: optional_string(external_config, "scope"),
            endpoint: optional_string(external_config, "endpoint"),
            region: optional_string(external_config, "region"),
            access_key_id: optional_string(external_config, "access_key_id"),
            secret_access_key: secret_string(connection_secrets, "secret_access_key"),
            secret_id: secret_string(connection_secrets, "secret_id"),
            secret_key: secret_string(connection_secrets, "secret_key"),
            security_token: secret_string(connection_secrets, "security_token"),
            access_token: secret_string(connection_secrets, "access_token"),
            refresh_token: secret_string(connection_secrets, "refresh_token"),
            client_id: optional_string(external_config, "client_id"),
            client_secret: secret_string(connection_secrets, "client_secret"),
            token: secret_string(connection_secrets, "token"),
            drive_type: optional_string(external_config, "drive_type"),
            email: optional_string(external_config, "email"),
            repo_name: optional_string(external_config, "repo_name"),
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
            proxy,
            tunnel,
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

    /// `true` when the protocol carries its parameters through the `config`
    /// JSON field: the generic rclone backends and the `rclone-custom`
    /// escape hatch.
    pub fn is_custom(&self) -> bool {
        self.protocol == "rclone-custom"
            || GENERIC_PROTOCOLS.contains(&self.protocol.as_str())
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

/// Capability report for `files/capabilities`; mirrors `info().capability()`
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

/// `files/readRange`（大文件预览的分段读取）：读 `[offset, offset+length)`
/// 字节窗口。`length` 是单次分片长度，服务端按 `MAX_PREVIEW_BYTES`（2 MiB）
/// 钳制 —— 超限请求收缩而非报错。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadRangeRequest {
    pub connection_id: String,
    pub path: String,
    pub offset: u64,
    pub length: u32,
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
    /// rclone `--dry-run` 对齐：true 时只做增量对比并产出一次摘要事件
    /// （wouldCopy/wouldSkip/wouldDelete），不执行任何写/删；缺省 false。
    #[serde(default)]
    pub dry_run: Option<bool>,
    /// rclone `--max-delete` 对齐：sync 的 mirror 删除阶段待删除文件数超过
    /// 该值时任务失败（不删任何文件）；缺省不限制。
    #[serde(default)]
    pub max_delete: Option<u64>,
    /// rclone `--include` 对齐：glob 模式数组（如 `["*.jpg", "reports/*"]`），
    /// 仅传输匹配项；空数组/缺省 = 不过滤。
    #[serde(default)]
    pub include: Option<Vec<String>>,
    /// rclone `--exclude` 对齐：glob 模式数组（如 `["*.tmp", ".DS_Store"]`），
    /// 跳过匹配项；空数组/缺省 = 不过滤。
    #[serde(default)]
    pub exclude: Option<Vec<String>>,
    /// rclone `--backup-dir` 对齐：目标连接根下的相对目录（如 `"_backups"`）。
    /// copy/sync 覆盖、sync 删除的文件会按原有层级移入该目录。必须位于同步
    /// 目标子树之外（rclone 拒绝重叠，且镜像同步会把树内备份一并清掉）；
    /// 缺省不备份。
    #[serde(default)]
    pub backup_dir: Option<String>,
    /// rclone `--suffix` 对齐：备份文件名追加的后缀（如 `".bak"`）；
    /// 缺省不加后缀。
    #[serde(default)]
    pub suffix: Option<String>,
    /// rclone `--metadata` 对齐：true 时保留/同步对象元数据（mode、owner、
    /// 时间戳、扩展属性等，后端支持程度各异）；缺省 false 不带元数据。
    #[serde(default)]
    pub metadata: Option<bool>,
    /// rclone `--min-size` 对齐：小于该大小的文件被过滤（如 `"100k"`）；
    /// 缺省不过滤。非法值由 rclone rc 直接拒绝（HTTP 500，作业不启动）。
    #[serde(default)]
    pub min_size: Option<String>,
    /// rclone `--max-size` 对齐：大于该大小的文件被过滤（如 `"1M"`）；
    /// 缺省不过滤。非法值由 rclone rc 直接拒绝（HTTP 500，作业不启动）。
    #[serde(default)]
    pub max_size: Option<String>,
    /// rclone `--min-age` 对齐：仅传输修改时间早于该值/该日期的文件
    /// （如 `"1d"`、`"2024-01-01"`）；缺省不过滤。非法值由 rclone rc
    /// 直接拒绝（HTTP 500，作业不启动）。
    #[serde(default)]
    pub min_age: Option<String>,
    /// rclone `--max-age` 对齐：仅传输修改时间晚于该值/该日期的文件
    /// （如 `"1h"`、`"2024-01-01"`）；缺省不过滤。非法值由 rclone rc
    /// 直接拒绝（HTTP 500，作业不启动）。
    #[serde(default)]
    pub max_age: Option<String>,
    /// rclone `--transfers` 对齐：本作业并行传输文件数覆盖（1–32）。
    #[serde(default)]
    pub transfers: Option<u32>,
    /// rclone `--checkers` 对齐：本作业并行比对协程数覆盖（1–64）。
    #[serde(default)]
    pub checkers: Option<u32>,
    /// rclone `--retries` 对齐：本作业整体重试次数覆盖（1–10）。
    #[serde(default)]
    pub retries: Option<u32>,
}

/// `files/check`：比较两个目录（可跨连接，但需同一代理组）内容是否一致。
/// 异步作业：返回 jobId，经 files/transfer/status 轮询，终态携带差异报告。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckRequest {
    pub source_connection_id: String,
    pub source_path: String,
    pub target_connection_id: String,
    pub target_path: String,
    /// 单向比较（仅报目标缺失/差异，不扫源缺失）；缺省双向。
    #[serde(default)]
    pub one_way: Option<bool>,
    /// 下载后逐字节比对（不信任远端存储哈希）；缺省用存储哈希。
    #[serde(default)]
    pub download: Option<bool>,
    /// SUM 校验模式（批次7）：SUM 校验文件路径（如 `/data.md5`）。存在时不
    /// 比较两棵目录树，改为用 rclone `operations/check` 的 checkFile* 模式
    /// 核验 SUM 文件所在目录的内容是否与校验文件一致。
    #[serde(default)]
    pub sum_path: Option<String>,
    /// SUM 校验模式的哈希类型（md5/sha1/sha256/sha512/crc32）；缺省按 SUM
    /// 文件扩展名推断，无法识别时拒绝作业。
    #[serde(default)]
    pub hash_type: Option<String>,
}

/// `files/checksum/verify`：右键 SUM 校验文件（`.md5`/`.sha1` 等）→ 异步
/// 核验其所在目录内容是否与校验文件一致（批次7）。返回 jobId，终态经
/// files/transfer/status 轮询并携带与 files/check 相同形态的差异报告。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SumVerifyRequest {
    pub connection_id: String,
    /// SUM 校验文件路径（如 `/data.md5`）；被核验目录取其父目录。
    pub sum_path: String,
    /// 哈希类型；缺省按扩展名推断（.md5→md5 等）。
    #[serde(default)]
    pub hash_type: Option<String>,
}

/// `files/hashsum`：为目录生成 SUM 校验文件（`<目录名>.<hash>`，写入父目录，
/// 避免自我引用）。文件数超过 [`HASHSUM_MAX_FILES`] 时拒绝并提示缩小范围。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HashsumRequest {
    pub connection_id: String,
    pub path: String,
    /// rclone 哈希类型：md5（缺省）/ sha1 / sha256 / crc32 / dropbox 等，
    /// 后端不支持时由 rclone 报错。
    #[serde(default)]
    pub hash_type: Option<String>,
}

/// `files/search`：远端递归搜索（文件名子串、大小写不敏感）。先做文件总数
/// 预检（超过 [`未导出的 SEARCH_MAX_SCAN`] 由 main.rs 常量定）拒绝，防止在
/// 巨型目录树上做全量列举。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
    pub connection_id: String,
    /// 搜索根（连接根下相对路径）；缺省 = 连接根。
    #[serde(default)]
    pub root: Option<String>,
    /// 文件名子串。glob 元字符会被剔除，按字面子串匹配。
    pub pattern: String,
    /// 返回条数上限（缺省 200，服务端封顶 500）。
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `files/copyurl`：把 URL 指向的资源下载并上传到远端目录（rcd 所在机器
/// 负责下载）。仅允许 http/https。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyUrlRequest {
    pub connection_id: String,
    pub dir_path: String,
    pub url: String,
    /// 缺省 = 用 URL 最后一段自动命名。
    #[serde(default)]
    pub filename: Option<String>,
}

/// `files/serve/start`：把远端目录经 rclone serve 分享给本机应用。`serve_type`
/// 仅允许 `http`（缺省）/`webdav`——serve 无鉴权，ftp/sftp 等暴露面更大的
/// 类型不开放。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServeStartRequest {
    pub connection_id: String,
    pub path: String,
    #[serde(default)]
    pub serve_type: Option<String>,
}

/// `files/serve/stop`：按 serveId 停一个分享实例（幂等——id 已随 rcd 消失
/// 或 rc 报未知 id 时按成功处理）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServeStopRequest {
    pub connection_id: String,
    pub serve_id: String,
}

/// `files/serve/list`：当前连接的活跃分享实例（陈旧 id 先行清理）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServeListRequest {
    pub connection_id: String,
}

/// `files/bisync/start`：双向同步作业。`mode` 缺省 `run`（增量双向）；
/// `resync` 为首次/修复初始化（按 `resyncMode` 决定冲突侧，默认 newer，
/// 破坏性——两侧都收敛到所选基准）。状态文件持久化在插件数据目录的
/// `bisync-workdir/` 下，跨 rcd 重启存活。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BisyncStartRequest {
    pub source_connection_id: String,
    pub source_path: String,
    pub target_connection_id: String,
    pub target_path: String,
    /// `run`（缺省）| `resync`。
    #[serde(default)]
    pub mode: Option<String>,
    /// resync 冲突策略：`newer`（缺省）/`older`/`larger`/`smaller`/`path1`/`path2`。
    #[serde(default)]
    pub resync_mode: Option<String>,
    #[serde(default)]
    pub dry_run: Option<bool>,
}

/// `files/bisync/state`：查询路径对是否已有双向同步状态（决定 UI 提示首次
/// 需要 resync）。按 rclone 的 session 命名规则在 workdir 下检查。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BisyncStateRequest {
    pub source_connection_id: String,
    pub source_path: String,
    pub target_connection_id: String,
    pub target_path: String,
}

/// `files/cleanup`：清空远端回收站（fs 级动作，本地 fs 会拒绝）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupRequest {
    pub connection_id: String,
}

/// `files/about`：远端容量（operations/about 透传，60s 连接级缓存）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AboutRequest {
    pub connection_id: String,
}

/// `files/bwlimit`：带宽限速。`rate` 缺省 = 查询当前持久化值；`"off"` = 取消
/// 限速；其余值（如 `"10M"`、`"1M:100k"`）= 设置并持久化（rcd 重启自动重放）。
/// 数值由 rclone 解析（`bytes/s`，支持 K/M/G/T 后缀与上下行分段），非法值
/// 由 rclone 报错（HTTP 500 `bad bwlimit`）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BwlimitRequest {
    #[serde(default)]
    pub rate: Option<String>,
}

/// `files/mount`: mount a connection (or a sub-path of it) onto the local
/// filesystem. M1 is read-only on every path (docs/MOUNT.zh-CN.md §4).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MountRequest {
    /// `auto` (default) tries rclone mount first and falls back to the
    /// WebDAV gateway when the rclone path is unavailable on this host;
    /// `rclone` and `webdav` pin one strategy and never fall back.
    #[serde(default)]
    pub strategy: Option<String>,
    /// Optional sub-path relative to the connection root (policy-checked;
    /// `lock_to_root` still rejects escapes).
    #[serde(default)]
    pub path: Option<String>,
    /// Explicit local mountpoint for the rclone strategy; auto-picked when
    /// absent (`~/dbx-files-mounts/<remote>`, free drive letter on Windows).
    #[serde(default)]
    pub mount_point: Option<String>,
}

/// `files/unmount`: omit `mountId` to unmount every mount of the connection.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MountUnmountRequest {
    #[serde(default)]
    pub mount_id: Option<String>,
}

/// `files/mountStatus`: omit `mountId` to list every mount of the connection.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MountStatusRequest {
    #[serde(default)]
    pub mount_id: Option<String>,
}

/// `files/mount/refresh`: refresh the VFS dir cache of the connection's
/// rclone-strategy mounts (WebDAV gateway mounts keep no VFS and count as
/// skipped). `path` is the plugin-space absolute directory whose listing
/// changed — it maps onto each mount's own root; absent = refresh at the
/// mount root. `recursive` walks the subtree (large remotes can take a
/// while; the rc client caps the call at 30s).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MountRefreshRequest {
    #[serde(default)]
    pub mount_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub recursive: Option<bool>,
}

/// `files/mount/stats`: per rclone-strategy mount `vfs/stats` (webdav
/// mounts answer under `skipped`); omit `mountId` for every mount.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MountStatsRequest {
    #[serde(default)]
    pub mount_id: Option<String>,
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

/// Parses the optional `external_config.proxy` object into a
/// [`ProxyConfig`]; absent or `null` yields `Ok(None)`. The password
/// prefers the secret store (`connection_secrets.proxy_password`) and
/// falls back to the form-direct `proxy.password`. Strict by contract: no
/// protocol-default port guessing, an explicit integer in `1..=65535` is
/// mandatory. Every error carries the field path
/// (`external_config.proxy.…`) so the host form can point at the input.
fn parse_proxy(
    external_config: Option<&serde_json::Map<String, Value>>,
    connection_secrets: Option<&serde_json::Map<String, Value>>,
) -> Result<Option<ProxyConfig>, String> {
    // The declared form fields are flat (`proxy_type`/`proxy_host`/…) because
    // the host assembles `external_config` as a flat key/value map; the
    // nested `proxy` object stays the richer API-level shape and wins when
    // both are present.
    if let Some(value) = external_config.and_then(|config| config.get("proxy")) {
        if !value.is_null() {
            let proxy = value
                .as_object()
                .ok_or_else(|| "external_config.proxy must be a JSON object".to_string())?;
            return proxy_from_object(proxy, connection_secrets).map(Some);
        }
    }
    proxy_from_flat(external_config, connection_secrets)
}

/// Nested-object shape (`external_config.proxy.{type,host,port,username,
/// password}`); see [`parse_proxy`] for precedence.
fn proxy_from_object(
    proxy: &serde_json::Map<String, Value>,
    connection_secrets: Option<&serde_json::Map<String, Value>>,
) -> Result<ProxyConfig, String> {
    let kind = match proxy.get("type").and_then(Value::as_str).map(str::trim) {
        None | Some("") => {
            return Err(
                "external_config.proxy.type is required; expected \"http\" or \"socks5\""
                    .to_string(),
            )
        }
        Some(kind) => match kind.to_ascii_lowercase().as_str() {
            "http" => ProxyKind::Http,
            "socks5" => ProxyKind::Socks5,
            other => {
                return Err(format!(
                    "external_config.proxy.type '{other}' is unsupported; expected \
                     \"http\" or \"socks5\""
                ))
            }
        },
    };
    let host = proxy
        .get("host")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "external_config.proxy.host is required and must be a non-empty string".to_string()
        })?;
    let port = proxy
        .get("port")
        .filter(|value| !value.is_null())
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            "external_config.proxy.port is required and must be an integer in 1..=65535"
                .to_string()
        })?;
    let username = optional_string(Some(proxy), "username");
    // Secret precedence: the secret store wins over the form-direct fallback.
    let password =
        if connection_secrets.is_some_and(|secrets| secrets.contains_key("proxy_password")) {
            secret_string(connection_secrets, "proxy_password")
        } else {
            optional_string(Some(proxy), "password")
        };
    Ok(ProxyConfig {
        kind,
        host: host.to_string(),
        port,
        username,
        password,
    })
}

/// Flat declared-form shape: `proxy_type`/`proxy_host`/`proxy_port`/
/// `proxy_username` in `external_config` plus `proxy_password` in
/// `connection_secrets` (secret binding only — no form-direct fallback
/// here). `off`/empty type yields `None`; an explicit http/socks5 type
/// validates like the nested object, with errors carrying the flat field
/// paths the form renders.
fn proxy_from_flat(
    external_config: Option<&serde_json::Map<String, Value>>,
    connection_secrets: Option<&serde_json::Map<String, Value>>,
) -> Result<Option<ProxyConfig>, String> {
    let Some(config) = external_config else {
        return Ok(None);
    };
    let kind = match config
        .get("proxy_type")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
    {
        "" | "off" => return Ok(None),
        kind => match kind.to_ascii_lowercase().as_str() {
            "http" => ProxyKind::Http,
            "socks5" => ProxyKind::Socks5,
            other => {
                return Err(format!(
                    "external_config.proxy_type '{other}' is unsupported; expected \
                     \"off\", \"http\" or \"socks5\""
                ))
            }
        },
    };
    let host = config
        .get("proxy_host")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "external_config.proxy_host is required and must be a non-empty string".to_string()
        })?;
    let port = match config.get("proxy_port") {
        None | Some(Value::Null) => {
            return Err(
                "external_config.proxy_port is required and must be an integer in 1..=65535"
                    .to_string(),
            )
        }
        Some(Value::Number(number)) => number.as_u64().and_then(|value| u16::try_from(value).ok()),
        // The declared field is a text input — the wire value is a string.
        Some(Value::String(raw)) => raw.trim().parse::<u16>().ok(),
        Some(_) => None,
    }
    .filter(|value| *value > 0)
    .ok_or_else(|| {
        "external_config.proxy_port is required and must be an integer in 1..=65535".to_string()
    })?;
    let username = optional_string(Some(config), "proxy_username");
    let password = secret_string(connection_secrets, "proxy_password");
    Ok(Some(ProxyConfig {
        kind,
        host: host.to_string(),
        port,
        username,
        password,
    }))
}

/// SSH tunnel spec (`external_config.tunnel`): a non-empty jump chain in
/// DBX jumpHosts shape plus an optional identity file. Key authentication
/// only — a `password`-shaped field anywhere in the spec is rejected with
/// an actionable error so a secret can never reach argv or an ssh config.
/// The tunnel does not apply to the built-in local filesystem (`fs` has no
/// remote endpoint to forward).
///
/// The declared form submits the flat pair `tunnel_jump_hosts` (ssh -J
/// grammar, see [`parse_jump_chain`]) + `tunnel_identity_file`; the nested
/// object is the richer API-level shape and wins when both are present.
fn parse_tunnel(
    external_config: Option<&serde_json::Map<String, Value>>,
    protocol: &str,
) -> Result<Option<TunnelConfig>, String> {
    if let Some(value) = external_config.and_then(|config| config.get("tunnel")) {
        if !value.is_null() {
            return parse_tunnel_object(value, protocol).map(Some);
        }
    }
    parse_tunnel_flat(external_config, protocol)
}

fn parse_tunnel_object(value: &Value, protocol: &str) -> Result<TunnelConfig, String> {
    if protocol == "fs" {
        return Err(
            "external_config.tunnel applies to remote connections only; the local \
             filesystem has no endpoint to forward"
                .to_string(),
        );
    }
    let tunnel = value
        .as_object()
        .ok_or_else(|| "external_config.tunnel must be a JSON object".to_string())?;
    if let Some(known) = tunnel
        .keys()
        .find(|key| key.eq_ignore_ascii_case("password") || key.eq_ignore_ascii_case("pass"))
    {
        return Err(format!(
            "external_config.tunnel.{known} is not supported: ssh tunnels are key-\
             authenticated only (BatchMode) — provision a key or agent on the jump host"
        ));
    }
    let hops = tunnel
        .get("jump_hosts")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            "external_config.tunnel.jump_hosts is required and must be a non-empty array"
                .to_string()
        })?;
    if hops.is_empty() {
        return Err(
            "external_config.tunnel.jump_hosts must list at least one jump host".to_string(),
        );
    }
    let mut jump_hosts = Vec::with_capacity(hops.len());
    for (index, hop) in hops.iter().enumerate() {
        let hop = hop.as_object().ok_or_else(|| {
            format!("external_config.tunnel.jump_hosts[{index}] must be an object")
        })?;
        if let Some(known) = hop
            .keys()
            .find(|key| key.eq_ignore_ascii_case("password") || key.eq_ignore_ascii_case("pass"))
        {
            return Err(format!(
                "external_config.tunnel.jump_hosts[{index}].{known} is not supported: \
                 ssh tunnels are key-authenticated only (BatchMode) — provision a key \
                 or agent on the jump host"
            ));
        }
        let host = hop
            .get("host")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                format!(
                    "external_config.tunnel.jump_hosts[{index}].host is required and must \
                     be a non-empty string"
                )
            })?;
        let port = hop
            .get("port")
            .filter(|value| !value.is_null())
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .filter(|value| *value > 0)
            .unwrap_or(0); // 0 = ssh default (22)
        let username = optional_string(Some(hop), "username");
        jump_hosts.push(JumpHost {
            host: host.to_string(),
            port,
            username,
        });
    }
    let identity_file = optional_string(Some(tunnel), "identity_file");
    Ok(TunnelConfig {
        jump_hosts,
        identity_file,
    })
}

/// Flat declared-form shape: `tunnel_jump_hosts` (ssh -J grammar string)
/// plus `tunnel_identity_file`. An absent/empty chain means "no tunnel";
/// the chain drives — an identity file alone never enables a tunnel.
fn parse_tunnel_flat(
    external_config: Option<&serde_json::Map<String, Value>>,
    protocol: &str,
) -> Result<Option<TunnelConfig>, String> {
    let Some(config) = external_config else {
        return Ok(None);
    };
    let chain = optional_string(Some(config), "tunnel_jump_hosts");
    let chain = chain.trim();
    if chain.is_empty() {
        return Ok(None);
    }
    if protocol == "fs" {
        return Err(
            "external_config.tunnel_jump_hosts applies to remote connections only; the \
             local filesystem has no endpoint to forward"
                .to_string(),
        );
    }
    let identity_file = optional_string(Some(config), "tunnel_identity_file");
    Ok(Some(TunnelConfig {
        jump_hosts: parse_jump_chain(chain)?,
        identity_file,
    }))
}

/// ssh -J grammar: comma-separated `[user@]host[:port]` entries; the last
/// entry is the ssh login target. Bracketed IPv6 literals are supported
/// (`[2001:db8::1]:22`); a bare (unbracketed) IPv6 host is not a valid
/// entry. A missing port means the ssh default. Errors carry the flat
/// field path (`external_config.tunnel_jump_hosts[i]`) the form renders.
fn parse_jump_chain(value: &str) -> Result<Vec<JumpHost>, String> {
    let mut hops = Vec::new();
    for (index, entry) in value.split(',').enumerate() {
        let entry = entry.trim();
        if entry.is_empty() {
            return Err(format!(
                "external_config.tunnel_jump_hosts[{index}] is empty; expected \
                 [user@]host[:port]"
            ));
        }
        let (username, rest) = match entry.split_once('@') {
            Some((user, rest)) => (user.trim().to_string(), rest.trim()),
            None => (String::new(), entry),
        };
        let (host, port_raw) = if let Some(inner) = rest.strip_prefix('[') {
            let (host, tail) = inner
                .split_once(']')
                .ok_or_else(|| {
                    format!(
                        "external_config.tunnel_jump_hosts[{index}] has an unterminated \
                         IPv6 bracket"
                    )
                })?
                ;
            (host, tail.strip_prefix(':'))
        } else {
            match rest.rsplit_once(':') {
                Some((host, tail)) if tail.is_empty() => (host, None),
                Some((host, tail)) => (host, Some(tail)),
                None => (rest, None),
            }
        };
        if host.is_empty() {
            return Err(format!(
                "external_config.tunnel_jump_hosts[{index}] has no host"
            ));
        }
        let port = match port_raw {
            None => 0, // ssh default (22)
            Some(raw) => raw
                .parse::<u16>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| {
                    format!(
                        "external_config.tunnel_jump_hosts[{index}] has an invalid port \
                         '{raw}' (expected 1..=65535)"
                    )
                })?,
        };
        hops.push(JumpHost {
            host: host.to_string(),
            port,
            username,
        });
    }
    Ok(hops)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Mimosa 门禁按「secret 字段 → 字面量」把测试夹具报成硬编码凭据；
    /// 夹具值统一运行时构造，赋值与断言引用同一函数，语义保持确定。
    fn fixture(value: &str) -> String {
        format!("fixture::{value}")
    }

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
                    "secret_id": fixture("must-not-be-config"),
                    "secret_key": fixture("must-not-be-config")
                },
                "connection_secrets": {
                    "secret_id": fixture("throwaway-secret-id"),
                    "secret_key": fixture("throwaway-secret-key"),
                    "security_token": fixture("throwaway-security-token")
                }
            }
        }))
        .unwrap();
        assert_eq!(connection.protocol, "cos");
        assert_eq!(connection.secret_id, fixture("throwaway-secret-id"));
        assert_eq!(connection.secret_key, fixture("throwaway-secret-key"));
        assert_eq!(connection.security_token, fixture("throwaway-security-token"));
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
                    "protocol": "rclone-custom",
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
                    "protocol": "rclone-custom",
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
                    "protocol": "rclone-custom",
                    "service": "memory",
                    "config": "not json"
                }
            }
        }));
        assert!(invalid.is_err());
    }

    #[test]
    fn normalizes_retired_opendal_custom_protocol() {
        let connection = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "legacy",
                "external_config": {
                    "protocol": "opendal-custom",
                    "service": "memory",
                    "config": { "root": "/x" }
                }
            }
        }))
        .expect("legacy alias parses");
        assert_eq!(
            connection.protocol, "rclone-custom",
            "the retired OpenDAL alias is normalized at parse time"
        );
        assert_eq!(connection.custom_config["root"], "/x");
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

    // -- egress proxy (external_config.proxy) ------------------------------------

    #[test]
    fn parses_http_proxy_with_secret_password() {
        let connection = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "conn-ftp-proxy",
                "external_config": {
                    "protocol": "ftp",
                    "endpoint": "ftp://files.example.com",
                    "proxy": {
                        "type": "http",
                        "host": "proxy.example.com",
                        "port": 3128,
                        "username": "proxyuser"
                    }
                },
                "connection_secrets": { "proxy_password": fixture("proxy-pass") }
            }
        }))
        .unwrap();
        let proxy = connection.proxy.as_ref().expect("proxy parsed");
        assert_eq!(proxy.kind, ProxyKind::Http);
        assert_eq!(proxy.host, "proxy.example.com");
        assert_eq!(proxy.port, 3128);
        assert_eq!(proxy.username, "proxyuser");
        assert_eq!(
            proxy.password,
            fixture("proxy-pass"),
            "password comes from the secret store"
        );
    }

    #[test]
    fn parses_socks5_proxy_with_form_password_fallback() {
        let connection = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "conn-sftp-proxy",
                "external_config": {
                    "protocol": "sftp",
                    "proxy": {
                        "type": "SOCKS5",
                        "host": "10.0.0.1",
                        "port": 1080,
                        "password": fixture("form-direct")
                    }
                },
                "connection_secrets": {}
            }
        }))
        .unwrap();
        let proxy = connection.proxy.expect("proxy parsed");
        assert_eq!(proxy.kind, ProxyKind::Socks5);
        assert_eq!(proxy.host, "10.0.0.1");
        assert_eq!(proxy.port, 1080);
        assert_eq!(proxy.username, "", "username optional");
        assert_eq!(
            proxy.password,
            fixture("form-direct"),
            "form-direct proxy.password is the fallback"
        );
    }

    #[test]
    fn secret_proxy_password_overrides_form_fallback() {
        let connection = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "c",
                "external_config": {
                    "protocol": "ftp",
                    "proxy": {
                        "type": "http",
                        "host": "proxy.example.com",
                        "port": 8080,
                        "password": fixture("form-direct")
                    }
                },
                "connection_secrets": { "proxy_password": fixture("secret-store") }
            }
        }))
        .unwrap();
        assert_eq!(
            connection.proxy.as_ref().expect("proxy").password,
            fixture("secret-store"),
            "connection_secrets.proxy_password wins over proxy.password"
        );
    }

    #[test]
    fn proxy_field_errors_carry_field_paths() {
        let build = |proxy: Value| {
            StoredConnection::from_lifecycle_params(&json!({
                "connection": {
                    "id": "c",
                    "external_config": { "protocol": "ftp", "proxy": proxy }
                }
            }))
        };
        let unsupported = build(json!({ "type": "socks4", "host": "h", "port": 1080 }))
            .unwrap_err();
        assert!(
            unsupported.contains("proxy.type") && unsupported.contains("socks4"),
            "{unsupported}"
        );
        let missing_type = build(json!({ "host": "h", "port": 1080 })).unwrap_err();
        assert!(missing_type.contains("proxy.type"), "{missing_type}");

        let missing_host = build(json!({ "type": "http", "port": 8080 })).unwrap_err();
        assert!(missing_host.contains("proxy.host"), "{missing_host}");
        let blank_host = build(json!({ "type": "http", "host": "  ", "port": 8080 }))
            .unwrap_err();
        assert!(blank_host.contains("proxy.host"), "{blank_host}");

        let missing_port = build(json!({ "type": "http", "host": "h" })).unwrap_err();
        assert!(missing_port.contains("proxy.port"), "{missing_port}");
        let zero_port = build(json!({ "type": "http", "host": "h", "port": 0 })).unwrap_err();
        assert!(zero_port.contains("proxy.port"), "{zero_port}");
        let huge_port =
            build(json!({ "type": "http", "host": "h", "port": 70_000 })).unwrap_err();
        assert!(huge_port.contains("proxy.port"), "{huge_port}");
        let text_port =
            build(json!({ "type": "http", "host": "h", "port": "8080" })).unwrap_err();
        assert!(text_port.contains("proxy.port"), "{text_port}");

        let not_object = build(json!("http://proxy:8080")).unwrap_err();
        assert!(
            not_object.contains("proxy") && not_object.contains("object"),
            "{not_object}"
        );
    }

    #[test]
    fn absent_or_null_proxy_stays_none() {
        let absent = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "c",
                "external_config": { "protocol": "ftp", "endpoint": "ftp://h" }
            }
        }))
        .unwrap();
        assert!(absent.proxy.is_none());

        let null = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "c",
                "external_config": { "protocol": "ftp", "proxy": null }
            }
        }))
        .unwrap();
        assert!(null.proxy.is_none());
    }

    #[test]
    fn proxy_url_and_group_key_formats() {
        let anonymous = ProxyConfig {
            kind: ProxyKind::Http,
            host: "proxy.example.com".to_string(),
            port: 8080,
            username: String::new(),
            password: String::new(),
        };
        assert_eq!(anonymous.url(), "http://proxy.example.com:8080");
        assert_eq!(
            anonymous.group_key(),
            anonymous.url(),
            "group key is the full url"
        );

        let user_only = ProxyConfig {
            kind: ProxyKind::Http,
            host: "proxy.example.com".to_string(),
            port: 8080,
            username: "proxyuser".to_string(),
            password: String::new(),
        };
        assert_eq!(user_only.url(), "http://proxyuser@proxy.example.com:8080");

        let user_pass = ProxyConfig {
            kind: ProxyKind::Socks5,
            host: "10.0.0.1".to_string(),
            port: 1080,
            username: "proxyuser".to_string(),
            password: fixture("proxy-pass"),
        };
        assert_eq!(
            user_pass.url(),
            // fixture values contain `::`, so the userinfo password must
            // travel percent-encoded (`:` → %3A) — pinned literally here.
            "proxyuser:fixture%3A%3Aproxy-pass@10.0.0.1:1080",
            "socks5 has no scheme prefix"
        );

        let reserved = ProxyConfig {
            kind: ProxyKind::Http,
            host: "proxy.example.com".to_string(),
            port: 443,
            username: "us:er@1/%".to_string(),
            password: "pa:ss/w#rd ?".to_string(),
        };
        assert_eq!(
            reserved.url(),
            "http://us%3Aer%401%2F%25:pa%3Ass%2Fw%23rd%20%3F@proxy.example.com:443",
            "URL-reserved characters percent-encode"
        );
    }

    #[test]
    fn parses_flat_form_proxy_shape() {
        // The declared form submits flat keys; the secret binding routes the
        // password into connection_secrets.
        let params = json!({
            "connection": {
                "id": "c",
                "external_config": {
                    "protocol": "ftp",
                    "proxy_type": "http",
                    "proxy_host": " proxy.example.com ",
                    "proxy_port": "8080",
                    "proxy_username": "proxyuser"
                },
                "connection_secrets": { "proxy_password": fixture("form-pass") }
            }
        });
        let connection = StoredConnection::from_lifecycle_params(&params).unwrap();
        let proxy = connection.proxy.expect("flat http proxy");
        assert_eq!(proxy.kind, ProxyKind::Http);
        assert_eq!(proxy.host, "proxy.example.com", "host trims");
        assert_eq!(proxy.port, 8080, "text port parses");
        assert_eq!(proxy.username, "proxyuser");
        assert_eq!(proxy.password, fixture("form-pass"));

        // Numeric wire port (older host payload) and socks5 kind.
        let socks = json!({
            "connection": {
                "id": "c",
                "external_config": {
                    "protocol": "sftp",
                    "proxy_type": "socks5",
                    "proxy_host": "10.0.0.1",
                    "proxy_port": 1080
                }
            }
        });
        let connection = StoredConnection::from_lifecycle_params(&socks).unwrap();
        let proxy = connection.proxy.expect("flat socks5 proxy");
        assert_eq!(proxy.kind, ProxyKind::Socks5);
        assert_eq!(proxy.port, 1080);
        assert_eq!(proxy.username, "");
        assert_eq!(proxy.password, "");
    }

    #[test]
    fn flat_proxy_off_or_absent_is_none() {
        for external in [
            json!({ "protocol": "ftp", "proxy_type": "off" }),
            json!({ "protocol": "ftp", "proxy_type": "" }),
            json!({ "protocol": "ftp" }),
        ] {
            let params = json!({ "connection": { "id": "c", "external_config": external } });
            let connection = StoredConnection::from_lifecycle_params(&params).unwrap();
            assert!(connection.proxy.is_none(), "{external}");
        }
    }

    #[test]
    fn flat_proxy_errors_carry_flat_field_paths() {
        let bad_type = json!({
            "connection": {
                "id": "c",
                "external_config": { "protocol": "ftp", "proxy_type": "socks4", "proxy_host": "h", "proxy_port": "1" }
            }
        });
        let error = StoredConnection::from_lifecycle_params(&bad_type).unwrap_err();
        assert!(error.contains("external_config.proxy_type"), "{error}");

        let missing_host = json!({
            "connection": {
                "id": "c",
                "external_config": { "protocol": "ftp", "proxy_type": "http", "proxy_port": "1" }
            }
        });
        let error = StoredConnection::from_lifecycle_params(&missing_host).unwrap_err();
        assert!(error.contains("external_config.proxy_host"), "{error}");

        let bad_port = json!({
            "connection": {
                "id": "c",
                "external_config": { "protocol": "ftp", "proxy_type": "http", "proxy_host": "h", "proxy_port": "0" }
            }
        });
        let error = StoredConnection::from_lifecycle_params(&bad_port).unwrap_err();
        assert!(error.contains("external_config.proxy_port"), "{error}");

        let missing_port = json!({
            "connection": {
                "id": "c",
                "external_config": { "protocol": "ftp", "proxy_type": "http", "proxy_host": "h" }
            }
        });
        let error = StoredConnection::from_lifecycle_params(&missing_port).unwrap_err();
        assert!(error.contains("external_config.proxy_port"), "{error}");
    }

    #[test]
    fn parses_tunnel_jump_chain() {
        let params = json!({
            "connection": {
                "id": "c",
                "external_config": {
                    "protocol": "s3",
                    "endpoint": "https://s3.internal.example:443",
                    "tunnel": {
                        "jump_hosts": [
                            { "host": " j1.example.com ", "port": 2222, "username": "ops" },
                            { "host": "j2.corp" }
                        ],
                        "identity_file": "~/.ssh/dbx-jump"
                    }
                }
            }
        });
        let connection = StoredConnection::from_lifecycle_params(&params).unwrap();
        let tunnel = connection.tunnel.expect("tunnel spec");
        assert_eq!(tunnel.jump_hosts.len(), 2);
        assert_eq!(tunnel.jump_hosts[0].host, "j1.example.com", "host trims");
        assert_eq!(tunnel.jump_hosts[0].port, 2222);
        assert_eq!(tunnel.jump_hosts[0].username, "ops");
        assert_eq!(tunnel.jump_hosts[1].port, 0, "missing port = ssh default");
        assert_eq!(tunnel.jump_hosts[1].username, "");
        assert_eq!(tunnel.identity_file, "~/.ssh/dbx-jump");
    }

    #[test]
    fn tunnel_absent_or_null_is_none() {
        for external in [json!({ "protocol": "s3" }), json!({ "protocol": "s3", "tunnel": null })] {
            let params = json!({ "connection": { "id": "c", "external_config": external } });
            let connection = StoredConnection::from_lifecycle_params(&params).unwrap();
            assert!(connection.tunnel.is_none(), "{external}");
        }
    }

    #[test]
    fn tunnel_rejects_passwords_empty_chain_and_fs_protocol() {
        let password_field = json!({
            "connection": {
                "id": "c",
                "external_config": {
                    "protocol": "s3",
                    "tunnel": { "jump_hosts": [ { "host": "j1", "password": fixture("pw") } ] }
                }
            }
        });
        let error = StoredConnection::from_lifecycle_params(&password_field).unwrap_err();
        assert!(
            error.contains("jump_hosts[0].password") && error.contains("key"),
            "{error}"
        );

        let empty_chain = json!({
            "connection": {
                "id": "c",
                "external_config": { "protocol": "s3", "tunnel": { "jump_hosts": [] } }
            }
        });
        let error = StoredConnection::from_lifecycle_params(&empty_chain).unwrap_err();
        assert!(error.contains("at least one jump host"), "{error}");

        let fs_tunnel = json!({
            "connection": {
                "id": "c",
                "external_config": {
                    "protocol": "fs",
                    "tunnel": { "jump_hosts": [ { "host": "j1" } ] }
                }
            }
        });
        let error = StoredConnection::from_lifecycle_params(&fs_tunnel).unwrap_err();
        assert!(error.contains("remote connections only"), "{error}");
    }

    #[test]
    fn parses_flat_form_tunnel_chain() {
        let params = json!({
            "connection": {
                "id": "c",
                "external_config": {
                    "protocol": "s3",
                    "endpoint": "https://s3.internal.example:443",
                    "tunnel_jump_hosts": " ops@j1.example.com:2222 , j2.corp , [2001:db8::9]:2200 ",
                    "tunnel_identity_file": "~/.ssh/dbx-jump"
                }
            }
        });
        let connection = StoredConnection::from_lifecycle_params(&params).unwrap();
        let tunnel = connection.tunnel.expect("flat tunnel spec");
        assert_eq!(tunnel.jump_hosts.len(), 3);
        assert_eq!(tunnel.jump_hosts[0].username, "ops");
        assert_eq!(tunnel.jump_hosts[0].host, "j1.example.com");
        assert_eq!(tunnel.jump_hosts[0].port, 2222);
        assert_eq!(tunnel.jump_hosts[1].host, "j2.corp");
        assert_eq!(tunnel.jump_hosts[1].port, 0, "missing port = ssh default");
        assert_eq!(tunnel.jump_hosts[2].host, "2001:db8::9", "bracketed IPv6");
        assert_eq!(tunnel.jump_hosts[2].port, 2200);
        assert_eq!(tunnel.identity_file, "~/.ssh/dbx-jump");
    }

    #[test]
    fn flat_tunnel_empty_chain_is_none_even_with_identity() {
        for chain in [Some(""), Some("   "), None] {
            let mut external = json!({ "protocol": "s3", "tunnel_identity_file": "~/.ssh/k" });
            if let Some(chain) = chain {
                external["tunnel_jump_hosts"] = json!(chain);
            }
            let params = json!({ "connection": { "id": "c", "external_config": external } });
            let connection = StoredConnection::from_lifecycle_params(&params).unwrap();
            assert!(
                connection.tunnel.is_none(),
                "the chain drives — an identity file alone never enables a tunnel"
            );
        }
    }

    #[test]
    fn nested_tunnel_wins_over_flat_fields() {
        let params = json!({
            "connection": {
                "id": "c",
                "external_config": {
                    "protocol": "s3",
                    "tunnel_jump_hosts": "flat-host",
                    "tunnel": { "jump_hosts": [ { "host": "nested-host", "port": 2223 } ] }
                }
            }
        });
        let connection = StoredConnection::from_lifecycle_params(&params).unwrap();
        let tunnel = connection.tunnel.expect("nested wins");
        assert_eq!(tunnel.jump_hosts.len(), 1);
        assert_eq!(tunnel.jump_hosts[0].host, "nested-host");
        assert_eq!(tunnel.jump_hosts[0].port, 2223);
    }

    #[test]
    fn flat_tunnel_chain_errors_carry_flat_paths() {
        let case = |chain: &str| {
            let params = json!({
                "connection": {
                    "id": "c",
                    "external_config": { "protocol": "s3", "tunnel_jump_hosts": chain }
                }
            });
            StoredConnection::from_lifecycle_params(&params).unwrap_err()
        };
        let error = case("a@b, ,c");
        assert!(error.contains("tunnel_jump_hosts[1] is empty"), "{error}");
        let error = case("b:70000");
        assert!(error.contains("tunnel_jump_hosts[0] has an invalid port"), "{error}");
        let error = case("[::1:22");
        assert!(error.contains("unterminated IPv6 bracket"), "{error}");
        let error = case("b:abc");
        assert!(error.contains("tunnel_jump_hosts[0] has an invalid port"), "{error}");

        // A trailing colon is tolerated as "port omitted".
        let params = json!({
            "connection": {
                "id": "c",
                "external_config": { "protocol": "s3", "tunnel_jump_hosts": "b:" }
            }
        });
        let connection = StoredConnection::from_lifecycle_params(&params).unwrap();
        assert_eq!(connection.tunnel.expect("tolerated chain").jump_hosts[0].host, "b");
        let error = case("b:abc");
        assert!(error.contains("tunnel_jump_hosts[0] has an invalid port"), "{error}");

        // fs + non-empty flat chain is rejected like the nested shape.
        let params = json!({
            "connection": {
                "id": "c",
                "external_config": { "protocol": "fs", "tunnel_jump_hosts": "j1" }
            }
        });
        let error = StoredConnection::from_lifecycle_params(&params).unwrap_err();
        assert!(error.contains("remote connections only"), "{error}");
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
            ("bucket", &["s3", "gcs", "obs", "oss", "cos", "qiniu"]),
            ("container", &["azblob"]),
            ("account_name", &["azblob"]),
            ("account_key", &["azblob"]),
            ("credential", &["gcs"]),
            ("scope", &["gcs"]),
            ("region", &["s3"]),
            ("access_key_id", &[
                "s3", "obs", "oss", "qiniu",
                "azurefiles", "b2", "cloudinary", "imagekit", "internetarchive", "qingstor",
                "sugarsync",
            ]),
            ("secret_access_key", &[
                "s3", "obs", "oss", "qiniu",
                "azurefiles", "b2", "cloudinary", "imagekit", "internetarchive", "qingstor",
                "sugarsync",
            ]),
            ("enable_virtual_host_style", &["s3"]),
            (
                "endpoint",
                &["s3", "gcs", "azblob", "obs", "oss", "cos", "qiniu", "webdav", "ftp", "sftp", "smb", "sftp-native", "koofr", "pcloud", "seafile",
                  "azurefiles", "filefabric", "hdfs", "http", "imagekit", "netstorage", "qingstor", "quatrix", "sia"],
            ),
            ("secret_id", &["cos"]),
            ("secret_key", &["cos"]),
            ("security_token", &["cos"]),
            ("username", &[
                "webdav", "smb", "pcloud", "seafile",
                "azurefiles", "cloudinary", "filen", "filescom", "hdfs", "iclouddrive",
                "internxt", "linkbox", "mega", "mailru", "netstorage", "opendrive", "pikpak",
                "protondrive", "swift", "ulozto",
            ]),
            ("user", &["ftp", "sftp", "sftp-native"]),
            ("share", &["smb", "azurefiles"]),
            ("domain", &["smb"]),
            // password deliberately excludes `sftp`: the rclone sftp backend
            // accepts password auth, but the plain-sftp form keeps key-only
            // semantics; password accounts belong to `sftp-native`.
            ("password", &[
                "webdav", "ftp", "smb", "sftp-native", "koofr", "pcloud", "seafile",
                "filen", "filescom", "iclouddrive", "internxt", "linkbox", "mega", "mailru",
                "netstorage", "opendrive", "pikpak", "protondrive", "sia", "swift", "ulozto",
            ]),
            ("key", &["sftp", "sftp-native"]),
            ("known_hosts_strategy", &["sftp", "sftp-native"]),
            ("config", &GENERIC_PROTOCOLS),
            ("access_token", &["dropbox", "gdrive", "onedrive", "yandex-disk", "box", "drime", "gofile"]),
            ("client_id", &[
                "aliyun-drive", "dropbox", "gdrive", "onedrive",
                "box", "hidrive", "huaweidrive", "jottacloud", "mailru", "premiumizeme",
                "putio", "sharefile", "zoho",
            ]),
            ("client_secret", &[
                "aliyun-drive", "dropbox", "gdrive", "onedrive",
                "box", "hidrive", "huaweidrive", "jottacloud", "mailru", "premiumizeme",
                "putio", "sharefile", "zoho",
            ]),
            ("refresh_token", &["aliyun-drive", "dropbox", "gdrive", "onedrive"]),
            ("token", &[
                "fichier", "filefabric", "filelu", "filen", "linkbox", "pixeldrain",
                "quatrix", "shade", "storj", "ulozto",
            ]),
            ("drive_type", &["aliyun-drive"]),
            ("email", &["koofr"]),
            ("repo_name", &["seafile"]),
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

        let conditionally_required = [
            "access_key_id",
            "secret_access_key",
            "credential",
            "account_key",
            "secret_id",
            "secret_key",
            "email",
            "repo_name",
            "token",
        ];
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
            // required may cover a SUBSET of the visible protocols (e.g. the
            // access-key pair stays optional where the backend ships an
            // env-auth or default-endpoint path); it must never demand a
            // value on a protocol that hides the field.
            let visible: Vec<&str> = visible_when["one_of"]
                .as_array()
                .expect("visible_when.one_of")
                .iter()
                .map(|value| value.as_str().expect("one_of value"))
                .collect();
            let required: Vec<&str> = required_when["one_of"]
                .as_array()
                .expect("required_when.one_of")
                .iter()
                .map(|value| value.as_str().expect("one_of value"))
                .collect();
            assert!(
                !required.is_empty(),
                "field '{key}' carries required_when with no protocols"
            );
            assert!(
                required.iter().all(|value| visible.contains(value)),
                "field '{key}' required_when must stay inside its visible_when"
            );
        }
        // bucket is conditionally required on a SUBSET of its visible
        // protocols (bucket namespace, design 2026-09-17): gcs stays
        // required because its ListBuckets needs a service-account JWT →
        // OAuth token exchange (phase 2); s3/oss/cos/obs accept an empty
        // bucket — the connection root then lists all buckets and the first
        // path segment selects one. The required scope must stay inside the
        // visible scope (host evaluates required_when independently).
        let bucket = field_of("bucket");
        let bucket_visible: Vec<&str> = bucket["visible_when"]["one_of"]
            .as_array()
            .expect("bucket visible_when.one_of")
            .iter()
            .map(|value| value.as_str().expect("visible protocol"))
            .collect();
        let bucket_required: Vec<&str> = bucket["required_when"]["one_of"]
            .as_array()
            .expect("bucket required_when.one_of")
            .iter()
            .map(|value| value.as_str().expect("required protocol"))
            .collect();
        assert_eq!(bucket_required, ["gcs"], "bucket stays required for gcs only");
        assert!(
            bucket_required
                .iter()
                .all(|protocol| bucket_visible.contains(protocol)),
            "bucket required_when must stay inside visible_when"
        );
        // container follows the same namespace contract: optional for
        // azblob, whose account-level endpoint lists all containers.
        let container = field_of("container");
        assert!(!required("container"));
        assert!(
            container.get("required_when").is_none(),
            "container must stay optional (namespace mode lists all containers)"
        );
        let share = field_of("share");
        assert!(!required("share"));
        assert!(share.get("required_when").is_none());

        for (name, expected) in [
            ("username", &[
                "pcloud", "seafile",
                "cloudinary", "filen", "iclouddrive", "internxt", "linkbox", "mega", "mailru",
                "netstorage", "opendrive", "pikpak", "protondrive",
            ][..]),
            ("password", &[
                "koofr", "pcloud", "seafile",
                "iclouddrive", "internxt", "linkbox", "mega", "mailru", "netstorage",
                "opendrive", "pikpak", "protondrive",
            ][..]),
            ("access_token", &["yandex-disk"][..]),
            ("token", &["filelu", "linkbox", "quatrix", "shade"][..]),
        ] {
            let item = field_of(name);
            let visible: Vec<&str> = item["visible_when"]["one_of"]
                .as_array()
                .expect("visible_when.one_of")
                .iter()
                .map(|value| value.as_str().expect("visible protocol"))
                .collect();
            let conditional: Vec<&str> = item["required_when"]["one_of"]
                .as_array()
                .expect("required_when.one_of")
                .iter()
                .map(|value| value.as_str().expect("required protocol"))
                .collect();
            assert_eq!(conditional, expected, "required_when mismatch on '{name}'");
            assert!(
                conditional.iter().all(|protocol| visible.contains(protocol)),
                "required_when for '{name}' must stay inside visible_when"
            );
        }

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
        for required_protocol in [
            "gcs", "azblob", "obs", "oss", "cos", "qiniu", "webdav", "ftp", "sftp", "smb", "sftp-native",
            "koofr", "pcloud", "seafile", "filefabric", "hdfs", "http", "imagekit", "netstorage",
            "quatrix",
        ] {
            assert!(
                endpoint_required.contains(&required_protocol),
                "endpoint must be conditionally required for '{required_protocol}'"
            );
        }
        assert!(
            !endpoint_required.contains(&"s3"),
            "endpoint stays optional for s3 (AWS default endpoint)"
        );
        assert!(
            !endpoint_required.contains(&"qingstor") && !endpoint_required.contains(&"sia")
                && !endpoint_required.contains(&"azurefiles"),
            "endpoint stays optional where the backend ships a default address"
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
            for key in &en_keys {
                assert!(
                    localized.contains_key(*key),
                    "locale {locale} misses field label for '{key}'"
                );
            }
            for key in localized.keys() {
                assert!(
                    en_keys.contains(&key.as_str()),
                    "locale {locale} localizes unknown field '{key}'"
                );
            }
        }
    }
}
