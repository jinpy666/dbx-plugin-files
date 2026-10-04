//! Scheduler wire/persistence models (`files/schedule/*`, schedules.json,
//! runs.json). CamelCase on the wire, matching every other `files/*` family.
//!
//! Secret red line: tasks carry connection ids and paths only — credentials
//! stay in the host-managed, in-memory connection registry.

use serde::{Deserialize, Serialize};

use crate::model::{BisyncStartRequest, DirJobRequest};

/// Persisted run-history ceiling for `runs.json` (ring, newest last).
pub const SCHEDULE_RUN_HISTORY_LIMIT: usize = 500;

/// Execution family of a scheduled task. Phase 1 ships the three rclone
/// kinds; `restic` snapshot tasks join later as a fourth variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleKind {
    /// `sync/sync` mirror — destination deletions travel (gated).
    Sync,
    /// `sync/copy` — additive copy, destination extras kept.
    Copy,
    /// `sync/bisync` — bidirectional; first ever run is a resync.
    Bisync,
}

/// Task options — the rclone dir-job parameter set (same wire names and
/// defaults as `DirJobRequest`) plus the two scheduler post-run hooks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScheduleOptions {
    pub dry_run: Option<bool>,
    pub max_delete: Option<u64>,
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    /// Versioned-backup directory (destination-root-relative): overwritten
    /// (copy/sync) and deleted (sync) files move here preserving hierarchy.
    pub backup_dir: Option<String>,
    /// Suffix appended to backed-up file names (pairs with `backup_dir`).
    pub suffix: Option<String>,
    pub metadata: Option<bool>,
    pub update: Option<bool>,
    pub existing: Option<bool>,
    pub immutable: Option<bool>,
    pub min_size: Option<String>,
    pub max_size: Option<String>,
    pub min_age: Option<String>,
    pub max_age: Option<String>,
    pub transfers: Option<u32>,
    pub checkers: Option<u32>,
    pub retries: Option<u32>,
    /// Post-run hook: verify the run with an `operations/check` job over the
    /// same source/target pair. A failed verification downgrades the run to
    /// success-with-warning (the transfer itself did land).
    pub verify_after: bool,
    /// Post-run hook: prune files in `backup_dir` older than this many days
    /// (`operations/delete` + `MinAge` filter). Requires `backup_dir`.
    pub retention_days: Option<u32>,
}

/// One persisted schedule task (`schedules.json`). `next_run_at` is unix
/// epoch millis (store convention) recomputed on every fire and on sidecar
/// start — a shutdown period shifts the schedule, it never back-fills.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleTask {
    pub id: String,
    pub name: String,
    pub kind: ScheduleKind,
    pub source_connection_id: String,
    pub source_path: String,
    /// Usually equals the source id (same-connection backup); cross-connection
    /// pairs must share a proxy group (enforced by the dir-job starter).
    pub target_connection_id: String,
    pub target_path: String,
    /// Five-field cron expression, validated at create/update.
    pub cron: String,
    pub enabled: bool,
    #[serde(default)]
    pub options: ScheduleOptions,
    /// Bisync pairs must resync once; the first successful run stamps this
    /// and later runs go out in plain `run` mode.
    #[serde(default)]
    pub bisync_resync_done: bool,
    pub created_at: u64,
    #[serde(default)]
    pub last_run_at: Option<u64>,
    /// `success | failed | canceled | skipped` (mirrors RunRecord::status).
    #[serde(default)]
    pub last_run_status: Option<String>,
    #[serde(default)]
    pub next_run_at: Option<u64>,
}

impl ScheduleTask {
    /// The equivalent dir-job request for Sync/Copy kinds (same wire shape
    /// the workbench `files/syncDir`|`files/copyDir` arms receive).
    pub fn to_dir_job_request(&self) -> DirJobRequest {
        let options = &self.options;
        DirJobRequest {
            source_connection_id: self.source_connection_id.clone(),
            source_path: self.source_path.clone(),
            target_connection_id: self.target_connection_id.clone(),
            target_path: self.target_path.clone(),
            dry_run: options.dry_run,
            max_delete: options.max_delete,
            include: options.include.clone(),
            exclude: options.exclude.clone(),
            backup_dir: options.backup_dir.clone(),
            suffix: options.suffix.clone(),
            metadata: options.metadata,
            update: options.update,
            existing: options.existing,
            immutable: options.immutable,
            min_size: options.min_size.clone(),
            max_size: options.max_size.clone(),
            min_age: options.min_age.clone(),
            max_age: options.max_age.clone(),
            transfers: options.transfers,
            checkers: options.checkers,
            retries: options.retries,
        }
    }

