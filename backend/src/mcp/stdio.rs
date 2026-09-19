// ---------------------------------------------------------------------------
// Standalone stdio MCP server (`--mcp`, design §0.2/§5 stdio row)
// ---------------------------------------------------------------------------
//
// The sidecar doubles as a plain MCP server (protocol 2024-11-05,
// newline-delimited JSON-RPC 2.0) so AI clients can call the files tool
// surface without the DBX host — the realistic exposure path verified on a
// real machine. Structure mirrors the ssh plugin's `run_mcp_stdio`:
//
// - entry exclusivity: `--mcp` (main.rs) returns before the framed
//   `PluginServer::serve()` is ever started; one process runs exactly one of
//   the two modes;
// - the tool dispatch is the SAME `run_tool` the DBX bridge `mcp/call` uses
//   (stdio is a new entry, never a copied tool surface); only the connection
//   sourcing differs: there is no host lifecycle, so connection parameters
//   travel inline with the call and are pooled in the rclone registry under
//   their parameter-hash id;
// - UI-intent tools answer a hard UNAVAILABLE (degradation matrix stdio row)
//   instead of burning the 5s report wait on a frontend that cannot exist.

/// MCP protocol revision the stdio server speaks (ssh `PROTOCOL_VERSION`).
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use super::*;

const PROTOCOL_VERSION: &str = "2024-11-05";

/// UI-intent tool family in stdio mode: UNAVAILABLE by design (no workbench
/// to drive; `files_ui_quick_paths` is pure meta-discovery and stays usable).
pub(crate) const STDIO_UI_TOOLS: [&str; 4] = [
    "files_ui_focus",
    "files_ui_search",
    "files_ui_select",
    "files_ui_state",
];

/// The stdio UNAVAILABLE answer (design §5: 明确 UNAVAILABLE，不假死). The
/// marker, the tool name and both fallbacks (digest / DBX bridge) are
/// greppable and actionable for an AI caller.
fn unavailable_message(tool: &str) -> String {
    format!(
        "UNAVAILABLE: 此工具需要 DBX 工作台（工作台模式可用）。tool '{tool}' drives the DBX \
         workbench, which is not present in standalone stdio mode; use files_scan_digest for \
         sidecar-local reads, or call this tool through the DBX MCP bridge (dbx_call_plugin_tool)."
    )
}

/// Tools whose execution resolves a single generic `connectionId`/inline
/// `connection` parameter — the stdio surfaces relax exactly that pair into
/// an anyOf. `files_cursor_next` is a pure session lookup and runs without a
/// connection, and `files_sync` addresses explicit `sourceConnectionId` /
/// `targetConnectionId` parameters (and cannot run in stdio anyway — no
/// event channel), so neither takes the generic parameter.
pub(crate) fn needs_connection_id(tool: &str) -> bool {
    !matches!(tool, "files_cursor_next" | "files_sync")
}

/// Pool id for an inline connection payload: `mcp-inline-` + 16 hex chars of
/// the parameter hash ([`params_hash`] is canonical — serde_json object keys
/// are sorted, so key order does not matter). Pure and unit-tested.
pub(crate) fn inline_pool_id(connection: &Value) -> String {
    format!("mcp-inline-{:016x}", params_hash(connection))
}

