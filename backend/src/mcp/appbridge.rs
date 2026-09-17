// ---------------------------------------------------------------------------
// DBX app bridge client (`appbridge`, stdio-only; ssh backend/src/app_bridge.rs
// 结构对齐、ldap backend/internal/mcp/appbridge.go 同族)
// ---------------------------------------------------------------------------
//
// standalone `--mcp` stdio mode has no embedded emitter, so a tool call that
// references a `connectionId` not pooled in this session is forwarded to the
// running DBX app's local TCP bridge: the app listens on `127.0.0.1:<port>`,
// publishes the port in `<app_data_dir>/mcp-bridge-port`, and
// `POST /call-plugin-tool` relays the call to the app's own files sidecar
// (same process as the workbench) — saved connections work without inline
// credentials and credentials never travel in tool arguments.
//
// fail-closed contract: a missing/corrupt port file, an unreachable port or a
// non-200 answer immediately returns an actionable error carrying the shared
// "DBX app bridge" prefix (the caller merges it with the inline-credential
// guidance); no hang, no silent re-dial.
//
// Deliberately dependency-free: a minimal hand-written HTTP/1.1 POST with an
// explicit `Content-Length`, response read to EOF, status line + body split.
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Port discovery file the app writes into its resolved app-data dir
/// (host `mcp_bridge.rs` `MCP_BRIDGE_PORT_FILE`, family-wide).
const PORT_FILE: &str = "mcp-bridge-port";
/// Plugin identity the bridge expects for this sidecar's calls; the app
/// uses it to route the relay to the right plugin sidecar.
const PLUGIN_ID: &str = "io.dbx.files";
/// Fallback app-data location when `DBX_APP_DATA_DIR` is unset (macOS).
const DEFAULT_APP_DATA_SUBPATH: &str = "Library/Application Support/com.dbx.app";
/// Budget for the local TCP hop.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// TCP probe budget guarding against a stale port file.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// The app may run a workbench approval/long digest behind the relay, so
/// the HTTP read must outlast the forwarded tool timeout (contract: read
/// budget >= the forwarded `timeout_ms`).
const READ_MARGIN: Duration = Duration::from_secs(150);
/// The app reads the whole request with a single 64 KiB read, so the
/// request must fit one write inside that window.
const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// Wake budget after a launch attempt (`ensure` keeps polling the port
/// file; files tools have no dedicated wake path so the default is only
/// reached while an app start is genuinely in flight).
pub(super) const DEFAULT_ENSURE_WAIT: Duration = Duration::from_secs(30);

/// `<app_data_dir>` resolution order: env `DBX_APP_DATA_DIR` (non-empty),
/// then `$HOME/Library/Application Support/com.dbx.app`.
fn default_app_data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("DBX_APP_DATA_DIR")
        .map(PathBuf::from)
        .filter(|dir| !dir.as_os_str().is_empty())
    {
        return Some(dir);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(DEFAULT_APP_DATA_SUBPATH))
}

/// Reads the bridge port the app published (decimal, whitespace-tolerant).
/// A missing or corrupted file yields `None` — the port file outlives a
/// killed app, so the port is never guessed.
pub(super) fn published_port(app_data_dir: Option<&Path>) -> Option<u16> {
    let dir = match app_data_dir {
        Some(dir) => dir.to_path_buf(),
        None => default_app_data_dir()?,
    };
    let text = std::fs::read_to_string(dir.join(PORT_FILE)).ok()?;
    text.trim().parse::<u16>().ok().filter(|port| *port > 0)
}

/// Builds the `/call-plugin-tool` JSON body (snake_case fields, per the
/// bridge contract; five fields exactly).
fn request_body(connection_id: &str, tool: &str, arguments: &Value, timeout_ms: u64) -> Value {
    json!({
        "plugin_id": PLUGIN_ID,
        "connection_id": connection_id,
        "tool": tool,
        "arguments": arguments,
        "timeout_ms": timeout_ms,
    })
}

/// Splits a minimal HTTP response into `(status_code, body)`. No chunked
/// handling: the app writes small bodies with an explicit Content-Length
/// and closes the socket.
fn split_http_response(raw: &str) -> Result<(u16, &str), String> {
    let (head, body) = raw.split_once("\r\n\r\n").ok_or(
        "DBX app bridge returned a malformed HTTP response (no header/body split)",
    )?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or("DBX app bridge returned an unreadable HTTP status line")?;
    Ok((status, body))
}

