//! MCP tool surface for `io.dbx.files` (shared/IMPL_PLAN_PLUGIN_MCP §2/§3/§4/§6.2).
//!
//! Skeleton mirrors the ssh plugin's `mcp.rs`: the DBX MCP bridge calls the
//! sidecar methods `mcp/tools` (discovery), `mcp/call` (execution, with the
//! connection lifecycle payload forwarded by the host) and
//! `mcp/settings/get|set` (operator-tunable limits). Tool naming follows the
//! `<domain>_<action>` family (`files_scan_digest`, `files_ui_focus`, ...).
//!
//! Layers implemented here (design §0/§3/§4):
//! - **UI intents**: `files_ui_focus` / `files_ui_search` / `files_ui_select`
//!   emit a `files/ui/intent` sidecar event, register the intent in an
//!   in-process state table (TTL 60s, LRU 20) and wait for the frontend
//!   `files/ui/state/report` call (default 5s, tunable). No frontend →
//!   `{intentId, state:"pending", hint}` — the degradation matrix never
//!   blocks the caller. `files_ui_state` reads an intent result or the
//!   latest snapshot report.
//! - **Local read**: `files_scan_digest` walks the tree in the sidecar
//!   (depth/entry caps), applies glob/size/mtime predicates locally and
//!   returns only the aggregate (digest format by default); `files_cursor_next`
//!   pages the materialized locator rows (paths) from a TTL/LRU session.
//! - **Two-phase writes**: `files_delete` / `files_purge` refuse to execute
//!   without a one-time `confirmToken` (60s TTL, parameter-hash bound) and
//!   answer the first call with a preview instead. `files_write` /
//!   `files_mkdir` / `files_rename` execute directly. Every MCP write is
//!   audited with `source:"mcp"`.
//! - **Directory sync**: `files_sync` delegates to the workbench
//!   `files/syncDir|copyDir` enqueue (incremental size+mtime compare; the
//!   optional mirror delete is gated by the target's `allow_delete`;
//!   `dryRun` plans only, `maxDelete` bounds deletions) and answers with a
//!   `jobId` to poll via `files/transfer/status`. The async job needs the
//!   DBX event channel, so standalone stdio answers an explicit refusal.
//! - **Token economy**: single-response cap 16 KiB (truncated flag), cell
//!   width 120 (locator fields never truncated), rows format clamped at 20,
//!   binary content never leaves the sidecar over MCP.

use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use dbx_plugin_sdk::PluginEmitter;
use serde_json::{json, Value};

use crate::engine::{ops, Engine};
use crate::model::{StoredConnection, MAX_INLINE_WRITE_BYTES};
use crate::store::Store;
use crate::transfers::{DirJobKind, JobTable};

// ---------------------------------------------------------------------------
// Constants (design §3 "硬上限")
// ---------------------------------------------------------------------------

/// Default single-response cap; `mcp/settings/set` may lower it freely or
/// raise it to its ceiling but never disable it.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 16 * 1024;
/// Default per-cell width; locator fields (`path` / `intentId` / `cursorId`)
/// are never truncated (design §3 "定位字段不截断").
pub const DEFAULT_CELL_WIDTH: usize = 120;
/// Default wait for the frontend `ui/state/report` answer (design §1).
pub const DEFAULT_REPORT_WAIT_MILLIS: u64 = 5_000;
/// Default materialized cursor rows (design §3 "上限 1 万条").
pub const DEFAULT_MAX_CURSOR_ROWS: usize = 10_000;
/// Default cursor session TTL, 10 minutes (design §3).
pub const DEFAULT_CURSOR_TTL_SECS: u64 = 600;
/// Default cursor LRU capacity (design §3 "LRU ≤8 会话").
pub const DEFAULT_MAX_CURSOR_SESSIONS: usize = 8;
/// Default confirmToken TTL, 60 seconds (design §4).
pub const DEFAULT_CONFIRM_TTL_SECS: u64 = 60;

/// Scan budget (design §6.2: "depth 8 / 10 万条 clamp").
pub const DEFAULT_SCAN_DEPTH: u32 = 8;
pub const MAX_SCAN_DEPTH: u32 = 16;
pub const MAX_SCAN_ENTRIES: usize = 100_000;

const GROUP_LIMIT: usize = 20;
const TOP_N: usize = 10;
const SAMPLE_ROWS: usize = 5;
const ROWS_FORMAT_LIMIT: usize = 20;
/// Digest shape limits are settings-driven (`digestGroupLimit` / `digestTopN` /
/// `digestSampleRows` / `digestRowLimit`); these defaults match the design
/// hard caps and are also the `mcp/settings/set` ceilings (ldap `Settings`
/// shape parity — AGENTS.md 硬性规则「同族参数一致」).
const CURSOR_PAGE: u64 = 20;
/// Rows returned per page never exceed this, mirroring `n<=20`.
pub const MAX_CURSOR_PAGE: u64 = 20;
const QUICK_PATHS_LIMIT: u64 = 50;

/// Intent state table budgets (design §1: TTL 60s, 最近 20 条).
const INTENT_TTL_MILLIS: u128 = 60_000;
const INTENT_MAX_ENTRIES: usize = 20;

/// MCP advisory write budget (design §6.2: 建议 ≤1 MiB, hard cap stays 4 MiB).
pub const MCP_RECOMMENDED_WRITE_BYTES: usize = 1024 * 1024;

const INTENT_EVENT: &str = "files/ui/intent";
const PENDING_HINT: &str = "workbench not open or frontend did not respond; \
 fall back to files_scan_digest (re-check later with files_ui_state)";
const MCP_SOURCE: &str = "mcp";

/// Upper bounds for `mcp/settings/set` (ssh `McpLimits::sanitized` pattern,
/// values aligned with the ldap `settingsFields` table): a setting may be
/// lowered freely or raised up to its ceiling, never beyond. The digest limits
/// are exactly the design hard caps (组数 ≤20、topN ≤10、样本 ≤5、rows ≤20).
const MAX_RESPONSE_BYTES_CEILING: usize = 1024 * 1024;
const CELL_WIDTH_CEILING: usize = 2_000;
const REPORT_WAIT_MILLIS_CEILING: u64 = 30_000;
const MAX_CURSOR_ROWS_CEILING: usize = 100_000;
const CURSOR_TTL_SECS_CEILING: u64 = 3_600;
const MAX_CURSOR_SESSIONS_CEILING: usize = 32;
const CONFIRM_TTL_SECS_CEILING: u64 = 600;