/// camelCase inline connection payload → [`StoredConnection`], reusing the
/// tolerant lifecycle parser so backend validation stays single-sourced.
/// Field names mirror the files connection form (see MCP.zh-CN.md「方式二」
/// 凭据参数表): `protocol`/`root`/`bucket`/`endpoint`/`region`/
/// `accessKeyId`/`secretAccessKey`/`secretId`/`secretKey`/`username`/
/// `user`/`password`/`key`/…
/// plus the convenience `protocol: "local"` (alias of `fs` for the "give me
/// a root and go" path used by credential-free smoke runs).
pub(crate) fn stored_connection_from_inline(connection: &Value) -> Result<StoredConnection, String> {
    let Some(map) = connection.as_object() else {
        return Err("connection must be a JSON object".to_string());
    };
    let protocol = map
        .get("protocol")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "Missing protocol in connection (one of: local, fs, s3, gcs, azblob, obs, oss, cos, webdav, ftp, \
             sftp, smb, sftp-native, rclone-custom, aliyun-drive, dropbox, gdrive, koofr, onedrive, pcloud, seafile, yandex-disk)"
                .to_string()
        })?;
    let protocol = match protocol {
        "local" | "localFs" | "localfs" | "localFS" => "fs",
        other => other,
    };
    // camelCase inline key → external_config / connection_secrets key. The
    // protocol-specific keys stay optional: the lifecycle parser tolerates
    // them and each backend builder enforces its own required keys.
    let mut external = serde_json::Map::new();
    external.insert("protocol".to_string(), json!(protocol));
    for (inline_key, config_key) in [
        ("root", "root"),
        ("bucket", "bucket"),
        ("endpoint", "endpoint"),
        ("region", "region"),
        ("container", "container"),
        ("accountName", "account_name"),
        ("scope", "scope"),
        ("accessKeyId", "access_key_id"),
        ("enableVirtualHostStyle", "enable_virtual_host_style"),
        ("username", "username"),
        ("user", "user"),
        ("share", "share"),
        ("domain", "domain"),
        ("knownHostsStrategy", "known_hosts_strategy"),
        ("proxyType", "proxy_type"),
        ("proxyHost", "proxy_host"),
        ("proxyPort", "proxy_port"),
        ("proxyUsername", "proxy_username"),
        ("tunnelJumpHosts", "tunnel_jump_hosts"),
        ("tunnelIdentityFile", "tunnel_identity_file"),
        ("readOnly", "read_only"),
        ("allowDelete", "allow_delete"),
        ("lockToRoot", "lock_to_root"),
        ("timeoutSecs", "timeout_secs"),
        ("service", "service"),
        ("config", "config"),
        ("clientId", "client_id"),
        ("driveType", "drive_type"),
        ("email", "email"),
        ("repoName", "repo_name"),
    ] {
        if let Some(value) = map.get(inline_key).filter(|value| !value.is_null()) {
            external.insert(config_key.to_string(), value.clone());
        }
    }
    let mut secrets = serde_json::Map::new();
    for (inline_key, secret_key) in [
        ("secretAccessKey", "secret_access_key"),
        ("credential", "credential"),
        ("accountKey", "account_key"),
        ("secretId", "secret_id"),
        ("secretKey", "secret_key"),
        ("securityToken", "security_token"),
        ("password", "password"),
        ("key", "key"),
        ("accessToken", "access_token"),
        ("clientSecret", "client_secret"),
        ("refreshToken", "refresh_token"),
        ("proxyPassword", "proxy_password"),
    ] {
        if let Some(value) = map.get(inline_key).and_then(Value::as_str) {
            secrets.insert(secret_key.to_string(), json!(value));
        }
    }
    // An explicit `connection.id` wins; otherwise the parameter-hash pool id
    // keys the engine entry so repeated identical calls share one Operator.
    let id = map
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| inline_pool_id(connection));
    let name = map
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("stdio-inline");
    StoredConnection::from_lifecycle_params(&json!({
        "connection": {
            "id": id,
            "name": name,
            "external_config": external,
            "connection_secrets": secrets,
        }
    }))
}

