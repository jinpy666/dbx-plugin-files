// ---------------------------------------------------------------------------
// Tool dispatch (`mcp/call`)
// ---------------------------------------------------------------------------

use dbx_plugin_sdk::PluginEmitter;
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::model::StoredConnection;
use crate::store::Store;

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
// Phase D: rclone dispatch (the only engine since Phase D landed)
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

/// rclone dispatch context. The storage-touching tools route through it —
/// the rclone engine is the only engine, so every production entry attaches
/// it before any tool can run. `engine` is the connection registry; `store`
/// carries the `source:"mcp"` audit sink; `start_sync` shares the workbench
/// sync-job mirror so `files/transfer/status` remains the single poll
/// surface.
pub struct RcloneRoute {
    pub engine: std::sync::Arc<crate::rclone::RcloneEngine>,
    pub store: std::sync::Arc<Store>,
    pub start_sync: Option<SyncJobStarter>,
}

/// Resolves the connection reference to an rclone binding via the shared
/// engine resolver: `__local__` maps onto the rclone `local` backend rooted
/// at `/` (the built-in filesystem); anything else
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
/// rc call, so the depth cap is emulated by segment counting.
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
    /// The rclone dispatch route (attached by every production entry). A
    /// missing route means the entry constructed `Mcp` without a storage
    /// engine — an explicit error, never a silent no-op.
    fn storage_route(&self) -> Result<&RcloneRoute, String> {
        self.rclone_route()
            .ok_or_else(|| "storage tools require the rclone engine route, which is not \
                             attached in this session"
                .to_string())
    }

    /// `mcp/call` entry used by the DBX MCP bridge: registers the forwarded
    /// connection lifecycle payload (so `connectionId` resolves like any
    /// workbench connection), then dispatches the tool. Returns the MCP
    /// content envelope with the 16 KiB token-economy cap applied.
    pub async fn call(
        &self,
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
            // The forwarded lifecycle registers into the rclone registry so
            // every storage tool resolves the connectionId like a workbench
            // connection.
            let route = self.storage_route()?;
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
            if let Some(map) = arguments.as_object_mut() {
                map.entry("connectionId".to_string())
                    .or_insert_with(|| json!(lifecycle_id(lifecycle)));
            }
        }
        let mut payload = self
            .run_tool(tool, &arguments, store, Some(emitter))
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
            // Storage tools dispatch through the rclone route (the only
            // engine); see the *_rclone methods below.
            "files_scan_digest" => {
                self.scan_digest_rclone(self.storage_route()?, arguments).await
            }
            "files_cursor_next" => self.cursor_next(arguments),
            "files_ui_quick_paths" => {
                self.quick_paths_rclone(self.storage_route()?, arguments).await
            }

            // -- writes (design §4) ----------------------------------------------
            "files_write" => self.files_write_rclone(self.storage_route()?, arguments).await,
            "files_mkdir" => self.files_mkdir_rclone(self.storage_route()?, arguments).await,
            "files_rename" => self.files_rename_rclone(self.storage_route()?, arguments).await,
            "files_delete" | "files_purge" => {
                self.files_delete_rclone(self.storage_route()?, tool, arguments).await
            }

            // -- directory sync (§8.4 via MCP) -------------------------------------
            "files_sync" => self.files_sync_rclone(self.storage_route()?, emitter, arguments).await,

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
// rclone tool bodies (the only engine). Gates split the established way:
// connection read_only/allow_delete bind on the registry binding here, path
// whitelist + lock_to_root inside the rclone ops layer (shared
// policy::PathPolicy).
// ---------------------------------------------------------------------------