mod state;

pub use state::Mcp;
use state::IntentLookup;


// ---------------------------------------------------------------------------
// Tool dispatch (`mcp/call`)
// ---------------------------------------------------------------------------

/// Write tools are excluded from `mcp/tools` for read-only connections
/// (design §4 "而非注册了再报错") and re-gated at execution time.
#[cfg_attr(not(test), allow(dead_code))] // 剔除清单由 tools 清单过滤逻辑消费，测试外仅作契约常量
pub const WRITE_TOOLS: [&str; 5] = [
    "files_write",
    "files_mkdir",
    "files_rename",
    "files_delete",
    "files_purge",
];

/// UI-intent tool family (design §2); referenced from the protocol docs and
/// unit tests.
#[allow(dead_code)]
pub const UI_TOOLS: [&str; 4] = [
    "files_ui_focus",
    "files_ui_search",
    "files_ui_select",
    "files_ui_state",
];

impl Mcp {
    /// `mcp/call` entry used by the DBX MCP bridge: registers the forwarded
    /// connection lifecycle payload (so `connectionId` resolves like any
    /// workbench connection), then dispatches the tool. Returns the MCP
    /// content envelope with the 16 KiB token-economy cap applied.
    pub async fn call(
        &self,
        engine: &Engine,
        transfers: &JobTable,
        store: &Store,
        emitter: &PluginEmitter,
        params: &Value,
    ) -> Result<Value, String> {
        let tool = required_str(params, "tool")?;
        let mut arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !arguments.is_object() {
            return Err("arguments must be a JSON object".to_string());
        }
        if let Some(lifecycle) = params.get("lifecycle") {
            let connection = StoredConnection::from_lifecycle_params(lifecycle)?;
            engine.connect(connection).map_err(|error| {
                format!("Failed to register the forwarded connection lifecycle: {error}")
            })?;
            if let Some(map) = arguments.as_object_mut() {
                map.entry("connectionId".to_string())
                    .or_insert_with(|| json!(lifecycle_id(lifecycle)));
            }
        }
        let mut payload = self
            .run_tool(
                tool,
                &arguments,
                engine,
                transfers,
                store,
                Some(emitter),
            )
            .await?;
        Ok(self.finalize_payload(&mut payload))
    }

    /// Shared response post-processing: the 16 KiB token-economy cap plus the
    /// MCP content envelope. Used by the DBX bridge `mcp/call` and the
    /// standalone stdio entry so both transports emit identical tool results.
    fn finalize_payload(&self, payload: &mut Value) -> Value {
        let settings = self.current_settings();
        cap_response(payload, settings.response_limit_bytes, settings.cell_width);
        content_envelope(payload)
    }