/// stdio tools/list 里内联 `connection` 对象的属性声明（与
/// [`stored_connection_from_inline`] 的 camelCase 键一一同名：漏声明的键
/// 会被严格校验的 MCP 宿主丢弃）。
pub(crate) fn inline_connection_properties() -> Value {
    json!({
    "protocol": { "type": "string",
        "description": "Storage protocol (required): local (alias of fs), fs, s3, gcs, azblob, obs, oss, cos, webdav, ftp, sftp, smb, sftp-native, rclone-custom, aliyun-drive, dropbox, gdrive, koofr, onedrive, pcloud, seafile, yandex-disk" },
    "root": { "type": "string", "description": "Root path; operations are confined under it" },
    "bucket": { "type": "string", "description": "Bucket (s3/gcs/obs/oss/cos); optional on s3/oss/obs/cos — leave empty to list all buckets at the connection root" },
    "container": { "type": "string", "description": "Azure Blob container (azblob)" },
    "accountName": { "type": "string", "description": "Azure Storage account name (azblob)" },
    "credential": { "type": "string", "description": "Base64 Google credential JSON (gcs; stays in process memory only)" },
    "accountKey": { "type": "string", "description": "Azure Storage account key (stays in process memory only)" },
    "scope": { "type": "string", "description": "Google OAuth scope (gcs)" },
    "region": { "type": "string", "description": "Region (s3/oss)" },
    "endpoint": { "type": "string", "description": "Endpoint URL (s3/gcs/azblob/obs/oss/cos)" },
    "accessKeyId": { "type": "string", "description": "Access key id (s3/obs/oss)" },
    "secretAccessKey": { "type": "string", "description": "Secret access key (s3/obs/oss; stays in process memory only)" },
    "secretId": { "type": "string", "description": "Tencent Cloud COS secret id (stays in process memory only)" },
    "secretKey": { "type": "string", "description": "Tencent Cloud COS secret key (stays in process memory only)" },
    "securityToken": { "type": "string", "description": "Tencent Cloud COS STS security token (stays in process memory only)" },
    "enableVirtualHostStyle": { "type": "boolean", "description": "Virtual-host style addressing (s3)" },
    "username": { "type": "string", "description": "Username (webdav/smb)" },
    "user": { "type": "string", "description": "User (ftp/sftp/sftp-native)" },
    "password": { "type": "string", "description": "Password (webdav/ftp/smb/sftp-native; stays in process memory only)" },
    "key": { "type": "string", "description": "Private key (sftp/sftp-native; stays in process memory only)" },
    "knownHostsStrategy": { "type": "string", "description": "known_hosts strategy (sftp/sftp-native)" },
    "proxyType": { "type": "string", "description": "Proxy kind: off (default), http, socks5 (ftp/sftp-native/sftp; other protocols dial direct)" },
    "proxyHost": { "type": "string", "description": "Proxy host (required when proxyType is http/socks5)" },
    "proxyPort": { "type": "string", "description": "Proxy port 1-65535 (required when proxyType is http/socks5)" },
    "proxyUsername": { "type": "string", "description": "Proxy username (optional; empty = anonymous)" },
    "proxyPassword": { "type": "string", "description": "Proxy password (optional; stays in process memory only)" },
    "tunnelJumpHosts": { "type": "string", "description": "SSH tunnel jump chain, ssh -J syntax: comma-separated [user@]host[:port]; the last entry is the login target. Key auth only; rclone engine only; empty = no tunnel" },
    "tunnelIdentityFile": { "type": "string", "description": "Private key path for the SSH tunnel (optional; empty = ssh defaults/agent)" },
    "share": { "type": "string", "description": "Share (smb)" },
    "domain": { "type": "string", "description": "Domain (smb)" },
    "service": { "type": "string", "description": "Custom rclone backend type (rclone-custom)" },
    "config": { "type": "object", "description": "Custom backend parameters JSON (rclone-custom)" },
    "accessToken": { "type": "string", "description": "OAuth access token (drive services; stays in process memory only)" },
    "clientId": { "type": "string", "description": "OAuth client id (drive services)" },
    "clientSecret": { "type": "string", "description": "OAuth client secret (drive services; stays in process memory only)" },
    "refreshToken": { "type": "string", "description": "OAuth refresh token (drive services; stays in process memory only)" },
    "driveType": { "type": "string", "description": "Alibaba Drive type (resource/share/backup)" },
    "email": { "type": "string", "description": "Koofr account email" },
    "repoName": { "type": "string", "description": "Seafile library name" },
    "readOnly": { "type": "boolean", "description": "Open read-only (write tools refused)" },
    "allowDelete": { "type": "boolean", "description": "Allow delete-class tools (default true; set false to refuse delete/purge)" },
    "lockToRoot": { "type": "boolean", "description": "Confine paths to the root prefix" },
    "timeoutSecs": { "type": "integer", "description": "Operation timeout seconds" },
    "id": { "type": "string", "description": "Explicit connection id (default: hash-pooled mcp-inline-…)" },
    "name": { "type": "string", "description": "Display name" },
    })
}

