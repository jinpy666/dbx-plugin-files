// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use serde_json::{json, Value};

use super::{
    numeric_arg_or, numeric_arg_u64, missing_required, params_hash, required_str,
    unix_millis_now, CELL_WIDTH_CEILING, CONFIRM_TTL_SECS_CEILING, CURSOR_PAGE,
    CURSOR_TTL_SECS_CEILING, DEFAULT_CELL_WIDTH, DEFAULT_CONFIRM_TTL_SECS,
    DEFAULT_CURSOR_TTL_SECS, DEFAULT_MAX_CURSOR_ROWS, DEFAULT_MAX_CURSOR_SESSIONS,
    DEFAULT_MAX_RESPONSE_BYTES, DEFAULT_REPORT_WAIT_MILLIS, GROUP_LIMIT,
    INTENT_MAX_ENTRIES, INTENT_TTL_MILLIS, MAX_CURSOR_PAGE, MAX_CURSOR_ROWS_CEILING,
    MAX_CURSOR_SESSIONS_CEILING, MAX_RESPONSE_BYTES_CEILING, REPORT_WAIT_MILLIS_CEILING,
    ROWS_FORMAT_LIMIT, SAMPLE_ROWS, TOP_N,
};

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
    pub(crate) fn from_json(value: &Value) -> Self {
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
pub(crate) struct IntentEntry {
    action: String,
    #[allow(dead_code)]
    params: Value,
    status: String, // pending | applied | rejected
    summary: Option<Value>,
    reason: Option<String>,
    expires_at_millis: u128,
}

/// Three-state result of an intent table lookup (ldap `LookupStatus` parity).
pub(crate) enum IntentLookup {
    Found(Value),
    Expired,
    Unknown,
}

pub(crate) struct CursorSession {
    pub(crate) rows: Vec<String>,
    pub(crate) offset: usize,
    pub(crate) expires_at_millis: u128,
}

/// A one-time two-phase confirm token (design §4): parameter-hash bound,
/// TTL'd, consumed on first use.
pub(crate) struct ConfirmEntry {
    pub(crate) hash: u64,
    pub(crate) expires_at_millis: u128,
}

/// LRU-bounded ordered map: the least-recently-touched entry is evicted past
/// `cap`. Tiny capacities (8 / 20) make the linear scan the honest choice.
pub(crate) struct LruTable<T> {
    pub(crate) entries: Vec<(String, T)>,
    pub(crate) cap: usize,
}

impl<T> LruTable<T> {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            entries: Vec::new(),
            cap,
        }
    }

    pub(crate) fn get(&mut self, key: &str) -> Option<&mut T> {
        let index = self.entries.iter().position(|(id, _)| id == key)?;
        let (id, entry) = self.entries.remove(index);
        self.entries.push((id, entry));
        self.entries.last_mut().map(|(_, entry)| entry)
    }

    pub(crate) fn put(&mut self, key: String, entry: T) {
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
    pub(crate) fn set_cap(&mut self, cap: usize) {
        self.cap = cap;
        while self.entries.len() > self.cap {
            self.entries.remove(0);
        }
    }

    pub(crate) fn remove(&mut self, key: &str) -> Option<T> {
        let index = self.entries.iter().position(|(id, _)| id == key)?;
        Some(self.entries.remove(index).1)
    }

    pub(crate) fn retain<F>(&mut self, mut keep: F)
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
    pub(crate) snapshot: Mutex<Option<Value>>,
    pub(crate) cursors: Mutex<LruTable<CursorSession>>,
    pub(crate) confirms: Mutex<HashMap<String, ConfirmEntry>>,
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

    pub(crate) fn current_settings(&self) -> McpSettings {
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
    pub(crate) fn register_intent(&self, intent_id: &str, action: &str, params: Value, now: u128) {
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
    pub(crate) fn intent_lookup(&self, intent_id: &str, now: u128) -> IntentLookup {
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

    pub(crate) fn lock_intents(&self) -> std::sync::MutexGuard<'_, LruTable<IntentEntry>> {
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
    pub(crate) fn cursor_put(&self, rows: Vec<String>) -> (String, bool) {
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
    pub(crate) fn cursor_next(&self, arguments: &Value) -> Result<Value, String> {
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

    pub(crate) fn lock_cursors(&self) -> std::sync::MutexGuard<'_, LruTable<CursorSession>> {
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
    pub(crate) const MAX_CONFIRM_ENTRIES: usize = 1024;

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
    pub(crate) fn confirm_begin(&self, arguments: &Value) -> (String, u128) {
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
    pub(crate) fn confirm_verify(&self, token: &str, arguments: &Value) -> Result<(), String> {
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