    async fn run_tool(
        &self,
        tool: &str,
        arguments: &Value,
        engine: &Engine,
        transfers: &JobTable,
        store: &Store,
        emitter: Option<&PluginEmitter>,
    ) -> Result<Value, String> {
        match tool {
            // -- UI intent tools (design §1/§6.2) --------------------------------
            "files_ui_focus" => self
                .ui_intent_tool(emitter, tool, arguments, |arguments| {
                    missing_required(arguments, &["panel"])?;
                    let panel = required_str(arguments, "panel")?;
                    if !matches!(panel, "browse" | "transfers" | "audit") {
                        return Err("panel must be one of: browse, transfers, audit".to_string());
                    }
                    Ok(json!({
                        "panel": panel,
                        "connectionId": arguments.get("connectionId")
                            .and_then(Value::as_str).unwrap_or_default(),
                    }))
                })
                .await,
            "files_ui_search" => self
                .ui_intent_tool(emitter, tool, arguments, |arguments| {
                    missing_required(arguments, &["path"])?;
                    let path = required_str(arguments, "path")?;
                    // Navigation intent: fill the path field and trigger the
                    // workbench listPaged pipeline (design §6.2).
                    Ok(json!({
                        "path": path,
                        "trigger": "listPaged",
                        "connectionId": arguments.get("connectionId")
                            .and_then(Value::as_str).unwrap_or_default(),
                    }))
                })
                .await,
            "files_ui_select" => self
                .ui_intent_tool(emitter, tool, arguments, |arguments| {
                    missing_required(arguments, &["path"])?;
                    let path = required_str(arguments, "path")?;
                    Ok(json!({
                        "path": path,
                        "connectionId": arguments.get("connectionId")
                            .and_then(Value::as_str).unwrap_or_default(),
                    }))
                })
                .await,
            "files_ui_state" => {
                let now = unix_millis_now() as u128;
                if let Some(id) = arguments
                    .get("intentId")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    return match self.intent_lookup(id, now) {
                        IntentLookup::Found(state) => Ok(state),
                        IntentLookup::Expired => {
                            Ok(json!({ "intentId": id, "state": "expired" }))
                        }
                        IntentLookup::Unknown => Err(format!(
                            "unknown intentId: {id} (intents are per-process and expire after \
                             60s); re-issue the files_ui_* call, or omit intentId to read the \
                             latest workbench snapshot"
                        )),
                    };
                }
                // Without intentId: the latest workbench snapshot (ldap
                // `uiState` shape: `{snapshot: <summary or null>}`).
                let snapshot = self
                    .snapshot
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .clone();
                return Ok(json!({ "snapshot": snapshot.unwrap_or(Value::Null) }));
            }

            // -- local read (design §3) ------------------------------------------
            "files_scan_digest" => self.scan_digest_tool(engine, arguments).await,
            "files_cursor_next" => self.cursor_next(arguments),
            "files_ui_quick_paths" => {
                let connection_id = required_str(arguments, "connectionId")?;
                let operator = engine.operator(connection_id)?;
                let connection = engine.connection(connection_id)?;
                let payload = ops::quick_paths(&operator, &connection).await?;
                let limit = numeric_arg_or(arguments, "limit", QUICK_PATHS_LIMIT)?
                    .clamp(1, QUICK_PATHS_LIMIT) as usize;
                let all = payload
                    .get("paths")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                Ok(json!({ "paths": &all[..limit.min(all.len())] }))
            }

            // -- writes (design §4) ----------------------------------------------
            "files_write" => {
                missing_required(arguments, &["path", "dataBase64"])?;
                let connection = engine.connection(required_str(arguments, "connectionId")?)?;
                ensure_writable(&connection)?;
                let path = file_target_path(required_str(arguments, "path")?, "path")?;
                // An empty string is a legitimate payload: it creates an empty
                // file (the workbench write path allows it too). Only a
                // missing/non-string value is a parameter error.
                let data_base64 = match arguments.get("dataBase64") {
                    Some(Value::String(text)) => text.as_str(),
                    _ => {
                        return Err(
                            "Parameter 'dataBase64' must be a base64 string (an empty string \
                             creates an empty file)"
                                .to_string(),
                        )
                    }
                };
                let data = BASE64_STANDARD
                    .decode(data_base64.as_bytes())
                    .map_err(|error| {
                        format!(
                            "Invalid base64 in dataBase64: {error}; dataBase64 must be standard \
                             base64 (RFC 4648, no data-URI prefix)"
                        )
                    })?;
                if data.len() > MAX_INLINE_WRITE_BYTES {
                    return Err(format!(
                        "Inline write payload of {} bytes exceeds {}; use the upload channel \
                         or the workbench transfer pane",
                        data.len(),
                        MAX_INLINE_WRITE_BYTES
                    ));
                }
                let operator = engine.operator(&connection.id)?;
                let bytes = data.len();
                ops::write(&operator, &path, data).await?;
                audit_mcp(store, &connection, "files/write", &path, "ok");
                let mut result = json!({ "success": true, "path": path, "bytes": bytes });
                if bytes > MCP_RECOMMENDED_WRITE_BYTES {
                    result["hint"] = json!(
                        "Payload above the MCP-recommended 1 MiB; prefer the workbench transfer \
                         pane or the upload channel for larger files"
                    );
                }
                Ok(result)
            }
            "files_mkdir" => {
                missing_required(arguments, &["path"])?;
                let connection = engine.connection(required_str(arguments, "connectionId")?)?;
                ensure_writable(&connection)?;
                let raw = required_str(arguments, "path")?;
                validate_path_shape(raw, "path")?;
                let operator = engine.operator(&connection.id)?;
                ops::mkdir(&operator, raw).await?;
                audit_mcp(store, &connection, "files/mkdir", raw, "ok");
                Ok(json!({ "success": true, "path": raw }))
            }
            "files_rename" => {
                missing_required(arguments, &["path", "newPath"])?;
                let connection = engine.connection(required_str(arguments, "connectionId")?)?;
                ensure_writable(&connection)?;
                // Rename removes the source path — same delete gate as the
                // workbench path (main.rs files/rename).
                ensure_deletable(&connection)?;
                let path = file_target_path(required_str(arguments, "path")?, "path")?;
                let new_path = file_target_path(required_str(arguments, "newPath")?, "newPath")?;
                let operator = engine.operator(&connection.id)?;
                let source_is_dir = ops::is_dir_path(&operator, &path).await?;
                if source_is_dir {
                    // Directory rename degrades to the async copy+delete job,
                    // identical to the workbench path. The job's progress
                    // events need the host event channel: stdio mode answers
                    // with explicit guidance instead of enqueueing a job no
                    // one can observe.
                    let Some(emitter) = emitter else {
                        return Err(
                            "Directory rename runs as an async progress job over the DBX event \
                             channel, which is unavailable in standalone stdio mode; rename \
                             files individually or use the DBX workbench"
                                .to_string(),
                        );
                    };
                    let job_id = transfers
                        .enqueue_copy_job(
                            &connection,
                            &operator,
                            &connection,
                            &operator,
                            &path,
                            &new_path,
                            true,
                            DirJobKind::Rename,
                            engine.backend_identity(&connection.id, &connection.id)?,
                            emitter,
                        )
                        .await?;
                    audit_mcp(store, &connection, "files/rename", &path, "ok");
                    return Ok(json!({
                        "success": true,
                        "transport": "job",
                        "jobId": job_id,
                        "path": path,
                        "newPath": new_path,
                        "hint": "Directory rename runs as an async job; poll files/transfer/status",
                    }));
                }
                ops::rename(&operator, &path, &new_path).await?;
                audit_mcp(store, &connection, "files/rename", &path, "ok");
                Ok(json!({ "success": true, "transport": "native", "path": path, "newPath": new_path }))
            }
            "files_delete" | "files_purge" => {
                missing_required(arguments, &["path"])?;
                let connection = engine.connection(required_str(arguments, "connectionId")?)?;
                ensure_deletable(&connection)?;
                let path = required_str(arguments, "path")?;
                validate_path_shape(path, "path")?;
                if tool == "files_purge" {
                    refuse_root_purge(&connection, path)?;
                }
                let operator = engine.operator(&connection.id)?;
                // Preview data is collected before any deletion so the caller
                // sees what WOULD be deleted; nothing is written on this arm.
                let preview = match ops::stat(&operator, path).await {
                    Ok(entry) => json!({
                        "tool": tool,
                        "connectionId": connection.id,
                        "path": path,
                        "kind": entry.kind,
                        "size": entry.size,
                    }),
                    // OpenDAL delete is idempotent: a missing path is previewed
                    // as absent instead of failing the flow.
                    Err(_) => json!({
                        "tool": tool,
                        "connectionId": connection.id,
                        "path": path,
                        "kind": "missing",
                    }),
                };
                // Execution spelling is derived from the preview kind: dir
                // markers keep their trailing `/`, file paths drop one — the
                // canonical target never silently no-ops an LLM-typo'd path.
                let delete_target =
                    canonical_delete_target(path, preview.get("kind").and_then(Value::as_str));
                let Some(token) = arguments
                    .get("confirmToken")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                else {
                    // Two-phase stage 1: preview + one-time token, no write.
                    let (confirm_token, expires_at) = self.confirm_begin(&arguments_without_token(arguments));
                    return Ok(json!({
                        "preview": preview,
                        "confirmToken": confirm_token,
                        "expiresAt": iso_millis(expires_at as i64),
                        "note": "nothing written yet; repeat the same arguments with \
                                 confirmToken to execute",
                    }));
                };
                // Stage 2: token + unchanged parameters → execute.
                self.confirm_verify(token, &arguments_without_token(arguments))?;
                let action = if tool == "files_delete" {
                    ops::delete(&operator, &delete_target).await?;
                    "files/delete"
                } else {
                    ops::purge(&operator, &delete_target).await?;
                    "files/purge"
                };
                audit_mcp(store, &connection, action, path, "ok");
                Ok(json!({ "success": true, "path": path }))
            }

            // -- directory sync (§8.4 via MCP) -------------------------------------
            "files_sync" => {
                // Parameter enumeration, flag tolerance, path shapes and the
                // target write/mirror-delete gates all run before anything
                // async is touched ([`parse_sync_request`], unit-tested pure).
                let request = parse_sync_request(engine, arguments)?;
                // The dir job reports through the DBX event channel +
                // `files/transfer/status`; standalone stdio has neither, so it
                // answers with explicit guidance instead of enqueueing a job
                // nobody can observe (files_rename dir-path parity).
                let Some(emitter) = emitter else {
                    return Err(SYNC_STDIO_UNAVAILABLE.to_string());
                };
                let source_operator = engine.operator(&request.source.id)?;
                let target_operator = engine.operator(&request.target.id)?;
                let job_id = transfers
                    .enqueue_dir_job(
                        &request.source,
                        &source_operator,
                        &request.target,
                        &target_operator,
                        &request.source_path,
                        &request.target_path,
                        request.sync,
                        request.dry_run,
                        request.max_delete,
                        engine.backend_identity(&request.source.id, &request.target.id)?,
                        emitter,
                    )
                    .await?;
                audit_mcp(
                    store,
                    &request.target,
                    "files/sync",
                    &format!(
                        "{}:{} -> {}",
                        request.source.id, request.source_path, request.target_path
                    ),
                    "ok",
                );
                let mut result = json!({
                    "success": true,
                    "transport": "job",
                    "jobId": job_id,
                    "sourceConnectionId": request.source.id,
                    "sourcePath": request.source_path,
                    "targetConnectionId": request.target.id,
                    "targetPath": request.target_path,
                    "sync": request.sync,
                    "dryRun": request.dry_run,
                });
                if let Some(max_delete) = request.max_delete {
                    result["maxDelete"] = json!(max_delete);
                }
                result["hint"] = if request.dry_run {
                    // Dry run: the plan/compare phase emits ONE summary event
                    // (files/transfer/progress with toCopy/toDelete counts and
                    // path samples) and the job completes without writes.
                    json!("Dry run: nothing is copied or deleted; the job plans/compares, \
                           emits one summary event and completes — poll files/transfer/status \
                           for the counts")
                } else {
                    json!("Async directory job enqueued; poll files/transfer/status (or the \
                           workbench transfers pane) for progress")
                };
                if request.sync && !request.dry_run {
                    // The destructive half of mirror semantics, spelled out in
                    // the response too (the tool description carries it as
                    // well) — an LLM must never learn about deletions only
                    // after they happened.
                    result["warning"] = json!(
                        "sync=true mirrors the source: files present on the target but missing \
                         from the source are DELETED (bounded by maxDelete when set)"
                    );
                }
                Ok(result)
            }

            other => Err(unknown_tool_message(other)),
        }
    }

    /// Shared UI-intent flow (design §1): validate → register pending intent →
    /// emit `files/ui/intent` → wait for the frontend report → applied /
    /// rejected / pending(timeout). The wait is the degradation matrix's
    /// "工作台未打开 / 前端未响应" cell: never an error, always `pending` + hint.
    async fn ui_intent_tool<F>(
        &self,
        emitter: Option<&PluginEmitter>,
        tool: &str,
        arguments: &Value,
        build_params: F,
    ) -> Result<Value, String>
    where
        F: Fn(&Value) -> Result<Value, String>,
    {
        // Standalone stdio never reaches this arm (the stdio entry answers the
        // UNAVAILABLE short-circuit first); the bridge path always carries an
        // emitter. The defensive error keeps the signature honest.
        let emitter = emitter.ok_or_else(|| {
            "UI intent tools require the DBX workbench event channel".to_string()
        })?;
        let intent_params = build_params(arguments)?;
        let action = match tool {
            "files_ui_focus" => "focus",
            "files_ui_search" => "search",
            "files_ui_select" => "select",
            _ => "unknown",
        };
        let intent_id = format!("i-{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
        let now = unix_millis_now() as u128;
        self.register_intent(&intent_id, action, intent_params.clone(), now);
        let _ = emitter.event(
            INTENT_EVENT,
            json!({
                "intentId": intent_id,
                "action": action,
                "params": intent_params,
            }),
        );
        let settings = self.current_settings();
        let deadline =
            std::time::Instant::now() + Duration::from_millis(settings.report_wait_millis);
        loop {
            tokio::time::sleep(Duration::from_millis(25)).await;
            match self.intent_lookup(&intent_id, unix_millis_now() as u128) {
                IntentLookup::Found(state) if state["state"] != "pending" => return Ok(state),
                IntentLookup::Expired => {
                    return Ok(json!({ "intentId": intent_id, "state": "expired" }))
                }
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
        }
        // Timeout: the degradation matrix's "工作台未打开 / 前端未响应" cell —
        // never an error, always `pending` + hint (ldap `waitIntent` parity).
        Ok(json!({
            "intentId": intent_id,
            "state": "pending",
            "hint": PENDING_HINT,
        }))
    }

    // -- local read: files_scan_digest (design §3/§6.2) -----------------------

    async fn scan_digest_tool(&self, engine: &Engine, arguments: &Value) -> Result<Value, String> {
        let connection_id = required_str(arguments, "connectionId")?;
        let operator = engine.operator(connection_id)?;
        let start = normalize_slashes(match optional_str(arguments, "path")? {
            Some(raw) => raw,
            None => "/",
        });
        validate_path_shape(&start, "path")?;
        // depth clamps into its range (LLM-friendly), but a non-numeric value
        // must fail with the legal range spelled out (MCP_ACCEPTANCE §3.3:
        // 报错列合法值), not just "non-negative integer".
        let depth = numeric_arg_or(arguments, "depth", u64::from(DEFAULT_SCAN_DEPTH))
            .map_err(|error| format!("{error}; depth accepts an integer in 1..={MAX_SCAN_DEPTH}"))?
            .clamp(1, u64::from(MAX_SCAN_DEPTH)) as u32;
        let filter = ScanFilter::from_arguments(arguments)?;
        let format = normalized_format(optional_str(arguments, "format")?)?;
        // The walk stays fully inside the sidecar: raw entries are aggregated
        // and only the locator fields of matched rows are retained.
        let settings = self.current_settings();
        let mut walk = WalkState::new(filter);
        walk_subtree(&operator, &start, depth, &mut walk, MAX_SCAN_ENTRIES).await?;
        let stats = aggregate_rows(&walk.matched, &DigestLimits {
            group_limit: settings.digest_group_limit,
            top_n: settings.digest_top_n,
        });
        let (cursor_id, cursor_truncated) = self.cursor_put(
            walk.matched
                .iter()
                .map(|row| row.path.clone())
                .collect(),
        );
        let mut payload = json!({
            "connectionId": connection_id,
            "path": start,
            "matched": walk.matched.len(),
            "scanned": walk.scanned,
            "scanTruncated": walk.truncated,
            "cursorId": cursor_id,
        });
        match format.as_str() {
            "rows" => {
                payload["rows"] =
                    json!(take_rows(&walk.matched, settings.digest_row_limit));
            }
            _ => {
                payload["sample"] =
                    json!(take_rows(&walk.matched, settings.digest_sample_rows));
                payload["stats"] = json!({
                    "totalBytes": stats.total_bytes,
                    "byExtension": stats.by_extension,
                    "largest": stats.largest,
                    "newest": stats.newest,
                });
            }
        }
        if cursor_truncated {
            payload["cursorTruncated"] = json!(true);
        }
        Ok(payload)
    }
}

/// Digest output format, case-normalized (MCP_ACCEPTANCE §3.3 枚举大小写归一):
/// `"ROWS"`, `" Rows "` and `"rows"` all select the rows shape; any other
/// value is refused with the legal values spelled out (never silently
/// downgraded to the digest default).
fn normalized_format(raw: Option<&str>) -> Result<String, String> {
    match raw {
        None => Ok("digest".to_string()),
        Some(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            if !matches!(normalized.as_str(), "digest" | "rows") {
                return Err(format!(
                    "format must be 'digest' or 'rows' (got '{value}'); case-insensitive, \
                     e.g. \"ROWS\""
                ));
            }
            Ok(normalized)
        }
    }
}

impl Mcp {
    /// `mcp/tools` (discovery). `params.lifecycle` / `params.connectionId` are
    /// optional: when they resolve to a read-only connection the write tools
    /// are excluded from the list (design §4 "不注册进工具清单"); without a
    /// resolvable connection all tools are listed and `mcp/call` re-gates.
    pub fn tool_definitions(&self, engine: &Engine, params: &Value) -> Value {
        let read_only = params
            .get("lifecycle")
            .and_then(|lifecycle| StoredConnection::from_lifecycle_params(lifecycle).ok())
            .map(|connection| connection.read_only)
            .or_else(|| {
                params
                    .get("connectionId")
                    .and_then(Value::as_str)
                    .and_then(|id| engine.connection(id).ok())
                    .map(|connection| connection.read_only)
            })
            .unwrap_or(false);
        self.definitions_for(read_only)
    }

    fn definitions_for(&self, read_only: bool) -> Value {
        let mut tools = json!([
            {
                "name": "files_ui_focus",
                "description": "Focus a Files workbench panel (browse | transfers | audit). Requires the DBX workbench to be open; without a frontend the call reports state=pending with a hint, fall back to files_scan_digest instead.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "connectionId": { "type": "string", "description": "DBX connection id the panel operates on" },
                        "panel": { "type": "string", "enum": ["browse", "transfers", "audit"], "description": "Panel to focus" },
                    },
                    "required": ["panel"],
                },
            },
            {
                "name": "files_ui_search",
                "description": "Fill the workbench path field and trigger a listPaged navigation so the user sees and can keep working with the results. Requires the DBX workbench; without a frontend the call reports state=pending with a hint, fall back to files_scan_digest instead.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "connectionId": { "type": "string", "description": "DBX connection id to navigate" },
                        "path": { "type": "string", "description": "Directory path to navigate to" },
                    },
                    "required": ["path"],
                },
            },
            {
                "name": "files_ui_select",
                "description": "Locate and highlight an entry by path in the workbench file table; the hit row's key fields come back in the intent summary. Requires the DBX workbench.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "connectionId": { "type": "string", "description": "DBX connection id the table belongs to" },
                        "path": { "type": "string", "description": "Entry path to locate" },
                    },
                    "required": ["path"],
                },
            },
            {
                "name": "files_ui_state",
                "description": "Read a previous UI intent's result by intentId, or (without intentId) the latest workbench snapshot reported by the frontend via files/ui/state/report (panel, counts, selected path). Use it to re-check an intent that returned pending.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "intentId": { "type": "string", "description": "intentId from a previous files_ui_* call; omit to read the latest snapshot" },
                    },
                },
            },
            {
                "name": "files_scan_digest",
                "description": "Recursively scan a directory tree inside the sidecar (depth default 8, 100k entry cap) with glob/size/mtime predicates and return ONLY the aggregate: count, extension group-by (<=20 groups), top largest/newest files (<=10), total bytes, <=5 sample rows and a cursorId. Default format 'digest'; pass format:'rows' for up to 20 locator rows. Binary content never leaves the sidecar.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "connectionId": { "type": "string", "description": "DBX connection id (or __local__ for the built-in local filesystem)" },
                        "path": { "type": "string", "description": "Root of the scan (default '/')" },
                        "glob": { "type": "string", "description": "Glob pattern matched against the full path; * stays within a path segment, ** crosses segments, ? matches one char" },
                        "minSizeBytes": { "type": "integer", "description": "Files smaller than this are skipped" },
                        "maxSizeBytes": { "type": "integer", "description": "Files larger than this are skipped" },
                        "modifiedSince": { "type": "integer", "description": "Unix epoch millis; skip entries modified before" },
                        "modifiedUntil": { "type": "integer", "description": "Unix epoch millis; skip entries modified after" },
                        "depth": { "type": "integer", "description": "Recursion depth cap (1-16, default 8)" },
                        "format": { "type": "string", "enum": ["digest", "rows"], "description": "digest (default): aggregate + sample; rows: up to 20 locator rows" },
                    },
                    "required": ["connectionId"],
                },
            },
            {
                "name": "files_cursor_next",
                "description": "Fetch the next batch (n<=20) of locator rows (paths) from a files_scan_digest cursor session. Conditions are not re-sent and the backend is not re-scanned. Expired cursors (10 minutes) answer with an explicit error suggesting a fresh files_scan_digest.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "cursorId": { "type": "string", "description": "cursorId returned by files_scan_digest" },
                        "n": { "type": "integer", "description": "Rows to return (default 20, max 20)" },
                        "offset": { "type": "integer", "description": "Start offset; omit to continue where the previous batch stopped" },
                    },
                    "required": ["cursorId"],
                },
            },
            {
                "name": "files_ui_quick_paths",
                "description": "Quick-jump locations of the connection (root + stat-verified user directories on unconfined fs connections). Pure discovery, limit hard-capped at 50.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "connectionId": { "type": "string", "description": "DBX connection id" },
                        "limit": { "type": "integer", "description": "Max entries to return (1-50, default 50)" },
                    },
                    "required": ["connectionId"],
                },
            },
            {
                "name": "files_sync",
                "description": "Synchronize/copy a directory tree between two connections (workbench files/syncDir|copyDir semantics; incremental size+mtime compare skips unchanged files). sync:true makes it a MIRROR — files present on the target but missing from the source are DELETED (rclone-sync semantics; requires a target connection with allow_delete, and maxDelete can bound the deletions). dryRun:true plans and compares only — no copies, no deletes — and is strongly recommended before any sync:true run. Runs as an async job over the DBX event channel: the answer carries a jobId to poll via files/transfer/status. Never available in standalone stdio mode.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "sourceConnectionId": { "type": "string", "description": "Source connection id (may be read-only; the source is never modified)" },
                        "sourcePath": { "type": "string", "description": "Source directory path ('/' syncs the whole source tree)" },
                        "targetConnectionId": { "type": "string", "description": "Target connection id (must be writable; with sync:true it must also allow delete)" },
                        "targetPath": { "type": "string", "description": "Target directory path ('/' = the connection root)" },
                        "sync": { "type": "boolean", "default": false, "description": "Mirror delete switch (default false). true DELETES files on the target that are missing from the source; destructive on the target — run a dryRun first. Refused when the target disallows delete (allow_delete=false), a dry run included" },
                        "dryRun": { "type": "boolean", "default": false, "description": "Plan only (default false): no copies, no deletes; one summary event with toCopy/toDelete counts and path samples, then the job completes" },
                        "maxDelete": { "type": "integer", "description": "Abort the sync when more than this many target files would be deleted (omit = unlimited; rclone --max-delete)" },
                    },
                    "required": ["sourceConnectionId", "sourcePath", "targetConnectionId", "targetPath"],
                },
            },
        ])
        .as_array()
        .cloned()
        .unwrap_or_default();
        // Read-only connections never list the write tools (design §4
        // "不注册进工具清单"); each omitted entry carries its reason
        // (ldap `Tools` shape parity: `{tools, omittedWriteTools}`).
        let mut omitted: Vec<Value> = Vec::new();
        if read_only {
            for (name, reason) in [
                ("files_write", "connection is configured read-only; write tools are not registered (design §4)"),
                ("files_mkdir", "connection is configured read-only; write tools are not registered (design §4)"),
                ("files_rename", "connection is configured read-only; write tools are not registered (design §4)"),
                ("files_delete", "connection is read-only or disallows delete (allow_delete=false); write tools are not registered (design §4)"),
                ("files_purge", "connection is read-only or disallows delete (allow_delete=false); write tools are not registered (design §4)"),
            ] {
                omitted.push(json!({ "name": name, "reason": reason }));
            }
        } else {
            tools.extend(
                json!([
                    {
                        "name": "files_write",
                        "description": "Create or overwrite a small file (base64 payload; hard cap 4 MiB, MCP-recommended <=1 MiB — larger transfers belong in the workbench upload channel). Executes directly and is audited with source 'mcp'.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "connectionId": { "type": "string", "description": "DBX connection id" },
                                "path": { "type": "string", "description": "Target file path" },
                                "dataBase64": { "type": "string", "description": "base64 file content (<=4 MiB decoded; an empty string creates an empty file)" },
                            },
                            "required": ["connectionId", "path", "dataBase64"],
                        },
                    },
                    {
                        "name": "files_mkdir",
                        "description": "Create a directory (mkdir -p semantics). Executes directly and is audited with source 'mcp'.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "connectionId": { "type": "string", "description": "DBX connection id" },
                                "path": { "type": "string", "description": "Directory path to create" },
                            },
                            "required": ["connectionId", "path"],
                        },
                    },
                    {
                        "name": "files_rename",
                        "description": "Rename/move within one connection (a file executes natively; a directory rename degrades to an async copy+delete job whose jobId is returned). Audited with source 'mcp'.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "connectionId": { "type": "string", "description": "DBX connection id" },
                                "path": { "type": "string", "description": "Source path" },
                                "newPath": { "type": "string", "description": "Target path" },
                            },
                            "required": ["connectionId", "path", "newPath"],
                        },
                    },
                    {
                        "name": "files_delete",
                        "description": "Delete a file (two-phase: the first call without confirmToken returns a preview plus a one-time confirmToken (60s) and writes nothing — repeat the same arguments with the token to execute). Read-only connections never list this tool.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "connectionId": { "type": "string", "description": "DBX connection id" },
                                "path": { "type": "string", "description": "Path to delete" },
                                "confirmToken": { "type": "string", "description": "One-time token from the preview call; parameters must be unchanged" },
                            },
                            "required": ["connectionId", "path"],
                        },
                    },
                    {
                        "name": "files_purge",
                        "description": "Recursively delete a directory tree (two-phase preview/confirm like files_delete). Refuses the connection root and '/'. Read-only connections never list this tool.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "connectionId": { "type": "string", "description": "DBX connection id" },
                                "path": { "type": "string", "description": "Directory tree to purge (never the root)" },
                                "confirmToken": { "type": "string", "description": "One-time token from the preview call; parameters must be unchanged" },
                            },
                            "required": ["connectionId", "path"],
                        },
                    },
                ])
                .as_array()
                .cloned()
                .unwrap_or_default(),
            );
        }
        let mut result = json!({ "tools": tools });
        if !omitted.is_empty() {
            result["omittedWriteTools"] = json!(omitted);
        }
        result
    }

    /// stdio `tools/list`：复用注册表全量清单（工具名/描述/语义不变），并为
    /// 连接类工具补充内联连接参数声明——严格校验的 MCP 宿主会丢弃未声明
    /// 参数（ssh/ldap/kafka 同因显式声明），同时把 required 的 connectionId
    /// 放宽为 anyOf 二选一（connectionId 或内联 `connection` 对象）。UI 类
    /// 工具与免连接的 `files_cursor_next` 保持原 schema。
    fn stdio_tool_list(&self) -> Value {
        let mut list = self.definitions_for(false);
        let Some(tools) = list.get_mut("tools").and_then(Value::as_array_mut) else {
            return list;
        };
        for tool in tools {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            if STDIO_UI_TOOLS.contains(&name) || !needs_connection_id(name) {
                continue;
            }
            let Some(schema) = tool.get_mut("inputSchema").and_then(Value::as_object_mut) else {
                continue;
            };
            if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
                properties.insert("connectionId".to_string(), json!({
                    "type": "string",
                    "description": "Connection id: \"__local__\" (built-in local filesystem), \
                     a pooled inline-connection id from an earlier call in this stdio session \
                     (mcp-inline-…), or a DBX saved connection id (forwarded through the \
                     running DBX app's local bridge). Omit to connect by inline parameters \
                     instead; in DBX workbench/bridge mode credentials are resolved by the \
                     host and never travel in tool arguments",
                }));
                properties.insert("connection".to_string(), json!({
                    "type": "object",
                    "description": "Inline connection parameters (standalone stdio mode; stay \
                     in process memory only, pooled by a hash of these fields). camelCase, \
                     aligned with the connection form",
                    "properties": inline_connection_properties(),
                    "required": ["protocol"],
                }));
            }
            if let Some(required) = schema.get("required").and_then(Value::as_array).cloned() {
                let relaxed: Vec<Value> = required
                    .iter()
                    .filter(|entry| entry.as_str() != Some("connectionId"))
                    .cloned()
                    .collect();
                schema.insert("required".to_string(), json!(relaxed));
                schema.insert("anyOf".to_string(), json!([
                    { "required": ["connectionId"] },
                    { "required": ["connection"] },
                ]));
            }
        }
        list
    }
}

