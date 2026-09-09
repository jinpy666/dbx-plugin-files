//! Local data + audit persistence (M0 common doc §4).
//!
//! Data dir resolution (first available wins; a variable counts only when
//! present and non-blank after trim — see `resolve_data_dir`):
//! 1. `DBX_PLUGIN_DATA_DIR` (host-injected, as-is);
//! 2. `<DBX_DATA_DIR>/plugin-data/io.dbx.files` (portable/web host root);
//! 3. platform user data dir + `dbx-plugin-data/io.dbx.files`
//!    (macOS `~/Library/Application Support`,
//!    unix `${XDG_DATA_HOME:-~/.local/share}`, Windows `%APPDATA%`);
//! 4. `$TMPDIR/dbx-plugin-data/io.dbx.files` — never-failing last resort only
//!    (temp dirs are wiped on reboot and once lost this store's data).
//! - `prefs.json`      — UI preferences, non-sensitive, whole-object rewrite;
//! - `transfers.json`  — finished transfer history, ring-capped at
//!   `model::TRANSFER_HISTORY_LIMIT` (200);
//! - `audit.jsonl`     — append-only write-op audit log, one JSON object per
//!   line: `{"time":"RFC3339","connectionId","action","target","result"}`.
//!
//! Credentials red line: nothing in this module ever stores secret values;
//! `target` carries paths/URIs only.

#![allow(dead_code)]

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::TRANSFER_HISTORY_LIMIT;

/// Plugin id used at every level of the data-dir layout below.
const PLUGIN_ID: &str = "io.dbx.files";

/// A persisted finished-transfer record. Deliberately slim: runtime handles
/// (`JobHandle`) never reach the disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferRecord {
    pub task_id: String,
    pub connection_id: String,
    /// `"upload" | "download"`.
    pub kind: String,
    pub remote_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    pub transferred_bytes: u64,
    /// `completed | failed | canceled` (finished states only).
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
}

/// One audit log line (JSONL).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AuditRecord {
    /// RFC3339 UTC timestamp with second precision.
    pub time: String,
    pub connection_id: String,
    /// e.g. `files/write`, `files/purge`.
    pub action: String,
    /// Path/DN-like target only — never a value or credential.
    pub target: String,
    /// `ok | denied | error`.
    pub result: String,
}

/// File-backed store. All methods are blocking + best-effort: a failed write
/// is an error to the caller, a failed read of a missing file is a default.
pub struct Store {
    data_dir: PathBuf,
}

