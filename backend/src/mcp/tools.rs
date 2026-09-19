// ---------------------------------------------------------------------------
// Tool dispatch (`mcp/call`)
// ---------------------------------------------------------------------------

use dbx_plugin_sdk::PluginEmitter;
use serde_json::{json, Value};

use crate::engine::Engine;
use crate::model::StoredConnection;
use crate::store::Store;
use crate::transfers::JobTable;

use super::*;

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

// ---------------------------------------------------------------------------
// Phase D: rclone-mode dispatch (`DBX_FILES_ENGINE=rclone`)
// ---------------------------------------------------------------------------

/// Owned starter for one rclone dir sync job (workbench
/// `files/syncDir|copyDir` semantics; `main.rs::rclone_start_dir_job`).
/// `None` in standalone stdio mode — no DBX event channel, no job mirror.
pub type SyncJobStarter = std::sync::Arc<
    dyn Fn(
            crate::model::DirJobRequest,
            bool,
            Option<PluginEmitter>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>
        + Send
        + Sync,
>;

/// rclone-mode dispatch context. The storage-touching tools route through it
/// when the sidecar runs the rclone engine; tool schemas and response shapes
/// stay identical to the OpenDAL path. `engine` doubles as the connection
/// registry; `store` carries the `source:"mcp"` audit sink; `start_sync`
/// shares the workbench sync-job mirror so `files/transfer/status` remains
/// the single poll surface.
pub struct RcloneRoute {
    pub engine: std::sync::Arc<crate::rclone::RcloneEngine>,
    pub store: std::sync::Arc<Store>,
    pub start_sync: Option<SyncJobStarter>,
}

/// Resolves the connection reference to an rclone binding via the shared
/// engine resolver: `__local__` maps onto the rclone `local` backend rooted
/// at `/` (OpenDAL engine parity for the built-in filesystem); anything else
/// must be a connected registry id.
fn rclone_binding(
    route: &RcloneRoute,
    connection_id: &str,
) -> Result<crate::rclone::registry::RemoteBinding, String> {
    route.engine.binding(connection_id)
}

/// Plain-field snapshot of a parsed sync request (the request value itself
/// moves into the starter closure; `model::DirJobRequest` is not Clone).
struct RcloneSyncArgs {
    source_id: String,
    source_path: String,
    target_id: String,
    target_path: String,
    sync: bool,
    dry_run: bool,
    max_delete: Option<u64>,
}

impl RcloneSyncArgs {
    fn from_request(request: &crate::model::DirJobRequest, sync: bool) -> Self {
        Self {
            source_id: request.source_connection_id.clone(),
            source_path: request.source_path.clone(),
            target_id: request.target_connection_id.clone(),
            target_path: request.target_path.clone(),
            sync,
            dry_run: request.dry_run.unwrap_or(false),
            max_delete: request.max_delete,
        }
    }
}

/// Number of path segments `path` sits below `start` (both MCP-absolute;
/// `start` "/" is the root). The rclone scan enumerates recursively in one
/// rc call, so the depth cap is emulated by segment counting instead of the
/// per-level OpenDAL walk.
fn depth_below(start: &str, path: &str) -> usize {
    let start = start.trim_matches('/');
    let base_len = if start.is_empty() {
        0
    } else {
        start.split('/').count()
    };
    let path_len = path
        .trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .count();
    path_len.saturating_sub(base_len)
}

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
            // Phase D: in rclone mode the forwarded lifecycle registers into
            // the rclone registry (the OpenDAL engine table is not consulted
            // by any tool in that mode).
            if let Some(route) = self.rclone_route() {
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
                    return Err(format!(
                        "Failed to register the forwarded connection lifecycle: {error}"
                    ));
                }
            } else {
                engine.connect(connection).map_err(|error| {
                    format!("Failed to register the forwarded connection lifecycle: {error}")
                })?;
            }
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
    pub(crate) fn finalize_payload(&self, payload: &mut Value) -> Value {
        let settings = self.current_settings();
        cap_response(payload, settings.response_limit_bytes, settings.cell_width);
        content_envelope(payload)
    }

    pub(crate) async fn run_tool(
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
            // Phase D: storage-touching tools dispatch through the rclone
            // route when the sidecar runs `DBX_FILES_ENGINE=rclone`; the
            // OpenDAL bodies below are the default-engine path (same tool
            // schemas, same response shapes — see the rclone_* methods).
            "files_scan_digest" => match self.rclone_route() {
                Some(route) => self.scan_digest_rclone(route, arguments).await,
                None => self.scan_digest_tool(engine, arguments).await,
            },
            "files_cursor_next" => self.cursor_next(arguments),
            "files_ui_quick_paths" => {
                if let Some(route) = self.rclone_route() {
                    return self.quick_paths_rclone(route, arguments).await;
                }
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
                if let Some(route) = self.rclone_route() {
                    return self.files_write_rclone(route, arguments).await;
                }
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
                if let Some(route) = self.rclone_route() {
                    return self.files_mkdir_rclone(route, arguments).await;
                }
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
                if let Some(route) = self.rclone_route() {
                    return self.files_rename_rclone(route, arguments).await;
                }
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
                if let Some(route) = self.rclone_route() {
                    return self.files_delete_rclone(route, tool, arguments).await;
                }
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
                if let Some(route) = self.rclone_route() {
                    return self.files_sync_rclone(route, emitter, arguments).await;
                }
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
    pub(crate) async fn ui_intent_tool<F>(
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

    pub(crate) async fn scan_digest_tool(&self, engine: &Engine, arguments: &Value) -> Result<Value, String> {
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
pub(crate) fn normalized_format(raw: Option<&str>) -> Result<String, String> {
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

// ---------------------------------------------------------------------------
// rclone-mode tool bodies (Phase D). Response shapes mirror the OpenDAL
// bodies above field-for-field; gates split the established way: connection
// read_only/allow_delete bind on the registry binding here, path whitelist +
// lock_to_root inside the rclone ops layer (shared policy::PathPolicy).
// ---------------------------------------------------------------------------

impl Mcp {
    /// `files_scan_digest` over the rclone engine: one recursive
    /// `operations/list` replaces the per-directory OpenDAL walk; the
    /// depth/entry budgets and the digest/cursor contract are unchanged.
    pub(crate) async fn scan_digest_rclone(
        &self,
        route: &RcloneRoute,
        arguments: &Value,
    ) -> Result<Value, String> {
        let connection_id = required_str(arguments, "connectionId")?;
        let binding = rclone_binding(route, connection_id)?;
        let client = route.engine.client_for_binding(&binding).await?;
        let start = normalize_slashes(match optional_str(arguments, "path")? {
            Some(raw) => raw,
            None => "/",
        });
        validate_path_shape(&start, "path")?;
        let depth = numeric_arg_or(arguments, "depth", u64::from(DEFAULT_SCAN_DEPTH))
            .map_err(|error| format!("{error}; depth accepts an integer in 1..={MAX_SCAN_DEPTH}"))?
            .clamp(1, u64::from(MAX_SCAN_DEPTH)) as u32;
        let filter = ScanFilter::from_arguments(arguments)?;
        let format = normalized_format(optional_str(arguments, "format")?)?;
        let settings = self.current_settings();
        let entries = crate::rclone::ops::list(
            &client,
            &crate::rclone::call_fs(&binding),
            &start,
            true,
            &binding.root,
            binding.lock_to_root,
        )
        .await?;
        let mut walk = WalkState::new(filter);
        for entry in entries {
            // Entries deeper than the cap exist in the recursive listing but
            // are neither counted nor matched (the OpenDAL walk never visits
            // them).
            if depth_below(&start, &entry.path) > depth as usize {
                continue;
            }
            if walk.exhausted(MAX_SCAN_ENTRIES) {
                walk.truncated = true;
                break;
            }
            walk.visit(PathRow {
                path: entry.path.clone(),
                kind: entry.kind,
                size: entry.size,
                modified_at: entry.modified_at,
            });
        }
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

    /// `files_ui_quick_paths` over the rclone engine (rclone/ops.rs has the
    /// shape-aligned twin).
    pub(crate) async fn quick_paths_rclone(
        &self,
        route: &RcloneRoute,
        arguments: &Value,
    ) -> Result<Value, String> {
        let connection_id = required_str(arguments, "connectionId")?;
        let binding = rclone_binding(route, connection_id)?;
        let client = route.engine.client_for_binding(&binding).await?;
        let payload = crate::rclone::ops::quick_paths(
            &client,
            &crate::rclone::call_fs(&binding),
            &binding.backend_type,
            &binding.root,
        )
        .await?;
        let limit =
            numeric_arg_or(arguments, "limit", QUICK_PATHS_LIMIT)?.clamp(1, QUICK_PATHS_LIMIT) as usize;
        let all = payload
            .get("paths")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(json!({ "paths": &all[..limit.min(all.len())] }))
    }

    /// `files_write` over the rclone engine (`ops::write_bytes`: rc
    /// uploadfile + movefile, overwrite semantics).
    pub(crate) async fn files_write_rclone(
        &self,
        route: &RcloneRoute,
        arguments: &Value,
    ) -> Result<Value, String> {
        missing_required(arguments, &["path", "dataBase64"])?;
        let connection_id = required_str(arguments, "connectionId")?;
        let binding = rclone_binding(route, connection_id)?;
        ensure_binding_writable(&binding)?;
        let path = file_target_path(required_str(arguments, "path")?, "path")?;
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
        let client = route.engine.client_for_binding(&binding).await?;
        let remote = crate::engine::ops::policy::PathPolicy::from_parts(
            &binding.root,
            binding.lock_to_root,
            false,
            true,
        )
        .check_write(&path)
        .map(|resolved| resolved.relative)?;
        crate::rclone::ops::write_bytes(
            &client,
            &crate::rclone::call_fs(&binding),
            &remote,
            &data,
        )
        .await?;
        audit_mcp_id(&route.store, connection_id, "files/write", &path, "ok");
        let bytes = data.len();
        let mut result = json!({ "success": true, "path": path, "bytes": bytes });
        if bytes > MCP_RECOMMENDED_WRITE_BYTES {
            result["hint"] = json!(
                "Payload above the MCP-recommended 1 MiB; prefer the workbench transfer \
                 pane or the upload channel for larger files"
            );
        }
        Ok(result)
    }

    /// `files_mkdir` over the rclone engine (rc `operations/mkdir`, mkdir -p).
    pub(crate) async fn files_mkdir_rclone(
        &self,
        route: &RcloneRoute,
        arguments: &Value,
    ) -> Result<Value, String> {
        missing_required(arguments, &["path"])?;
        let connection_id = required_str(arguments, "connectionId")?;
        let binding = rclone_binding(route, connection_id)?;
        ensure_binding_writable(&binding)?;
        let raw = required_str(arguments, "path")?;
        validate_path_shape(raw, "path")?;
        let client = route.engine.client_for_binding(&binding).await?;
        crate::rclone::ops::mkdir(
            &client,
            &crate::rclone::call_fs(&binding),
            raw,
            &binding.root,
            binding.lock_to_root,
        )
        .await?;
        audit_mcp_id(&route.store, connection_id, "files/mkdir", raw, "ok");
        Ok(json!({ "success": true, "path": raw }))
    }

    /// `files_rename` over the rclone engine (rc `operations/movefile`).
    /// Deviation from the OpenDAL path, recorded for the handoff report:
    /// directory renames do not degrade to an async copy+delete job here —
    /// the rclone engine carries no per-entry dir-job machinery, so the call
    /// goes to rclone's movefile and surfaces rclone's own answer/error.
    pub(crate) async fn files_rename_rclone(
        &self,
        route: &RcloneRoute,
        arguments: &Value,
    ) -> Result<Value, String> {
        missing_required(arguments, &["path", "newPath"])?;
        let connection_id = required_str(arguments, "connectionId")?;
        let binding = rclone_binding(route, connection_id)?;
        ensure_binding_writable(&binding)?;
        // Rename removes the source path — same delete gate as the workbench
        // path (main.rs files/rename).
        ensure_binding_deletable(&binding)?;
        let path = file_target_path(required_str(arguments, "path")?, "path")?;
        let new_path = file_target_path(required_str(arguments, "newPath")?, "newPath")?;
        let client = route.engine.client_for_binding(&binding).await?;
        crate::rclone::ops::rename(
            &client,
            &crate::rclone::call_fs(&binding),
            &path,
            &new_path,
            &binding.root,
            binding.lock_to_root,
        )
        .await?;
        audit_mcp_id(&route.store, connection_id, "files/rename", &path, "ok");
        Ok(json!({ "success": true, "transport": "native", "path": path, "newPath": new_path }))
    }

    /// `files_delete` / `files_purge` over the rclone engine: identical
    /// two-phase preview/confirm flow, preview built from the rclone stat,
    /// execution via `ops::delete_file` / `ops::purge` (both idempotent on
    /// missing paths like the OpenDAL engine).
    pub(crate) async fn files_delete_rclone(
        &self,
        route: &RcloneRoute,
        tool: &str,
        arguments: &Value,
    ) -> Result<Value, String> {
        missing_required(arguments, &["path"])?;
        let connection_id = required_str(arguments, "connectionId")?;
        let binding = rclone_binding(route, connection_id)?;
        ensure_binding_deletable(&binding)?;
        let path = required_str(arguments, "path")?;
        validate_path_shape(path, "path")?;
        if tool == "files_purge" {
            refuse_root_purge_root(&binding.root, path)?;
        }
        let client = route.engine.client_for_binding(&binding).await?;
        let fs = crate::rclone::call_fs(&binding);
        // Preview before any deletion (identical shape to the OpenDAL arm).
        let preview = match crate::rclone::ops::stat(
            &client,
            &fs,
            path,
            &binding.root,
            binding.lock_to_root,
        )
        .await
        {
            Ok(entry) => json!({
                "tool": tool,
                "connectionId": connection_id,
                "path": path,
                "kind": entry.kind,
                "size": entry.size,
            }),
            // rclone delete/purge are idempotent for missing paths: preview
            // as absent instead of failing the flow (OpenDAL arm parity).
            Err(_) => json!({
                "tool": tool,
                "connectionId": connection_id,
                "path": path,
                "kind": "missing",
            }),
        };
        let Some(token) = arguments
            .get("confirmToken")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            let (confirm_token, expires_at) = self.confirm_begin(&arguments_without_token(arguments));
            return Ok(json!({
                "preview": preview,
                "confirmToken": confirm_token,
                "expiresAt": iso_millis(expires_at as i64),
                "note": "nothing written yet; repeat the same arguments with \
                         confirmToken to execute",
            }));
        };
        self.confirm_verify(token, &arguments_without_token(arguments))?;
        // rclone ops trim slash spellings internally; the bare validated path
        // is the delete target (the dir-marker dance exists for OpenDAL
        // prefix backends only).
        let action = if tool == "files_delete" {
            crate::rclone::ops::delete_file(
                &client,
                &fs,
                path,
                &binding.root,
                binding.lock_to_root,
            )
            .await?;
            "files/delete"
        } else {
            crate::rclone::ops::purge(
                &client,
                &fs,
                path,
                &binding.root,
                binding.lock_to_root,
            )
            .await?;
            "files/purge"
        };
        audit_mcp_id(&route.store, connection_id, action, path, "ok");
        Ok(json!({ "success": true, "path": path }))
    }

    /// `files_sync` over the rclone engine: the pure parse twin validates
    /// arguments, then the injected starter enqueues through the workbench
    /// syncDir path (same binding gates, same job mirror, same DirJob wire
    /// shape). Response keys are identical to the OpenDAL arm; the jobId is
    /// pollable via `files/transfer/status` on the rclone mirrors.
    pub(crate) async fn files_sync_rclone(
        &self,
        route: &RcloneRoute,
        emitter: Option<&PluginEmitter>,
        arguments: &Value,
    ) -> Result<Value, String> {
        let parsed = parse_rclone_sync_request(arguments)?;
        // The dir job reports through the DBX event channel +
        // `files/transfer/status`; standalone stdio has neither (OpenDAL-arm
        // parity for the refusal).
        let Some(emitter) = emitter else {
            return Err(SYNC_STDIO_UNAVAILABLE.to_string());
        };
        let Some(start_sync) = route.start_sync.as_ref() else {
            return Err(SYNC_STDIO_UNAVAILABLE.to_string());
        };
        // Field values are read out before the request moves into the
        // starter (DirJobRequest is not Clone by contract).
        let RcloneSyncArgs {
            source_id,
            source_path,
            target_id,
            target_path,
            sync,
            dry_run,
            max_delete,
        } = RcloneSyncArgs::from_request(&parsed.request, parsed.sync);
        let job_id = start_sync(parsed.request, parsed.sync, Some(emitter.clone())).await?;
        audit_mcp_id(
            &route.store,
            &target_id,
            "files/sync",
            &format!("{source_id}:{source_path} -> {target_path}"),
            "ok",
        );
        let mut result = json!({
            "success": true,
            "transport": "job",
            "jobId": job_id,
            "sourceConnectionId": source_id,
            "sourcePath": source_path,
            "targetConnectionId": target_id,
            "targetPath": target_path,
            "sync": sync,
            "dryRun": dry_run,
        });
        if let Some(max_delete) = max_delete {
            result["maxDelete"] = json!(max_delete);
        }
        result["hint"] = if dry_run {
            json!("Dry run: nothing is copied or deleted; the job plans/compares, \
                   emits one summary event and completes — poll files/transfer/status \
                   for the counts")
        } else {
            json!("Async directory job enqueued; poll files/transfer/status (or the \
                   workbench transfers pane) for progress")
        };
        if parsed.sync && !dry_run {
            result["warning"] = json!(
                "sync=true mirrors the source: files present on the target but missing \
                 from the source are DELETED (bounded by maxDelete when set)"
            );
        }
        Ok(result)
    }
}

/// Every registered tool name, for the unknown-tool self-correction hint
/// (ssh `TOOL_NAMES` / ldap `available:` parity). Kept in one place so the
/// hint can never drift from the dispatch table.
pub(crate) const ALL_TOOL_NAMES: [&str; 13] = [
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
pub(crate) fn unknown_tool_message(name: &str) -> String {
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
