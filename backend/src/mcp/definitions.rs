use serde_json::{json, Value};

use crate::engine::Engine;
use crate::model::StoredConnection;

use super::*;

impl Mcp {
    /// `mcp/tools` (discovery). `params.lifecycle` / `params.connectionId` are
    /// optional: when they resolve to a read-only connection the write tools
    /// are excluded from the list (design §4 "不注册进工具清单"); without a
    /// resolvable connection all tools are listed and `mcp/call` re-gates.
    /// Phase D: the connectionId lookup consults the rclone registry when the
    /// sidecar runs the rclone engine.
    pub fn tool_definitions(&self, engine: &Engine, params: &Value) -> Value {
        let read_only = params
            .get("lifecycle")
            .and_then(|lifecycle| StoredConnection::from_lifecycle_params(lifecycle).ok())
            .map(|connection| connection.read_only)
            .or_else(|| {
                params
                    .get("connectionId")
                    .and_then(Value::as_str)
                    .and_then(|id| {
                        if let Some(route) = self.rclone_route() {
                            return route
                                .engine
                                .registry
                                .get(id)
                                .map(|binding| binding.read_only);
                        }
                        engine.connection(id).ok().map(|connection| connection.read_only)
                    })
            })
            .unwrap_or(false);
        self.definitions_for(read_only)
    }

    pub(crate) fn definitions_for(&self, read_only: bool) -> Value {
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
    pub(crate) fn stdio_tool_list(&self) -> Value {
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