    /// The bisync request for this pair; `resync` is chosen by the run path
    /// (first-ever run), not baked in here.
    pub fn to_bisync_request(&self, resync: bool) -> BisyncStartRequest {
        BisyncStartRequest {
            source_connection_id: self.source_connection_id.clone(),
            source_path: self.source_path.clone(),
            target_connection_id: self.target_connection_id.clone(),
            target_path: self.target_path.clone(),
            mode: Some(if resync { "resync" } else { "run" }.to_string()),
            resync_mode: None,
            dry_run: self.options.dry_run,
        }
    }
}

/// One run of one task (`runs.json`, ring-capped). The record is written on
/// claim (status `running`) and finalized in place by run_id on the terminal
/// event. `skipped` = the scheduler fired but the connection was unavailable
/// (post-restart) — distinct from a failed run that reached rclone.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    pub run_id: String,
    pub task_id: String,
    /// Transfers-panel job id while/after the run (absent for skipped runs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// `"schedule" | "manual"`.
    pub trigger: String,
    /// `running | success | failed | canceled | skipped`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<u64>,
    /// Failure message, or a `verify:`/`retention:` warning note on an
    /// otherwise successful run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// -- wire requests ----------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleCreateRequest {
    pub name: String,
    pub kind: ScheduleKind,
    pub source_connection_id: String,
    pub source_path: String,
    /// Absent = same as source (the common same-connection backup).
    #[serde(default)]
    pub target_connection_id: Option<String>,
    pub target_path: String,
    pub cron: String,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub options: ScheduleOptions,
}

/// Full-object replace: the UI edits a task it already holds, so every
/// field travels again. Runtime bookkeeping (`created_at`, `last_run_*`,
/// `next_run_at`, `bisync_resync_done`) survives the replace.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleUpdateRequest {
    pub id: String,
    pub name: String,
    pub kind: ScheduleKind,
    pub source_connection_id: String,
    pub source_path: String,
    #[serde(default)]
    pub target_connection_id: Option<String>,
    pub target_path: String,
    pub cron: String,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub options: ScheduleOptions,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleTaskRequest {
    pub id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleHistoryRequest {
    /// Absent = all tasks' history.
    #[serde(default)]
    pub id: Option<String>,
    /// Newest-first result cap (default 100, max 500).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> ScheduleTask {
        ScheduleTask {
            id: "t1".into(),
            name: "nightly".into(),
            kind: ScheduleKind::Sync,
            source_connection_id: "c1".into(),
            source_path: "/data".into(),
            target_connection_id: "c1".into(),
            target_path: "/mirror".into(),
            cron: "0 3 * * *".into(),
            enabled: true,
            options: ScheduleOptions {
                backup_dir: Some("_backups".into()),
                verify_after: true,
                ..Default::default()
            },
            bisync_resync_done: false,
            created_at: 1_000,
            last_run_at: None,
            last_run_status: None,
            next_run_at: None,
        }
    }

    #[test]
    fn dir_job_request_carries_options_with_dir_job_defaults() {
        let request = task().to_dir_job_request();
        assert_eq!(request.source_path, "/data");
        assert_eq!(request.target_path, "/mirror");
        assert_eq!(request.backup_dir.as_deref(), Some("_backups"));
        assert_eq!(request.dry_run, None, "absent option stays absent (rclone default)");
        assert_eq!(request.transfers, None);
    }

    #[test]
    fn bisync_request_resync_flag_comes_from_the_caller() {
        let task = task();
        assert_eq!(task.to_bisync_request(true).mode.as_deref(), Some("resync"));
        assert_eq!(task.to_bisync_request(false).mode.as_deref(), Some("run"));
    }

    #[test]
    fn task_json_roundtrip_keeps_camel_case_and_defaults() {
        let value = serde_json::to_value(task()).unwrap();
        assert_eq!(value["sourceConnectionId"], "c1");
        assert_eq!(value["bisyncResyncDone"], false);
        let back: ScheduleTask = serde_json::from_value(value).unwrap();
        assert_eq!(back, task());
        // A legacy file without the newer optional fields deserializes.
        let legacy = serde_json::json!({
            "id": "t2", "name": "n", "kind": "copy",
            "sourceConnectionId": "c", "sourcePath": "/a",
            "targetConnectionId": "c", "targetPath": "/b",
            "cron": "* * * * *", "enabled": true, "createdAt": 5
        });
        let parsed: ScheduleTask = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed.kind, ScheduleKind::Copy);
        assert!(!parsed.options.verify_after);
    }
}