mod scan;

use scan::{aggregate_rows, take_rows, walk_subtree, DigestLimits, ScanFilter, WalkState};


mod truncate;

use truncate::{cap_response, content_envelope};

mod guards;

use guards::{audit_mcp, ensure_deletable, ensure_writable, parse_sync_request, refuse_root_purge, SYNC_STDIO_UNAVAILABLE};


// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    match value.get(key) {
        None | Some(Value::Null) => Err(format!("Missing required parameter: {key}")),
        Some(Value::String(text)) if !text.is_empty() => Ok(text),
        Some(_) => Err(format!("Parameter '{key}' must be a non-empty string")),
    }
}

/// Enumerates EVERY absent required BUSINESS parameter in one error
/// (`Missing required parameters: a, b`, ssh `missing_required` 口径拉齐
/// MCP_ACCEPTANCE §3.9): an LLM caller fixes all gaps in a single turn
/// instead of discovering them one fail-fast round at a time. Present-but-
/// invalid values (wrong type, empty string) are NOT listed here — the
/// per-parameter `required_str`/type checks that follow name them precisely.
///
/// Scope note (第七轮口径): the enumerated keys are the tool's business
/// parameters only — `connectionId` (and the inline `connection` payload)
/// stay on the singular `Missing required parameter: connectionId` message
/// from [`required_str`], which is both the stdio anyOf放宽后的 schema
/// required 集合（核对器空参探针比对口径）and the registered §3.7
/// "连接参数门先于业务参数校验" ordering（与 ssh 现状一致，核对器以
/// CONNECTION_GATE_RE 归一为 WARN）。
fn missing_required(arguments: &Value, keys: &[&str]) -> Result<(), String> {
    let missing: Vec<&str> = keys
        .iter()
        .copied()
        .filter(|key| matches!(arguments.get(key), None | Some(Value::Null)))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Missing required parameters: {}",
            missing.join(", ")
        ))
    }
}

