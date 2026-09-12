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
//! - **Token economy**: single-response cap 16 KiB (truncated flag), cell
//!   width 120 (locator fields never truncated), rows format clamped at 20,
//!   binary content never leaves the sidecar over MCP.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use dbx_plugin_sdk::PluginEmitter;
use serde_json::{json, Value};

use crate::engine::{ops, Engine};
use crate::model::{StoredConnection, MAX_INLINE_WRITE_BYTES};
use crate::store::{AuditRecord, Store};
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
/// Locator fields keep their full text across every truncation pass.
const ANCHOR_FIELDS: [&str; 3] = ["path", "intentId", "cursorId"];
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

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Operator-tunable MCP limits, persisted in `<plugin_data_dir>/mcp-settings.json`.
/// Shared field names/semantics mirror the ldap Go `Settings` shape (AGENTS.md
/// 硬性规则「同族参数一致」); the trailing four are files-specific cursor/confirm
/// budgets (design §3.4/§4, ldap has no cursor materialization of paths).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSettings {
    /// UI intent report wait (design §1: default 5s, tunable).
    pub report_wait_millis: u64,
    /// Per-cell truncation width (locator fields never truncated, §3).
    pub cell_width: usize,
    /// groupBy group cap (design hard cap 20).
    pub digest_group_limit: usize,
    /// topN sample cap (design hard cap 10).
    pub digest_top_n: usize,
    /// digest sample rows (design hard cap 5).
    pub digest_sample_rows: usize,
    /// `format:"rows"` per-call rows (design hard cap 20).
    pub digest_row_limit: usize,
    /// Single-tool response cap (default 16 KiB, §3).
    pub response_limit_bytes: usize,
    /// Materialized cursor rows cap (design §3.4: 1 万条).
    pub max_cursor_rows: usize,
    /// Cursor session TTL, 10 minutes (design §3.4).
    pub cursor_ttl_secs: u64,
    /// Cursor LRU capacity (design §3.4: ≤8 会话).
    pub max_cursor_sessions: usize,
    /// confirmToken TTL, 60 seconds (design §4).
    pub confirm_ttl_secs: u64,
}

impl Default for McpSettings {
    fn default() -> Self {
        Self {
            report_wait_millis: DEFAULT_REPORT_WAIT_MILLIS,
            cell_width: DEFAULT_CELL_WIDTH,
            digest_group_limit: GROUP_LIMIT,
            digest_top_n: TOP_N,
            digest_sample_rows: SAMPLE_ROWS,
            digest_row_limit: ROWS_FORMAT_LIMIT,
            response_limit_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_cursor_rows: DEFAULT_MAX_CURSOR_ROWS,
            cursor_ttl_secs: DEFAULT_CURSOR_TTL_SECS,
            max_cursor_sessions: DEFAULT_MAX_CURSOR_SESSIONS,
            confirm_ttl_secs: DEFAULT_CONFIRM_TTL_SECS,
        }
    }
}

impl McpSettings {
    /// Clamps every field into its valid range. Applied after loading the
    /// settings file and after every update, so a corrupted or hand-edited
    /// file can never disable the caps (ssh `McpLimits::sanitized` pattern).
    fn sanitized(self) -> Self {
        Self {
            report_wait_millis: self.report_wait_millis.clamp(1, REPORT_WAIT_MILLIS_CEILING),
            cell_width: self.cell_width.clamp(1, CELL_WIDTH_CEILING),
            digest_group_limit: self.digest_group_limit.clamp(1, GROUP_LIMIT),
            digest_top_n: self.digest_top_n.clamp(1, TOP_N),
            digest_sample_rows: self.digest_sample_rows.clamp(1, SAMPLE_ROWS),
            digest_row_limit: self.digest_row_limit.clamp(1, ROWS_FORMAT_LIMIT),
            response_limit_bytes: self.response_limit_bytes.clamp(1024, MAX_RESPONSE_BYTES_CEILING),
            max_cursor_rows: self.max_cursor_rows.clamp(100, MAX_CURSOR_ROWS_CEILING),
            cursor_ttl_secs: self.cursor_ttl_secs.clamp(10, CURSOR_TTL_SECS_CEILING),
            max_cursor_sessions: self
                .max_cursor_sessions
                .clamp(1, MAX_CURSOR_SESSIONS_CEILING),
            confirm_ttl_secs: self.confirm_ttl_secs.clamp(10, CONFIRM_TTL_SECS_CEILING),
        }
    }

    /// Parses the persisted JSON, falling back per-field to the defaults for
    /// missing or non-numeric entries.
    fn from_json(value: &Value) -> Self {
        let defaults = Self::default();
        let u = |key: &str, fallback: u64| value.get(key).and_then(Value::as_u64).unwrap_or(fallback);
        let us = |key: &str, fallback: usize| u(key, fallback as u64) as usize;
        Self {
            report_wait_millis: u("reportWaitMs", defaults.report_wait_millis),
            cell_width: us("cellWidth", defaults.cell_width),
            digest_group_limit: us("digestGroupLimit", defaults.digest_group_limit),
            digest_top_n: us("digestTopN", defaults.digest_top_n),
            digest_sample_rows: us("digestSampleRows", defaults.digest_sample_rows),
            digest_row_limit: us("digestRowLimit", defaults.digest_row_limit),
            response_limit_bytes: us("responseLimitBytes", defaults.response_limit_bytes),
            max_cursor_rows: us("maxCursorRows", defaults.max_cursor_rows),
            cursor_ttl_secs: u("cursorTtlSecs", defaults.cursor_ttl_secs),
            max_cursor_sessions: us("maxCursorSessions", defaults.max_cursor_sessions),
            confirm_ttl_secs: u("confirmTtlSecs", defaults.confirm_ttl_secs),
        }
        .sanitized()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "reportWaitMs": self.report_wait_millis,
            "cellWidth": self.cell_width,
            "digestGroupLimit": self.digest_group_limit,
            "digestTopN": self.digest_top_n,
            "digestSampleRows": self.digest_sample_rows,
            "digestRowLimit": self.digest_row_limit,
            "responseLimitBytes": self.response_limit_bytes,
            "maxCursorRows": self.max_cursor_rows,
            "cursorTtlSecs": self.cursor_ttl_secs,
            "maxCursorSessions": self.max_cursor_sessions,
            "confirmTtlSecs": self.confirm_ttl_secs,
        })
    }

    /// Reads the persisted settings; a missing or corrupted file falls back
    /// to the defaults and never fails.
    fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .map(|value| Self::from_json(&value))
            .unwrap_or_default()
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let text = serde_json::to_string_pretty(&self.to_json())
            .map_err(|error| format!("Failed to encode MCP settings: {error}"))?;
        std::fs::write(path, text)
            .map_err(|error| format!("Failed to write MCP settings {}: {error}", path.display()))
    }
}

// ---------------------------------------------------------------------------
// State tables
// ---------------------------------------------------------------------------

/// One UI intent tracked between the `files/ui/intent` emit and the frontend
/// report (design §1 "intent 状态表").
#[derive(Debug, Clone)]
struct IntentEntry {
    action: String,
    #[allow(dead_code)]
    params: Value,
    status: String, // pending | applied | rejected
    summary: Option<Value>,
    reason: Option<String>,
    expires_at_millis: u128,
}