/// One stdio input line → what the server loop does with it (pure,
/// unit-tested): `Ok(None)` blank line to skip; `Ok(Some(request))` a parsed
/// JSON-RPC request to dispatch; `Err(response)` a non-JSON line — write this
/// parse-error response (`-32700`, null id).
pub(crate) fn parse_request_line(line: &str) -> Result<Option<Value>, Value> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    match serde_json::from_str(line) {
        Ok(value) => Ok(Some(value)),
        Err(error) => Err(parse_error_response(format!("Parse error: {error}"))),
    }
}

fn write_response(stdout: &std::sync::Mutex<io::Stdout>, response: Value) -> io::Result<()> {
    let mut guard = stdout
        .lock()
        .map_err(|poisoned| io::Error::other(poisoned.to_string()))?;
    writeln!(guard, "{response}")?;
    guard.flush()
}

/// Single input-line ceiling (`DBX_FILES_MCP_STDIO_MAX_LINE`, bytes). The
/// request side has no JSON-schema cap (an 8 MiB inline `dataBase64` write is
/// a legitimate line), but the reader must stay bounded: an unbounded line is
/// an unbounded allocation from any writer on the other end of the pipe.
/// Default 16 MiB comfortably fits the 4 MiB decode-cap writes (base64 x4/3 ≈
/// 5.5 MiB plus JSON structure); tests/smoke may lower it to exercise the
/// over-limit path cheaply.
pub(crate) const DEFAULT_STDIO_MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

pub(crate) fn stdio_max_line_bytes() -> usize {
    std::env::var("DBX_FILES_MCP_STDIO_MAX_LINE")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_STDIO_MAX_LINE_BYTES)
}

/// `-32700` answer for one unreadable input line (null id), keyed by cause.
fn parse_error_response(message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": { "code": -32700, "message": message },
    })
}