/// Optional string argument that fails loudly when present but not a usable
/// string — a non-string `path` silently falling back to `/` would scan or
/// target the wrong tree entirely (fail-fast over silent guesswork).
fn optional_str<'a>(arguments: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.is_empty() => Ok(Some(text)),
        Some(_) => Err(format!("Parameter '{key}' must be a non-empty string")),
    }
}

/// Numeric argument tolerance for LLM callers (design「容错性」): numbers pass
/// through, numeric strings (`"1024"`, whitespace-trimmed) and integral floats
/// (`8.0`) parse, everything else — including negatives and fractions — is a
/// clear error naming the offending key. `None` when absent or null.
fn numeric_arg_u64(arguments: &Value, key: &str) -> Result<Option<u64>, String> {
    const HINT: &str = "must be a non-negative integer";
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => {
            if let Some(value) = number.as_u64() {
                return Ok(Some(value));
            }
            // LLMs emit integral floats (`8.0`) freely.
            if let Some(value) = number.as_f64() {
                if value.fract() == 0.0 && (0.0..=u64::MAX as f64).contains(&value) {
                    return Ok(Some(value as u64));
                }
            }
            Err(format!("{key} {HINT}"))
        }
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            trimmed
                .parse::<u64>()
                .map(Some)
                .map_err(|_| format!("{key} {HINT}"))
        }
        Some(_) => Err(format!("{key} {HINT}")),
    }
}