/// Three-state result of an intent table lookup (ldap `LookupStatus` parity).
enum IntentLookup {
    Found(Value),
    Expired,
    Unknown,
}

struct CursorSession {
    rows: Vec<String>,
    offset: usize,
    expires_at_millis: u128,
}

/// A one-time two-phase confirm token (design §4): parameter-hash bound,
/// TTL'd, consumed on first use.
struct ConfirmEntry {
    hash: u64,
    expires_at_millis: u128,
}

/// LRU-bounded ordered map: the least-recently-touched entry is evicted past
/// `cap`. Tiny capacities (8 / 20) make the linear scan the honest choice.
struct LruTable<T> {
    entries: Vec<(String, T)>,
    cap: usize,
}

impl<T> LruTable<T> {
    fn new(cap: usize) -> Self {
        Self {
            entries: Vec::new(),
            cap,
        }
    }

    fn get(&mut self, key: &str) -> Option<&mut T> {
        let index = self.entries.iter().position(|(id, _)| id == key)?;
        let (id, entry) = self.entries.remove(index);
        self.entries.push((id, entry));
        self.entries.last_mut().map(|(_, entry)| entry)
    }

    fn put(&mut self, key: String, entry: T) {
        self.entries.retain(|(id, _)| id != &key);
        self.entries.push((key, entry));
        while self.entries.len() > self.cap {
            self.entries.remove(0);
        }
    }

    fn remove(&mut self, key: &str) -> Option<T> {
        let index = self.entries.iter().position(|(id, _)| id == key)?;
        Some(self.entries.remove(index).1)
    }

    fn retain<F>(&mut self, mut keep: F)
    where
        F: FnMut(&T) -> bool,
    {
        self.entries.retain(|(_, entry)| keep(entry));
    }
}

/// All MCP in-process state: settings mirror + intent table + snapshot +
/// cursor sessions + confirm tokens. Shared behind `Arc` from `Plugin`.
pub struct Mcp {
    settings: RwLock<McpSettings>,
    settings_path: PathBuf,
    intents: Mutex<LruTable<IntentEntry>>,
    snapshot: Mutex<Option<Value>>,
    cursors: Mutex<LruTable<CursorSession>>,
    confirms: Mutex<HashMap<String, ConfirmEntry>>,
}

impl Mcp {
    pub fn new(data_dir: PathBuf) -> Self {
        let settings_path = data_dir.join("mcp-settings.json");
        let settings = McpSettings::load(&settings_path);
        Self {
            settings: RwLock::new(settings),
            settings_path,
            intents: Mutex::new(LruTable::new(INTENT_MAX_ENTRIES)),
            snapshot: Mutex::new(None),
            cursors: Mutex::new(LruTable::new(DEFAULT_MAX_CURSOR_SESSIONS)),
            confirms: Mutex::new(HashMap::new()),
        }
    }

    fn current_settings(&self) -> McpSettings {
        self.settings
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// `mcp/settings/get`: the effective MCP limits, wrapped as
    /// `{settings, responseLimitBytes}` (ldap `SettingsGet` shape parity).
    pub fn settings_get(&self) -> Value {
        let settings = self.current_settings();
        json!({
            "settings": settings.to_json(),
            "responseLimitBytes": settings.response_limit_bytes,
        })
    }

    /// `mcp/settings/set`: whitelist partial update with per-field validation,
    /// persisted to `mcp-settings.json`; returns the complete wrapped object.
    /// Unknown fields are tolerated (partial-update semantics, ldap parity).
    pub fn settings_set(&self, updates: &Value) -> Result<Value, String> {
        fn checked_u64(
            updates: &Value,
            key: &str,
            ceiling: u64,
        ) -> Result<Option<u64>, String> {
            match updates.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(value) => {
                    let parsed = value.as_u64().ok_or_else(|| {
                        format!("{key} must be a positive integer")
                    })?;
                    if parsed < 1 || parsed > ceiling {
                        return Err(format!("{key} must be between 1 and {ceiling}"));
                    }
                    Ok(Some(parsed))
                }
            }
        }
        let mut limits = self.current_settings();
        if let Some(value) = checked_u64(updates, "reportWaitMs", REPORT_WAIT_MILLIS_CEILING)? {
            limits.report_wait_millis = value;
        }
        if let Some(value) = checked_u64(updates, "cellWidth", CELL_WIDTH_CEILING as u64)? {
            limits.cell_width = value as usize;
        }
        if let Some(value) = checked_u64(updates, "digestGroupLimit", GROUP_LIMIT as u64)? {
            limits.digest_group_limit = value as usize;
        }
        if let Some(value) = checked_u64(updates, "digestTopN", TOP_N as u64)? {
            limits.digest_top_n = value as usize;
        }
        if let Some(value) = checked_u64(updates, "digestSampleRows", SAMPLE_ROWS as u64)? {
            limits.digest_sample_rows = value as usize;
        }
        if let Some(value) = checked_u64(updates, "digestRowLimit", ROWS_FORMAT_LIMIT as u64)? {
            limits.digest_row_limit = value as usize;
        }
        if let Some(value) =
            checked_u64(updates, "responseLimitBytes", MAX_RESPONSE_BYTES_CEILING as u64)?
        {
            limits.response_limit_bytes = value as usize;
        }
        if let Some(value) = checked_u64(updates, "maxCursorRows", MAX_CURSOR_ROWS_CEILING as u64)? {
            limits.max_cursor_rows = value as usize;
        }
        if let Some(value) = checked_u64(updates, "cursorTtlSecs", CURSOR_TTL_SECS_CEILING)? {
            limits.cursor_ttl_secs = value;
        }
        if let Some(value) =
            checked_u64(updates, "maxCursorSessions", MAX_CURSOR_SESSIONS_CEILING as u64)?
        {
            limits.max_cursor_sessions = value as usize;
        }
        if let Some(value) = checked_u64(updates, "confirmTtlSecs", CONFIRM_TTL_SECS_CEILING)? {
            limits.confirm_ttl_secs = value;
        }
        limits = limits.sanitized();
        limits.save(&self.settings_path)?;
        *self
            .settings
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = limits.clone();
        Ok(self.settings_get())
    }

    // -- intent state table (design §1) --------------------------------------

    /// Registers a new `pending` intent and evicts stale/overflowing entries.
    fn register_intent(&self, intent_id: &str, action: &str, params: Value, now: u128) {
        let mut intents = self.lock_intents();
        intents.retain(|entry| entry.expires_at_millis > now);
        intents.put(
            intent_id.to_string(),
            IntentEntry {
                action: action.to_string(),
                params,
                status: "pending".to_string(),
                summary: None,
                reason: None,
                expires_at_millis: now + INTENT_TTL_MILLIS,
            },
        );
    }