/// `--mcp` entry (main.rs; mutually exclusive with the framed protocol — the
/// caller returns here before `PluginServer::serve()` starts). Mirrors the
/// ssh plugin's `run_mcp_stdio`: one request per spawned task so a slow tool
/// call cannot stall ping/tools/list, and a bounded drain of in-flight
/// handlers when stdin closes.
///
/// Line-reading robustness (可靠性纵深): bytes are read `read_until(b'\n')`
/// and lossily decoded, so an invalid-UTF-8 line answers -32700 instead of
/// killing the session (`BufRead::lines()` would propagate the error and end
/// the server), oversized lines are refused at the ceiling without being
/// parsed, and CRLF/blank lines are tolerated.
pub fn run_mcp_stdio(data_dir: PathBuf) -> io::Result<()> {
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| io::Error::other(format!("Failed to create async runtime: {error}")))?;
    std::fs::create_dir_all(&data_dir).map_err(|error| {
        io::Error::other(format!(
            "Failed to create plugin data directory {}: {error}",
            data_dir.display()
        ))
    })?;
    let store = Arc::new(Store::new(data_dir.clone()));
    let mut mcp_inner = Mcp::new(data_dir.clone());
    // The rclone engine is the only engine: storage tools dispatch through
    // it (inline connections register into its registry below). `start_sync`
    // stays None: files_sync refuses in stdio either way (no event channel).
    mcp_inner.attach_rclone(super::tools::RcloneRoute {
        engine: Arc::new(crate::rclone::RcloneEngine::new()),
        store: Arc::clone(&store),
        start_sync: None,
    });
    let mcp = Arc::new(mcp_inner);
    let server = Arc::new(StdioServer {
        mcp,
        store,
        bridge_fallback: true,
        bridge_ensure_wait: appbridge::DEFAULT_ENSURE_WAIT,
    });
    // Spawned handlers may finish out of order; the mutex keeps each JSON-RPC
    // line intact and id-based correlation makes ordering irrelevant.
    let stdout = Arc::new(std::sync::Mutex::new(io::stdout()));
    let mut in_flight = Vec::new();
    let max_line = stdio_max_line_bytes();
    let mut raw: Vec<u8> = Vec::new();
    let stdin = io::stdin();
    loop {
        raw.clear();
        let read = stdin.lock().read_until(b'\n', &mut raw)?;
        if read == 0 {
            break; // EOF: stdin closed
        }
        let line = String::from_utf8_lossy(&raw);
        let line = line.trim_end_matches(['\n', '\r']);
        if line.len() > max_line {
            write_response(
                &stdout,
                parse_error_response(format!(
                    "Parse error: request line of {} bytes exceeds the {}-byte limit \
                     (DBX_FILES_MCP_STDIO_MAX_LINE)",
                    line.len(),
                    max_line
                )),
            )?;
            continue;
        }
        let request = match parse_request_line(line) {
            Ok(Some(request)) => request,
            Ok(None) => continue,
            Err(response) => {
                write_response(&stdout, response)?;
                continue;
            }
        };
        let server = Arc::clone(&server);
        let stdout = Arc::clone(&stdout);
        in_flight.push(runtime.spawn(async move {
            if let Some(response) = server.dispatch(request).await {
                let _ = write_response(&stdout, response);
            }
        }));
    }
    // stdin is closed: drain in-flight handlers (bounded, as a runaway
    // handler must not pin the process forever) before the runtime drops.
    let drain = async {
        for handle in in_flight {
            let _ = handle.await;
        }
    };
    let _ = runtime.block_on(async { tokio::time::timeout(Duration::from_secs(300), drain).await });
    Ok(())
}


/// Standalone stdio server state: the same tool surface as the DBX bridge
/// (`mcp/call` → [`Mcp::run_tool`]) with no host lifecycle — connections come
/// from inline call parameters pooled in the rclone registry by parameter
/// hash, and a saved-connection `connectionId` forwards to the running DBX
/// app through the local TCP bridge ([`appbridge`]).
pub(crate) struct StdioServer {
    pub(crate) mcp: Arc<Mcp>,
    pub(crate) store: Arc<Store>,
    /// L1 stdio bridge fallback switch: forwards unpooled-`connectionId`
    /// calls to the running DBX app through the local TCP bridge. Always on
    /// in production; tests flip it off to keep decision paths hermetic (no
    /// real app bridge on the box).
    pub(crate) bridge_fallback: bool,
    /// Wake budget the forward path grants `appbridge::ensure` after a launch
    /// attempt (default 30s; the bridge fail-closed test shortens it).
    pub(crate) bridge_ensure_wait: Duration,
}