/// [`numeric_arg_u64`] with a default for the absent/null case; callers clamp
/// the result into their own valid range.
fn numeric_arg_or(arguments: &Value, key: &str, default: u64) -> Result<u64, String> {
    Ok(numeric_arg_u64(arguments, key)?.unwrap_or(default))
}

/// Strips the whitespace + trailing-`/` spelling an LLM may echo back, keeping
/// the rest of the path verbatim (a leading `/` is OpenDAL-normalized anyway).
/// The root collapses to `/`.
fn normalize_slashes(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Path-shape hard gate for every MCP tool path (adversarial-input round):
/// rejects what no legitimate MCP caller sends and no backend should have to
/// interpret —
/// - `..` segments (parent traversal): the MCP face is absolute-path only, and
///   `/..`, `/a/../..` spellings must never reach the Operator (fs backends
///   reject them late/confusingly; custom adapters may resolve them);
/// - `.` segments: a `/`-rooted tool path never needs one; `/.` is root
///   spelled confusingly and must hit the red line instead of resolving past
///   `refuse_root_purge`'s trim;
/// - control characters (NUL, newline, …): JSON can carry `\u0000`; backends
///   would answer with raw OS errors or create unmanageable file names;
/// - paths above 4096 bytes (NAME_MAX regimes fail far earlier; a proactive
///   bound gives a clean message instead of a backend `ENAMETOOLONG`).
///
/// Deliberately NOT here (design-inside conservative behavior, pinned by
/// tests + docs): `~` expansion (the protocol never expands — `~` is a literal
/// name), unicode NFC/NFD normalization, and URL-decoding (`%2e%2e` stays
/// literal). None of them can smuggle a traversal or a root bypass; they only
/// ever address a literally-named entry.
fn validate_path_shape(raw: &str, key: &str) -> Result<(), String> {
    if raw.chars().any(char::is_control) {
        return Err(format!(
            "Parameter '{key}' contains control characters, which are not valid in a path"
        ));
    }
    if raw.len() > 4096 {
        return Err(format!(
            "Parameter '{key}' is {} bytes long, above the 4096-byte path limit",
            raw.len()
        ));
    }
    if raw.split('/').any(|segment| matches!(segment, "." | "..")) {
        return Err(format!(
            "Parameter '{key}' contains a '.' or '..' path segment; MCP tool paths are \
             absolute below the connection root — re-send the resolved path"
        ));
    }
    Ok(())
}

/// Normalized file write/rename target: trailing slashes dropped (OpenDAL file
/// ops reject the directory-marker spelling), the connection root rejected —
/// writing or renaming onto it is never meaningful.
fn file_target_path(raw: &str, key: &str) -> Result<String, String> {
    let path = normalize_slashes(raw);
    validate_path_shape(&path, key)?;
    if path == "/" {
        return Err(format!(
            "Parameter '{key}' must be a path below the connection root, not the root itself"
        ));
    }
    Ok(path)
}

/// OpenDAL path spelling for a delete/purge target, decided by the preview
/// stat's kind: directory markers keep exactly one trailing `/` (prefix-based
/// backends resolve the marker only through that form), file paths never carry
/// one — an LLM-echoed `a.txt/` must delete `a.txt` instead of silently
/// no-op'ing against a missing marker. A missing path collapses to its bare
/// spelling (OpenDAL delete is idempotent).
fn canonical_delete_target(raw: &str, kind: Option<&str>) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    if kind == Some("dir") {
        format!("{trimmed}/")
    } else {
        trimmed.to_string()
    }
}