    /// `files/ui/state/report`: updates the intent entry (with intentId) or
    /// stores the latest UI snapshot (without, ldap `ReportUIState` semantics).
    /// Unknown/expired intentIds are an explicit error — a late report from the
    /// frontend surfaces only on the frontend path, never blocks a caller.
    pub fn report(&self, params: &Value) -> Result<Value, String> {
        let intent_id = params
            .get("intentId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let status = params
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("applied")
            .trim()
            .to_string();
        match intent_id {
            Some(id) => {
                if !matches!(status.as_str(), "applied" | "rejected") {
                    return Err("status must be applied or rejected".to_string());
                }
                let now = unix_millis_now() as u128;
                let mut intents = self.lock_intents();
                let entry = intents.get(id);
                match entry {
                    Some(entry) if entry.expires_at_millis > now => {
                        entry.status = status;
                        entry.summary = params.get("summary").cloned();
                        entry.reason = params
                            .get("reason")
                            .and_then(Value::as_str)
                            .map(|value| value.trim().to_string());
                        Ok(json!({ "success": true, "intentId": id }))
                    }
                    Some(_) => {
                        intents.remove(id);
                        Err(format!("intent \"{id}\" is unknown or expired"))
                    }
                    None => Err(format!("intent \"{id}\" is unknown or expired")),
                }
            }
            None => {
                // Snapshot-type report: the frontend pushes its latest UI
                // state after key actions (design §1 "UI 快照"); the snapshot
                // is the summary payload only (ldap `SetSnapshot` parity).
                let mut snapshot = self
                    .snapshot
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                *snapshot = Some(params.get("summary").cloned().unwrap_or_else(|| json!({})));
                Ok(json!({ "success": true }))
            }
        }
    }

    /// Three-state intent lookup mirroring the ldap `IntentStore::Get`:
    /// found / expired (pruned on read) / unknown.
    fn intent_lookup(&self, intent_id: &str, now: u128) -> IntentLookup {
        let mut intents = self.lock_intents();
        let Some(entry) = intents.get(intent_id) else {
            return IntentLookup::Unknown;
        };
        if entry.expires_at_millis <= now {
            intents.remove(intent_id);
            return IntentLookup::Expired;
        }
        let mut value = json!({
            "intentId": intent_id,
            "action": entry.action,
            "state": entry.status,
        });
        if let Some(summary) = &entry.summary {
            value["summary"] = summary.clone();
        }
        if let Some(reason) = &entry.reason {
            value["reason"] = json!(reason);
        }
        IntentLookup::Found(value)
    }

    fn lock_intents(&self) -> std::sync::MutexGuard<'_, LruTable<IntentEntry>> {
        self.intents
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    // -- cursor sessions (design §3.4) ---------------------------------------

    /// Materializes locator rows (paths) into a fresh cursor session and
    /// returns its id. Rows beyond `max_cursor_rows` are dropped (the digest
    /// response reports `cursorTruncated`).
    fn cursor_put(&self, rows: Vec<String>) -> (String, bool) {
        let settings = self.current_settings();
        let truncated = rows.len() > settings.max_cursor_rows;
        let mut rows = rows;
        rows.truncate(settings.max_cursor_rows);
        let cursor_id = format!("cur-{}", uuid::Uuid::new_v4().simple());
        let now = unix_millis_now() as u128;
        let mut cursors = self.lock_cursors();
        cursors.retain(|session| session.expires_at_millis > now);
        cursors.put(
            cursor_id.clone(),
            CursorSession {
                rows,
                offset: 0,
                expires_at_millis: now + u128::from(settings.cursor_ttl_secs) * 1_000,
            },
        );
        (cursor_id, truncated)
    }

    /// `files_cursor_next {cursorId, n<=20, offset?}`: the next batch of
    /// locator rows. Conditions are never re-sent and the source is never
    /// re-scanned; an unknown cursor is distinct from an expired one (ldap
    /// `CursorStore::Next` semantics) and both answer with an actionable
    /// error suggesting a fresh digest (design §3.4).
    fn cursor_next(&self, arguments: &Value) -> Result<Value, String> {
        let cursor_id = required_str(arguments, "cursorId")?;
        let n = arguments
            .get("n")
            .and_then(Value::as_u64)
            .unwrap_or(CURSOR_PAGE)
            .clamp(1, MAX_CURSOR_PAGE);
        // offset<0 / absent = continue from the in-session cursor.
        let offset = arguments
            .get("offset")
            .and_then(Value::as_i64)
            .map(|value| usize::try_from(value.max(0)).unwrap_or(0));
        let now = unix_millis_now() as u128;
        let mut cursors = self.lock_cursors();
        let Some(session) = cursors.get(cursor_id) else {
            return Err(format!("unknown cursorId: {cursor_id}"));
        };
        if session.expires_at_millis <= now {
            cursors.remove(cursor_id);
            return Err(format!(
                "cursor expired (10 minutes); re-run files_scan_digest"
            ));
        }
        let start = offset.unwrap_or(session.offset).min(session.rows.len());
        let end = (start + n as usize).min(session.rows.len());
        let rows: Vec<Value> = session.rows[start..end]
            .iter()
            .map(|path| json!({ "path": path }))
            .collect();
        session.offset = end;
        Ok(json!({
            "rows": rows,
            "offset": start,
            "nextOffset": end,
            "done": end >= session.rows.len(),
        }))
    }

    fn lock_cursors(&self) -> std::sync::MutexGuard<'_, LruTable<CursorSession>> {
        self.cursors
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    // -- two-phase confirm tokens (design §4) --------------------------------

    /// Starts the two-phase flow: stores a fresh one-time token bound to the
    /// parameter hash and returns `(token, expiresAtMillis)`.
    fn confirm_begin(&self, arguments: &Value) -> (String, u128) {
        let settings = self.current_settings();
        let token = format!("c-{}", uuid::Uuid::new_v4().simple());
        let now = unix_millis_now() as u128;
        self.confirms
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(
                token.clone(),
                ConfirmEntry {
                    hash: params_hash(arguments),
                    expires_at_millis: now + u128::from(settings.confirm_ttl_secs) * 1_000,
                },
            );
        (token, now + u128::from(settings.confirm_ttl_secs) * 1_000)
    }

    /// Verifies and consumes the token. The token is one-time: it is removed
    /// whatever the outcome, so a parameter mismatch (or a replay) always
    /// requires a fresh preview.
    fn confirm_verify(&self, token: &str, arguments: &Value) -> Result<(), String> {
        let entry = self
            .confirms
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(token);
        let Some(entry) = entry else {
            return Err(
                "confirmToken unknown or already used; request a new preview".to_string(),
            );
        };
        if entry.expires_at_millis <= unix_millis_now() as u128 {
            return Err("confirmToken expired (60s); request a new preview".to_string());
        }
        if entry.hash != params_hash(arguments) {
            return Err(
                "arguments changed since the preview; request a new confirmToken".to_string(),
            );
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tool dispatch (`mcp/call`)
// ---------------------------------------------------------------------------

/// Write tools are excluded from `mcp/tools` for read-only connections
/// (design §4 "而非注册了再报错") and re-gated at execution time.
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
            .run_tool(tool, &arguments, engine, transfers, store, emitter)
            .await?;
        let settings = self.current_settings();
        cap_response(
            &mut payload,
            settings.response_limit_bytes,
            settings.cell_width,
        );
        Ok(content_envelope(&payload))
    }

    async fn run_tool(
        &self,
        tool: &str,
        arguments: &Value,
        engine: &Engine,
        transfers: &JobTable,
        store: &Store,
        emitter: &PluginEmitter,
    ) -> Result<Value, String> {
        match tool {
            // -- UI intent tools (design §1/§6.2) --------------------------------
            "files_ui_focus" => self
                .ui_intent_tool(emitter, tool, arguments, |arguments| {
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
                        IntentLookup::Unknown => Err(format!("unknown intentId: {id}")),
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
                let limit = arguments
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(QUICK_PATHS_LIMIT)
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
                let connection = engine.connection(required_str(arguments, "connectionId")?)?;
                ensure_writable(&connection)?;
                let path = required_str(arguments, "path")?;
                let data_base64 = required_str(arguments, "dataBase64")?;
                let data = BASE64_STANDARD
                    .decode(data_base64.as_bytes())
                    .map_err(|error| format!("Invalid base64 file data: {error}"))?;
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
                ops::write(&operator, path, data).await?;
                audit_mcp(store, &connection, "files/write", path, "ok");
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
                let connection = engine.connection(required_str(arguments, "connectionId")?)?;
                ensure_writable(&connection)?;
                let path = required_str(arguments, "path")?;
                let operator = engine.operator(&connection.id)?;
                ops::mkdir(&operator, path).await?;
                audit_mcp(store, &connection, "files/mkdir", path, "ok");
                Ok(json!({ "success": true, "path": path }))
            }
            "files_rename" => {
                let connection = engine.connection(required_str(arguments, "connectionId")?)?;
                ensure_writable(&connection)?;
                // Rename removes the source path — same delete gate as the
                // workbench path (main.rs files/rename).
                ensure_deletable(&connection)?;
                let path = required_str(arguments, "path")?;
                let new_path = required_str(arguments, "newPath")?;
                let operator = engine.operator(&connection.id)?;
                let source_is_dir = ops::is_dir_path(&operator, path).await?;
                if source_is_dir {
                    // Directory rename degrades to the async copy+delete job,
                    // identical to the workbench path.
                    let job_id = transfers
                        .enqueue_copy_job(
                            &connection,
                            &operator,
                            &connection,
                            &operator,
                            path,
                            new_path,
                            true,
                            DirJobKind::Rename,
                            emitter,
                        )
                        .await?;
                    audit_mcp(store, &connection, "files/rename", path, "ok");
                    return Ok(json!({
                        "success": true,
                        "transport": "job",
                        "jobId": job_id,
                        "path": path,
                        "newPath": new_path,
                        "hint": "Directory rename runs as an async job; poll files/transfer/status",
                    }));
                }
                ops::rename(&operator, path, new_path).await?;
                audit_mcp(store, &connection, "files/rename", path, "ok");
                Ok(json!({ "success": true, "transport": "native", "path": path, "newPath": new_path }))
            }
            "files_delete" | "files_purge" => {
                let connection = engine.connection(required_str(arguments, "connectionId")?)?;
                ensure_deletable(&connection)?;
                let path = required_str(arguments, "path")?;
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
                    ops::delete(&operator, path).await?;
                    "files/delete"
                } else {
                    ops::purge(&operator, path).await?;
                    "files/purge"
                };
                audit_mcp(store, &connection, action, path, "ok");
                Ok(json!({ "success": true, "path": path }))
            }

            other => Err(format!("Unknown tool: {other}")),
        }
    }

    /// Shared UI-intent flow (design §1): validate → register pending intent →
    /// emit `files/ui/intent` → wait for the frontend report → applied /
    /// rejected / pending(timeout). The wait is the degradation matrix's
    /// "工作台未打开 / 前端未响应" cell: never an error, always `pending` + hint.
    async fn ui_intent_tool<F>(
        &self,
        emitter: &PluginEmitter,
        tool: &str,
        arguments: &Value,
        build_params: F,
    ) -> Result<Value, String>
    where
        F: Fn(&Value) -> Result<Value, String>,
    {
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
        let start = arguments
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("/")
            .trim()
            .to_string();
        let start = if start.is_empty() { "/".to_string() } else { start };
        let depth = arguments
            .get("depth")
            .and_then(Value::as_u64)
            .unwrap_or(u64::from(DEFAULT_SCAN_DEPTH))
            .clamp(1, u64::from(MAX_SCAN_DEPTH)) as u32;
        let filter = ScanFilter::from_arguments(arguments)?;
        let format = arguments
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("digest");
        if !matches!(format, "digest" | "rows") {
            return Err("format must be 'digest' or 'rows'".to_string());
        }
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
        match format {
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
                                "dataBase64": { "type": "string", "description": "base64 file content (<=4 MiB decoded)" },
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
}

// ---------------------------------------------------------------------------
// Digest scan: filter, walk, aggregate
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
    fn from_arguments(arguments: &Value) -> Result<Self, String> {
        let read_u64 = |key: &str| -> Result<Option<u64>, String> {
            match arguments.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(value) => value
                    .as_u64()
                    .map(Some)
                    .ok_or_else(|| format!("{key} must be a non-negative integer")),
            }
        };
        Ok(Self {
            glob: arguments
                .get("glob")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            min_size: read_u64("minSizeBytes")?,
            max_size: read_u64("maxSizeBytes")?,
            modified_since: read_u64("modifiedSince")?,
            modified_until: read_u64("modifiedUntil")?,
        })
    }

    /// A row matches when the glob (if any) hits and the size/mtime predicates
    /// pass. Directory entries only need the glob (size/mtime predicates do
    /// not apply to them); they are always walked.
    fn matches(&self, row: &PathRow) -> bool {
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

/// Accumulator for the local walk.
struct WalkState {
    filter: ScanFilter,
    matched: Vec<PathRow>,
    scanned: usize,
    truncated: bool,
}

impl WalkState {
    fn new(filter: ScanFilter) -> Self {
        Self {
            filter,
            matched: Vec::new(),
            scanned: 0,
            truncated: false,
        }
    }
}

/// Breadth-first walk with a depth cap (design §6.2). Reuses `ops::list` per
/// level so directory vs. file kinds stay authoritative. `max_entries` is the
/// absolute scanned budget (100k clamp). Everything happens in the sidecar.
async fn walk_subtree(
    operator: &opendal::Operator,
    start: &str,
    depth: u32,
    state: &mut WalkState,
    max_entries: usize,
) -> Result<(), String> {
    let mut queue = std::collections::VecDeque::new();
    queue.push_back((start.to_string(), depth));
    while let Some((dir, remaining_depth)) = queue.pop_front() {
        if state.scanned >= max_entries {
            state.truncated = true;
            break;
        }
        let entries = ops::list(operator, &dir, false).await?;
        for entry in entries {
            state.scanned += 1;
            if state.scanned > max_entries {
                state.truncated = true;
                break;
            }
            let row = PathRow {
                path: entry.path.clone(),
                kind: entry.kind,
                size: entry.size,
                modified_at: entry.modified_at,
            };
            let is_dir = row.kind == "dir";
            if state.filter.matches(&row) {
                state.matched.push(row);
            }
            if is_dir && remaining_depth > 1 {
                queue.push_back((entry.path.clone(), remaining_depth - 1));
            }
        }
    }
    Ok(())
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

fn take_rows(rows: &[PathRow], limit: usize) -> Vec<Value> {
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

// ---------------------------------------------------------------------------
// Response envelope + token-economy caps
// ---------------------------------------------------------------------------

/// MCP content envelope (ssh `call_tool` shape).
fn content_envelope(payload: &Value) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(payload).unwrap_or_default(),
        }],
        "isError": false,
    })
}

/// Truncates a string to `width` chars (char-boundary safe).
pub fn truncate_cell(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        text.chars().take(width).collect()
    }
}

