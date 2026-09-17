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

mod truncate;

use truncate::{cap_response, content_envelope};

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
// files_sync: directory sync over MCP (§8.4)
// ---------------------------------------------------------------------------

/// stdio refusal for `files_sync` (files_rename dir-path parity): the dir
/// job's progress events need the host event channel, which standalone stdio
/// does not have.
const SYNC_STDIO_UNAVAILABLE: &str = "files_sync runs as an async progress job over the DBX event \
     channel, which is unavailable in standalone stdio mode; call it through the DBX MCP bridge \
     (dbx_call_plugin_tool) or use the DBX workbench transfers pane";

/// Everything `files_sync` validates and resolves BEFORE the async job
/// machinery is touched. Built by [`parse_sync_request`] (pure + unit-tested);
/// the `files_sync` arm only feeds it to [`JobTable::enqueue_dir_job`], which
/// re-runs the target gates as defense in depth.
#[derive(Debug)]
struct SyncRequest {
    source: StoredConnection,
    source_path: String,
    target: StoredConnection,
    target_path: String,
    sync: bool,
    dry_run: bool,
    max_delete: Option<u64>,
}

/// Validates a `files_sync` call end to end at the synchronous layer:
/// missing-parameter enumeration (schema `required` order), LLM-tolerant flag
/// parsing, target gates, and path-shape hard gates.
///
/// Gate semantics mirror the workbench `files/syncDir` path
/// (`transfers::validate_dir_job_gates`): the TARGET must be writable, and
/// `sync=true` additionally requires `allow_delete` — a dry run included,
/// because its plan still proposes deletions (enqueue re-checks the same).
/// The SOURCE connection may be read-only: copying FROM a read-only source is
/// a legitimate topology (nothing is ever written to the source).
fn parse_sync_request(engine: &Engine, arguments: &Value) -> Result<SyncRequest, String> {
    missing_required(
        arguments,
        &["sourceConnectionId", "sourcePath", "targetConnectionId", "targetPath"],
    )?;
    let source = engine.connection(required_str(arguments, "sourceConnectionId")?)?;
    let target = engine.connection(required_str(arguments, "targetConnectionId")?)?;
    let sync = bool_arg_or(arguments, "sync", false)?;
    let dry_run = bool_arg_or(arguments, "dryRun", false)?;
    let max_delete = numeric_arg_u64(arguments, "maxDelete")?;
    // Gates before path validation — caller-side parameter errors (wrong
    // flag types) and permission walls surface before spelling nitpicks,
    // same ordering as files_delete/files_rename.
    ensure_writable(&target)?;
    if sync {
        ensure_deletable(&target)?;
    }
    // '/' is legitimate on both sides (whole-tree sync, rclone style);
    // only the shape gates apply.
    let source_path = normalize_slashes(required_str(arguments, "sourcePath")?);
    let target_path = normalize_slashes(required_str(arguments, "targetPath")?);
    validate_path_shape(&source_path, "sourcePath")?;
    validate_path_shape(&target_path, "targetPath")?;
    Ok(SyncRequest {
        source,
        source_path,
        target,
        target_path,
        sync,
        dry_run,
        max_delete,
    })
}

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

/// Tools whose execution resolves a single generic `connectionId`/inline
/// `connection` parameter — the stdio surfaces relax exactly that pair into
/// an anyOf. `files_cursor_next` is a pure session lookup and runs without a
/// connection, and `files_sync` addresses explicit `sourceConnectionId` /
/// `targetConnectionId` parameters (and cannot run in stdio anyway — no
/// event channel), so neither takes the generic parameter.
fn needs_connection_id(tool: &str) -> bool {
    !matches!(tool, "files_cursor_next" | "files_sync")
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

mod appbridge;


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

#[cfg(test)]
mod tests;