impl StdioServer {
    pub(crate) async fn dispatch(&self, request: Value) -> Option<Value> {
        let id = request.get("id").cloned();
        // JSON-RPC 2.0 request validity (MCP_ACCEPTANCE §2 -32600 tier): a
        // missing/non-string method or an id outside {string, number, null}
        // is an invalid REQUEST (not an unknown method), answered with the
        // -32600 tier before any dispatch. The `jsonrpc` version field is
        // deliberately NOT validated (family-wide with ssh/ldap/kafka): real
        // MCP clients omit or vary it and there is no behavior difference to
        // guard —宽容不校验, pinned by tests.
        if !request
            .get("method")
            .and_then(Value::as_str)
            .map(|method| !method.is_empty())
            .unwrap_or(false)
        {
            let id = id.filter(|id| is_valid_jsonrpc_id(id));
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32600,
                    "message": "Invalid request: missing method",
                },
            }));
        }
        if let Some(id) = &id {
            if !is_valid_jsonrpc_id(id) {
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {
                        "code": -32600,
                        "message": "Invalid request: id must be a string, number, or null",
                    },
                }));
            }
        }
        let method = request["method"].as_str().unwrap_or_default().to_string();
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        // Notifications are never answered (JSON-RPC), initialized included.
        if method.starts_with("notifications/") {
            return None;
        }
        // JSON-RPC error tiering (family-wide with ssh/ldap/kafka): the
        // transport layer uses the standard codes — parse -32700 (line
        // reader), unknown method -32601, invalid params -32602, invalid
        // request -32600 — while tool-level errors are MCP `isError` results
        // (ldap/kafka stdio parity + the MCP spec guidance that the caller
        // must see the failure in-band to self-correct), not protocol-level
        // -32000 errors. The smoke SKIP gate keys on the "Method not found"
        // text, not the numeric code.
        let result: Result<Value, (i64, String)> = match method.as_str() {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": {
                    "name": "io.dbx.files",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({
                "tools": self.mcp.stdio_tool_list()["tools"].clone(),
            })),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if name.is_empty() {
                    Err((
                        -32602,
                        "Invalid params: missing tool name in tools/call params".to_string(),
                    ))
                } else {
                    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                    if !arguments.is_object() {
                        // Same semantics as the DBX bridge's mcp/call guard: a
                        // non-object arguments payload is a caller bug
                        // (structural, not tool-level).
                        Err((
                            -32602,
                            "Invalid params: arguments must be a JSON object".to_string(),
                        ))
                    } else {
                        Ok(self.call_tool(&name, &arguments).await.unwrap_or_else(
                            |message|
                            json!({
                                "content": [{ "type": "text", "text": message }],
                                "isError": true,
                            }),
                        ))
                    }
                }
            }
            other => Err((-32601, format!("Method not found: {other}"))),
        };
        Some(match (id, result) {
            (Some(id), Ok(result)) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            (Some(id), Err((code, message))) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": code, "message": message },
            }),
            // A request without an id is invalid JSON-RPC; reply with a null id.
            (None, result) => json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32600, "message": result.err().map(|(_, message)| message).unwrap_or_else(|| "Invalid request".to_string()) },
            }),
        })
    }

    async fn call_tool(&self, name: &str, arguments: &Value) -> Result<Value, String> {
        // Degradation matrix stdio row: UI intent tools answer UNAVAILABLE
        // immediately — no workbench exists to report back, so the 5s intent
        // wait would be dead time for every caller.
        if STDIO_UI_TOOLS.contains(&name) {
            return Err(unavailable_message(name));
        }
        // Unknown tool names must fail as "Unknown tool" BEFORE the
        // connection-resolution guidance: otherwise `prepare_arguments`
        // would answer an unregistered name with "Missing required
        // parameter: connectionId", sending the LLM hunting for a parameter
        // instead of the right tool name.
        if !ALL_TOOL_NAMES.contains(&name) {
            return Err(unknown_tool_message(name));
        }
        // L1 stdio bridge fallback: a call referencing a `connectionId` that
        // is NOT pooled in this session (the normal state of a standalone
        // `--mcp` session referencing a DBX saved connection) is forwarded to
        // the running DBX app's own sidecar through the local TCP bridge, so
        // saved connections work without inline credentials and credentials
        // never travel in tool arguments. A failed leg (app down, older app
        // without the route, refused call) never falls back to a silent
        // re-dial: it degrades to the fail-closed guidance error carrying the
        // bridge reason plus the inline-credential ways out.
        if self.bridge_fallback {
            if let Some((connection_id, forwarded)) = self.bridge_forward_plan(name, arguments) {
                return match self.forward_via_bridge(&connection_id, name, &forwarded).await {
                    Ok(result) => Ok(result),
                    Err(reason) => Err(unknown_connection_guidance(&connection_id, Some(&reason))),
                };
            }
        }
        let arguments = self.prepare_arguments(name, arguments).await?;
        // Same dispatch as the DBX bridge (`emitter: None` — no workbench
        // event channel); the 16 KiB cap + envelope post-processing is shared.
        let mut payload = self
            .mcp
            .run_tool(name, &arguments, &self.store, None)
            .await?;
        Ok(self.mcp.finalize_payload(&mut payload))
    }

    /// L1 stdio bridge fallback decision: `Some((connection_id, arguments))`
    /// when the call must be forwarded through the DBX app bridge, `None`
    /// when the local path owns the call (session/UI tools, an inline
    /// `connection` payload, the built-in `__local__` filesystem, or an id
    /// already pooled in this engine). Pure and unit-tested.
    pub(crate) fn bridge_forward_plan(&self, tool: &str, arguments: &Value) -> Option<(String, Value)> {
        // Session tools (`files_cursor_next` runs on a local cursor session)
        // and UI tools (gated to UNAVAILABLE before this point) never forward.
        if !needs_connection_id(tool) || STDIO_UI_TOOLS.contains(&tool) {
            return None;
        }
        // Inline credentials present: the local path owns the call (the
        // parameter-hash pool is the cheaper, hermetic route).
        if arguments.get("connection").is_some_and(Value::is_object) {
            return None;
        }
        let id = arguments
            .get("connectionId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())?;
        // The built-in `__local__` filesystem and every pooled inline id
        // stay local (inline connections pool in the rclone registry). Only
        // a genuinely unknown saved-connection id forwards.
        if id == crate::rclone::LOCAL_CONNECTION_ID || self.mcp.rclone_pooled(id) {
            return None;
        }
        Some((id.to_string(), arguments.clone()))
    }

    /// Forwards one connection-bound tool call through the DBX app bridge
    /// (L1): `ensure` verifies/waits for the published port (waking the app
    /// best-effort), then relays with the snake_case five-field contract.
    /// The forwarded timeout follows the family default (300s clamp
    /// upstream); files tools carry no per-call tool timeout argument. The
    /// 200 body is the app's MCP content envelope and is returned verbatim;
    /// a non-envelope shape (defensive) is wrapped as a success payload.
    async fn forward_via_bridge(
        &self,
        connection_id: &str,
        tool: &str,
        arguments: &Value,
    ) -> Result<Value, String> {
        appbridge::ensure(self.bridge_ensure_wait).await?;
        let result = appbridge::call_plugin_tool(
            connection_id,
            tool,
            arguments.clone(),
            Duration::from_secs(300),
        )
        .await?;
        // The app-side answer is the host mcp/call MCP content envelope
        // ({content, isError}) — pass it through verbatim; wrapping again
        // would bury the app's answer one JSON level deeper.
        if result.get("content").is_some() && result.get("isError").is_some() {
            return Ok(result);
        }
        Ok(json!({
            "content": [{ "type": "text", "text": result.to_string() }],
            "isError": false,
        }))
    }

    /// Stdio argument normalization: an inline `connection` payload is parsed
    /// (camelCase → lifecycle shape, backend keys validated by the rclone
    /// registry build) and pooled in the rclone registry under its
    /// parameter-hash id; a bare
    /// `connectionId` must already resolve in-process (`__local__` or a pooled
    /// inline id — the DBX app bridge forward owns unpooled ids upstream in
    /// [`StdioServer::call_tool`], so this branch only fires with the fallback
    /// switched off). Calls without any connection reference only pass for the
    /// connection-free tools (`files_cursor_next`).
    async fn prepare_arguments(&self, tool: &str, arguments: &Value) -> Result<Value, String> {
        if !arguments.is_object() {
            // Same semantics as the DBX bridge's mcp/call guard: a non-object
            // arguments payload is a caller bug, reported before anything else
            // (the stdio dispatch maps it to JSON-RPC -32602).
            return Err("arguments must be a JSON object".to_string());
        }
        match arguments.get("connection") {
            Some(connection) => {
                let connection = stored_connection_from_inline(connection)?;
                // Inline connections register into the rclone registry under
                // their parameter-hash id (the only engine's connection
                // table); tools then resolve the normalized connectionId.
                let route = self
                    .mcp
                    .rclone_route()
                    .ok_or_else(|| "storage tools require the rclone engine route, which is \
                                     not attached in this session"
                        .to_string())?;
                let connection = route
                    .engine
                    .prepare(&connection.id, &connection)
                    .await
                    .map_err(|error| format!("Failed to reach the rclone engine: {error}"))?;
                let client = route
                    .engine
                    .client_for(&connection)
                    .await
                    .map_err(|error| format!("Failed to reach the rclone engine: {error}"))?;
                if let Err(error) = crate::rclone::registry::connect(
                    &route.engine.registry,
                    &client,
                    &connection,
                )
                .await
                {
                    route.engine.release_tunnel(&connection.id).await;
                    return Err(error);
                }
                let mut normalized = arguments.clone();
                if let Some(map) = normalized.as_object_mut() {
                    map.insert("connectionId".to_string(), json!(connection.id));
                }
                Ok(normalized)
            }
            None => {
                let referenced = arguments
                    .get("connectionId")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                match referenced {
                    Some(id)
                        if id != crate::rclone::LOCAL_CONNECTION_ID
                            && !self.mcp.rclone_pooled(id) =>
                    {
                        Err(unknown_connection_guidance(id, None))
                    }
                    Some(_) => Ok(arguments.clone()),
                    None if needs_connection_id(tool) => Err(
                        "Missing required parameter: connectionId — in standalone stdio mode \
                         pass inline connection parameters (\"connection\": {\"protocol\": \
                         \"local\", \"root\": \"/data\"}) or connectionId \"__local__\" for the \
                         built-in local filesystem"
                            .to_string(),
                    ),
                    None => Ok(arguments.clone()),
                }
            }
        }
    }
}