/// Token-economy caps on a finished tool payload (design §3): cells to
/// `cell_width` chars (anchor fields excepted), then arrays trimmed until the
/// serialization fits `max_bytes`; a final fallback hard-truncates remaining
/// long strings when no array can shrink further. `truncated: true` is set on
/// the top-level object when anything was cut. Returns the truncated flag.
pub fn cap_response(value: &mut Value, max_bytes: usize, cell_width: usize) -> bool {
    let cell_cut = cap_cells(value, cell_width);
    let mut trimmed = false;
    while serde_json::to_string(value)
        .map(|text| text.len())
        .unwrap_or(0)
        > max_bytes
    {
        if !trim_longest_array(value) {
            break;
        }
        trimmed = true;
    }
    let still_over = serde_json::to_string(value)
        .map(|text| text.len())
        .unwrap_or(0)
        > max_bytes;
    if still_over && hard_truncate_strings(value, cell_width) {
        trimmed = true;
    }
    let truncated = cell_cut || trimmed;
    if truncated {
        if let Some(map) = value.as_object_mut() {
            map.insert("truncated".to_string(), Value::Bool(true));
        }
    }
    truncated
}

/// Pass 1: every string cell longer than `width` is cut, except object
/// members named in [`ANCHOR_FIELDS`] (design §3 "定位字段不截断").
fn cap_cells(value: &mut Value, width: usize) -> bool {
    let mut cut = false;
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if ANCHOR_FIELDS.contains(&key.as_str()) {
                    continue;
                }
                cut |= cap_cells(entry, width);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                cut |= cap_cells(item, width);
            }
        }
        Value::String(text) if text.chars().count() > width => {
            *text = truncate_cell(text, width);
            cut = true;
        }
        _ => {}
    }
    cut
}