/// One hand-written POST to the app bridge: connect, single-write the
/// request, read to EOF, split the response. A 200 yields `Ok(body)`;
/// anything else becomes `Err` carrying the shared "DBX app bridge"
/// failure prefix. `read_timeout_hint` explains what may still run behind
/// the wait.
async fn post(path: &str, body: Vec<u8>, read_budget: Duration) -> Result<String, String> {
    // Fast-fail port lookup: `ensure` has usually verified the port just
    // before, so a missing file here means the app vanished mid-call.
    let Some(port) = published_port(None) else {
        return Err(
            "DBX app bridge port not found: the DBX app has not published mcp-bridge-port"
                .to_string(),
        );
    };
    let mut request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);
    if request.len() > MAX_REQUEST_BYTES {
        return Err(format!(
            "DBX app bridge request is {} bytes, above the {MAX_REQUEST_BYTES}-byte single-write ceiling",
            request.len()
        ));
    }

    let mut stream =
        tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(("127.0.0.1", port)))
            .await
            .map_err(|_| format!("DBX app bridge connect to 127.0.0.1:{port} timed out"))?
            .map_err(|error| {
                format!("DBX app bridge connect to 127.0.0.1:{port} failed: {error}")
            })?;
    stream
        .write_all(&request)
        .await
        .map_err(|error| format!("DBX app bridge write failed: {error}"))?;

    // Read to EOF: the app closes the socket after answering, and the
    // budget belongs to the route (forwarded tool timeout + margin).
    let mut raw = Vec::new();
    match tokio::time::timeout(read_budget, stream.read_to_end(&mut raw)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => return Err(format!("DBX app bridge read failed: {error}")),
        Err(_) => {
            return Err(format!(
                "DBX app bridge read timed out after {read_budget:?} \
                 (a long digest or a workbench approval may still be running)"
            ))
        }
    }
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let (status, body) = split_http_response(&raw)?;
    if status == 200 {
        return Ok(body.trim().to_string());
    }
    Err(format!(
        "DBX app bridge returned HTTP {status}: {}",
        body.trim()
    ))
}

/// Forwards one tool call through the app bridge. The 200 body is the
/// app's `mcp/call` result (already MCP-content wrapped) and is returned
/// verbatim; any other outcome becomes `Err` with the "DBX app bridge"
/// prefix.
pub(super) async fn call_plugin_tool(
    connection_id: &str,
    tool: &str,
    arguments: Value,
    timeout: Duration,
) -> Result<Value, String> {
    let body = serde_json::to_vec(&request_body(
        connection_id,
        tool,
        &arguments,
        timeout.as_millis() as u64,
    ))
    .map_err(|error| format!("Failed to encode the DBX app bridge request: {error}"))?;
    let text = post("/call-plugin-tool", body, timeout + READ_MARGIN).await?;
    serde_json::from_str::<Value>(&text)
        .map_err(|error| format!("DBX app bridge returned invalid JSON: {error}"))
}

/// TCP probe proving something is actually listening on the published
/// port: the port file outlives a killed app and would otherwise hand out
/// a stale port forever.
async fn alive_port() -> Option<u16> {
    let port = published_port(None)?;
    let target = ("127.0.0.1", port);
    match tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect(target)).await {
        Ok(Ok(stream)) => {
            drop(stream);
            Some(port)
        }
        _ => None,
    }
}

/// Best-effort app launch (`DBX_APP_LAUNCH_CMD` override, else macOS
/// `open -a DBX.app`); failure is not fatal because the port-file poll
/// below is the source of truth.
fn launch_app() {
    let launch =
        std::env::var("DBX_APP_LAUNCH_CMD").ok().filter(|cmd| !cmd.trim().is_empty());
    let mut command = match launch {
        Some(cmd) => {
            let mut command = std::process::Command::new("sh");
            command.arg("-c").arg(cmd);
            command
        }
        None => {
            let mut command = std::process::Command::new("open");
            command.args(["-a", "DBX.app"]);
            command
        }
    };
    let _ = command.spawn();
}

