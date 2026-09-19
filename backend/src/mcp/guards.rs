// ---------------------------------------------------------------------------
// Gates + audit (main.rs-aligned copies; kept local so the mcp module owns its
// full decision surface without importing main)
// ---------------------------------------------------------------------------

use crate::model::DirJobRequest;
use crate::store::{AuditRecord, Store};

use serde_json::Value;

use super::{
    missing_required, normalize_slashes, numeric_arg_u64, required_str, validate_path_shape,
    MCP_SOURCE,
};

// -- connection gates (bindings carry the flags; the message texts mirror the
//    StoredConnection twins pinned by main.rs's gate-parity tests) ----------

pub(crate) fn ensure_binding_writable(
    binding: &crate::rclone::registry::RemoteBinding,
) -> Result<(), String> {
    if binding.read_only {
        Err("Connection is read-only; write operations are rejected".to_string())
    } else {
        Ok(())
    }
}

pub(crate) fn ensure_binding_deletable(
    binding: &crate::rclone::registry::RemoteBinding,
) -> Result<(), String> {
    if binding.read_only {
        Err("Connection is read-only; delete operations are rejected".to_string())
    } else if !binding.allow_delete {
        Err(
            "Connection disallows delete operations (allow_delete=false); enable allowDelete \
             on the connection (or use a connection that allows it) to run delete/purge"
                .to_string(),
        )
    } else {
        Ok(())
    }
}

/// Root refusal for the rclone arms, message-identical to main.rs
/// `refuse_purge_of_root` (§8.2 red line).
pub(crate) fn refuse_root_purge_root(root: &str, path: &str) -> Result<(), String> {
    fn core(path: &str) -> String {
        path.trim().trim_matches('/').to_string()
    }
    if core(path).is_empty() {
        return Err(
            "Purge of the connection root '/' is refused; purge a subdirectory instead"
                .to_string(),
        );
    }
    if !root.is_empty() && core(path) == core(root) {
        return Err(format!(
            "Purge of the connection root '{root}' is refused; purge a subdirectory instead"
        ));
    }
    Ok(())
}

/// MCP-write audit: same AuditRecord baseline, `source:"mcp"` (design §4).
pub(crate) fn audit_mcp_id(store: &Store, connection_id: &str, action: &str, target: &str, result: &str) {
    let record = AuditRecord {
        time: crate::store::format_rfc3339(crate::store::unix_millis_now() as i64),
        connection_id: connection_id.to_string(),
        action: action.to_string(),
        target: target.to_string(),
        result: result.to_string(),
        source: Some(MCP_SOURCE.to_string()),
    };
    if let Err(error) = store.append_audit(record) {
        eprintln!("[io.dbx.files] mcp audit write failed: {error}");
    }
}

// ---------------------------------------------------------------------------
// files_sync: directory sync over MCP (§8.4)
// ---------------------------------------------------------------------------

/// stdio refusal for `files_sync` (files_rename dir-path parity): the dir
/// job's progress events need the host event channel, which standalone stdio
/// does not have.
pub(crate) const SYNC_STDIO_UNAVAILABLE: &str = "files_sync runs as an async progress job over the DBX event \
     channel, which is unavailable in standalone stdio mode; call it through the DBX MCP bridge \
     (dbx_call_plugin_tool) or use the DBX workbench transfers pane";

/// Boolean argument with a default (absent/null → `default`), LLM string
/// tolerance for the intent spellings (`"true"/"1"/"yes"/"on"` and inverses —
/// model.rs `bool_field` 同族口径), and a fail-fast error for any other
/// present-but-invalid value: a silent flip on the sync delete switch would
/// be worse than a clear error.
fn bool_arg_or(arguments: &Value, key: &str, default: bool) -> Result<bool, String> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(Value::String(text)) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            _ => Err(format!("Parameter '{key}' must be a boolean (true/false)")),
        },
        Some(_) => Err(format!("Parameter '{key}' must be a boolean (true/false)")),
    }
}

// ---------------------------------------------------------------------------
// files_sync over the rclone engine (Phase D): id-level validation only —
// connection bindings, gates and path whitelists are re-applied by the shared
// workbench starter (`main.rs::rclone_start_dir_job`), defense in depth.
// ---------------------------------------------------------------------------

/// rclone-mode twin of [`SyncRequest`]: connection ids instead of records
/// (the rclone registry resolves them inside the starter).
#[derive(Debug)]
pub(crate) struct RcloneSyncRequest {
    pub(crate) request: DirJobRequest,
    pub(crate) sync: bool,
}

/// Pure parse for `files_sync` (id-level validation only): missing-parameter
/// enumeration, LLM-tolerant flags, path normalization and path-shape hard
/// gates. The binding gates run in the shared workbench starter.
pub(crate) fn parse_rclone_sync_request(arguments: &Value) -> Result<RcloneSyncRequest, String> {
    missing_required(
        arguments,
        &["sourceConnectionId", "sourcePath", "targetConnectionId", "targetPath"],
    )?;
    let sync = bool_arg_or(arguments, "sync", false)?;
    let dry_run = bool_arg_or(arguments, "dryRun", false)?;
    let max_delete = numeric_arg_u64(arguments, "maxDelete")?;
    // '/' is legitimate on both sides (whole-tree sync, rclone style);
    // only the shape gates apply.
    let source_path = normalize_slashes(required_str(arguments, "sourcePath")?);
    let target_path = normalize_slashes(required_str(arguments, "targetPath")?);
    validate_path_shape(&source_path, "sourcePath")?;
    validate_path_shape(&target_path, "targetPath")?;
    Ok(RcloneSyncRequest {
        sync,
        request: DirJobRequest {
            source_connection_id: required_str(arguments, "sourceConnectionId")?.to_string(),
            source_path,
            target_connection_id: required_str(arguments, "targetConnectionId")?.to_string(),
            target_path,
            dry_run: Some(dry_run),
            max_delete,
        },
    })
}