/// Pass 2: drop the last element of the largest non-empty array. Returns
/// false when no non-empty array remains.
fn trim_longest_array(value: &mut Value) -> bool {
    fn longest(value: &Value) -> Option<usize> {
        match value {
            Value::Object(map) => map.values().filter_map(longest).max(),
            Value::Array(items) => {
                let own = (!items.is_empty()).then_some(items.len());
                own.into_iter()
                    .chain(items.iter().filter_map(longest))
                    .max()
            }
            _ => None,
        }
    }
    fn pop(value: &mut Value, target_len: usize) -> bool {
        match value {
            Value::Object(map) => map.values_mut().any(|entry| pop(entry, target_len)),
            Value::Array(items) => {
                if items.len() == target_len {
                    items.pop();
                    return true;
                }
                items.iter_mut().any(|item| pop(item, target_len))
            }
            _ => false,
        }
    }
    longest(value).map(|len| pop(value, len)).unwrap_or(false)
}

/// Fallback pass: truncate every remaining long string (anchors included).
fn hard_truncate_strings(value: &mut Value, width: usize) -> bool {
    let mut cut = false;
    match value {
        Value::Object(map) => {
            for entry in map.values_mut() {
                cut |= hard_truncate_strings(entry, width);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                cut |= hard_truncate_strings(item, width);
            }
        }
        Value::String(text) if text.chars().count() > width => {
            *text = truncate_cell(text, width);
            cut = true;
        }
        _ => {}
    }
    cut
}

// ---------------------------------------------------------------------------
// Gates + audit (main.rs-aligned copies; kept local so the mcp module owns its
// full decision surface without importing main)
// ---------------------------------------------------------------------------

fn ensure_writable(connection: &StoredConnection) -> Result<(), String> {
    if connection.read_only {
        Err("Connection is read-only; write operations are rejected".to_string())
    } else {
        Ok(())
    }
}

fn ensure_deletable(connection: &StoredConnection) -> Result<(), String> {
    if connection.read_only {
        Err("Connection is read-only; delete operations are rejected".to_string())
    } else if !connection.allow_delete {
        Err("Connection disallows delete operations (allow_delete=false)".to_string())
    } else {
        Ok(())
    }
}

/// Same root refusal as main.rs `files/purge` (§8.2 red line).
fn refuse_root_purge(connection: &StoredConnection, path: &str) -> Result<(), String> {
    fn core(path: &str) -> String {
        path.trim().trim_matches('/').to_string()
    }
    if core(path).is_empty() {
        return Err(
            "Purge of the connection root '/' is refused; purge a subdirectory instead"
                .to_string(),
        );
    }
    if !connection.root.is_empty() && core(path) == core(&connection.root) {
        return Err(format!(
            "Purge of the connection root '{}' is refused; purge a subdirectory instead",
            connection.root
        ));
    }
    Ok(())
}