impl Mcp {
    /// `files_scan_digest` over the rclone engine: one recursive
    /// `operations/list` walk; the
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
            // are neither counted nor matched.
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
        let remote = crate::policy::PathPolicy::from_parts(
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
    /// Recorded deviation: directory renames do not degrade to an async
    /// copy+delete job here —
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
    /// missing paths).
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
        // Preview before any deletion.
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
            // as absent instead of failing the flow.
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
        // is the delete target (the dir-marker spelling matters for prefix
        // backends only).
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
    /// shape). The jobId is pollable via `files/transfer/status` on the
    /// rclone mirrors.
    pub(crate) async fn files_sync_rclone(
        &self,
        route: &RcloneRoute,
        emitter: Option<&PluginEmitter>,
        arguments: &Value,
    ) -> Result<Value, String> {
        let parsed = parse_rclone_sync_request(arguments)?;
        // The dir job reports through the DBX event channel +
        // `files/transfer/status`; standalone stdio has neither, hence the
        // refusal.
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

// ---------------------------------------------------------------------------
// Digest scan: filter, aggregate (the rclone walk in `scan_digest_rclone`
// feeds pre-enumerated entries through the pure filter/aggregate layer)
// ---------------------------------------------------------------------------

/// One matched entry retained for aggregation/cursor materialization.
#[derive(Debug, Clone)]
pub struct PathRow {
    pub path: String,
    pub kind: &'static str,
    pub size: Option<u64>,
    pub modified_at: Option<u64>,
}

/// Local predicates (design §3.1): evaluated inside the sidecar against every
/// scanned entry; nothing but the verdict leaves the loop.
#[derive(Debug, Clone, Default)]
pub struct ScanFilter {
    pub glob: Option<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub modified_since: Option<u64>,
    pub modified_until: Option<u64>,
}

impl ScanFilter {
    pub(crate) fn from_arguments(arguments: &Value) -> Result<Self, String> {
        Ok(Self {
            glob: optional_str(arguments, "glob")?.map(str::to_string),
            min_size: numeric_arg_u64(arguments, "minSizeBytes")?,
            max_size: numeric_arg_u64(arguments, "maxSizeBytes")?,
            modified_since: numeric_arg_u64(arguments, "modifiedSince")?,
            modified_until: numeric_arg_u64(arguments, "modifiedUntil")?,
        })
    }

    /// A row matches when the glob (if any) hits and the size/mtime predicates
    /// pass. Directory entries only need the glob (size/mtime predicates do
    /// not apply to them); they are always walked.
    pub(crate) fn matches(&self, row: &PathRow) -> bool {
        if let Some(glob) = &self.glob {
            if !glob_match(glob, &row.path) {
                return false;
            }
        }
        if row.kind == "dir" {
            return true;
        }
        if let Some(min) = self.min_size {
            if row.size.unwrap_or(0) < min {
                return false;
            }
        }
        if let Some(max) = self.max_size {
            if row.size.unwrap_or(0) > max {
                return false;
            }
        }
        if let Some(since) = self.modified_since {
            match row.modified_at {
                Some(modified) if modified < since => return false,
                // Entries without mtime cannot prove recency; keep the row
                // rather than silently dropping half the tree.
                _ => {}
            }
        }
        if let Some(until) = self.modified_until {
            match row.modified_at {
                Some(modified) if modified <= until => {}
                Some(_) => return false,
                // Backends without mtime cannot prove recency; keep the row
                // rather than silently dropping half the tree.
                None => {}
            }
        }
        true
    }
}

/// Accumulator for the local scan.
pub(crate) struct WalkState {
    filter: ScanFilter,
    pub(crate) matched: Vec<PathRow>,
    pub(crate) scanned: usize,
    pub(crate) truncated: bool,
}

impl WalkState {
    pub(crate) fn new(filter: ScanFilter) -> Self {
        Self {
            filter,
            matched: Vec::new(),
            scanned: 0,
            truncated: false,
        }
    }

    /// Records one visited entry (filter applied, scanned counter advanced).
    pub(crate) fn visit(&mut self, row: PathRow) {
        self.scanned += 1;
        if self.filter.matches(&row) {
            self.matched.push(row);
        }
    }

    /// Whether the absolute scanned budget is used up.
    pub(crate) fn exhausted(&self, max_entries: usize) -> bool {
        self.scanned >= max_entries
    }
}

/// Digest shape limits (settings-driven; defaults are the design hard caps).
#[derive(Debug, Clone, Copy)]
pub struct DigestLimits {
    pub group_limit: usize,
    pub top_n: usize,
}

impl Default for DigestLimits {
    fn default() -> Self {
        Self {
            group_limit: GROUP_LIMIT,
            top_n: TOP_N,
        }
    }
}

/// Pure aggregation over the matched rows (unit-tested): total bytes,
/// extension group-by (≤group_limit, count desc), top-N largest/newest
/// (≤top_n). Directory entries never contribute to byte totals or file top-Ns.
pub fn aggregate_rows(rows: &[PathRow], limits: &DigestLimits) -> DigestStats {
    let mut total_bytes: u64 = 0;
    let mut groups: HashMap<String, (u64, u64)> = HashMap::new();
    let mut files: Vec<&PathRow> = Vec::new();
    for row in rows {
        if row.kind == "dir" {
            continue;
        }
        total_bytes += row.size.unwrap_or(0);
        let entry = groups
            .entry(extension_of(&row.path))
            .or_insert((0u64, 0u64));
        entry.0 += 1;
        entry.1 += row.size.unwrap_or(0);
        files.push(row);
    }
    let mut by_extension: Vec<Value> = groups
        .into_iter()
        .map(|(extension, (count, bytes))| {
            json!({
                "extension": extension,
                "count": count,
                "bytes": bytes,
            })
        })
        .collect();
    by_extension.sort_by(|a, b| {
        b["count"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["count"].as_u64().unwrap_or(0))
            .then_with(|| {
                a["extension"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["extension"].as_str().unwrap_or(""))
            })
    });
    by_extension.truncate(limits.group_limit);
    files.sort_by(|a, b| b.size.unwrap_or(0).cmp(&a.size.unwrap_or(0)));
    let largest: Vec<Value> = files.iter().take(limits.top_n).map(|row| row_json(row)).collect();
    files.sort_by(|a, b| {
        b.modified_at
            .unwrap_or(0)
            .cmp(&a.modified_at.unwrap_or(0))
    });
    let newest: Vec<Value> = files.iter().take(limits.top_n).map(|row| row_json(row)).collect();
    DigestStats {
        total_bytes,
        by_extension,
        largest,
        newest,
    }
}

pub struct DigestStats {
    pub total_bytes: u64,
    pub by_extension: Vec<Value>,
    pub largest: Vec<Value>,
    pub newest: Vec<Value>,
}

fn row_json(row: &PathRow) -> Value {
    let mut value = json!({
        "path": row.path,
        "kind": row.kind,
    });
    if let Some(size) = row.size {
        value["size"] = json!(size);
    }
    if let Some(modified) = row.modified_at {
        value["modifiedAt"] = json!(modified);
    }
    value
}

pub(crate) fn take_rows(rows: &[PathRow], limit: usize) -> Vec<Value> {
    rows.iter().take(limit).map(row_json).collect()
}

/// Lowercased extension without the dot, or "(none)" for extensionless names
/// (dotfiles count as extensionless).
pub fn extension_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            extension.to_ascii_lowercase()
        }
        _ => "(none)".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Glob matching (pure, unit-tested): ** crosses segments, * stays within one,
// ? matches a single char.
// ---------------------------------------------------------------------------

pub fn glob_match(pattern: &str, text: &str) -> bool {
    // Slash-free patterns match the basename (common glob semantics);
    // path-shaped patterns match against the full path. The leading slash of
    // both sides is trimmed so "/a/*.txt" and "a/*.txt" behave identically
    // (otherwise split('/') would inject an empty leading pattern segment).
    if !pattern.contains('/') {
        let basename = text.rsplit('/').next().unwrap_or(text);
        return segment_match(pattern, basename);
    }
    let text = text.trim_start_matches('/');
    let pattern = pattern.trim_start_matches('/');
    match_segments(
        &pattern.split('/').collect::<Vec<_>>(),
        &text.split('/').collect::<Vec<_>>(),
    )
}

fn match_segments(pattern: &[&str], text: &[&str]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((&"**", rest)) => {
            // `**` consumes zero or more segments.
            if match_segments(rest, text) {
                return true;
            }
            (1..=text.len()).any(|index| match_segments(rest, &text[index..]))
        }
        Some((&first, rest)) => {
            let Some((head, tail)) = text.split_first() else {
                return false;
            };
            segment_match(first, head) && match_segments(rest, tail)
        }
    }
}

/// Single-segment wildcard match (`*` and `?` never cross '/').
fn segment_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    fn inner(p: &[char], t: &[char]) -> bool {
        match (p.split_first(), t.split_first()) {
            (None, None) => true,
            (None, Some(_)) => false,
            (Some((&'*', rest)), _) => (0..=t.len()).any(|skip| inner(rest, &t[skip..])),
            (Some((&'?', rest)), Some((_, t_rest))) => inner(rest, t_rest),
            (Some((&pc, rest)), Some((&tc, t_rest))) => pc == tc && inner(rest, t_rest),
            (Some(_), None) => false,
        }
    }
    inner(&pattern, &text)
}