/// JSON-RPC 2.0 id validity: string, number, or null. Objects/arrays/booleans
/// are invalid requests (-32600 tier), never echoed back verbatim.
fn is_valid_jsonrpc_id(id: &Value) -> bool {
    id.is_string() || id.is_number() || id.is_null()
}

/// Fail-closed guidance for a connection reference this stdio session cannot
/// resolve: what failed (the bridge leg reason when a forward was attempted,
/// `None` with the fallback switched off) plus the actionable ways out, named
/// with files' own inline parameter fields (protocol/root/bucket/endpoint/
/// region/accessKeyId/secretAccessKey/secretId/secretKey — see MCP.zh-CN.md「方式二」).
fn unknown_connection_guidance(connection_id: &str, bridge_reason: Option<&str>) -> String {
    let bridge = match bridge_reason {
        Some(reason) => format!("the DBX app bridge is unavailable ({reason})"),
        None => "the DBX app bridge fallback is disabled in this session".to_string(),
    };
    format!(
        "Unknown connectionId '{connection_id}': not pooled in this stdio session and {bridge}. \
         Ways out: re-send the inline connection parameters (\"connection\": {{\"protocol\": \
         \"local\", \"root\": \"/data\"}}, or {{\"protocol\": \"s3\", \"bucket\": \"…\", \
         \"endpoint\": \"…\", \"region\": \"…\", \"accessKeyId\": \"…\", \"secretAccessKey\": \
         \"…\"}}), use connectionId \"__local__\" for the built-in local filesystem, or start \
         the DBX app so its saved connections resolve through the bridge."
    )
}