/// MCP-write audit: same AuditRecord baseline, `source:"mcp"` (design §4).
fn audit_mcp(store: &Store, connection: &StoredConnection, action: &str, target: &str, result: &str) {
    let record = AuditRecord {
        time: crate::store::format_rfc3339(crate::store::unix_millis_now() as i64),
        connection_id: connection.id.clone(),
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
// Small helpers
// ---------------------------------------------------------------------------

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| format!("Missing required parameter: {key}"))
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

// ---------------------------------------------------------------------------
// Tests (pure logic; no remote I/O)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mcp() -> Mcp {
        Mcp::new(std::env::temp_dir().join(format!(
            "dbx-files-mcp-test-{}",
            uuid::Uuid::new_v4()
        )))
    }

    fn row(path: &str, kind: &'static str, size: Option<u64>, modified: Option<u64>) -> PathRow {
        PathRow {
            path: path.to_string(),
            kind,
            size,
            modified_at: modified,
        }
    }

    // -- settings ------------------------------------------------------------

    #[test]
    fn settings_defaults_and_validation() {
        let mcp = mcp();
        let settings = McpSettings::from_json(&json!({}));
        assert_eq!(settings, McpSettings::default());
        // Wrapped shape (ldap SettingsGet parity).
        let wrapped = mcp.settings_get();
        assert_eq!(wrapped["responseLimitBytes"], 16_384, "{wrapped}");
        assert_eq!(wrapped["settings"]["reportWaitMs"], 5_000);
        assert_eq!(wrapped["settings"]["cellWidth"], 120);
        assert_eq!(wrapped["settings"]["digestGroupLimit"], 20);

        let updated = mcp
            .settings_set(&json!({ "reportWaitMs": 300, "cellWidth": 80 }))
            .unwrap();
        assert_eq!(updated["settings"]["reportWaitMs"], 300);
        assert_eq!(updated["settings"]["cellWidth"], 80);
        // Values apply to the live state.
        assert_eq!(mcp.current_settings().report_wait_millis, 300);
        assert_eq!(mcp.current_settings().cell_width, 80);
        // Invalid values are explicit errors.
        assert!(mcp.settings_set(&json!({ "reportWaitMs": 0 })).is_err());
        assert!(mcp
            .settings_set(&json!({ "responseLimitBytes": 64 * 1024 * 1024 }))
            .is_err());
        assert!(mcp.settings_set(&json!({ "cellWidth": "big" })).is_err());
        assert!(mcp.settings_set(&json!({ "reportWaitMs": 31_000 })).is_err());
        // Digest limits are pinned to the design hard caps.
        assert!(mcp.settings_set(&json!({ "digestGroupLimit": 21 })).is_err());
        assert!(mcp.settings_set(&json!({ "digestTopN": 11 })).is_err());
        assert!(mcp.settings_set(&json!({ "digestRowLimit": 20 })).is_ok());
        // Unknown fields are tolerated (partial-update semantics) and the
        // current values are untouched.
        let tolerated = mcp.settings_set(&json!({ "unknownField": 1 })).unwrap();
        assert_eq!(tolerated["settings"]["reportWaitMs"], 300);
    }

    #[test]
    fn settings_from_corrupt_json_falls_back_to_defaults() {
        let parsed = McpSettings::from_json(&json!({ "responseLimitBytes": "oops" }));
        assert_eq!(parsed.response_limit_bytes, DEFAULT_MAX_RESPONSE_BYTES);
        let parsed = McpSettings::from_json(&json!({ "cellWidth": -5 }));
        assert_eq!(parsed.cell_width, DEFAULT_CELL_WIDTH);
    }

    // -- glob matching -------------------------------------------------------

    #[test]
    fn glob_match_table() {
        assert!(glob_match("*.txt", "/a/b/report.txt"));
        assert!(!glob_match("*.txt", "/a/b/report.log"));
        assert!(
            !glob_match("/a/*.txt", "/a/b/c.txt"),
            "* stays in one segment"
        );
        assert!(
            glob_match("/a/**/*.txt", "/a/b/c/d.txt"),
            "** crosses segments"
        );
        assert!(
            glob_match("/a/**/*.txt", "/a/d.txt"),
            "** may match zero segments"
        );
        assert!(glob_match("/a/?ile.txt", "/a/file.txt"));
        assert!(!glob_match("/a/??ile.txt", "/a/file.txt"));
        assert!(glob_match("*.log", "/logs/app.log"));
        assert!(!glob_match("*.log", "/logs/app.txt"));
        assert!(glob_match("/logs/*.log", "/logs/sub/app.log") == false);
    }

    // -- digest aggregation (pure) --------------------------------------------

    #[test]
    fn digest_aggregates_groups_topn_and_total_bytes() {
        let rows = vec![
            row("/d/a.log", "file", Some(100), Some(1_000)),
            row("/d/b.log", "file", Some(50), Some(3_000)),
            row("/d/c.txt", "file", Some(900), Some(2_000)),
            row("/d/sub", "dir", None, None),
            row("/d/noext", "file", Some(10), Some(4_000)),
        ];
        let stats = aggregate_rows(&rows, &DigestLimits::default());
        // Dirs excluded from byte totals.
        assert_eq!(stats.total_bytes, 1_060);
        // groupBy extension: .log has 2 entries → first; tie of single-count
        // groups broken by name; "(none)" for extensionless files.
        let extensions: Vec<&str> = stats
            .by_extension
            .iter()
            .map(|group| group["extension"].as_str().unwrap())
            .collect();
        assert_eq!(extensions, vec!["log", "(none)", "txt"]);
        assert_eq!(stats.by_extension[0]["count"], 2);
        assert_eq!(stats.by_extension[0]["bytes"], 150);
        // topN largest ≤10, sorted desc, dirs never included.
        assert_eq!(stats.largest.len(), 4);
        assert_eq!(stats.largest[0]["path"], "/d/c.txt");
        // newest first by modifiedAt.
        assert_eq!(stats.newest[0]["path"], "/d/noext");
    }

    #[test]
    fn digest_group_limit_clamps_at_twenty() {
        let rows: Vec<PathRow> = (0..30)
            .map(|index| row(&format!("/d/f{index}.e{index}"), "file", Some(1), None))
            .collect();
        let stats = aggregate_rows(&rows, &DigestLimits::default());
        assert_eq!(stats.by_extension.len(), GROUP_LIMIT);
        // Settings can only lower the hard caps, never raise them.
        let stats = aggregate_rows(&rows, &DigestLimits { group_limit: 3, top_n: 10 });
        assert_eq!(stats.by_extension.len(), 3);
    }

    #[test]
    fn digest_topn_clamps_at_ten() {
        let rows: Vec<PathRow> = (0..25)
            .map(|index| row(&format!("/d/f{index}.txt"), "file", Some(index), None))
            .collect();
        let stats = aggregate_rows(&rows, &DigestLimits::default());
        assert_eq!(stats.largest.len(), TOP_N);
        assert_eq!(stats.largest[0]["path"], "/d/f24.txt");
        assert_eq!(stats.newest.len(), TOP_N);
    }

    #[test]
    fn scan_filter_predicates() {
        let mut filter = ScanFilter::default();
        filter.min_size = Some(60);
        assert!(
            !filter.matches(&row("/d/a.log", "file", Some(50), None)),
            "size predicate applies to files"
        );
        assert!(filter.matches(&row("/d/x/a.log", "file", Some(100), None)));
        assert!(
            filter.matches(&row("/d/x", "dir", None, None)),
            "dirs pass the size predicate for walking"
        );
        let mut filter = ScanFilter::default();
        filter.glob = Some("**/*.log".to_string());
        assert!(filter.matches(&row("/d/x/a.log", "file", Some(1), None)));
        assert!(!filter.matches(&row("/d/x/a.txt", "file", Some(1), None)));
        assert!(
            !filter.matches(&row("/d/x", "dir", None, None)),
            "the glob also gates directory rows"
        );
        let mut filter = ScanFilter::default();
        filter.modified_since = Some(1_500);
        filter.modified_until = Some(2_500);
        assert!(filter.matches(&row("/d/a", "file", Some(1), Some(2_000))));
        assert!(!filter.matches(&row("/d/b", "file", Some(1), Some(1_000))));
        assert!(!filter.matches(&row("/d/c", "file", Some(1), Some(3_000))));
        // Entries without mtime are kept (cannot prove recency).
        assert!(filter.matches(&row("/d/d", "file", Some(1), None)));
    }

    // -- cursor LRU/TTL -------------------------------------------------------

    #[test]
    fn cursor_pages_and_expires() {
        let mcp = mcp();
        // Insert directly through the internal table to control TTL.
        {
            let mut cursors = mcp.lock_cursors();
            cursors.put(
                "cur-1".to_string(),
                CursorSession {
                    rows: (0..25).map(|index| format!("/f/{index}.txt")).collect(),
                    offset: 0,
                    expires_at_millis: u128::from(unix_millis_now()) + 60_000,
                },
            );
        }

        let page = mcp.cursor_next(&json!({ "cursorId": "cur-1" })).unwrap();
        assert_eq!(page["rows"].as_array().unwrap().len(), 20);
        assert_eq!(page["offset"], 0);
        assert_eq!(page["nextOffset"], 20);
        assert_eq!(page["done"], false);
        assert_eq!(page["rows"][0]["path"], "/f/0.txt");

        // n is clamped at 20 and cannot overdraw.
        let page = mcp
            .cursor_next(&json!({ "cursorId": "cur-1", "n": 500 }))
            .unwrap();
        assert_eq!(page["rows"].as_array().unwrap().len(), 5);
        assert_eq!(page["offset"], 20);
        assert_eq!(page["nextOffset"], 25);
        assert_eq!(page["done"], true);
        // Exhausted cursor: a further page is empty with done:true.
        let page = mcp
            .cursor_next(&json!({ "cursorId": "cur-1" }))
            .unwrap();
        assert_eq!(page["rows"].as_array().unwrap().len(), 0);
        assert_eq!(page["done"], true);
        // An explicit offset re-reads from anywhere in the session.
        let page = mcp
            .cursor_next(&json!({ "cursorId": "cur-1", "offset": 2, "n": 3 }))
            .unwrap();
        assert_eq!(page["offset"], 2);
        assert_eq!(page["rows"][0]["path"], "/f/2.txt");
        assert!(mcp.cursor_next(&json!({})).is_err(), "missing cursorId");
    }

    #[test]
    fn cursor_ttl_expires_with_actionable_error() {
        let mcp = mcp();
        {
            let mut cursors = mcp.lock_cursors();
            cursors.put(
                "cur-old".to_string(),
                CursorSession {
                    rows: vec!["/a".to_string()],
                    offset: 0,
                    expires_at_millis: (unix_millis_now() as u128).saturating_sub(1),
                },
            );
        }
        let error = mcp
            .cursor_next(&json!({ "cursorId": "cur-old" }))
            .unwrap_err();
        assert!(error.contains("cursor expired"), "{error}");
        assert!(error.contains("files_scan_digest"), "{error}");
    }

    #[test]
    fn cursor_lru_evicts_oldest_session() {
        let mcp = mcp();
        {
            let mut cursors = mcp.lock_cursors();
            cursors.cap = 2;
            for index in 0..3 {
                cursors.put(
                    format!("cur-{index}"),
                    CursorSession {
                        rows: vec![format!("/{index}")],
                        offset: 0,
                        expires_at_millis: u128::from(unix_millis_now()) + 60_000,
                    },
                );
            }
        }
        let error = mcp.cursor_next(&json!({ "cursorId": "cur-0" })).unwrap_err();
        assert!(error.contains("unknown cursorId"), "{error}");
        assert!(mcp.cursor_next(&json!({ "cursorId": "cur-2" })).is_ok());
        // Touching cur-1 refreshes its recency so a new put evicts cur-2.
        {
            let mut cursors = mcp.lock_cursors();
            assert!(cursors.get("cur-1").is_some());
            cursors.put(
                "cur-3".to_string(),
                CursorSession {
                    rows: vec![],
                    offset: 0,
                    expires_at_millis: u128::from(unix_millis_now()) + 60_000,
                },
            );
        }
        assert!(mcp.cursor_next(&json!({ "cursorId": "cur-2" })).is_err());
        assert!(mcp.cursor_next(&json!({ "cursorId": "cur-1" })).is_ok());
    }

    // -- confirmToken hash / expiry / one-time --------------------------------

    #[test]
    fn params_hash_is_deterministic_and_field_sensitive() {
        let a = json!({ "connectionId": "c", "path": "/x" });
        let b = json!({ "path": "/x", "connectionId": "c" });
        assert_eq!(params_hash(&a), params_hash(&b), "key order irrelevant");
        let c = json!({ "connectionId": "c", "path": "/y" });
        assert_ne!(
            params_hash(&a),
            params_hash(&c),
            "parameter change flips the hash"
        );
    }

    #[test]
    fn arguments_without_token_strips_only_the_token() {
        let arguments = json!({ "connectionId": "c", "path": "/x", "confirmToken": "t" });
        let stripped = arguments_without_token(&arguments);
        assert!(stripped.get("confirmToken").is_none());
        assert_eq!(stripped["path"], "/x");
        assert_eq!(stripped["connectionId"], "c");
        // The preview hash binding covers the stripped form: hashing the
        // stripped arguments at preview time and verify time matches even
        // though the verify call carries the token.
        assert_eq!(params_hash(&stripped), params_hash(&arguments_without_token(&arguments)));
    }

    #[test]
    fn confirm_token_flow_hash_expiry_one_time() {
        let mcp = mcp();
        let stripped = json!({ "connectionId": "c", "path": "/tmp/x" });
        let (token, _expires) = mcp.confirm_begin(&stripped);
        // Same parameters verify (and consume).
        mcp.confirm_verify(&token, &stripped).unwrap();
        // One-time: a replay fails with the preview hint.
        let error = mcp.confirm_verify(&token, &stripped).unwrap_err();
        assert!(error.contains("unknown or already used"), "{error}");

        // Tampered parameters invalidate (token consumed by the failed check).
        let (token, _expires) = mcp.confirm_begin(&stripped);
        let tampered = json!({ "connectionId": "c", "path": "/etc" });
        let error = mcp.confirm_verify(&token, &tampered).unwrap_err();
        assert!(error.contains("arguments changed"), "{error}");
        // The invalidated token cannot be retried either.
        assert!(mcp.confirm_verify(&token, &stripped).is_err());

        // Expired token refuses (consumed too).
        let (token, _expires) = mcp.confirm_begin(&stripped);
        {
            let mut confirms = mcp
                .confirms
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            confirms.get_mut(&token).unwrap().expires_at_millis = 0;
        }
        let error = mcp.confirm_verify(&token, &stripped).unwrap_err();
        assert!(error.contains("expired"), "{error}");
    }

    // -- intent state table ----------------------------------------------------

    #[test]
    fn intent_report_updates_status_and_snapshot_lands() {
        let mcp = mcp();
        let now = unix_millis_now() as u128;
        mcp.register_intent("it-1", "search", json!({ "path": "/data" }), now);
        // Pending right after registration.
        let state = match mcp.intent_lookup("it-1", now) {
            IntentLookup::Found(value) => value,
            _ => panic!("expected found"),
        };
        assert_eq!(state["state"], "pending");
        // Frontend reports applied → applied + summary visible.
        mcp.report(&json!({
            "intentId": "it-1",
            "status": "applied",
            "summary": { "count": 7 },
        }))
        .unwrap();
        let state = match mcp.intent_lookup("it-1", now) {
            IntentLookup::Found(value) => value,
            _ => panic!("expected found"),
        };
        assert_eq!(state["state"], "applied");
        assert_eq!(state["summary"]["count"], 7);
        // Rejected reports carry the reason through.
        mcp.register_intent("it-2", "focus", json!({}), now);
        mcp.report(&json!({
            "intentId": "it-2",
            "status": "rejected",
            "reason": "panel unavailable",
        }))
        .unwrap();
        let state = match mcp.intent_lookup("it-2", now) {
            IntentLookup::Found(value) => value,
            _ => panic!("expected found"),
        };
        assert_eq!(state["state"], "rejected");
        assert_eq!(state["reason"], "panel unavailable");
        // Unknown/expired intentIds are an explicit report error (ldap parity).
        let error = mcp
            .report(&json!({ "intentId": "it-nope", "status": "applied" }))
            .unwrap_err();
        assert!(error.contains("unknown or expired"), "{error}");

        // Snapshot report (no intentId) stores the summary payload; the
        // status whitelist does not apply to snapshot reports.
        mcp.report(&json!({
            "status": "snapshot",
            "summary": { "panel": "browse", "selected": "/data/x" },
        }))
        .unwrap();
        let snapshot = mcp
            .snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .unwrap();
        assert_eq!(snapshot["panel"], "browse");
        assert_eq!(snapshot["selected"], "/data/x");
        // Snapshot reports without a summary land an empty object.
        mcp.report(&json!({})).unwrap();
        let snapshot = mcp
            .snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .unwrap();
        assert_eq!(snapshot, json!({}));
        // Intent reports with an invalid status are an explicit error.
        assert!(mcp
            .report(&json!({ "intentId": "it-1", "status": "weird" }))
            .is_err());
    }

    #[test]
    fn intent_table_ttl_and_lru() {
        let mcp = mcp();
        mcp.register_intent("it-a", "focus", json!({}), 1_000);
        mcp.register_intent("it-b", "focus", json!({}), 1_000);
        // 21st entry evicts the oldest (it-a) past the LRU cap of 20.
        for index in 0..19 {
            mcp.register_intent(&format!("it-f{index}"), "focus", json!({}), 2_000);
        }
        assert!(
            matches!(mcp.intent_lookup("it-a", 2_000), IntentLookup::Unknown),
            "LRU eviction"
        );
        assert!(
            matches!(mcp.intent_lookup("it-b", 2_000), IntentLookup::Found(_)),
            "recent entry stays"
        );
        // TTL: past 60s the entry reads as expired (and is pruned).
        assert!(
            matches!(
                mcp.intent_lookup("it-b", 1_000 + INTENT_TTL_MILLIS + 1),
                IntentLookup::Expired
            ),
            "60s TTL expiry"
        );
        // Expired entries do not come back and unknown ids stay unknown.
        assert!(matches!(mcp.intent_lookup("it-b", 2_100), IntentLookup::Unknown));
        assert!(matches!(mcp.intent_lookup("it-x", 2_100), IntentLookup::Unknown));
    }

    // -- 16 KiB response cap ----------------------------------------------------

    #[test]
    fn cap_response_truncates_cells_but_never_anchors() {
        let long_cell = "x".repeat(500);
        let long_path = format!("/{}", "y".repeat(500));
        let mut payload = json!({
            "note": long_cell,
            "sample": [{ "path": long_path, "size": 1 }],
        });
        assert!(cap_response(&mut payload, 16 * 1024, 120));
        assert_eq!(payload["note"].as_str().unwrap().chars().count(), 120);
        assert_eq!(
            payload["sample"][0]["path"].as_str().unwrap().chars().count(),
            501,
            "anchor fields keep full width"
        );
        assert_eq!(payload["truncated"], true);
    }

    #[test]
    fn cap_response_trims_arrays_until_fits() {
        let mut payload = json!({
            "rows": (0..2_000)
                .map(|index| json!({ "path": format!("/f/{index}.txt") }))
                .collect::<Vec<_>>(),
        });
        assert!(cap_response(&mut payload, 2 * 1024, 120));
        let raw = serde_json::to_string(&payload).unwrap();
        assert!(
            raw.len() <= 2 * 1024 + 64,
            "payload trimmed under the cap: {}",
            raw.len()
        );
        assert_eq!(payload["truncated"], true);
        // A payload within the cap is untouched.
        let mut small = json!({ "ok": true });
        assert!(!cap_response(&mut small, 16 * 1024, 120));
        assert!(small.get("truncated").is_none());
    }

    #[test]
    fn cap_response_hard_truncates_when_no_arrays_left() {
        // One enormous locator string + no arrays: the fallback pass must keep
        // the response under the cap even at the cost of the anchor.
        let mut payload = json!({ "path": format!("/{}", "z".repeat(50_000)) });
        assert!(cap_response(&mut payload, 1024, 120));
        let raw = serde_json::to_string(&payload).unwrap();
        assert!(raw.len() <= 2048, "hard fallback: {}", raw.len());
        assert_eq!(payload["truncated"], true);
    }

    // -- tool list filtering (design §4) -----------------------------------------

    #[test]
    fn tool_definitions_exclude_write_tools_for_read_only_connections() {
        let mcp = mcp();
        let all = mcp.definitions_for(false)["tools"]
            .as_array()
            .unwrap()
            .clone();
        let names: Vec<&str> = all
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        for write in WRITE_TOOLS {
            assert!(names.contains(&write), "{write} listed for writable connections");
        }
        for ui in UI_TOOLS {
            assert!(names.contains(&ui), "{ui} always listed");
        }
        assert!(names.contains(&"files_scan_digest"));
        assert!(names.contains(&"files_cursor_next"));
        assert!(names.contains(&"files_ui_quick_paths"));

        let readonly = mcp.definitions_for(true);
        let names: Vec<&str> = readonly["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        for write in WRITE_TOOLS {
            assert!(
                !names.contains(&write),
                "{write} must NOT be listed for read-only connections"
            );
        }
        assert!(names.contains(&"files_scan_digest"), "reads stay available");
        assert_eq!(
            names.len(),
            all.len() - WRITE_TOOLS.len(),
            "exactly the write tools are dropped"
        );
        // Each omitted write tool carries its reason (ldap Tools shape parity).
        let omitted = readonly["omittedWriteTools"].as_array().unwrap();
        assert_eq!(omitted.len(), WRITE_TOOLS.len());
        for entry in omitted {
            assert!(entry["reason"].as_str().unwrap().contains("read-only"));
        }
        // Writable listings carry no omittedWriteTools key.
        assert!(mcp.definitions_for(false).get("omittedWriteTools").is_none());
    }

    // -- gates (mirror of the main.rs semantics) ---------------------------------

    #[test]
    fn write_gates_mirror_main_semantics() {
        let connection = |read_only: bool, allow_delete: bool| {
            StoredConnection::from_lifecycle_params(&json!({
                "connection": {
                    "id": "c",
                    "external_config": {
                        "protocol": "fs",
                        "read_only": read_only,
                        "allow_delete": allow_delete,
                    },
                }
            }))
            .unwrap()
        };
        assert!(ensure_writable(&connection(false, true)).is_ok());
        assert!(ensure_writable(&connection(true, true)).is_err());
        assert!(ensure_deletable(&connection(false, false)).is_err());
        assert!(ensure_deletable(&connection(true, true)).is_err());
        // Root purge red line.
        assert!(refuse_root_purge(&connection(false, true), "/").is_err());
        assert!(refuse_root_purge(&connection(false, true), "  ").is_err());
        assert!(refuse_root_purge(&connection(false, true), "/data/x").is_ok());
    }
}
