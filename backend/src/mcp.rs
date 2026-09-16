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
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
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

    /// Applies a (possibly retuned) capacity immediately: the next `put`
    /// evicts past the new cap, and a shrink evicts the overflow right here so
    /// `mcp/settings/set maxCursorSessions` takes effect on the live table
    /// (churn requirement: settings 调整即时生效).
    fn set_cap(&mut self, cap: usize) {
        self.cap = cap;
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
    ///
    /// Concurrency note (第七轮并发审查): the settings read and the table lock
    /// are two separate critical sections, so a `settings_set` racing this
    /// call may observe a stale `max_cursor_sessions` for exactly one
    /// materialization — the LRU cap then self-heals, because every `put`
    /// re-applies the (freshly read) cap. The contract is "the next
    /// materialization after a retune converges" (pinned by
    /// `cursor_sessions_churn_with_lru_and_retuned_settings` and the
    /// concurrent churn test), never "the retune is atomic with in-flight
    /// puts". The same stale-snapshot tolerance applies to `cursor_ttl_secs`
    /// and `max_cursor_rows`: values are read once per materialization.
    fn cursor_put(&self, rows: Vec<String>) -> (String, bool) {
        let settings = self.current_settings();
        let truncated = rows.len() > settings.max_cursor_rows;
        let mut rows = rows;
        rows.truncate(settings.max_cursor_rows);
        let cursor_id = format!("cur-{}", uuid::Uuid::new_v4().simple());
        let now = unix_millis_now() as u128;
        let mut cursors = self.lock_cursors();
        cursors.retain(|session| session.expires_at_millis > now);
        // The retuned `maxCursorSessions` (settings) governs the LRU cap on
        // every materialization — never the value frozen at Mcp::new.
        cursors.set_cap(settings.max_cursor_sessions);
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
    ///
    /// Concurrency note (第七轮并发审查): the whole lookup → expiry check →
    /// offset read/advance sequence runs inside ONE critical section, so two
    /// concurrent pages of the same cursor can never observe (or return) the
    /// same offset window — there is no check-then-act gap across locks.
    fn cursor_next(&self, arguments: &Value) -> Result<Value, String> {
        missing_required(arguments, &["cursorId"])?;
        let cursor_id = required_str(arguments, "cursorId")?;
        let n = numeric_arg_or(arguments, "n", CURSOR_PAGE)?.clamp(1, MAX_CURSOR_PAGE);
        // offset absent = continue from the in-session cursor; an explicit
        // value (number or numeric string) re-reads from anywhere. A negative
        // is a clear error, never a silent restart.
        let offset = numeric_arg_u64(arguments, "offset")?.map(|value| value as usize);
        let now = unix_millis_now() as u128;
        let mut cursors = self.lock_cursors();
        let Some(session) = cursors.get(cursor_id) else {
            return Err(format!(
                "unknown cursorId: {cursor_id} (cursor sessions are per-process and may have \
                 expired or been evicted); re-run files_scan_digest to get a fresh cursorId"
            ));
        };
        if session.expires_at_millis <= now {
            let ttl_secs = self.current_settings().cursor_ttl_secs;
            cursors.remove(cursor_id);
            return Err(format!(
                "cursor expired (TTL {ttl_secs}s); re-run files_scan_digest to get a fresh \
                 cursorId"
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

    /// Confirm-token table ceiling. The TTL bounds each token's lifetime but
    /// nothing else did: a caller issuing previews without ever confirming
    /// would grow the table without bound for the whole process. 1024 far
    /// exceeds any honest session (previews are per-delete/purge calls); past
    /// it the earliest-expiring token is evicted (the newest tokens — the ones
    /// a caller is about to confirm — always survive).
    const MAX_CONFIRM_ENTRIES: usize = 1024;

    /// Starts the two-phase flow: stores a fresh one-time token bound to the
    /// parameter hash and returns `(token, expiresAtMillis)`.
    ///
    /// Concurrency note (第七轮并发审查): prune → hard-cap eviction → insert
    /// all run inside one critical section, so concurrent previews can never
    /// overshoot `MAX_CONFIRM_ENTRIES` (no check-then-act gap). The cap
    /// eviction picks the earliest-expiring entry; tokens issued within the
    /// same millisecond share an `expires_at_millis`, so under that pressure
    /// the eviction is a documented arbitrary pick — the victim must simply
    /// re-preview ("unknown or already used"), there is no double-write.
    fn confirm_begin(&self, arguments: &Value) -> (String, u128) {
        let settings = self.current_settings();
        let token = format!("c-{}", uuid::Uuid::new_v4().simple());
        let now = unix_millis_now() as u128;
        let mut confirms = self
            .confirms
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // TTL churn: expired tokens never linger past the next preview (a
        // 500+ preview/consume/expire loop keeps the table at the live-set
        // size instead of growing monotonically).
        confirms.retain(|_, entry| entry.expires_at_millis > now);
        if confirms.len() >= Self::MAX_CONFIRM_ENTRIES {
            // Drop the entry that expires first: closest to natural expiry,
            // least likely to be the token the caller is holding right now.
            if let Some(oldest) = confirms
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at_millis)
                .map(|(key, _)| key.clone())
            {
                confirms.remove(&oldest);
            }
        }
        confirms.insert(
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
    ///
    /// Concurrency note (第七轮并发审查): the removal happens inside the lock
    /// and the hash/TTL checks run on the already-removed entry, so two
    /// threads racing `confirm_verify` on the same token can produce exactly
    /// one `Ok` — the loser sees "unknown or already used". Consumption is
    /// the atomic step; there is no verify-then-remove window.
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
            let ttl_secs = self.current_settings().confirm_ttl_secs;
            return Err(format!(
                "confirmToken expired (TTL {ttl_secs}s); request a new preview"
            ));
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
        Err(
            "Connection disallows delete operations (allow_delete=false); enable allowDelete \
             on the connection (or use a connection that allows it) to run delete/purge"
                .to_string(),
        )
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
const ALL_TOOL_NAMES: [&str; 12] = [
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
//   sourcing differs: there is no host lifecycle, so OpenDAL connection
//   parameters travel inline with the call and are pooled in the engine
//   under their parameter-hash id;
// - UI-intent tools answer a hard UNAVAILABLE (degradation matrix stdio row)
//   instead of burning the 5s report wait on a frontend that cannot exist.

/// MCP protocol revision the stdio server speaks (ssh `PROTOCOL_VERSION`).
const PROTOCOL_VERSION: &str = "2024-11-05";

/// UI-intent tool family in stdio mode: UNAVAILABLE by design (no workbench
/// to drive; `files_ui_quick_paths` is pure meta-discovery and stays usable).
const STDIO_UI_TOOLS: [&str; 4] = [
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

/// Tools whose execution resolves a storage connection. `files_cursor_next`
/// is a pure session lookup and runs without one.
fn needs_connection_id(tool: &str) -> bool {
    !matches!(tool, "files_cursor_next")
}

/// Pool id for an inline connection payload: `mcp-inline-` + 16 hex chars of
/// the parameter hash ([`params_hash`] is canonical — serde_json object keys
/// are sorted, so key order does not matter). Pure and unit-tested.
fn inline_pool_id(connection: &Value) -> String {
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
fn stored_connection_from_inline(connection: &Value) -> Result<StoredConnection, String> {
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
             sftp, smb, sftp-native, opendal-custom, aliyun-drive, dropbox, gdrive, koofr, onedrive, pcloud, seafile, yandex-disk)"
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
fn inline_connection_properties() -> Value {
    json!({
    "protocol": { "type": "string",
        "description": "Storage protocol (required): local (alias of fs), fs, s3, gcs, azblob, obs, oss, cos, webdav, ftp, sftp, smb, sftp-native, opendal-custom, aliyun-drive, dropbox, gdrive, koofr, onedrive, pcloud, seafile, yandex-disk" },
    "root": { "type": "string", "description": "OpenDAL root prefix" },
    "bucket": { "type": "string", "description": "Bucket (s3/gcs/obs/oss/cos)" },
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
    "share": { "type": "string", "description": "Share (smb)" },
    "domain": { "type": "string", "description": "Domain (smb)" },
    "service": { "type": "string", "description": "OpenDAL service name (opendal-custom)" },
    "config": { "type": "object", "description": "OpenDAL service config map (opendal-custom)" },
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
fn parse_request_line(line: &str) -> Result<Option<Value>, Value> {
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
const DEFAULT_STDIO_MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

fn stdio_max_line_bytes() -> usize {
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
    let mcp = Arc::new(Mcp::new(data_dir.clone()));
    let engine = Arc::new(Engine::new());
    let transfers = Arc::new(JobTable::new());
    let store = Arc::new(Store::new(data_dir));
    // Parity with the framed path: hydrate the persisted transfer history.
    {
        let transfers = Arc::clone(&transfers);
        let store = Arc::clone(&store);
        runtime.block_on(async move { transfers.load_history(&store).await });
    }
    let server = Arc::new(StdioServer {
        mcp,
        engine,
        transfers,
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
mod appbridge {
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
        /// (cargo runs tests on parallel threads; the lock is shared with the
        /// stdio bridge tests via the mcp module).
        fn env_guard() -> std::sync::MutexGuard<'static, ()> {
            super::super::BRIDGE_ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
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
}

/// Standalone stdio server state: the same tool surface as the DBX bridge
/// (`mcp/call` → [`Mcp::run_tool`]) with no host lifecycle — connections come
/// from inline call parameters pooled in the engine by parameter hash, and a
/// saved-connection `connectionId` forwards to the running DBX app through
/// the local TCP bridge ([`appbridge`]).
struct StdioServer {
    mcp: Arc<Mcp>,
    engine: Arc<Engine>,
    transfers: Arc<JobTable>,
    store: Arc<Store>,
    /// L1 stdio bridge fallback switch: forwards unpooled-`connectionId`
    /// calls to the running DBX app through the local TCP bridge. Always on
    /// in production; tests flip it off to keep decision paths hermetic (no
    /// real app bridge on the box).
    bridge_fallback: bool,
    /// Wake budget the forward path grants `appbridge::ensure` after a launch
    /// attempt (default 30s; the bridge fail-closed test shortens it).
    bridge_ensure_wait: Duration,
}

impl StdioServer {
    async fn dispatch(&self, request: Value) -> Option<Value> {
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
        let arguments = self.prepare_arguments(name, arguments)?;
        // Same dispatch as the DBX bridge (`emitter: None` — no workbench
        // event channel); the 16 KiB cap + envelope post-processing is shared.
        let mut payload = self
            .mcp
            .run_tool(
                name,
                &arguments,
                &self.engine,
                &self.transfers,
                &self.store,
                None,
            )
            .await?;
        Ok(self.mcp.finalize_payload(&mut payload))
    }

    /// L1 stdio bridge fallback decision: `Some((connection_id, arguments))`
    /// when the call must be forwarded through the DBX app bridge, `None`
    /// when the local path owns the call (session/UI tools, an inline
    /// `connection` payload, the built-in `__local__` filesystem, or an id
    /// already pooled in this engine). Pure and unit-tested.
    fn bridge_forward_plan(&self, tool: &str, arguments: &Value) -> Option<(String, Value)> {
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
        // `engine.connection` answers Ok for the built-in `__local__`
        // filesystem and every pooled inline id — both stay local. Only a
        // genuinely unknown saved-connection id forwards.
        if self.engine.connection(id).is_ok() {
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
    /// (camelCase → lifecycle shape, backend keys validated by the Operator
    /// build) and pooled in the engine under its parameter-hash id; a bare
    /// `connectionId` must already resolve in-process (`__local__` or a pooled
    /// inline id — the DBX app bridge forward owns unpooled ids upstream in
    /// [`StdioServer::call_tool`], so this branch only fires with the fallback
    /// switched off). Calls without any connection reference only pass for the
    /// connection-free tools (`files_cursor_next`).
    fn prepare_arguments(&self, tool: &str, arguments: &Value) -> Result<Value, String> {
        if !arguments.is_object() {
            // Same semantics as the DBX bridge's mcp/call guard: a non-object
            // arguments payload is a caller bug, reported before anything else
            // (the stdio dispatch maps it to JSON-RPC -32602).
            return Err("arguments must be a JSON object".to_string());
        }
        match arguments.get("connection") {
            Some(connection) => {
                let connection = stored_connection_from_inline(connection)?;
                self.engine.connect(connection.clone())?;
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
                        if id != crate::engine::LOCAL_CONNECTION_ID
                            && self.engine.connection(id).is_err() =>
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

// ---------------------------------------------------------------------------
// Tests (pure logic; no remote I/O)
// ---------------------------------------------------------------------------

/// Shared serialization for tests that mutate the process-global app-data env
/// vars (`DBX_APP_DATA_DIR` / `DBX_APP_LAUNCH_CMD`): cargo runs tests on
/// parallel threads in one process.
#[cfg(test)]
static BRIDGE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    /// The expiry messages surface the *tuned* TTL, never a hardcoded value:
    /// `cursorTtlSecs`/`confirmTtlSecs` are operator-tunable via
    /// `mcp/settings/set`, so the guidance an LLM reads must stay truthful.
    #[test]
    fn expiry_errors_report_the_tuned_ttl() {
        let mcp = mcp();
        mcp.settings_set(&json!({ "cursorTtlSecs": 45, "confirmTtlSecs": 120 }))
            .unwrap();
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
        assert!(error.contains("TTL 45s"), "{error}");
        let stripped = json!({ "connectionId": "c", "path": "/tmp/x" });
        let (token, _expires) = mcp.confirm_begin(&stripped);
        {
            let mut confirms = mcp
                .confirms
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            confirms.get_mut(&token).unwrap().expires_at_millis = 0;
        }
        let error = mcp.confirm_verify(&token, &stripped).unwrap_err();
        assert!(error.contains("TTL 120s"), "{error}");
    }

    /// An unknown cursor is distinct from an expired one and both guide the
    /// caller back to a fresh digest (design §3.4).
    #[test]
    fn cursor_unknown_id_guides_back_to_a_fresh_digest() {
        let mcp = mcp();
        let error = mcp
            .cursor_next(&json!({ "cursorId": "cur-nope" }))
            .unwrap_err();
        assert!(error.contains("unknown cursorId"), "{error}");
        assert!(error.contains("files_scan_digest"), "{error}");
    }

    /// LLM numeric-argument tolerance: numbers pass, numeric strings and
    /// integral floats parse, everything else is a clear error naming the key.
    #[test]
    fn numeric_arguments_accept_string_and_float_forms() {
        assert_eq!(numeric_arg_u64(&json!({ "n": "20" }), "n").unwrap(), Some(20));
        assert_eq!(
            numeric_arg_u64(&json!({ "n": " 7 " }), "n").unwrap(),
            Some(7)
        );
        assert_eq!(numeric_arg_u64(&json!({ "n": 8.0 }), "n").unwrap(), Some(8));
        assert_eq!(numeric_arg_u64(&json!({}), "n").unwrap(), None);
        assert_eq!(numeric_arg_u64(&json!({ "n": null }), "n").unwrap(), None);
        assert!(numeric_arg_u64(&json!({ "n": "fast" }), "n").is_err());
        assert!(numeric_arg_u64(&json!({ "n": -3 }), "n").is_err());
        assert!(numeric_arg_u64(&json!({ "n": 1.5 }), "n").is_err());
        assert!(numeric_arg_u64(&json!({ "n": true }), "n").is_err());
        let error = numeric_arg_u64(&json!({ "depth": "deep" }), "depth").unwrap_err();
        assert!(error.contains("depth"), "{error}");
        // Digest filter params share the same tolerance (glob strings aside).
        let filter = ScanFilter::from_arguments(&json!({
            "minSizeBytes": "1024",
            "modifiedSince": "1700000000000",
        }))
        .unwrap();
        assert_eq!(filter.min_size, Some(1024));
        assert_eq!(filter.modified_since, Some(1_700_000_000_000));
    }

    /// `files_cursor_next` honors numeric strings for `n`/`offset` the same as
    /// numbers (LLM callers quote integers constantly).
    #[test]
    fn cursor_next_accepts_numeric_strings() {
        let mcp = mcp();
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
        let page = mcp
            .cursor_next(&json!({ "cursorId": "cur-1", "n": "2" }))
            .unwrap();
        assert_eq!(page["rows"].as_array().unwrap().len(), 2);
        let page = mcp
            .cursor_next(&json!({ "cursorId": "cur-1", "n": "3", "offset": "20" }))
            .unwrap();
        assert_eq!(page["offset"], 20);
        assert_eq!(page["rows"][0]["path"], "/f/20.txt");
    }

    /// `required_str` distinguishes a missing key from a present-but-wrong
    /// value — "Missing required parameter" for a number-typed path misleads
    /// an LLM into re-sending the same broken payload.
    #[test]
    fn required_str_error_messages_are_actionable() {
        let arguments = json!({ "path": 123, "empty": "", "glob": "x" });
        let error = required_str(&arguments, "missing").unwrap_err();
        assert!(error.contains("Missing required parameter"), "{error}");
        let error = required_str(&arguments, "path").unwrap_err();
        assert!(error.contains("non-empty string"), "{error}");
        let error = required_str(&arguments, "empty").unwrap_err();
        assert!(error.contains("non-empty string"), "{error}");
        assert_eq!(required_str(&arguments, "glob").unwrap(), "x");
    }

    /// Delete/purge target spelling: directory markers keep exactly one
    /// trailing `/` (prefix-based backends resolve the marker only through
    /// it), file paths never carry one — an LLM-echoed `a.txt/` must delete
    /// `a.txt` instead of silently no-op'ing against a missing marker.
    #[test]
    fn canonical_delete_target_spells_directories_and_files() {
        assert_eq!(
            canonical_delete_target("/data/a.txt/", Some("file")),
            "/data/a.txt"
        );
        assert_eq!(
            canonical_delete_target("/data/sub", Some("dir")),
            "/data/sub/"
        );
        assert_eq!(
            canonical_delete_target("/data/sub/", Some("dir")),
            "/data/sub/"
        );
        assert_eq!(canonical_delete_target("/data/ghost/", None), "/data/ghost");
        assert_eq!(canonical_delete_target("/", Some("dir")), "/");
    }

    /// File write/rename targets reject the root and drop trailing slashes.
    #[test]
    fn file_target_path_rejects_root_and_trims_trailing_slashes() {
        assert_eq!(file_target_path("/data/a.txt/", "path").unwrap(), "/data/a.txt");
        assert_eq!(file_target_path("/data/a.txt", "path").unwrap(), "/data/a.txt");
        assert!(file_target_path("/", "path").is_err());
        assert!(file_target_path("  /  ", "path").is_err());
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

    #[test]
    fn stdio_tool_list_declares_inline_connection_and_relaxes_required() {
        let mcp = mcp();
        let tools = mcp.stdio_tool_list()["tools"].as_array().unwrap().clone();
        let by_name = |name: &str| {
            tools
                .iter()
                .find(|tool| tool["name"].as_str() == Some(name))
                .unwrap_or_else(|| panic!("{name} missing from stdio list"))
                .clone()
        };
        // Connection-bound tools declare the inline `connection` object
        // (strict MCP hosts drop undeclared params) and relax connectionId.
        for name in ["files_scan_digest", "files_write", "files_delete", "files_ui_quick_paths"] {
            let tool = by_name(name);
            let schema = tool["inputSchema"].as_object().unwrap();
            let properties = schema["properties"].as_object().unwrap();
            assert!(properties.contains_key("connection"), "{name}: inline connection undeclared");
            assert!(
                properties["connection"]["properties"]["protocol"].is_object(),
                "{name}: inline protocol field missing"
            );
            assert!(
                !properties["connectionId"]["description"]
                    .as_str()
                    .unwrap()
                    .is_empty(),
                "{name}: connectionId description rewritten for stdio"
            );
            assert_eq!(
                schema["anyOf"],
                json!([{ "required": ["connectionId"] }, { "required": ["connection"] }]),
                "{name}: required connectionId must relax to anyOf"
            );
            let required: Vec<&str> = schema["required"].as_array().unwrap().iter()
                .map(Value::as_str).map(Option::unwrap).collect();
            assert!(!required.contains(&"connectionId"), "{name}: connectionId still hard-required");
        }
        // files_write keeps its other required fields.
        assert!(by_name("files_write")["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry.as_str() == Some("path")));
        // UI tools and the connection-free cursor stay on the plain schema.
        for name in STDIO_UI_TOOLS {
            let tool = by_name(name);
            let schema = tool["inputSchema"].as_object().unwrap();
            assert!(schema.get("anyOf").is_none(), "{name}: UI schema must stay untouched");
            assert!(
                !schema["properties"].as_object().unwrap().contains_key("connection"),
                "{name}: UI schema must stay untouched"
            );
        }
        let cursor_tool = by_name("files_cursor_next");
        let cursor = cursor_tool["inputSchema"].as_object().unwrap();
        assert!(cursor.get("anyOf").is_none(), "cursor schema must stay untouched");
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

    // -- standalone stdio server (`--mcp`) ---------------------------------------

    fn stdio_server() -> (StdioServer, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let server = StdioServer {
            mcp: Arc::new(Mcp::new(data_dir.clone())),
            engine: Arc::new(Engine::new()),
            transfers: Arc::new(JobTable::new()),
            store: Arc::new(Store::new(data_dir)),
            // Hermetic: no real DBX app bridge on the test box — the bridge
            // fallback decision/forward paths are covered by the dedicated
            // mock-bridge tests below.
            bridge_fallback: false,
            bridge_ensure_wait: Duration::from_millis(300),
        };
        (server, dir)
    }

    fn stdio_dispatch(server: &StdioServer, method: &str, params: Value) -> Option<Value> {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(server.dispatch(json!({
                "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
            })))
    }

    fn stdio_result(server: &StdioServer, method: &str, params: Value) -> Value {
        let response = stdio_dispatch(server, method, params).expect("request must be answered");
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
    }

    fn stdio_error(server: &StdioServer, method: &str, params: Value) -> String {
        // Tool-level errors are MCP isError results (ldap/kafka stdio
        // parity); structural mistakes (-32602 tier) stay JSON-RPC errors.
        // Both shapes carry one actionable text — extract either.
        let response = stdio_dispatch(server, method, params).expect("request must be answered");
        if let Some(message) = response["error"]["message"].as_str() {
            return message.to_string();
        }
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// Unwraps the MCP content envelope into the tool payload (the shared
    /// `finalize_payload` shape: `{content:[{type:text}], isError:false}`).
    fn unwrap_envelope(result: &Value) -> Value {
        let content = result
            .get("content")
            .and_then(Value::as_array)
            .expect("content envelope");
        assert_eq!(result["isError"], false, "{result}");
        serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn stdio_line_parsing_skips_blank_and_reports_parse_errors() {
        assert_eq!(parse_request_line("").unwrap(), None);
        assert_eq!(parse_request_line("   ").unwrap(), None);
        let parsed = parse_request_line(r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(parsed["id"], 7);
        assert_eq!(parsed["method"], "ping");
        let error = parse_request_line("not json").unwrap_err();
        assert_eq!(error["error"]["code"], -32700);
        assert!(error["error"]["message"].as_str().unwrap().contains("Parse error"));
        assert_eq!(error["id"], Value::Null);
    }

    #[test]
    fn stdio_protocol_handshake_and_shapes() {
        let (server, _dir) = stdio_server();
        // initialize: MCP 2024-11-05, serverInfo name = io.dbx.files.
        let result = stdio_result(&server, "initialize", json!({}));
        assert_eq!(result["protocolVersion"], "2024-11-05", "{result}");
        assert_eq!(result["serverInfo"]["name"], "io.dbx.files");
        assert!(result["serverInfo"]["version"].is_string());
        assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(stdio_result(&server, "ping", json!({})), json!({}));
        // Notifications are never answered (notifications/initialized included).
        let answer = tokio::runtime::Runtime::new().unwrap().block_on(
            server.dispatch(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })),
        );
        assert!(answer.is_none(), "{answer:?}");
        // tools/list exposes the full tool surface with schemas.
        let tools = stdio_result(&server, "tools/list", json!({}))["tools"].clone();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        for expected in ["files_ui_focus", "files_scan_digest", "files_cursor_next", "files_delete"] {
            assert!(names.contains(&expected), "{expected} missing from {names:?}");
        }
        // Unknown method → standard JSON-RPC -32601 (Method not found),
        // family-wide with ssh/ldap/kafka; the smoke SKIP gate keys on the
        // message text ("Method not found"), S1 pins the numeric code.
        let response = stdio_dispatch(&server, "mcp/nonexistent", json!({})).unwrap();
        assert_eq!(response["error"]["code"], -32601);
        assert!(response["error"]["message"].as_str().unwrap().contains("Method not found"));
        // Structural tools/call problems are Invalid params (-32602): missing
        // tool name and a non-object arguments payload.
        let response = stdio_dispatch(&server, "tools/call", json!({})).unwrap();
        assert_eq!(response["error"]["code"], -32602, "{response}");
        assert!(response["error"]["message"].as_str().unwrap().contains("tool name"));
        let response = stdio_dispatch(
            &server,
            "tools/call",
            json!({ "name": "files_scan_digest", "arguments": "not-an-object" }),
        )
        .unwrap();
        assert_eq!(response["error"]["code"], -32602, "{response}");
        assert!(response["error"]["message"].as_str().unwrap().contains("arguments must be a JSON object"));
        // Tool-level errors are MCP isError results (ldap/kafka stdio
        // parity), not protocol-level JSON-RPC errors.
        let response = stdio_dispatch(
            &server,
            "tools/call",
            json!({ "name": "files_nonexistent", "arguments": {} }),
        )
        .unwrap();
        assert_eq!(response["result"]["isError"], true, "{response}");
        assert!(response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Unknown tool"));
        assert!(response.get("error").is_none(), "{response}");
        // A request without an id is invalid JSON-RPC → -32600 with null id.
        let answer = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(server.dispatch(json!({ "jsonrpc": "2.0", "method": "ping" })))
            .unwrap();
        assert_eq!(answer["id"], Value::Null);
        assert_eq!(answer["error"]["code"], -32600);
        // tools/call without a tool name.
        let error = stdio_error(&server, "tools/call", json!({}));
        assert!(error.contains("tool name"), "{error}");
    }

    #[test]
    fn stdio_ui_tools_answer_unavailable() {
        let (server, _dir) = stdio_server();
        for (tool, arguments) in [
            ("files_ui_focus", json!({ "panel": "browse" })),
            ("files_ui_search", json!({ "path": "/data" })),
            ("files_ui_select", json!({ "path": "/data/a.txt" })),
            ("files_ui_state", json!({})),
        ] {
            let error = stdio_error(
                &server,
                "tools/call",
                json!({ "name": tool, "arguments": arguments }),
            );
            assert!(error.contains("UNAVAILABLE"), "{tool}: {error}");
            assert!(error.contains(tool), "{tool}: {error}");
            assert!(error.contains("files_scan_digest"), "{tool}: {error}");
        }
        // Non-UI tools are untouched by the gate: a digest call surfaces its
        // real (argument) error instead of UNAVAILABLE.
        let error = stdio_error(
            &server,
            "tools/call",
            json!({ "name": "files_scan_digest", "arguments": { "path": "/" } }),
        );
        assert!(!error.contains("UNAVAILABLE"), "{error}");
    }

    // -- stdio bridge fallback (L1; ssh bridge_forward_plan 同构 + ldap M14) --

    /// stdio server with the bridge fallback ON (mock-bridge tests; each test
    /// controls `DBX_APP_DATA_DIR` / `DBX_APP_LAUNCH_CMD` under the shared
    /// env lock).
    fn stdio_bridge_server() -> (StdioServer, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let server = StdioServer {
            mcp: Arc::new(Mcp::new(data_dir.clone())),
            engine: Arc::new(Engine::new()),
            transfers: Arc::new(JobTable::new()),
            store: Arc::new(Store::new(data_dir)),
            bridge_fallback: true,
            bridge_ensure_wait: Duration::from_millis(500),
        };
        (server, dir)
    }

    /// L1 forward decision: pooled inline ids and `__local__` stay local,
    /// session tools and inline-`connection` calls never forward, and only a
    /// genuinely unpooled saved-connection id forwards with its arguments.
    #[test]
    fn bridge_forward_plan_forwards_only_unpooled_connection_ids() {
        let (server, _dir) = stdio_bridge_server();
        let pooled = stored_connection_from_inline(&json!({
            "protocol": "local", "root": "/tmp/plan-x",
        }))
        .unwrap();
        server.engine.connect(pooled.clone()).unwrap();

        let plan = server.bridge_forward_plan(
            "files_scan_digest",
            &json!({ "connectionId": pooled.id, "path": "/" }),
        );
        assert!(plan.is_none(), "pooled inline id stays local");

        let plan = server.bridge_forward_plan(
            "files_scan_digest",
            &json!({ "connectionId": "__local__", "path": "/" }),
        );
        assert!(plan.is_none(), "__local__ stays local");

        let plan = server.bridge_forward_plan("files_cursor_next", &json!({ "cursorId": "cur-x" }));
        assert!(plan.is_none(), "session tools stay local");

        let plan = server.bridge_forward_plan(
            "files_scan_digest",
            &json!({
                "connection": { "protocol": "local", "root": "/tmp/plan-y" },
                "connectionId": "saved-9", "path": "/",
            }),
        );
        assert!(plan.is_none(), "inline connection owns the call");

        let (id, forwarded) = server
            .bridge_forward_plan(
                "files_scan_digest",
                &json!({ "connectionId": "saved-jane", "path": "/" }),
            )
            .expect("unpooled saved id must forward");
        assert_eq!(id, "saved-jane");
        assert_eq!(forwarded["path"], "/", "arguments forward untouched");

        let plan = server.bridge_forward_plan("files_scan_digest", &json!({ "path": "/" }));
        assert!(plan.is_none(), "no connectionId: local guidance path");
    }

    /// Mock-bridge happy path: an unpooled connectionId forwards the
    /// snake_case five-field contract to the mock app, the app-side MCP
    /// envelope passes through as the stdio result, and the local pool stays
    /// unpolluted.
    #[test]
    fn stdio_bridge_forward_passes_the_app_envelope_through() {
        let _guard = BRIDGE_ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let envelope = r#"{"content":[{"type":"text","text":"{\"matched\":7,\"connectionId\":\"saved-jane\"}"}],"isError":false}"#;
        let (port, calls) = appbridge::spawn_mock_bridge(200, envelope);
        let app_data = tempfile::tempdir().unwrap();
        std::fs::write(app_data.path().join("mcp-bridge-port"), port.to_string()).unwrap();
        std::env::set_var("DBX_APP_DATA_DIR", app_data.path());
        std::env::set_var("DBX_APP_LAUNCH_CMD", ":");

        let (server, _dir) = stdio_bridge_server();
        let response = stdio_dispatch(
            &server,
            "tools/call",
            json!({
                "name": "files_scan_digest",
                "arguments": { "connectionId": "saved-jane", "path": "/data", "glob": "*" },
            }),
        )
        .unwrap();

        std::env::remove_var("DBX_APP_DATA_DIR");
        std::env::remove_var("DBX_APP_LAUNCH_CMD");

        assert!(response.get("error").is_none(), "{response}");
        let payload = unwrap_envelope(&response["result"]);
        assert_eq!(payload["matched"], 7, "{payload}");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "bridge called exactly once: {calls:?}");
        assert_eq!(calls[0]["plugin_id"], "io.dbx.files", "{calls:?}");
        assert_eq!(calls[0]["connection_id"], "saved-jane", "{calls:?}");
        assert_eq!(calls[0]["tool"], "files_scan_digest", "{calls:?}");
        assert_eq!(calls[0]["arguments"]["path"], "/data", "{calls:?}");
        // 转发不污染本地池。
        assert!(server.engine.connection("saved-jane").is_err());
    }

    /// A non-envelope app answer (defensive shape) wraps as a success
    /// payload instead of being buried one JSON level deeper.
    #[test]
    fn stdio_bridge_forward_wraps_a_non_envelope_payload_as_success() {
        let _guard = BRIDGE_ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (port, _calls) = appbridge::spawn_mock_bridge(200, r#"{"unexpected":"shape"}"#);
        let app_data = tempfile::tempdir().unwrap();
        std::fs::write(app_data.path().join("mcp-bridge-port"), port.to_string()).unwrap();
        std::env::set_var("DBX_APP_DATA_DIR", app_data.path());
        std::env::set_var("DBX_APP_LAUNCH_CMD", ":");

        let (server, _dir) = stdio_bridge_server();
        let response = stdio_dispatch(
            &server,
            "tools/call",
            json!({
                "name": "files_scan_digest",
                "arguments": { "connectionId": "saved-1", "path": "/" },
            }),
        )
        .unwrap();

        std::env::remove_var("DBX_APP_DATA_DIR");
        std::env::remove_var("DBX_APP_LAUNCH_CMD");

        assert!(response.get("error").is_none(), "{response}");
        let payload = unwrap_envelope(&response["result"]);
        assert_eq!(payload["unexpected"], "shape", "{payload}");
    }

    /// Bridge unreachable (unpublished port, no-op launch): the forward leg
    /// fails closed into the merged guidance error — bridge reason plus the
    /// inline-credential ways out named with files' own parameter fields.
    #[test]
    fn stdio_bridge_unreachable_fails_closed_with_inline_guidance() {
        let _guard = BRIDGE_ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let app_data = tempfile::tempdir().unwrap(); // 空目录：端口文件不存在
        std::env::set_var("DBX_APP_DATA_DIR", app_data.path());
        std::env::set_var("DBX_APP_LAUNCH_CMD", ":");
        let (server, _dir) = stdio_bridge_server();
        let error = stdio_error(
            &server,
            "tools/call",
            json!({
                "name": "files_scan_digest",
                "arguments": { "connectionId": "ghost", "path": "/" },
            }),
        );
        std::env::remove_var("DBX_APP_DATA_DIR");
        std::env::remove_var("DBX_APP_LAUNCH_CMD");

        assert!(error.contains("Unknown connectionId 'ghost'"), "{error}");
        assert!(error.contains("DBX app bridge"), "{error}");
        assert!(error.contains("inline connection parameters"), "{error}");
        // files 特化出路：内联参数字段点名（protocol/secretAccessKey）。
        assert!(error.contains("\"protocol\""), "{error}");
        assert!(error.contains("secretAccessKey"), "{error}");
    }

    #[test]
    fn inline_pool_key_is_hash_stable_and_sensitive() {
        let a = json!({ "protocol": "local", "root": "/tmp/x" });
        let b = json!({ "root": "/tmp/x", "protocol": "local" });
        assert_eq!(inline_pool_id(&a), inline_pool_id(&b), "key order irrelevant");
        assert!(inline_pool_id(&a).starts_with("mcp-inline-"));
        let c = json!({ "protocol": "local", "root": "/tmp/y" });
        assert_ne!(inline_pool_id(&a), inline_pool_id(&c));
        // Credentials are part of the key: two secrets never share a pool id.
        let secret = json!({ "protocol": "s3", "bucket": "b", "secretAccessKey": "k1" });
        let secret2 = json!({ "protocol": "s3", "bucket": "b", "secretAccessKey": "k2" });
        assert_ne!(inline_pool_id(&secret), inline_pool_id(&secret2));
    }

    #[test]
    fn inline_connection_maps_camel_case_form_fields() {
        let connection = stored_connection_from_inline(&json!({
            "protocol": "localFs",
            "root": "/data",
            "name": "smoke",
            "readOnly": true,
            "allowDelete": false,
            "lockToRoot": true,
            "timeoutSecs": 42,
        }))
        .unwrap();
        assert_eq!(connection.protocol, "fs", "localFs aliases fs");
        assert_eq!(connection.root, "/data");
        assert!(connection.read_only);
        assert!(!connection.allow_delete);
        assert!(connection.lock_to_root);
        assert_eq!(connection.timeout_secs, 42);
        assert!(connection.id.starts_with("mcp-inline-"), "{}", connection.id);

        let s3 = stored_connection_from_inline(&json!({
            "protocol": "s3",
            "bucket": "demo",
            "endpoint": "http://127.0.0.1:9000",
            "region": "us-east-1",
            "accessKeyId": "minioadmin",
            "secretAccessKey": "minioadmin",
            "id": "inline-c1",
        }))
        .unwrap();
        assert_eq!(s3.id, "inline-c1", "explicit id wins over the pool hash");
        assert_eq!(s3.bucket, "demo");
        assert_eq!(s3.access_key_id, "minioadmin");
        assert_eq!(s3.secret_access_key, "minioadmin");
        assert_eq!(s3.endpoint, "http://127.0.0.1:9000");

        let cos = stored_connection_from_inline(&json!({
            "protocol": "cos",
            "bucket": "demo-1250000000",
            "endpoint": "https://cos.ap-guangzhou.myqcloud.com",
            "secretId": "throwaway-secret-id",
            "secretKey": "throwaway-secret-key",
            "securityToken": "throwaway-security-token",
        }))
        .unwrap();
        assert_eq!(cos.protocol, "cos");
        assert_eq!(cos.secret_id, "throwaway-secret-id");
        assert_eq!(cos.secret_key, "throwaway-secret-key");
        assert_eq!(cos.security_token, "throwaway-security-token");

        let missing = stored_connection_from_inline(&json!({ "root": "/data" })).unwrap_err();
        assert!(missing.contains("protocol"), "{missing}");
        let unknown = stored_connection_from_inline(&json!({ "protocol": "gopher" })).unwrap_err();
        assert!(unknown.contains("Unsupported protocol"), "{unknown}");
        let not_object = stored_connection_from_inline(&json!("fs")).unwrap_err();
        assert!(not_object.contains("object"), "{not_object}");
    }

    /// Form↔MCP consistency: every camelCase key the inline parser maps into
    /// the lifecycle payload must be declared in the `tools/list` connection
    /// schema. Strict MCP hosts silently drop undeclared keys, so a drifted
    /// schema would quietly change connection behavior (e.g. drop the private
    /// key and fall back to anonymous) instead of erroring.
    #[test]
    fn inline_connection_schema_covers_every_mapped_key() {
        let properties = inline_connection_properties();
        let properties = properties.as_object().expect("schema properties object");
        let mapped: &[&str] = &[
            // config-bound keys, in stored_connection_from_inline mapping order
            "root",
            "bucket",
            "endpoint",
            "region",
            "container",
            "accountName",
            "scope",
            "accessKeyId",
            "enableVirtualHostStyle",
            "username",
            "user",
            "share",
            "domain",
            "knownHostsStrategy",
            "readOnly",
            "allowDelete",
            "lockToRoot",
            "timeoutSecs",
            "service",
            "config",
            "clientId",
            "driveType",
            "email",
            "repoName",
            // secret-bound keys
            "secretAccessKey",
            "credential",
            "accountKey",
            "secretId",
            "secretKey",
            "securityToken",
            "password",
            "key",
            "accessToken",
            "clientSecret",
            "refreshToken",
        ];
        for key in mapped {
            assert!(
                properties.contains_key(*key),
                "inline key '{key}' is mapped but undeclared in tools/list schema; \
                 strict MCP hosts would silently drop it"
            );
        }
        // protocol (validated below), id and name are transport fields — the
        // only schema entries allowed outside the mapping.
        let extra: Vec<&str> = properties
            .keys()
            .map(String::as_str)
            .filter(|key| *key != "protocol" && !mapped.contains(key))
            .collect();
        assert_eq!(extra.len(), 2, "undeclared schema keys: {extra:?}");
        assert!(properties.contains_key("id") && properties.contains_key("name"));
        // The protocol description must enumerate every engine protocol so an
        // LLM caller can never be offered a value the engine would reject.
        let description = properties["protocol"]["description"]
            .as_str()
            .expect("protocol description");
        for protocol in crate::model::PROTOCOLS {
            assert!(
                description.contains(protocol),
                "protocol '{protocol}' missing from the inline schema description"
            );
        }
    }

    /// Manifest↔MCP leg of the contract triangle: every connection-provider
    /// field in manifest.json must be reachable through the inline connection
    /// (config-bound → the camelCase config map, secret-bound → the secret
    /// map, display_name → `name`), and no mapping entry may exist without a
    /// manifest field behind it. Catches "field added to the form but never
    /// wired into MCP" drift.
    #[test]
    fn manifest_fields_are_fully_covered_by_inline_mapping() {
        let manifest: Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../manifest.json"
            ))
            .expect("manifest.json readable"),
        )
        .expect("manifest.json parses");
        let fields = manifest["contributions"]
            .as_array()
            .expect("contributions")
            .iter()
            .find(|item| item["type"] == "connection-provider")
            .expect("connection-provider")["fields"]
            .as_array()
            .expect("fields");
        let config_map: &[(&str, &str)] = &[
            ("root", "root"),
            ("bucket", "bucket"),
            ("endpoint", "endpoint"),
            ("region", "region"),
            ("container", "container"),
            ("account_name", "accountName"),
            ("scope", "scope"),
            ("access_key_id", "accessKeyId"),
            ("enable_virtual_host_style", "enableVirtualHostStyle"),
            ("username", "username"),
            ("user", "user"),
            ("share", "share"),
            ("domain", "domain"),
            ("known_hosts_strategy", "knownHostsStrategy"),
            ("read_only", "readOnly"),
            ("allow_delete", "allowDelete"),
            ("lock_to_root", "lockToRoot"),
            ("timeout_secs", "timeoutSecs"),
            ("service", "service"),
            ("config", "config"),
            ("client_id", "clientId"),
            ("drive_type", "driveType"),
            ("email", "email"),
            ("repo_name", "repoName"),
        ];
        let secret_map: &[(&str, &str)] = &[
            ("secret_access_key", "secretAccessKey"),
            ("credential", "credential"),
            ("account_key", "accountKey"),
            ("secret_id", "secretId"),
            ("secret_key", "secretKey"),
            ("security_token", "securityToken"),
            ("password", "password"),
            ("key", "key"),
            ("access_token", "accessToken"),
            ("client_secret", "clientSecret"),
            ("refresh_token", "refreshToken"),
        ];
        let mut covered: Vec<&str> = Vec::new();
        for field in fields {
            let key = field["key"].as_str().expect("field key");
            match field["binding"].as_str().expect("field binding") {
                "name" => assert_eq!(key, "display_name", "unexpected name-bound field"),
                "secret" => assert!(
                    secret_map.iter().any(|(source, _)| *source == key),
                    "secret field '{key}' missing from the inline secret mapping"
                ),
                _ if key == "protocol" => {}
                _ => assert!(
                    config_map.iter().any(|(source, _)| *source == key),
                    "config field '{key}' missing from the inline mapping; \
                     MCP callers could never set it"
                ),
            }
            covered.push(key);
        }
        for (source, _) in config_map.iter().chain(secret_map) {
            assert!(
                covered.contains(source),
                "inline mapping key '{source}' has no manifest field behind it"
            );
        }
    }

    /// The full stdio loop against a real localFs connection: mkdir/write via
    /// inline credentials, digest + cursor paging, two-phase delete with the
    /// parameter-hash confirmToken, pooled connectionId stability, the dir
    /// rename stdio refusal and the missing/unknown connectionId guidance.
    #[test]
    fn stdio_localfs_inline_round_trip_digest_cursor_two_phase() {
        let (server, dir) = stdio_server();
        // The fs root is separate from the plugin data dir: the audit trail
        // must not land inside the scanned tree.
        let fsroot = dir.path().join("fsroot");
        std::fs::create_dir_all(&fsroot).unwrap();
        let root = fsroot.to_string_lossy().to_string();
        let connection = json!({ "protocol": "local", "root": root, "name": "smoke" });

        // 建树走 stdio MCP 写工具本身（mkdir/write 单阶段直执行）。
        unwrap_envelope(&stdio_result(
            &server,
            "tools/call",
            json!({
                "name": "files_mkdir",
                "arguments": { "connection": connection, "path": "/data/sub" },
            }),
        ));
        for name in ["a.txt", "b.log"] {
            unwrap_envelope(&stdio_result(
                &server,
                "tools/call",
                json!({
                    "name": "files_write",
                    "arguments": {
                        "connection": connection,
                        "path": format!("/data/{name}"),
                        "dataBase64": BASE64_STANDARD.encode(format!("content {name}")),
                    },
                }),
            ));
        }

        // digest：递归扫描 + 本地聚合 + cursor 物化。
        let digest = unwrap_envelope(&stdio_result(
            &server,
            "tools/call",
            json!({
                "name": "files_scan_digest",
                "arguments": { "connection": connection, "path": "/data", "glob": "*" },
            }),
        ));
        assert_eq!(digest["matched"], 3, "{digest}");
        assert_eq!(digest["scanned"], 3, "{digest}");
        assert!(digest["cursorId"].as_str().unwrap().starts_with("cur-"));
        // 池化键稳定：同一 connection 参数的重复调用复用同一 engine 条目。
        let again = unwrap_envelope(&stdio_result(
            &server,
            "tools/call",
            json!({
                "name": "files_scan_digest",
                "arguments": { "connection": connection, "path": "/data", "format": "rows" },
            }),
        ));
        assert_eq!(digest["connectionId"], again["connectionId"], "pooled inline id");

        // cursor 翻页：不带 connectionId（纯会话查找）。
        let cursor_id = digest["cursorId"].as_str().unwrap();
        let page = unwrap_envelope(&stdio_result(
            &server,
            "tools/call",
            json!({
                "name": "files_cursor_next",
                "arguments": { "cursorId": cursor_id, "n": 2 },
            }),
        ));
        assert_eq!(page["rows"].as_array().unwrap().len(), 2, "{page}");
        assert_eq!(page["done"], false);

        // 两阶段 delete：preview → 参数改动作废 → 重新预览 → token 确认执行。
        let target = "/data/a.txt";
        let preview = unwrap_envelope(&stdio_result(
            &server,
            "tools/call",
            json!({
                "name": "files_delete",
                "arguments": { "connection": connection, "path": target },
            }),
        ));
        assert_eq!(preview["preview"]["path"], target, "{preview}");
        let token = preview["confirmToken"].as_str().unwrap().to_string();
        let error = stdio_error(
            &server,
            "tools/call",
            json!({
                "name": "files_delete",
                "arguments": { "connection": connection, "path": "/data/b.log", "confirmToken": token },
            }),
        );
        assert!(error.contains("arguments changed"), "{error}");
        let preview2 = unwrap_envelope(&stdio_result(
            &server,
            "tools/call",
            json!({
                "name": "files_delete",
                "arguments": { "connection": connection, "path": target },
            }),
        ));
        let confirmed = unwrap_envelope(&stdio_result(
            &server,
            "tools/call",
            json!({
                "name": "files_delete",
                "arguments": {
                    "connection": connection,
                    "path": target,
                    "confirmToken": preview2["confirmToken"],
                },
            }),
        ));
        assert_eq!(confirmed["success"], true, "{confirmed}");
        assert!(
            !fsroot.join("data/a.txt").exists(),
            "file deleted on disk"
        );
        // 一次性：同 token 重放 → unknown。
        let error = stdio_error(
            &server,
            "tools/call",
            json!({
                "name": "files_delete",
                "arguments": {
                    "connection": connection,
                    "path": target,
                    "confirmToken": preview2["confirmToken"],
                },
            }),
        );
        assert!(error.contains("unknown or already used"), "{error}");

        // 目录 rename 降级 job 需要工作台事件通道：stdio 下明确报错（不假死）。
        let error = stdio_error(
            &server,
            "tools/call",
            json!({
                "name": "files_rename",
                "arguments": { "connection": connection, "path": "/data/sub", "newPath": "/data/sub2" },
            }),
        );
        assert!(error.contains("standalone stdio"), "{error}");

        // 连接寻址引导：缺 connectionId / 未注册 id 都给出可执行出路。
        let error = stdio_error(
            &server,
            "tools/call",
            json!({ "name": "files_scan_digest", "arguments": { "path": "/" } }),
        );
        assert!(error.contains("Missing required parameter: connectionId"), "{error}");
        let error = stdio_error(
            &server,
            "tools/call",
            json!({ "name": "files_scan_digest", "arguments": { "connectionId": "smoke-nope", "path": "/" } }),
        );
        assert!(error.contains("Unknown connectionId 'smoke-nope'"), "{error}");
        // 引导错误带内联凭据出路（fallback 关闭时也保持同一消息形状）。
        assert!(error.contains("inline connection parameters"), "{error}");

        // arguments 非对象：清晰报错（与 DBX 桥 mcp/call 同语义）。
        let error = stdio_error(
            &server,
            "tools/call",
            json!({ "name": "files_scan_digest", "arguments": "not-an-object" }),
        );
        assert!(error.contains("arguments must be a JSON object"), "{error}");

        // quick_paths：limit 数字字符串被接受并 clamp（本地 fs 连接只有 root chip）。
        let quick = unwrap_envelope(&stdio_result(
            &server,
            "tools/call",
            json!({
                "name": "files_ui_quick_paths",
                "arguments": { "connection": connection, "limit": "1" },
            }),
        ));
        assert_eq!(quick["paths"].as_array().unwrap().len(), 1, "{quick}");

        // 空 dataBase64 建空文件（此前被 required_str 误拒，工作台路径允许）。
        unwrap_envelope(&stdio_result(
            &server,
            "tools/call",
            json!({
                "name": "files_write",
                "arguments": { "connection": connection, "path": "/data/empty.txt", "dataBase64": "" },
            }),
        ));
        assert_eq!(
            fsroot.join("data/empty.txt").metadata().unwrap().len(),
            0,
            "empty payload must create an empty file"
        );

        // 尾斜杠文件路径 delete：必须真删而不是静默 no-op（OpenDAL 陷阱）。
        let preview = unwrap_envelope(&stdio_result(
            &server,
            "tools/call",
            json!({
                "name": "files_delete",
                "arguments": { "connection": connection, "path": "/data/b.log/" },
            }),
        ));
        assert_eq!(preview["preview"]["kind"], "missing", "{preview}");
        unwrap_envelope(&stdio_result(
            &server,
            "tools/call",
            json!({
                "name": "files_delete",
                "arguments": {
                    "connection": connection,
                    "path": "/data/b.log/",
                    "confirmToken": preview["confirmToken"],
                },
            }),
        ));
        assert!(
            !fsroot.join("data/b.log").exists(),
            "trailing-slash delete must remove the real file"
        );

        // 两阶段 purge 成功路径：/data/sub 目录递归删除（磁盘校验）。
        let purge_preview = unwrap_envelope(&stdio_result(
            &server,
            "tools/call",
            json!({
                "name": "files_purge",
                "arguments": { "connection": connection, "path": "/data/sub" },
            }),
        ));
        assert_eq!(purge_preview["preview"]["kind"], "dir", "{purge_preview}");
        unwrap_envelope(&stdio_result(
            &server,
            "tools/call",
            json!({
                "name": "files_purge",
                "arguments": {
                    "connection": connection,
                    "path": "/data/sub",
                    "confirmToken": purge_preview["confirmToken"],
                },
            }),
        ));
        assert!(!fsroot.join("data/sub").exists(), "purged on disk");

        // 内联 readOnly 布尔字符串变体：意图是只读，写工具必须被拒（不能
        // 因为 "true" 是字符串就静默降级为可写连接）。
        let ro_root = dir.path().join("ro-root");
        std::fs::create_dir_all(&ro_root).unwrap();
        let ro_connection = json!({
            "protocol": "local",
            "root": ro_root.to_string_lossy(),
            "readOnly": "true",
        });
        let error = stdio_error(
            &server,
            "tools/call",
            json!({
                "name": "files_mkdir",
                "arguments": { "connection": ro_connection, "path": "/x" },
            }),
        );
        assert!(error.contains("read-only"), "{error}");

        // 审计：stdio 写路径与工作台共用 audit 基线，记 source:"mcp"。
        let audits = server.store.read_audit();
        assert!(
            audits
                .iter()
                .any(|record| record.action == "files/delete"
                    && record.source.as_deref() == Some("mcp")),
            "{audits:?}"
        );
    }

    // -- 第二轮：错误消息引导 + 可选字符串参数 fail-fast -----------------------

    #[test]
    fn unknown_tool_suggests_variants_and_lists_discovery() {
        // 分隔符/大小写变体给 Did-you-mean（ssh unknown_tool_message 同构）。
        let error = unknown_tool_message("FILES-SCANDIGEST");
        assert!(error.contains("Did you mean 'files_scan_digest'?"), "{error}");
        // 每个未知名都列出全部注册名与发现面（桥 mcp/tools / stdio tools/list）。
        let error = unknown_tool_message("files_nonexistent");
        assert!(!error.contains("Did you mean"), "{error}");
        for tool in ALL_TOOL_NAMES {
            assert!(error.contains(tool), "{tool} missing from: {error}");
        }
        assert!(error.contains("mcp/tools"), "{error}");
        assert!(error.contains("tools/list"), "{error}");
        // dispatch 真正走这条消息（run_tool 的 other 分支与 stdio 同源）。
        let (server, _dir) = stdio_server();
        let error = stdio_error(
            &server,
            "tools/call",
            json!({ "name": "files_nonexistent", "arguments": {} }),
        );
        assert!(error.contains("Unknown tool"), "{error}");
        assert!(error.contains("files_scan_digest"), "{error}");
    }

    #[test]
    fn unknown_intent_id_error_guides_caller() {
        let (server, _dir) = stdio_server();
        // files_ui_state 带 intentId 走 run_tool（不经 stdio 的 UI UNAVAILABLE
        // 短路？——走：该工具在 STDIO_UI_TOOLS 内。此处直接打 run_tool 验证
        // 桥路径（mcp/call）的错误文本。）
        let rt = tokio::runtime::Runtime::new().unwrap();
        let error = rt
            .block_on(server.mcp.run_tool(
                "files_ui_state",
                &json!({ "intentId": "i-nope" }),
                &server.engine,
                &server.transfers,
                &server.store,
                None,
            ))
            .expect_err("unknown intentId must error");
        assert!(error.contains("unknown intentId"), "{error}");
        assert!(error.contains("60s"), "{error}");
        assert!(error.contains("files_ui_"), "{error}");
        assert!(error.contains("omit intentId"), "{error}");
    }

    #[test]
    fn digest_rejects_non_string_optional_args_instead_of_silent_defaults() {
        let mcp = mcp();
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new();
        engine
            .connect(
                StoredConnection::from_lifecycle_params(&json!({
                    "connection": {
                        "id": "dig",
                        "external_config": { "protocol": "fs", "root": dir.path() },
                    }
                }))
                .unwrap(),
            )
            .unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        // path 传了非字符串：绝不静默回落 "/"（那会扫错整棵树）。
        let error = rt
            .block_on(mcp.scan_digest_tool(&engine, &json!({ "connectionId": "dig", "path": 123 })))
            .unwrap_err();
        assert!(error.contains("'path' must be a non-empty string"), "{error}");
        // glob/format 同理 fail-fast；空串也是明确拒绝而非静默忽略。
        for (key, value) in [("glob", json!(true)), ("format", json!(7)), ("path", json!(""))] {
            let error = rt
                .block_on(mcp.scan_digest_tool(
                    &engine,
                    &json!({ "connectionId": "dig", key: value }),
                ))
                .unwrap_err();
            assert!(
                error.contains(&format!("'{key}' must be a non-empty string")),
                "{key}: {error}"
            );
        }
        // 缺省值路径不受影响：path 缺省扫根、format 缺省 digest。
        let payload = rt
            .block_on(mcp.scan_digest_tool(&engine, &json!({ "connectionId": "dig" })))
            .unwrap();
        assert_eq!(payload["path"], "/", "{payload}");
        assert!(payload.get("stats").is_some(), "digest format by default");
    }

    #[test]
    fn allow_delete_refusal_names_the_way_out() {
        let connection = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "c",
                "external_config": { "protocol": "fs", "allow_delete": false },
            }
        }))
        .unwrap();
        let error = ensure_deletable(&connection).unwrap_err();
        assert!(error.contains("allow_delete=false"), "{error}");
        assert!(error.contains("allowDelete"), "{error}");
    }

    #[test]
    fn base64_error_names_the_expected_format() {
        let (server, dir) = stdio_server();
        let fsroot = dir.path().join("fsroot-b64");
        std::fs::create_dir_all(&fsroot).unwrap();
        let connection = json!({ "protocol": "local", "root": fsroot.to_string_lossy() });
        let error = stdio_error(
            &server,
            "tools/call",
            json!({
                "name": "files_write",
                "arguments": { "connection": connection, "path": "/x.txt", "dataBase64": "!!!not-base64!!!" },
            }),
        );
        assert!(error.contains("base64"), "{error}");
        assert!(error.contains("RFC 4648"), "{error}");
    }

    // -- 第五轮（可靠性纵深）：stdio 传输层健壮性 ------------------------------

    /// dispatch 一次原始 JSON-RPC 请求（id/method 形状由用例自定）。
    fn dispatch_raw(server: &StdioServer, request: Value) -> Option<Value> {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(server.dispatch(request))
    }

    #[test]
    fn stdio_dispatch_rejects_missing_method_and_non_scalar_ids() {
        let (server, _dir) = stdio_server();
        // 缺 method（含 method 非字符串）：invalid request -32600，不是
        // "Method not found"（空串 method 不是可纠正的未知方法名）。
        for request in [
            json!({ "jsonrpc": "2.0", "id": 1 }),
            json!({ "jsonrpc": "2.0", "id": 1, "method": "" }),
            json!({ "jsonrpc": "2.0", "id": 1, "method": 42 }),
        ] {
            let response = dispatch_raw(&server, request.clone()).expect("answered");
            assert_eq!(response["error"]["code"], -32600, "{request}");
            assert!(
                response["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("missing method"),
                "{response}"
            );
        }
        // id 为 object/array/boolean：无效请求 -32600，id 不回显。
        for bad_id in [json!({ "a": 1 }), json!([1, 2]), json!(true)] {
            let request = json!({ "jsonrpc": "2.0", "id": bad_id, "method": "ping" });
            let response = dispatch_raw(&server, request).expect("answered");
            assert_eq!(response["error"]["code"], -32600, "{response}");
            assert!(response["id"].is_null(), "{response}");
            assert!(
                response["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("id must be a string, number, or null"),
                "{response}"
            );
        }
        // 合法 id 形状照常：string / number / null 都得到正常响应。
        for good_id in [json!("str-1"), json!(7), json!(null)] {
            let request = json!({ "jsonrpc": "2.0", "id": good_id, "method": "ping" });
            let response = dispatch_raw(&server, request).expect("answered");
            assert_eq!(response["result"], json!({}), "{response}");
            assert_eq!(response["id"], good_id, "id echoed verbatim");
        }
        // 无 id 的非 notification 请求：-32600（保留既有形状），连接继续。
        let response = dispatch_raw(&server, json!({ "jsonrpc": "2.0", "method": "ping" }))
            .expect("answered");
        assert_eq!(response["error"]["code"], -32600, "{response}");
        // notifications/* 无 id：不回包（None）。
        assert!(dispatch_raw(
            &server,
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
        )
        .is_none());
    }

    #[test]
    fn stdio_dispatch_tolerates_missing_or_variant_jsonrpc_field() {
        // 设计内宽容（族一致：ssh/ldap/kafka 均不校验 jsonrpc 字段）：真实
        // MCP 客户端可能省略或变体，拒绝只会破坏兼容而没有可防护的行为。
        for version in [json!("1.0"), json!(2), json!(null)] {
            let (server, _dir) = stdio_server();
            let request = json!({ "jsonrpc": version, "id": 1, "method": "ping" });
            let response = dispatch_raw(&server, request).expect("answered");
            assert_eq!(response["result"], json!({}), "{response}");
        }
        let (server, _dir) = stdio_server();
        let response = dispatch_raw(&server, json!({ "id": 1, "method": "ping" })).expect("answered");
        assert_eq!(response["result"], json!({}), "{response}");
    }

    #[test]
    fn parse_request_line_tolerates_blank_and_whitespace_lines() {
        assert!(parse_request_line("").unwrap().is_none());
        assert!(parse_request_line("   ").unwrap().is_none());
        // 外层 reader 已 trim 行尾 CRLF；行内 JSON 正常解析。
        let parsed = parse_request_line(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
            .unwrap()
            .expect("valid line");
        assert_eq!(parsed["method"], "ping");
        // 非法 JSON：-32700 响应（null id），不是进程错误。
        let error = parse_request_line("not json").unwrap_err();
        assert_eq!(error["error"]["code"], -32700, "{error}");
    }

    #[test]
    fn stdio_max_line_env_is_parsed_with_safe_fallback() {
        // 与桥接用例共享进程级 env 锁：这些用例会动 DBX_APP_* 之外的
        // 进程全局变量。
        let _guard = BRIDGE_ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        std::env::set_var("DBX_FILES_MCP_STDIO_MAX_LINE", "1024");
        assert_eq!(stdio_max_line_bytes(), 1024);
        std::env::set_var("DBX_FILES_MCP_STDIO_MAX_LINE", " 8388608 ");
        assert_eq!(stdio_max_line_bytes(), 8_388_608);
        for junk in ["0", "-5", "big", ""] {
            std::env::set_var("DBX_FILES_MCP_STDIO_MAX_LINE", junk);
            assert_eq!(
                stdio_max_line_bytes(),
                DEFAULT_STDIO_MAX_LINE_BYTES,
                "junk {junk:?} falls back to the default"
            );
        }
        std::env::remove_var("DBX_FILES_MCP_STDIO_MAX_LINE");
        assert_eq!(stdio_max_line_bytes(), DEFAULT_STDIO_MAX_LINE_BYTES);
    }

    // -- 第五轮（可靠性纵深）：confirm/cursor/intent 存储 churn -----------------

    #[test]
    fn confirm_table_churns_at_ttl_and_hard_cap() {
        let table = mcp();
        let args = json!({ "connectionId": "c", "path": "/x" });
        // 600+ 次 preview（从不 confirm）：表被 TTL prune + 硬上限钳住，
        // 绝不单调增长（签发量超过硬上限，表停在上限）。
        for _ in 0..(Mcp::MAX_CONFIRM_ENTRIES + 200) {
            let (token, _) = table.confirm_begin(&args);
            assert!(token.starts_with("c-"));
        }
        {
            let confirms = table
                .confirms
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            assert_eq!(confirms.len(), Mcp::MAX_CONFIRM_ENTRIES, "capped");
            // 最新 token 必然在表内（淘汰的是最早过期者）。
            let now = unix_millis_now() as u128;
            assert!(confirms
                .values()
                .all(|entry| entry.expires_at_millis > now));
        }
        // 过期条目在下一次 preview 时被清走（TTL churn）。
        let expired_at = unix_millis_now() as u128 - 1;
        table
            .confirms
            .lock()
            .unwrap()
            .insert("c-expired".to_string(), ConfirmEntry { hash: 0, expires_at_millis: expired_at });
        let _ = table.confirm_begin(&args);
        assert!(
            !table.confirms.lock().unwrap().contains_key("c-expired"),
            "expired token pruned on the next preview"
        );
    }

    #[test]
    fn confirm_cap_evicts_the_earliest_expiring_token() {
        let table = mcp();
        {
            let mut confirms = table.confirms.lock().unwrap();
            for index in 0..Mcp::MAX_CONFIRM_ENTRIES {
                confirms.insert(
                    format!("c-fill-{index}"),
                    ConfirmEntry {
                        hash: 0,
                        // 远未来的过期时间 + 相对序：TTL prune 不参与，
                        // 只验证硬上限淘汰（最早过期者 = 序号最小者）。
                        expires_at_millis: unix_millis_now() as u128 + 1_000_000 + index as u128,
                    },
                );
            }
        }
        let (fresh, _) = table.confirm_begin(&json!({}));
        let confirms = table.confirms.lock().unwrap();
        assert_eq!(confirms.len(), Mcp::MAX_CONFIRM_ENTRIES, "cap holds");
        assert!(confirms.contains_key(&fresh), "the fresh token survives");
        assert!(
            !confirms.contains_key("c-fill-0"),
            "the earliest-expiring token is evicted"
        );
        assert!(confirms.contains_key("c-fill-1"), "newer filler stays");
    }

    #[test]
    fn confirm_ttl_retune_does_not_redate_issued_tokens() {
        let mcp = mcp();
        let (token, expires_at) = mcp.confirm_begin(&json!({ "connectionId": "c", "path": "/a" }));
        assert!(expires_at > unix_millis_now() as u128 + 55_000, "60s default");
        // 调小 TTL：已签发 token 的过期时间不追溯（begin 时固定）。
        mcp.settings_set(&json!({ "confirmTtlSecs": 10 })).unwrap();
        let (_, retuned_expires_at) = mcp.confirm_begin(&json!({ "connectionId": "c", "path": "/b" }));
        let now = unix_millis_now() as u128;
        assert!(
            retuned_expires_at <= now + 11_000,
            "new tokens use the retuned TTL: {retuned_expires_at} vs now {now}"
        );
        // 旧 token 在其原始 600s 窗口内仍然有效（消费即成功，一次性）。
        mcp.confirm_verify(&token, &json!({ "connectionId": "c", "path": "/a" }))
            .expect("pre-retune token keeps its original window");
        // 过期报文携带实际生效（调小后）的 TTL。
        let (token2, _) = mcp.confirm_begin(&json!({ "connectionId": "c", "path": "/c" }));
        mcp.confirms
            .lock()
            .unwrap()
            .get_mut(&token2)
            .unwrap()
            .expires_at_millis = unix_millis_now() as u128 - 1;
        let error = mcp
            .confirm_verify(&token2, &json!({ "connectionId": "c", "path": "/c" }))
            .unwrap_err();
        assert!(error.contains("TTL 10s"), "{error}");
    }

    #[test]
    fn cursor_sessions_churn_with_lru_and_retuned_settings() {
        let mcp = mcp();
        mcp.settings_set(&json!({ "maxCursorSessions": 2 })).unwrap();
        let (a, _) = mcp.cursor_put(vec!["/a".into()]);
        let (b, _) = mcp.cursor_put(vec!["/b".into()]);
        // 活跃会话不被误逐：touch A 后物化 C，被逐的是 B（LRU）。
        let page = mcp.cursor_next(&json!({ "cursorId": a })).unwrap();
        assert_eq!(page["rows"][0]["path"], "/a", "{page}");
        let (c, _) = mcp.cursor_put(vec!["/c".into()]);
        mcp.cursor_next(&json!({ "cursorId": a }))
            .expect("recently-touched session survives the LRU eviction");
        let error = mcp.cursor_next(&json!({ "cursorId": b })).unwrap_err();
        assert!(error.contains("unknown cursorId"), "{error}");
        assert!(
            error.contains("re-run files_scan_digest"),
            "evicted cursorId answers with the fresh-digest guidance: {error}"
        );
        let _ = mcp.cursor_next(&json!({ "cursorId": c })).unwrap();

        // settings 缩容即时生效：8 → 1，下一次物化后表只剩最新一条。
        mcp.settings_set(&json!({ "maxCursorSessions": 8 })).unwrap();
        for index in 0..8 {
            mcp.cursor_put(vec![format!("/n{index}")]);
        }
        {
            let cursors = mcp.lock_cursors();
            assert_eq!(cursors.entries.len(), 8, "cap raised to 8 holds 8");
        }
        mcp.settings_set(&json!({ "maxCursorSessions": 1 })).unwrap();
        let (last, _) = mcp.cursor_put(vec!["/last".into()]);
        {
            let cursors = mcp.lock_cursors();
            assert_eq!(cursors.entries.len(), 1, "shrink evicts overflow at once");
        }
        mcp.cursor_next(&json!({ "cursorId": last }))
            .expect("the newest session survives");
    }

    #[test]
    fn cursor_materialization_cap_churn_loops_stably() {
        let mcp = mcp();
        // 等效压测（真机 M12 用 DBX_FILES_MCP_CLAMP_FILES 调小同型）：物化
        // 上限钳到 sanitize 下限 100、会话上限 1，反复「打满 → 翻到底 →
        // 再物化（旧会话被逐）」50 轮，断言无 panic、行数恒被钳、表恒 1。
        mcp.settings_set(&json!({ "maxCursorRows": 100, "maxCursorSessions": 1 }))
            .unwrap();
        for round in 0..50 {
            let rows: Vec<String> = (0..101).map(|index| format!("/r{index}")).collect();
            let (cursor_id, truncated) = mcp.cursor_put(rows);
            assert!(truncated, "round {round}: 101 rows must report truncation");
            let page = mcp.cursor_next(&json!({ "cursorId": cursor_id, "offset": 99, "n": 20 }))
                .unwrap_or_else(|error| panic!("round {round}: {error}"));
            assert_eq!(page["rows"].as_array().unwrap().len(), 1, "{page}");
            assert_eq!(page["done"], true, "{page}");
            // 显式 offset 越过物化上限：恒空且 done（绝不漏出第 101 行）。
            let over = mcp.cursor_next(&json!({ "cursorId": cursor_id, "offset": 100 }))
                .unwrap();
            assert_eq!(over["rows"], json!([]), "{over}");
            {
                let cursors = mcp.lock_cursors();
                assert_eq!(cursors.entries.len(), 1, "round {round}: session table capped at 1");
                assert_eq!(
                    cursors.entries[0].1.rows.len(),
                    100,
                    "round {round}: materialized rows clamped"
                );
            }
        }
    }

    #[test]
    fn intent_churn_and_snapshot_stay_correct() {
        let mcp = mcp();
        let now = unix_millis_now() as u128;
        // 500 轮登记：LRU(20) + TTL 钳表；早期 intent 逐出、未知查报 Unknown。
        for index in 0..500 {
            mcp.register_intent(&format!("i-{index}"), "focus", json!({}), now);
        }
        assert!(matches!(
            mcp.intent_lookup("i-0", now),
            IntentLookup::Unknown
        ));
        assert!(matches!(
            mcp.intent_lookup("i-499", now),
            IntentLookup::Found(_)
        ));
        // 登记 → 回报 applied → 快照正确；过期 → Expired 且条目移除。
        mcp.register_intent("i-live", "search", json!({"path": "/data"}), now);
        let reported = mcp
            .report(&json!({
                "intentId": "i-live",
                "status": "applied",
                "summary": { "count": 3 },
            }))
            .unwrap();
        assert_eq!(reported["intentId"], "i-live", "{reported}");
        match mcp.intent_lookup("i-live", now) {
            IntentLookup::Found(state) => {
                assert_eq!(state["state"], "applied", "{state}");
                assert_eq!(state["summary"]["count"], 3, "{state}");
            }
            IntentLookup::Expired | IntentLookup::Unknown => {
                panic!("expected Found for i-live after an applied report")
            }
        }
        mcp.register_intent("i-dying", "select", json!({}), now);
        let future = now + 61_000; // TTL 60s 过后
        assert!(
            matches!(mcp.intent_lookup("i-dying", future), IntentLookup::Expired),
            "expired lookup reports Expired"
        );
        assert!(
            matches!(mcp.intent_lookup("i-dying", future), IntentLookup::Unknown),
            "the expired entry is removed on read"
        );
        // 过期条目不阻塞回报路径之外的快照：同一 UI state 反复上报 churn，
        // 快照恒为「最后写入」，形状不漂移。
        for round in 0..100 {
            let summary = json!({ "panel": "browse", "count": round });
            mcp.report(&json!({ "status": "snapshot", "summary": summary })).unwrap();
            let snapshot = mcp
                .snapshot
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone()
                .unwrap_or(Value::Null);
            assert_eq!(snapshot["count"], round, "snapshot is last-write-wins");
            assert_eq!(snapshot["panel"], "browse", "snapshot shape stable");
        }
    }

    #[test]
    fn digest_is_idempotent_across_repeated_calls() {
        let mcp = mcp();
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new();
        engine
            .connect(
                StoredConnection::from_lifecycle_params(&json!({
                    "connection": {
                        "id": "dig",
                        "external_config": { "protocol": "fs", "root": dir.path() },
                    }
                }))
                .unwrap(),
            )
            .unwrap();
        std::fs::create_dir_all(dir.path().join("d")).unwrap();
        for name in ["a.txt", "b.log"] {
            std::fs::write(dir.path().join("d").join(name), format!("content {name}")).unwrap();
        }
        let rt = tokio::runtime::Runtime::new().unwrap();
        let reference = rt
            .block_on(mcp.scan_digest_tool(&engine, &json!({ "connectionId": "dig", "path": "/d" })))
            .unwrap();
        for _ in 0..100 {
            let again = rt
                .block_on(mcp.scan_digest_tool(
                    &engine,
                    &json!({ "connectionId": "dig", "path": "/d" }),
                ))
                .unwrap();
            assert_eq!(again["matched"], reference["matched"], "{again}");
            assert_eq!(again["scanned"], reference["scanned"], "{again}");
            assert_eq!(again["stats"], reference["stats"], "aggregate drift");
            assert_eq!(again["sample"], reference["sample"], "sample drift");
            assert!(again["cursorId"].as_str().unwrap().starts_with("cur-"));
        }
        assert_eq!(reference["matched"], 2, "a.txt + b.log (no dir marker inside /d)");
    }

    // -- 第五轮（可靠性纵深）：MCP 路径形状门（对抗输入） -----------------------

    #[test]
    fn validate_path_shape_rejects_adversarial_segments() {
        // `..` 段（父遍历）与 `.` 段（根的混淆拼写）一律拒绝。
        for path in [
            "/..", "/../x", "/a/../b", "/a/../..", "a/../../etc", "/etc/..", "/.",
            "//.", "/./a", "/a/./b", "/data/./x.txt",
        ] {
            let error = validate_path_shape(path, "path").unwrap_err();
            assert!(
                error.contains("'.' or '..' path segment"),
                "{path}: {error}"
            );
        }
        // 控制字符（JSON \u0000 / 换行等）优雅报错，不 panic、不透传后端。
        for path in ["/a\nb", "/a\u{0}b", "/a\tb", "/a\u{1f}b"] {
            let error = validate_path_shape(path, "path").unwrap_err();
            assert!(error.contains("control characters"), "{path}: {error}");
        }
        // 超长路径（>4096 字节）：主动边界与清晰报文。
        let long = format!("/{}", "a".repeat(4096));
        let error = validate_path_shape(&long, "path").unwrap_err();
        assert!(error.contains("4096-byte path limit"), "{error}");
        // 合法形状放行（空格/unicode 文件名是正常输入）。
        for path in ["/a/b.txt", "/data/报告 v2.txt", "/café.txt"] {
            validate_path_shape(path, "path")
                .unwrap_or_else(|error| panic!("'{path}' should pass: {error}"));
        }
    }

    #[test]
    fn file_target_path_combines_shape_and_root_gates() {
        assert_eq!(
            file_target_path("/a.txt/", "path").unwrap(),
            "/a.txt",
            "trailing slash dropped"
        );
        for (path, marker) in [
            ("/", "not the root itself"),
            ("//", "not the root itself"),
            ("/..", "'.' or '..' path segment"),
            ("/.", "'.' or '..' path segment"),
            ("/a/../..", "'.' or '..' path segment"),
        ] {
            let error = file_target_path(path, "path").unwrap_err();
            assert!(error.contains(marker), "{path}: {error}");
        }
    }

    #[test]
    fn purge_and_delete_root_red_line_survives_bypass_spellings() {
        let (server, dir) = stdio_server();
        let fsroot = dir.path().join("fsroot-redline");
        let data = fsroot.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("keep.txt"), "keep").unwrap();
        let connection = json!({ "protocol": "local", "root": fsroot.to_string_lossy() });
        // 根红线对抗拼写：`/` `//` `/.` `//.` `/./` `/..` `  `（空白归根）
        // ——全部拒绝（红线门或路径形状门），且磁盘树完好。
        for path in ["/", "//", "/.", "//.", "/./", "/..", "  ", "/../", "a/../../.."] {
            let error = stdio_error(
                &server,
                "tools/call",
                json!({
                    "name": "files_purge",
                    "arguments": { "connection": connection, "path": path },
                }),
            );
            assert!(
                error.contains("refused")
                    || error.contains("'.' or '..' path segment")
                    || error.contains("root"),
                "purge({path:?}) must stay refused: {error}"
            );
        }
        let error = stdio_error(
            &server,
            "tools/call",
            json!({
                "name": "files_delete",
                "arguments": { "connection": connection, "path": "a/../../etc" },
            }),
        );
        assert!(error.contains("'.' or '..' path segment"), "{error}");
        assert!(data.join("keep.txt").exists(), "nothing was deleted");
        assert!(fsroot.join("data").exists(), "root survived every spelling");
    }

    #[test]
    fn tilde_and_percent_and_unicode_are_literal_never_expanded() {
        let (server, dir) = stdio_server();
        let fsroot = dir.path().join("fsroot-literal");
        std::fs::create_dir_all(&fsroot).unwrap();
        let connection = json!({ "protocol": "local", "root": fsroot.to_string_lossy() });
        // `~` 不展开：写的是连接根下的字面 `~` 目录（设计内保守行为）。
        unwrap_envelope(&stdio_result(
            &server,
            "tools/call",
            json!({
                "name": "files_write",
                "arguments": {
                    "connection": connection,
                    "path": "~/x.txt",
                    "dataBase64": BASE64_STANDARD.encode("tilde"),
                },
            }),
        ));
        assert!(fsroot.join("~").join("x.txt").exists(), "~ stays literal");
        // URL 编码风格不解码：`%2e%2e` 是字面目录名，绝不逃逸连接根。
        unwrap_envelope(&stdio_result(
            &server,
            "tools/call",
            json!({
                "name": "files_mkdir",
                "arguments": { "connection": connection, "path": "/%2e%2e" },
            }),
        ));
        assert!(fsroot.join("%2e%2e").exists(), "percent stays literal");
        // NFC / NFD 组合：不归一化，两个拼写是两个独立字面条目（fs 语义）。
        let nfc = "/caf\u{e9}.txt";
        let nfd = "/cafe\u{301}.txt";
        for path in [nfc, nfd] {
            unwrap_envelope(&stdio_result(
                &server,
                "tools/call",
                json!({
                    "name": "files_write",
                    "arguments": {
                        "connection": connection,
                        "path": path,
                        "dataBase64": BASE64_STANDARD.encode("u"),
                    },
                }),
            ));
        }
        let digest = unwrap_envelope(&stdio_result(
            &server,
            "tools/call",
            json!({
                "name": "files_scan_digest",
                "arguments": { "connection": connection, "path": "/", "glob": "*.txt" },
            }),
        ));
        // 插件契约是“不自行归一化”：两种拼写都按字面写成功（上方 unwrap）。
        // 落盘后剩几条由文件系统语义决定：APFS（macOS 默认卷）NFC/NFD 查找
        // 不敏感，后写覆盖先写只剩 1 条；ext4/NTFS 逐字节保留则是 2 条。
        // matched 还含 ~/x.txt（glob 对分隔符宽松），所以只数 café 条目。
        let cafe_entries = digest["sample"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter(|r| {
                        r["path"].as_str().is_some_and(|p| p.contains("caf"))
                    })
                    .count()
            })
            .unwrap_or(0);
        if cfg!(target_os = "macos") {
            assert_eq!(
                cafe_entries, 1,
                "APFS folds NFC/NFD; plugin itself must not: {digest}"
            );
        } else {
            assert_eq!(cafe_entries, 2, "NFC and NFD stay distinct entries: {digest}");
        }
    }

    // -- 第七轮（并发安全与口径拉齐）-----------------------------------------

    #[test]
    fn missing_required_enumerates_every_business_gap() {
        // 缺参枚举式（ssh 口径拉齐，MCP_ACCEPTANCE §3.9）：一次点名全部缺失
        // 的业务 required 参数，按 schema required 顺序。
        let arguments = json!({});
        let error = missing_required(&arguments, &["path", "dataBase64"]).unwrap_err();
        assert_eq!(error, "Missing required parameters: path, dataBase64");

        // null 视为缺失（absent 与 null 同罪）。
        let error = missing_required(
            &json!({ "path": null, "newPath": null }),
            &["path", "newPath"],
        )
        .unwrap_err();
        assert_eq!(error, "Missing required parameters: path, newPath");

        // 部分在场：只枚举缺失的那部分，保持 schema 顺序。
        let error = missing_required(&json!({ "path": "/a" }), &["path", "newPath"]).unwrap_err();
        assert_eq!(error, "Missing required parameters: newPath");

        // present-but-类型错误不混入枚举（由 required_str 精确点名）。
        let arguments = json!({ "path": 123, "newPath": "/b" });
        assert!(missing_required(&arguments, &["path", "newPath"]).is_ok());
        let error = required_str(&arguments, "path").unwrap_err();
        assert_eq!(error, "Parameter 'path' must be a non-empty string");

        // 空字符串不算缺失（files_write dataBase64="" 是合法空文件语义）。
        assert!(missing_required(&json!({ "dataBase64": "" }), &["dataBase64"]).is_ok());

        // 全部在场 → Ok。
        assert!(missing_required(&json!({ "path": "/a" }), &["path"]).is_ok());
    }

    #[test]
    fn missing_required_flows_through_tool_dispatch() {
        let (server, dir) = stdio_server();
        // stdio 连接门先于业务参数校验（§3.7，call_tool 拦截）：完全空参先
        // 报连接寻址指引——顺序张力为登记设计，核对器 CONNECTION_GATE_RE
        // 归一为 WARN。
        let error = stdio_error(
            &server,
            "tools/call",
            json!({ "name": "files_write", "arguments": {} }),
        );
        assert!(
            error.contains("Missing required parameter: connectionId"),
            "{error}"
        );
        // 内联连接在场（合法可解析），业务缺参枚举即达：一次点名 path +
        // dataBase64（stdio schema required 去 connectionId 后的集合，
        // 核对器 B1 口径）。
        let inline = json!({ "protocol": "local", "root": dir.path().to_string_lossy() });
        let error = stdio_error(
            &server,
            "tools/call",
            json!({
                "name": "files_write",
                "arguments": { "connection": inline },
            }),
        );
        assert!(
            error.contains("Missing required parameters: path, dataBase64"),
            "{error}"
        );
        // files_rename：path 在场只缺 newPath → 单点。
        let error = stdio_error(
            &server,
            "tools/call",
            json!({
                "name": "files_rename",
                "arguments": {
                    "connection": {
                        "protocol": "local",
                        "root": dir.path().to_string_lossy(),
                    },
                    "path": "/a",
                },
            }),
        );
        assert_eq!(error, "Missing required parameters: newPath", "{error}");
        // present-but-类型错误仍单独精确点名，不混入 Missing required 枚举。
        // 内联连接在场（合法可解析），业务参数类型错误才可达。
        let dir = tempfile::TempDir::new().unwrap();
        let error = stdio_error(
            &server,
            "tools/call",
            json!({
                "name": "files_write",
                "arguments": {
                    "connection": {
                        "protocol": "local",
                        "root": dir.path().to_string_lossy(),
                    },
                    "path": 123,
                    "dataBase64": "aGk=",
                },
            }),
        );
        assert!(
            error.contains("Parameter 'path' must be a non-empty string"),
            "{error}"
        );
        assert!(!error.contains("Missing required"), "{error}");
    }

    #[test]
    fn normalized_format_is_case_insensitive_and_lists_legal_values() {
        assert_eq!(normalized_format(None).unwrap(), "digest");
        assert_eq!(normalized_format(Some("rows")).unwrap(), "rows");
        // 大小写归一（§3.3）：真实调用方 `format: "ROWS"` 正常工作。
        assert_eq!(normalized_format(Some("ROWS")).unwrap(), "rows");
        assert_eq!(normalized_format(Some("Digest")).unwrap(), "digest");
        assert_eq!(normalized_format(Some(" Rows ")).unwrap(), "rows");
        // 非法值报错列合法值并带原值点名。
        let error = normalized_format(Some("bogus")).unwrap_err();
        assert!(
            error.contains("format must be 'digest' or 'rows'") && error.contains("bogus"),
            "{error}"
        );
    }

    #[test]
    fn depth_error_spells_the_legal_range_inline() {
        let (server, dir) = stdio_server();
        let fsroot = dir.path().join("fsroot");
        std::fs::create_dir_all(&fsroot).unwrap();
        let connection = json!({ "protocol": "local", "root": fsroot.to_string_lossy() });
        // 非数字 depth：报错点名参数并列出合法范围（§3.3），连接真实可解析。
        let error = stdio_error(
            &server,
            "tools/call",
            json!({
                "name": "files_scan_digest",
                "arguments": { "connection": connection, "path": "/", "depth": "bogus" },
            }),
        );
        assert!(error.contains("depth must be a non-negative integer"), "{error}");
        assert!(error.contains("1..=16"), "{error}");
    }

    /// 并发 confirm churn（Rust 侧等价于 Go -race 轮）：8 线程混并发
    /// preview/consume/hold 2000+ 次签发（超 1024 硬上限，淘汰参与）。
    /// 断言：令牌一次性（成功消费后重放必败）、表容量收敛 ≤1024、
    /// 无过期残留。
    #[test]
    fn confirm_table_survives_concurrent_churn_without_double_consumption() {
        let table = Arc::new(mcp());
        let args = json!({ "connectionId": "c", "path": "/x" });
        let rounds = 250u32;
        let threads = 8usize;
        let barrier = Arc::new(std::sync::Barrier::new(threads));
        let mut handles = Vec::new();
        for _ in 0..threads {
            let table = Arc::clone(&table);
            let args = args.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let mut held: Vec<String> = Vec::new();
                for round in 0..rounds {
                    let (token, _) = table.confirm_begin(&args);
                    match round % 3 {
                        0 => {
                            // 立即消费：一次成功（或同毫秒淘汰压力下的明确
                            // unknown），随后重放必须失败 —— 一次性。
                            match table.confirm_verify(&token, &args) {
                                Ok(()) => {}
                                Err(error) => assert!(
                                    error.contains("unknown or already used"),
                                    "fresh token must not fail with hash/expiry: {error}"
                                ),
                            }
                            assert!(
                                table.confirm_verify(&token, &args).is_err(),
                                "replay of a consumed token must fail"
                            );
                        }
                        1 => held.push(token),
                        _ => {} // 搁置：交给 TTL/上限淘汰路径
                    }
                }
                for token in held {
                    match table.confirm_verify(&token, &args) {
                        Ok(()) => {
                            assert!(
                                table.confirm_verify(&token, &args).is_err(),
                                "held token replay must fail"
                            );
                        }
                        Err(error) => {
                            // 并发淘汰下被逐的 token 明确报 unknown（设计内），
                            // 绝不允许 hash/expiry 之类别的错误。
                            assert!(
                                error.contains("unknown or already used"),
                                "held token failed unexpectedly: {error}"
                            );
                        }
                    }
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let confirms = table.confirms.lock().unwrap();
        assert!(
            confirms.len() <= Mcp::MAX_CONFIRM_ENTRIES,
            "capacity converges at the hard cap, got {}",
            confirms.len()
        );
        let now = unix_millis_now() as u128;
        assert!(
            confirms.values().all(|entry| entry.expires_at_millis > now),
            "no expired residue after concurrent churn"
        );
    }

    /// 并发 cursor churn：物化/翻页/淘汰混并发。阶段 A 钉「活跃会话不被
    /// 误逐」（物化驻留 2 条 < cap 4，4 线程高并发 touch + 读路径 churn，
    /// 活跃会话全程必活）；阶段 B 钉「容量收敛 + 翻页 offset 原子无重叠」
    /// （8 线程 × 100 唯一会话超压物化——每次 cursor_put 都是新 UUID 会话
    /// ——同时 4 线程并发翻同一会话，驱逐压力下成功翻页不重不漏越界）。
    #[test]
    fn cursor_table_survives_concurrent_materialize_page_evict() {
        let mcp = Arc::new(mcp());
        mcp.settings_set(&json!({ "maxCursorSessions": 4 })).unwrap();

        // 阶段 A：活跃保护。物化只进 2 条新会话（驻留 3 < cap 4，驱逐永不
        // 发生），5 个线程并发 touch 同一活跃会话（LRU get 的 remove+push
        // 与少量物化交错）——活跃会话绝不能在并发 touch 下失踪。
        let (active, _) = mcp.cursor_put((0..30).map(|index| format!("/a/{index}")).collect());
        {
            let seeders: Vec<_> = (0..2)
                .map(|index| {
                    let mcp = Arc::clone(&mcp);
                    std::thread::spawn(move || {
                        let _ = mcp.cursor_put(vec![format!("/seed/{index}")]);
                    })
                })
                .collect();
            for handle in seeders {
                handle.join().unwrap();
            }
            let touches: Vec<_> = (0..5)
                .map(|_| {
                    let mcp = Arc::clone(&mcp);
                    let active = active.clone();
                    std::thread::spawn(move || {
                        for _ in 0..1000 {
                            // 显式 offset=0：每次重读全窗（offset 语义见
                            // cursor_next——显式值任意重读）。n 被 clamp 到
                            // MAX_CURSOR_PAGE=20，30 行会话每页恒 20 行。
                            let page = mcp
                                .cursor_next(
                                    &json!({ "cursorId": active, "n": 30, "offset": 0 }),
                                )
                                .expect("active session must survive concurrent touches");
                            assert_eq!(page["rows"].as_array().unwrap().len(), 20, "{page}");
                            assert_eq!(page["offset"], 0, "{page}");
                            assert_eq!(page["nextOffset"], 20, "{page}");
                        }
                    })
                })
                .collect();
            for handle in touches {
                handle.join().unwrap();
            }
        }

        // 阶段 B：收敛 + 无重叠。8 线程 × 100 个唯一会话超压物化（>> cap），
        // 同时 4 线程并发翻同一 30 行会话（n=10，无 offset）：成功响应的行
        // 集合互不重叠（offset 推进原子，无双重消费）。
        let rows: Vec<String> = (0..30).map(|index| format!("/b/{index}")).collect();
        let (shared, _) = mcp.cursor_put(rows.clone());
        let barrier = Arc::new(std::sync::Barrier::new(8 + 4));
        let mut handles = Vec::new();
        for thread in 0..8 {
            let mcp = Arc::clone(&mcp);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for round in 0..100 {
                    let (id, _) = mcp.cursor_put(vec![format!("/c/{thread}-{round}")]);
                    assert!(id.starts_with("cur-"));
                }
            }));
        }
        let all_paths: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        for _ in 0..4 {
            let mcp = Arc::clone(&mcp);
            let barrier = Arc::clone(&barrier);
            let shared = shared.clone();
            let all_paths = Arc::clone(&all_paths);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                loop {
                    let page = match mcp.cursor_next(&json!({ "cursorId": shared, "n": 10 })) {
                        Ok(page) => page,
                        // 并发淘汰压力下 unknown 是设计内结果（重发 digest）。
                        Err(error) => {
                            assert!(error.contains("unknown cursorId"), "{error}");
                            break;
                        }
                    };
                    let done = page["done"].as_bool().unwrap();
                    for row in page["rows"].as_array().unwrap() {
                        all_paths
                            .lock()
                            .unwrap()
                            .push(row["path"].as_str().unwrap().to_string());
                    }
                    if done {
                        break;
                    }
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let mut paths = all_paths.lock().unwrap().clone();
        paths.sort();
        paths.dedup();
        assert!(
            paths.iter().all(|path| rows.contains(path)),
            "paged paths must come from the materialized set only"
        );
        // 容量收敛：阶段 B 后表 ≤ cap 且留存会话全未过期。
        let cursors = mcp.cursors.lock().unwrap();
        assert!(
            cursors.entries.len() <= 4,
            "cap converges, got {}",
            cursors.entries.len()
        );
        let now = unix_millis_now() as u128;
        assert!(
            cursors
                .entries
                .iter()
                .all(|(_, session)| session.expires_at_millis > now),
            "no expired cursor residue"
        );
    }

    /// stdio dispatch 是 spawn 并发（每行一个 tokio task，注释钉于
    /// `run_mcp_stdio`）：16 个并发 dispatch 各带唯一 id，断言响应 id
    /// 一一对应、互不串扰（id 关联性在并发下成立）。
    #[test]
    fn stdio_concurrent_dispatch_keeps_id_correlation() {
        let (server, _dir) = stdio_server();
        let server = Arc::new(server);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut handles = Vec::new();
            for id in 0..16u64 {
                let server = Arc::clone(&server);
                handles.push(tokio::spawn(async move {
                    let request = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "method": "tools/list",
                        "params": {},
                    });
                    (id, server.dispatch(request).await)
                }));
            }
            for handle in handles {
                let (id, response) = handle.await.unwrap();
                let response = response.expect("tools/list always answers");
                assert_eq!(response["id"], json!(id), "response id must correlate");
                assert!(
                    response["result"]["tools"].as_array().unwrap().len() >= 12,
                    "{response}"
                );
            }
        });
    }
}