impl Store {
    pub fn new(data_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&data_dir);
        Self { data_dir }
    }

    /// Default data directory, resolved by `resolve_data_dir` against the
    /// real environment: `DBX_PLUGIN_DATA_DIR`, else
    /// `<DBX_DATA_DIR>/plugin-data/io.dbx.files`, else the platform user
    /// data dir under `dbx-plugin-data/io.dbx.files`, else the OS temp dir
    /// as a never-failing last resort.
    pub fn default_dir() -> PathBuf {
        resolve_data_dir(|key| std::env::var_os(key))
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    // -- prefs.json ---------------------------------------------------------

    /// Loads `prefs.json`; returns `{}` when missing or unparsable.
    pub fn load_prefs(&self) -> Value {
        self.read_json("prefs.json")
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()))
    }

    /// Rewrites `prefs.json` atomically (tmp file + rename).
    pub fn save_prefs(&self, prefs: &Value) -> Result<(), String> {
        self.write_json_atomic("prefs.json", prefs)
    }

    // -- transfers.json -----------------------------------------------------

    /// Loads finished transfer history (newest last). Missing file → empty.
    pub fn load_transfers(&self) -> Vec<TransferRecord> {
        self.read_json("transfers.json")
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }

    /// Appends a finished record and ring-truncates to the newest
    /// `TRANSFER_HISTORY_LIMIT` entries.
    pub fn record_transfer(&self, record: TransferRecord) -> Result<(), String> {
        let mut records = self.load_transfers();
        records.push(record);
        if records.len() > TRANSFER_HISTORY_LIMIT {
            let drop = records.len() - TRANSFER_HISTORY_LIMIT;
            records.drain(..drop);
        }
        self.write_json_atomic("transfers.json", &serde_json::to_value(records).map_err(|error| error.to_string())?)
    }

    /// Removes finished-transfer history (optionally scoped to one
    /// connection) and returns how many records were dropped.
    /// P-FILES ⑥: backing store of `files/transfers/clear`.
    pub fn clear_transfers(&self, connection_id: Option<&str>) -> Result<usize, String> {
        let records = self.load_transfers();
        // None → 清空全部；Some(id) → 只清该连接的记录。
        let kept: Vec<TransferRecord> = records
            .iter()
            .filter(|record| match connection_id {
                Some(id) => record.connection_id != id,
                None => false,
            })
            .cloned()
            .collect();
        let removed = records.len() - kept.len();
        self.write_json_atomic("transfers.json", &serde_json::to_value(kept).map_err(|error| error.to_string())?)
            .map(|_| removed)
    }

    // -- audit.jsonl --------------------------------------------------------

    /// Appends one audit line. Never rewrites the file.
    pub fn append_audit(&self, record: AuditRecord) -> Result<(), String> {
        let path = self.data_dir.join("audit.jsonl");
        let mut line = serde_json::to_string(&record).map_err(|error| error.to_string())?;
        line.push('\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| format!("Failed to open audit log: {error}"))?;
        file.write_all(line.as_bytes())
            .map_err(|error| format!("Failed to append audit log: {error}"))
    }

    /// Reads the whole audit trail (oldest first); used by the audit panel.
    pub fn read_audit(&self) -> Vec<AuditRecord> {
        let Ok(content) = std::fs::read_to_string(self.data_dir.join("audit.jsonl")) else {
            return Vec::new();
        };
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    // -- internals ----------------------------------------------------------

    fn read_json(&self, file_name: &str) -> Option<Value> {
        let content = std::fs::read_to_string(self.data_dir.join(file_name)).ok()?;
        serde_json::from_str(&content).ok()
    }

    fn write_json_atomic(&self, file_name: &str, value: &Value) -> Result<(), String> {
        let path = self.data_dir.join(file_name);
        let tmp = self.data_dir.join(format!("{file_name}.tmp"));
        let body = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
        std::fs::write(&tmp, body).map_err(|error| format!("Failed to write {file_name}: {error}"))?;
        std::fs::rename(&tmp, &path).map_err(|error| format!("Failed to finalize {file_name}: {error}"))
    }
}

/// Resolves the plugin data dir purely from `lookup`, in priority order
/// (first available wins; "available" = present and non-blank after trim):
/// 1. `DBX_PLUGIN_DATA_DIR` — host-injected per-plugin dir, used as-is;
/// 2. `DBX_DATA_DIR` — host data root (portable/web mode, inherited by child
///    processes), under `plugin-data/` (deliberately not `plugins/`, which is
///    the installer registration tree);
/// 3. platform-standard user data dir + `dbx-plugin-data/io.dbx.files`
///    (macOS `~/Library/Application Support`,
///    unix `${XDG_DATA_HOME:-~/.local/share}`, Windows `%APPDATA%`);
/// 4. `std::env::temp_dir()/dbx-plugin-data/io.dbx.files` — last resort;
///    this function never fails. Kept only as a stopgap: `$TMPDIR` is wiped
///    on reboot and prefs/transfers/audit data with it.
fn resolve_data_dir(lookup: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    let non_blank = |value: Option<OsString>| {
        value.filter(|value| !value.to_string_lossy().trim().is_empty())
    };
    if let Some(dir) = non_blank(lookup("DBX_PLUGIN_DATA_DIR")) {
        return PathBuf::from(dir);
    }
    if let Some(root) = non_blank(lookup("DBX_DATA_DIR")) {
        return PathBuf::from(root).join("plugin-data").join(PLUGIN_ID);
    }
    if let Some(base) = platform_user_data_dir(&lookup) {
        return base.join("dbx-plugin-data").join(PLUGIN_ID);
    }
    std::env::temp_dir().join("dbx-plugin-data").join(PLUGIN_ID)
}

/// Platform-standard user data base dir (step 3 of `resolve_data_dir`).
/// `cfg!` run-time booleans keep every branch compiled in a single binary so
/// the active branch stays unit-testable on the build host.
fn platform_user_data_dir(lookup: &impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        lookup("HOME")
            .map(PathBuf::from)
            .map(|home| home.join("Library/Application Support"))
    } else if cfg!(windows) {
        lookup("APPDATA").map(PathBuf::from)
    } else {
        lookup("XDG_DATA_HOME")
            .filter(|value| !value.to_string_lossy().trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                lookup("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".local/share"))
            })
    }
}

