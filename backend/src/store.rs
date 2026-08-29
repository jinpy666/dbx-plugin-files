//! Local data + audit persistence (M0 common doc §4).
//!
//! Layout under `DBX_PLUGIN_DATA_DIR` (fallback `$TMPDIR/dbx-plugin-data/io.dbx.files`):
//! - `prefs.json`      — UI preferences, non-sensitive, whole-object rewrite;
//! - `transfers.json`  — finished transfer history, ring-capped at
//!   `model::TRANSFER_HISTORY_LIMIT` (200);
//! - `audit.jsonl`     — append-only write-op audit log, one JSON object per
//!   line: `{"time":"RFC3339","connectionId","action","target","result"}`.
//!
//! Credentials red line: nothing in this module ever stores secret values;
//! `target` carries paths/URIs only.

#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::TRANSFER_HISTORY_LIMIT;

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

    /// Default directory from `DBX_PLUGIN_DATA_DIR` (ssh-sftp main.rs:744
    /// semantics), falling back to the OS temp dir.
    pub fn default_dir() -> PathBuf {
        std::env::var_os("DBX_PLUGIN_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::temp_dir()
                    .join("dbx-plugin-data")
                    .join("io.dbx.files")
            })
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