/// Every registered tool name, for the unknown-tool self-correction hint
/// (ssh `TOOL_NAMES` / ldap `available:` parity). Kept in one place so the
/// hint can never drift from the dispatch table.
const ALL_TOOL_NAMES: [&str; 13] = [
    "files_ui_focus",
    "files_ui_search",
    "files_ui_select",
    "files_ui_state",
    "files_ui_quick_paths",
    "files_scan_digest",
    "files_cursor_next",
    "files_write",
    "files_mkdir",
    "files_rename",
    "files_delete",
    "files_purge",
    "files_sync",
];

/// Actionable error for an unregistered tool name: a separator/case variant
/// (`files-scandigest`, `FILES_SCAN_DIGEST`) suggests the exact registered
/// name and every miss lists the discovery surface (ssh
/// `unknown_tool_message` / ldap `(available: …)` parity).
fn unknown_tool_message(name: &str) -> String {
    let compact = |text: &str| text.to_ascii_lowercase().replace(['-', '_', ' '], "");
    let query = compact(name);
    let mut suggestion = None;
    for tool in ALL_TOOL_NAMES {
        let candidate = compact(tool);
        if candidate == query {
            suggestion = Some(tool);
            break;
        }
        if query.len() >= 4 && (candidate.contains(&query) || query.contains(&candidate)) {
            suggestion = Some(tool);
        }
    }
    format!(
        "Unknown tool: '{name}'. {}Available tools: {} (discovered via mcp/tools \
         over the DBX bridge, or tools/list in standalone stdio mode).",
        suggestion
            .map(|tool| format!("Did you mean '{tool}'? "))
            .unwrap_or_default(),
        ALL_TOOL_NAMES.join(", "),
    )
}

fn lifecycle_id(lifecycle: &Value) -> String {
    lifecycle
        .get("connection")
        .and_then(|connection| connection.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Deterministic FNV-1a over the canonical JSON of the arguments. Pure and
/// unit-tested; no crypto dependency is warranted for a same-process
/// liveness binding (the token itself is the capability).
pub fn params_hash(arguments: &Value) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let text = serde_json::to_string(arguments).unwrap_or_default();
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Arguments with `confirmToken` removed — the hash binding covers exactly the
/// parameters the caller must keep unchanged.
pub fn arguments_without_token(arguments: &Value) -> Value {
    let mut stripped = arguments.clone();
    if let Some(map) = stripped.as_object_mut() {
        map.remove("confirmToken");
    }
    stripped
}

fn unix_millis_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn iso_millis(unix_millis: i64) -> String {
    crate::store::format_rfc3339(unix_millis)
}

mod appbridge;
mod stdio;

pub use stdio::run_mcp_stdio;
use stdio::{inline_connection_properties, needs_connection_id, STDIO_UI_TOOLS};


// ---------------------------------------------------------------------------
// Tests (pure logic; no remote I/O)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