/// Waits for a reachable app bridge: verifies immediately when present,
/// otherwise wakes the app and re-reads + re-probes the port file every
/// 500 ms until `wait` elapses, so a relaunched app's fresh port is
/// picked up. The timeout error carries the shared "DBX app bridge"
/// prefix so callers (and smoke tests) can grep one actionable marker.
pub(super) async fn ensure(wait: Duration) -> Result<u16, String> {
    if let Some(port) = alive_port().await {
        return Ok(port);
    }
    launch_app();
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Some(port) = alive_port().await {
            return Ok(port);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "DBX app bridge unreachable after {}s: no reachable mcp-bridge-port; \
                 start the DBX app and retry",
                wait.as_secs()
            ));
        }
    }
}

/// Test-only in-process mock of the DBX app bridge (ssh/ldap smoke 的
/// stub-app 同构): binds 127.0.0.1:0, answers every parsed request with a
/// fixed `(status, body)`, and records each JSON body. `ensure`'s TCP
/// probe connects without sending data — those connections are skipped.
#[cfg(test)]
pub(super) fn spawn_mock_bridge(
    status: u16,
    body: &'static str,
) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = std::sync::Arc::clone(&calls);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut raw: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 4096];
            let split = loop {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break None,
                    Ok(n) => {
                        raw.extend_from_slice(&chunk[..n]);
                        if let Some(position) =
                            raw.windows(4).position(|window| window == b"\r\n\r\n")
                        {
                            let length: usize = String::from_utf8_lossy(&raw[..position])
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .and_then(|value| value.trim().parse().ok())
                                })
                                .unwrap_or(0);
                            if raw.len() >= position + 4 + length {
                                break Some(position + 4);
                            }
                        }
                    }
                }
            };
            // ensure 的探测连接不带数据：跳过（不是一次转发调用）。
            let Some(body_start) = split else { continue };
            let text = String::from_utf8_lossy(&raw[body_start..]).to_string();
            if let Ok(value) = serde_json::from_str::<Value>(text.trim()) {
                recorded.lock().unwrap().push(value);
            }
            let response = format!(
                "HTTP/1.1 {status} MOCK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    });
    (port, calls)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_port_parses_trimmed_decimal_ports() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PORT_FILE);
        std::fs::write(&path, "49152\n").unwrap();
        assert_eq!(published_port(Some(dir.path())), Some(49152));
        std::fs::write(&path, "  54321  ").unwrap();
        assert_eq!(published_port(Some(dir.path())), Some(54321));
    }

    #[test]
    fn published_port_rejects_garbage_and_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file.
        assert_eq!(published_port(Some(dir.path())), None);
        let path = dir.path().join(PORT_FILE);
        for garbage in ["not-a-port", "", "99999", "0", "49152.5"] {
            std::fs::write(&path, garbage).unwrap();
            assert_eq!(published_port(Some(dir.path())), None, "garbage: {garbage}");
        }
    }

    #[test]
    fn request_body_carries_every_snake_case_contract_field() {
        let body = request_body(
            "conn-1",
            "files_scan_digest",
            &json!({ "path": "/data" }),
            300_000,
        );
        let object = body.as_object().unwrap();
        for key in ["plugin_id", "connection_id", "tool", "arguments", "timeout_ms"] {
            assert!(object.contains_key(key), "missing {key}");
        }
        assert_eq!(object.len(), 5);
        assert_eq!(object["plugin_id"], "io.dbx.files");
        assert_eq!(object["connection_id"], "conn-1");
        assert_eq!(object["tool"], "files_scan_digest");
        assert_eq!(object["arguments"]["path"], "/data");
        assert_eq!(object["timeout_ms"], 300_000);
    }

    #[test]
    fn split_http_response_extracts_status_and_body() {
        let (status, body) = split_http_response(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 5\r\n\r\nhello",
        )
        .unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "hello");

        let (status, body) = split_http_response(
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\n\r\nno such route",
        )
        .unwrap();
        assert_eq!(status, 404);
        assert_eq!(body, "no such route");

        // Malformed inputs are refused instead of mis-parsed.
        assert!(split_http_response("garbage without a blank line").is_err());
        assert!(split_http_response("HTTP/1.1 notastatus\r\n\r\nx").is_err());
        assert!(split_http_response("").is_err());
    }

    // ---- offline ensure / forward contract tests -----------------------

    /// Serializes tests that mutate the process-global app-data env vars
    /// (cargo runs tests on parallel threads; the lock is local to the
    /// appbridge tests so this module stays self-contained).
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn ensure_fails_closed_when_the_bridge_is_unpublished() {
        let _guard = env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("DBX_APP_DATA_DIR", dir.path());
        std::env::set_var("DBX_APP_LAUNCH_CMD", ":");
        let started = std::time::Instant::now();
        let error = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ensure(Duration::from_millis(300)))
            .unwrap_err();
        std::env::remove_var("DBX_APP_DATA_DIR");
        std::env::remove_var("DBX_APP_LAUNCH_CMD");
        assert!(
            error.contains("DBX app bridge unreachable"),
            "fail-closed error: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "ensure must respect the wait budget, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn ensure_picks_up_a_published_port_without_burning_the_budget() {
        let _guard = env_guard();
        let (port, _calls) = spawn_mock_bridge(200, "{}");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PORT_FILE), port.to_string()).unwrap();
        std::env::set_var("DBX_APP_DATA_DIR", dir.path());
        std::env::set_var("DBX_APP_LAUNCH_CMD", ":");
        let started = std::time::Instant::now();
        let found = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ensure(Duration::from_millis(300)))
            .unwrap();
        std::env::remove_var("DBX_APP_DATA_DIR");
        std::env::remove_var("DBX_APP_LAUNCH_CMD");
        assert_eq!(found, port);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "an already-published port must return immediately, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn call_plugin_tool_forwards_contract_and_envelope() {
        let _guard = env_guard();
        let envelope = r#"{"content":[{"type":"text","text":"{\"matched\":7}"}],"isError":false}"#;
        let (port, calls) = spawn_mock_bridge(200, envelope);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PORT_FILE), port.to_string()).unwrap();
        std::env::set_var("DBX_APP_DATA_DIR", dir.path());

        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(call_plugin_tool(
                "saved-1",
                "files_scan_digest",
                json!({ "connectionId": "saved-1", "path": "/data" }),
                Duration::from_secs(2),
            ));
        std::env::remove_var("DBX_APP_DATA_DIR");

        let result = result.unwrap();
        // 200 envelope 逐字（content/isError 形状不被再包一层）。
        assert_eq!(result["isError"], false, "{result}");
        assert!(result["content"][0]["text"].is_string(), "{result}");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "{calls:?}");
        let body = &calls[0];
        assert_eq!(body["plugin_id"], "io.dbx.files", "{body}");
        assert_eq!(body["connection_id"], "saved-1", "{body}");
        assert_eq!(body["tool"], "files_scan_digest", "{body}");
        assert_eq!(body["arguments"]["path"], "/data", "{body}");
        assert_eq!(body["timeout_ms"], 2_000, "{body}");

        // 非 200：错误带 "DBX app bridge returned HTTP" 前缀与宿主错误体。
        let (bad_port, _bad_calls) = spawn_mock_bridge(
            404,
            r#"{"error":"Connection with id 'x' not found"}"#,
        );
        let bad_dir = tempfile::tempdir().unwrap();
        std::fs::write(bad_dir.path().join(PORT_FILE), bad_port.to_string()).unwrap();
        std::env::set_var("DBX_APP_DATA_DIR", bad_dir.path());
        let error = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(call_plugin_tool("x", "files_scan_digest", json!({}), Duration::from_secs(1)))
            .unwrap_err();
        std::env::remove_var("DBX_APP_DATA_DIR");
        assert!(
            error.contains("DBX app bridge returned HTTP 404"),
            "non-200 must fail closed: {error}"
        );

        // 非法 JSON：明确报错。
        let (junk_port, _junk) = spawn_mock_bridge(200, "not json");
        let junk_dir = tempfile::tempdir().unwrap();
        std::fs::write(junk_dir.path().join(PORT_FILE), junk_port.to_string()).unwrap();
        std::env::set_var("DBX_APP_DATA_DIR", junk_dir.path());
        let error = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(call_plugin_tool("x", "files_scan_digest", json!({}), Duration::from_secs(1)))
            .unwrap_err();
        std::env::remove_var("DBX_APP_DATA_DIR");
        assert!(
            error.contains("DBX app bridge returned invalid JSON"),
            "invalid JSON must fail closed: {error}"
        );
    }
}
