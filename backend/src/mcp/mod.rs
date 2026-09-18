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
use serde_json::Value;

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


mod definitions;
mod tools;

pub use tools::{RcloneRoute, SyncJobStarter};
pub(crate) use tools::unknown_tool_message;
use tools::ALL_TOOL_NAMES;
#[cfg(test)]
use tools::{normalized_format, UI_TOOLS, WRITE_TOOLS};

mod scan;

use scan::{aggregate_rows, take_rows, walk_subtree, DigestLimits, PathRow, ScanFilter, WalkState};


mod truncate;

use truncate::{cap_response, content_envelope};

mod guards;

use guards::{
    audit_mcp, audit_mcp_id, ensure_binding_deletable, ensure_binding_writable, ensure_deletable,
    ensure_writable, parse_rclone_sync_request, parse_sync_request, refuse_root_purge,
    refuse_root_purge_root, SYNC_STDIO_UNAVAILABLE,
};


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