/// Formats a unix-epoch-milliseconds timestamp as RFC3339 UTC
/// (`1970-01-01T00:00:00Z` style), no external time crate. Inverse-free by
/// design; audit consumers only parse standard RFC3339.
pub fn format_rfc3339(unix_millis: i64) -> String {
    let secs = unix_millis.div_euclid(1000);
    let millis = unix_millis.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let seconds_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant's `civil_from_days` algorithm (days since 1970-01-01 →
/// proleptic Gregorian y/m/d).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Current unix epoch milliseconds.
pub fn unix_millis_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::AuditRecord;
    use std::ffi::OsString;

    // -- data dir resolution (pure, env-free) -------------------------------

    /// Drives `resolve_data_dir` with a fixed lookup table — no `set_var`,
    /// no parallel-test races.
    fn resolve_with(table: &[(&str, &str)]) -> PathBuf {
        resolve_data_dir(|name| {
            table
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(*value))
        })
    }

    /// ① `DBX_PLUGIN_DATA_DIR` wins even when `DBX_DATA_DIR`/`HOME` exist.
    #[test]
    fn plugin_data_dir_wins_over_everything() {
        assert_eq!(
            resolve_with(&[
                ("DBX_PLUGIN_DATA_DIR", "/custom/plugin"),
                ("DBX_DATA_DIR", "/host/data"),
                ("HOME", "/Users/x"),
            ]),
            PathBuf::from("/custom/plugin")
        );
    }

    /// ② Blank/whitespace-only values count as unset.
    #[test]
    fn empty_values_are_treated_as_unset() {
        assert_eq!(
            resolve_with(&[
                ("DBX_PLUGIN_DATA_DIR", "  "),
                ("DBX_DATA_DIR", "/host/data"),
            ]),
            PathBuf::from("/host/data/plugin-data/io.dbx.files")
        );
    }

    /// ③ `DBX_DATA_DIR` maps to `<root>/plugin-data/io.dbx.files` (never
    /// `plugins/` — that is the installer registration tree).
    #[test]
    fn dbx_data_dir_maps_under_plugin_data() {
        assert_eq!(
            resolve_with(&[("DBX_DATA_DIR", "/host/data"), ("HOME", "/Users/x")]),
            PathBuf::from("/host/data/plugin-data/io.dbx.files")
        );
    }

    /// ④ Platform user-data branch, compiled/tested per platform.
    #[cfg(target_os = "macos")]
    #[test]
    fn platform_user_data_dir_macos_home() {
        assert_eq!(
            resolve_with(&[("HOME", "/Users/x")]),
            PathBuf::from("/Users/x/Library/Application Support/dbx-plugin-data/io.dbx.files")
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn platform_user_data_dir_unix_xdg_then_home() {
        assert_eq!(
            resolve_with(&[("XDG_DATA_HOME", "/xdg"), ("HOME", "/home/x")]),
            PathBuf::from("/xdg/dbx-plugin-data/io.dbx.files"),
            "XDG_DATA_HOME wins"
        );
        assert_eq!(
            resolve_with(&[("HOME", "/home/x")]),
            PathBuf::from("/home/x/.local/share/dbx-plugin-data/io.dbx.files")
        );
    }

    #[cfg(windows)]
    #[test]
    fn platform_user_data_dir_windows_appdata() {
        assert_eq!(
            resolve_with(&[("APPDATA", r"C:\Users\x\AppData\Roaming")]),
            PathBuf::from(r"C:\Users\x\AppData\Roaming")
                .join("dbx-plugin-data")
                .join("io.dbx.files")
        );
    }

    /// ⑤ Nothing set anywhere → temp-dir last resort (function never fails).
    #[test]
    fn nothing_set_falls_back_to_temp_dir() {
        assert_eq!(
            resolve_with(&[]),
            std::env::temp_dir()
                .join("dbx-plugin-data")
                .join("io.dbx.files")
        );
    }

    fn store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Store::new(dir.path().to_path_buf()), dir)
    }

    #[test]
    fn prefs_roundtrip_and_defaults() {
        let (store, _dir) = store();
        assert_eq!(store.load_prefs(), serde_json::json!({}), "missing prefs default to empty object");
        store
            .save_prefs(&serde_json::json!({ "pageSize": 100 }))
            .unwrap();
        assert_eq!(store.load_prefs()["pageSize"], 100);
    }

    #[test]
    fn transfer_history_rings_at_limit() {
        let (store, _dir) = store();
        for index in 0..(TRANSFER_HISTORY_LIMIT + 25) {
            store
                .record_transfer(TransferRecord {
                    task_id: format!("t{index}"),
                    connection_id: "c1".into(),
                    kind: "upload".into(),
                    remote_path: format!("/f{index}"),
                    total_bytes: Some(1),
                    transferred_bytes: 1,
                    status: "completed".into(),
                    error: None,
                    started_at: None,
                    finished_at: None,
                })
                .unwrap();
        }
        let records = store.load_transfers();
        assert_eq!(records.len(), TRANSFER_HISTORY_LIMIT);
        assert_eq!(records.last().unwrap().task_id, "t224", "newest kept");
        assert_eq!(records.first().unwrap().task_id, "t25", "oldest dropped");
    }

    /// P-FILES ⑥: `clear_transfers` — None wipes everything, Some(id) is
    /// scoped to one connection.
    #[test]
    fn transfer_history_clear_scopes_by_connection() {
        let (store, _dir) = store();
        for (task_id, connection) in [("a1", "c1"), ("b1", "c2"), ("a2", "c1")] {
            store
                .record_transfer(TransferRecord {
                    task_id: task_id.into(),
                    connection_id: connection.into(),
                    kind: "upload".into(),
                    remote_path: format!("/{task_id}"),
                    total_bytes: Some(1),
                    transferred_bytes: 1,
                    status: "completed".into(),
                    error: None,
                    started_at: None,
                    finished_at: None,
                })
                .unwrap();
        }
        let removed = store.clear_transfers(Some("c1")).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(
            store
                .load_transfers()
                .iter()
                .map(|record| record.task_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b1"],
            "other connection's history kept"
        );
        assert_eq!(store.clear_transfers(None).unwrap(), 1);
        assert!(store.load_transfers().is_empty());
        assert_eq!(store.clear_transfers(None).unwrap(), 0, "idempotent");
    }

    #[test]
    fn audit_appends_and_reads_back() {
        let (store, _dir) = store();
        store
            .append_audit(AuditRecord {
                time: format_rfc3339(1_700_000_000_000),
                connection_id: "c1".into(),
                action: "files/purge".into(),
                target: "/data/old".into(),
                result: "ok".into(),
            })
            .unwrap();
        store
            .append_audit(AuditRecord {
                time: format_rfc3339(1_700_000_001_000),
                connection_id: "c1".into(),
                action: "files/write".into(),
                target: "/data/new.txt".into(),
                result: "denied".into(),
            })
            .unwrap();
        let trail = store.read_audit();
        assert_eq!(trail.len(), 2);
        assert_eq!(trail[1].result, "denied");
        assert!(trail[0].time.ends_with('Z'));
    }

    #[test]
    fn rfc3339_formatter_matches_known_timestamps() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_rfc3339(1_700_000_000_000), "2023-11-14T22:13:20.000Z");
        // Leap-year day: 2024-02-29T12:00:00Z == 1709208000
        assert_eq!(format_rfc3339(1_709_208_000_000), "2024-02-29T12:00:00.000Z");
        // Pre-epoch handling must not panic (negative days).
        assert_eq!(format_rfc3339(-86_400_000), "1969-12-31T00:00:00.000Z");
    }

    #[test]
    fn audit_records_never_carry_secret_keys() {
        let (store, _dir) = store();
        let record = AuditRecord {
            time: format_rfc3339(unix_millis_now() as i64),
            connection_id: "c1".into(),
            action: "files/write".into(),
            target: "/data/file.bin".into(),
            result: "ok".into(),
        };
        store.append_audit(record.clone()).unwrap();
        let raw = std::fs::read_to_string(store.data_dir().join("audit.jsonl")).unwrap();
        assert!(!raw.to_ascii_lowercase().contains("password"));
        assert_eq!(
            serde_json::to_string(&record).unwrap(),
            serde_json::to_string(&store.read_audit()[0]).unwrap()
        );
    }
}
