//! Scheduler Task Provider (`io.dbx.files.tasks`) — the generic Scheduler's
//! frozen entry into this plugin (ADR `scheduler-task-contract.md` §6).
//!
//! Contract surface (fixed RPC only, no arbitrary `task/<custom>`):
//! - `task/validate` → `{ valid, errors[], warnings[] }` — connection /
//!   parameter / environment checks; never touches secret material.
//! - `task/execute`  → `{ success, exitCode?, message?, artifacts[] }` —
//!   one blocking run. Sync/copy jobs reuse the workbench's native rclone
//!   engine verbatim ([`crate::rclone_start_dir_job`], same job mirror,
//!   same `files/transfer/*` surface) — never a filesystem
//!   list→read→write loop (ADR §13 A7 red line).
//! - `task/stop`     → cancel a live run. The handler flips a cancel flag
//!   in the run registry and routes the underlying rclone job through the
//!   shared `files/transfer/cancel` arm, so cancellation is REAL (the rc
//!   job is stopped remotely, terminal-once).
//! - `task/status`   → introspection for a live run (defensive; the
//!   manifest declares no `resident` capability, so the host gates this
//!   off for resident semantics — we still answer for run-mode runs).
//! - `task/start`    → rejected: resident mode is not declared.
//!
//! Events (plugin → host, existing event channel, ADR §6.3 shapes):
//! - `task/log`      seq/stream/level/message per run (monotonic seq from 1).
//! - `task/progress` percent/current(bytes)/total(bytes)/completed(files).
//! - `task/artifact` metadata only (name/uri/contentType/size/checksum) —
//!   file BODIES never travel events or SQLite (ADR §10 red line).
//!
//! Verdict mapping (ADR §2.6 alignment): engine success →
//! `success: true, exitCode 0`; engine failure → `success: false,
//! exitCode 1` with a quantified message (how many bytes/files landed
//! before the failure); invalid config → `success: false, exitCode 2`
//! (non-retryable); cancel → `success: false`, no exitCode (the host
//! marks the TaskRun `cancelled`). The host owns the TaskRun state — the
//! provider only reports.
//!
//! Resume semantics: `resume = true` (default) relies on rclone's native
//! size/hash delta — files already present at the destination with
//! matching size (or checksum when enabled) are skipped, so an interrupted
//! run continues where it stopped; `resume = false` maps to
//! `--ignore-times` (full re-transfer). `overwrite = false` maps to
//! `--ignore-existing`. The engine-level behavior is pinned by live rcd
//! tests in `rclone::sync` (second copy skips identical / ignore-times
//! resends / ignore-existing never replaces).
//!
//! Secret red line: configs and logs carry connection ids and paths only;
//! every outbound log/artifact/message line passes [`redact`] as defense
//! in depth.

// Wire-model module convention (model.rs/store.rs/archive.rs parity): the
// request structs carry the FULL frozen contract shape (ADR §6.2) even where
// a field is only deserialized for fidelity and never read back.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use dbx_plugin_sdk::PluginEmitter;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::model::DirJobRequest;
use crate::rclone;

/// The provider id (ADR §2.2 namespace; never a bare "files").
pub const PROVIDER_ID: &str = "io.dbx.files.tasks";

/// Trigger ids supported by this provider (manifest `triggers[].id`).
pub const TRIGGER_SYNC: &str = "sync";
pub const TRIGGER_COPY: &str = "copy";

/// The top-level include filter behind `recursive = false`. rclone filter
/// semantics are basename-first (a pattern without a leading `/` matches the
/// file name at any level), so the root-anchored `/*` — not bare `*` — is
/// what restricts a job to direct children (live-verified v1.75.1).
pub const NON_RECURSIVE_INCLUDE: &str = "/*";

/// Poll cadence while watching a started dir job (scheduler parity).
pub(crate) const RUN_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Hard ceiling for one `task/execute` (scheduler `MAX_RUN_DURATION`
/// parity): a job that never terminates must not pin the execute request
/// forever. The underlying transfer stays visible/cancellable in the
/// transfers panel.
pub(crate) const MAX_RUN_DURATION: Duration = Duration::from_secs(24 * 60 * 60);

// ---------------------------------------------------------------------------
// Wire models (ADR §6.2 request/response shapes)
// ---------------------------------------------------------------------------

/// `task/validate` request: `{ task: {...}, connection, runtime }`.
/// `connection` / `runtime` are host-injected and unused here (secrets only
/// ever travel in the backend lifecycle request's controlled position).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidateRequest {
    pub task: ValidateTaskHead,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidateTaskHead {
    #[serde(default)]
    pub provider_id: Option<String>,
    pub trigger_id: String,
    #[serde(default)]
    pub connection_id: Option<String>,
    #[serde(default)]
    pub config_version: Option<u64>,
    #[serde(default)]
    pub config: Option<Value>,
}

/// `task/execute` request: `{ task: {taskId, runId, triggerId,
/// connectionId, configVersion, config}, run: {runId, attempt}, ... }`.
/// The `run` envelope is preferred for the run id; the task-head field is
/// the fallback (both carry the same value per ADR).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecuteRequest {
    pub task: ExecuteTaskHead,
    #[serde(default)]
    pub run: Option<RunHead>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecuteTaskHead {
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    pub trigger_id: String,
    #[serde(default)]
    pub connection_id: Option<String>,
    #[serde(default)]
    pub config_version: Option<u64>,
    #[serde(default)]
    pub config: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunHead {
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub attempt: Option<u32>,
}

/// `task/stop` request: `{ taskId, runId, sessionId, reason }`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopRequest {
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// `task/status` request: `{ taskId, runId, sessionId }`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusRequest {
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Trigger config (the task form's `binding: "config"` payload)
// ---------------------------------------------------------------------------

/// All knobs default to the safe engine semantics; unknown keys are ignored
/// (forward-compatible config evolution per `configVersion`).
#[derive(Debug, Clone, PartialEq)]
pub struct TriggerConfig {
    /// Copy trigger only: `file` (default) or `directory`.
    pub mode: CopyMode,
    /// `None` = the connection the task is bound to.
    pub source_connection_id: Option<String>,
    pub source_path: String,
    /// `None` = the source connection.
    pub destination_connection_id: Option<String>,
    pub destination_path: String,
    /// `false` → top-level files only (engine-native `["*"]` include).
    pub recursive: bool,
    /// Sync only: `true` (default) → mirror delete phase (rclone sync);
    /// `false` → additive copy (rclone copy semantics on the same trigger).
    pub delete_extra: bool,
    /// `true` (default) → replace differing destination files; `false` →
    /// never touch existing destination files (rclone --ignore-existing).
    pub overwrite: bool,
    /// Compare by hash instead of size+modtime (rclone --checksum).
    pub checksum: bool,
    /// `true` (default) → size/hash delta resume; `false` → re-transfer
    /// everything (rclone --ignore-times).
    pub resume: bool,
    /// Parallel file transfers (rclone --transfers), clamped 1–32.
    pub concurrency: Option<u32>,
    /// Compare only, write nothing (rclone --dry-run).
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMode {
    File,
    Directory,
}

impl CopyMode {
    fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("file") => Ok(Self::File),
            Some("directory") => Ok(Self::Directory),
            Some(other) => Err(format!(
                "mode must be \"file\" or \"directory\", got \"{other}\""
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
        }
    }
}

impl Default for TriggerConfig {
    fn default() -> Self {
        Self {
            mode: CopyMode::File,
            source_connection_id: None,
            source_path: String::new(),
            destination_connection_id: None,
            destination_path: String::new(),
            recursive: true,
            delete_extra: true,
            overwrite: true,
            checksum: false,
            resume: true,
            concurrency: None,
            dry_run: false,
        }
    }
}

impl TriggerConfig {
    /// Lenient parse: unknown keys ignored, missing keys defaulted. Shape
    /// errors (non-object config, bad enum values) surface as `Err`.
    pub fn parse(value: Option<&Value>) -> Result<Self, String> {
        let mut config = Self::default();
        let Some(value) = value else {
            return Ok(config);
        };
        let Value::Object(map) = value else {
            return Err("config must be a JSON object".to_string());
        };
        let text = |key: &str| -> Option<String> {
            map.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        let boolean = |key: &str, default: bool| -> Result<bool, String> {
            match map.get(key) {
                None | Some(Value::Null) => Ok(default),
                Some(Value::Bool(value)) => Ok(*value),
                Some(other) => Err(format!("{key} must be a boolean, got {other}")),
            }
        };
        config.mode = CopyMode::parse(map.get("mode").and_then(Value::as_str))?;
        config.source_connection_id = text("sourceConnectionId");
        config.source_path = text("sourcePath").unwrap_or_default();
        config.destination_connection_id = text("destinationConnectionId");
        config.destination_path = text("destinationPath").unwrap_or_default();
        config.recursive = boolean("recursive", true)?;
        config.delete_extra = boolean("deleteExtra", true)?;
        config.overwrite = boolean("overwrite", true)?;
        config.checksum = boolean("checksum", false)?;
        config.resume = boolean("resume", true)?;
        config.dry_run = boolean("dryRun", false)?;
        config.concurrency = match map.get("concurrency") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let raw = value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
                    .ok_or_else(|| format!("concurrency must be a number, got {value}"))?;
                Some(u32::try_from(raw).unwrap_or(u32::MAX))
            }
        };
        Ok(config)
    }
}

/// The validated task: config + the connection ids it runs against.
#[derive(Debug, Clone)]
pub struct ResolvedTask {
    pub trigger_id: String,
    pub config: TriggerConfig,
    pub source_connection_id: String,
    pub destination_connection_id: String,
}

/// `task/validate` answer pieces (also the execute pre-flight).
pub struct Validation {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub resolved: Option<ResolvedTask>,
}

impl Validation {
    fn failed(mut errors: Vec<String>) -> Self {
        errors.dedup();
        Self {
            errors,
            warnings: Vec::new(),
            resolved: None,
        }
    }
}

/// Validates one task head end-to-end: trigger identity, config shape,
/// config version, connection availability, connection gates and path
/// policy. Pure + read-only — no rc calls, no secret material. These are
/// exactly the gates the dir-job starter re-checks, so a `valid` task
/// cannot be doomed on its first run.
pub fn validate(
    engine: &Arc<rclone::RcloneEngine>,
    trigger_id: &str,
    connection_id: Option<&str>,
    config_version: Option<u64>,
    config: Option<&Value>,
) -> Validation {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    if trigger_id != TRIGGER_SYNC && trigger_id != TRIGGER_COPY {
        return Validation::failed(vec![format!(
            "unknown trigger \"{trigger_id}\" (expected \"{TRIGGER_SYNC}\" or \"{TRIGGER_COPY}\")"
        )]);
    }
    if let Some(version) = config_version {
        if version != 1 {
            errors.push(format!(
                "unsupported configVersion {version} (this provider speaks version 1)"
            ));
        }
    }
    let config = match TriggerConfig::parse(config) {
        Ok(config) => config,
        Err(error) => {
            errors.push(format!("invalid config: {error}"));
            return Validation::failed(errors);
        }
    };
    if config.source_path.is_empty() {
        errors.push("sourcePath is required".to_string());
    }
    if config.destination_path.is_empty() {
        errors.push("destinationPath is required".to_string());
    }
    if let Some(concurrency) = config.concurrency {
        if !(1..=32).contains(&concurrency) {
            errors.push(format!(
                "concurrency must be within 1-32, got {concurrency}"
            ));
        }
    }
    if config.source_path.is_empty() || config.destination_path.is_empty() {
        return Validation::failed(errors);
    }
    let bound = connection_id
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(bound) = bound else {
        errors
            .push("connectionId is required (bind this task to a storage connection)".to_string());
        return Validation::failed(errors);
    };
    let source_connection_id = config
        .source_connection_id
        .clone()
        .unwrap_or_else(|| bound.to_string());
    let destination_connection_id = config
        .destination_connection_id
        .clone()
        .unwrap_or_else(|| source_connection_id.clone());
    // Connection availability + gates. A binding that is not registered on
    // this engine (post-restart, host not yet reconnected) fails the run
    // the same way at execute time — validate must not bless it.
    let source_binding = match engine.binding(&source_connection_id) {
        Ok(binding) => binding,
        Err(error) => {
            errors.push(format!(
                "source connection \"{source_connection_id}\" is not available: {error}"
            ));
            return Validation::failed(errors);
        }
    };
    let destination_binding = match engine.binding(&destination_connection_id) {
        Ok(binding) => binding,
        Err(error) => {
            errors.push(format!(
                "destination connection \"{destination_connection_id}\" is not available: {error}"
            ));
            return Validation::failed(errors);
        }
    };
    if destination_binding.read_only {
        errors.push(format!(
            "destination connection \"{destination_connection_id}\" is read-only; sync/copy need write access"
        ));
    }
    let mirror = trigger_id == TRIGGER_SYNC && config.delete_extra;
    if mirror && !destination_binding.allow_delete {
        errors.push(format!(
            "destination connection \"{destination_connection_id}\" disallows deletes; deleteExtra needs delete access (turn deleteExtra off for additive copy semantics)"
        ));
    }
    // Path policy (whitelist/lock_to_root), same gates the dir-job starter
    // enforces. Permissive read_only/allow_delete flags here: those gates
    // are already handled explicitly above.
    let source_policy = crate::policy::PathPolicy::from_parts(
        &source_binding.root,
        source_binding.lock_to_root,
        false,
        true,
    );
    let source_rel = match source_policy.check_read(&config.source_path) {
        Ok(resolved) => resolved.relative,
        Err(error) => {
            errors.push(format!("sourcePath: {error}"));
            String::new()
        }
    };
    let destination_policy = crate::policy::PathPolicy::from_parts(
        &destination_binding.root,
        destination_binding.lock_to_root,
        false,
        true,
    );
    let destination_rel = match destination_policy.check_write(&config.destination_path) {
        Ok(resolved) => resolved.relative,
        Err(error) => {
            errors.push(format!("destinationPath: {error}"));
            String::new()
        }
    };
    // Syncing/copying a tree onto itself (or a nested overlap on the same
    // connection) is either a no-op or a runaway — reject up front.
    if !source_rel.is_empty()
        && source_connection_id == destination_connection_id
        && rclone::sync::paths_overlap(&source_rel, &destination_rel)
    {
        errors.push(format!(
            "source \"{source_rel}\" and destination \"{destination_rel}\" overlap on connection \"{source_connection_id}\""
        ));
    }
    // Cross-connection pairs must share one rcd (the starter rejects too —
    // say it here where the form can show it).
    if source_connection_id != destination_connection_id {
        if let Err(error) = rclone::ensure_same_proxy_group(&source_binding, &destination_binding) {
            errors.push(format!(
                "connections cannot exchange data directly: {error}"
            ));
        }
    }
    // Warnings: behaviors the user may not expect.
    if config.dry_run {
        warnings
            .push("dry run: nothing will be written or deleted (gates still apply)".to_string());
    }
    if !config.recursive {
        warnings.push(
            "recursive off: only the top-level files of the source directory travel".to_string(),
        );
    }
    if trigger_id == TRIGGER_SYNC && !config.delete_extra {
        warnings.push(
            "deleteExtra off: destination files missing at the source are kept (rclone copy semantics)"
                .to_string(),
        );
    }
    if !config.overwrite {
        warnings.push(
            "overwrite off: existing destination files are never replaced (rclone --ignore-existing)"
                .to_string(),
        );
    }
    if !config.resume {
        warnings
            .push("resume off: every file is re-transferred (rclone --ignore-times)".to_string());
    }
    if errors.is_empty() {
        Validation {
            errors,
            warnings,
            resolved: Some(ResolvedTask {
                trigger_id: trigger_id.to_string(),
                config,
                source_connection_id,
                destination_connection_id,
            }),
        }
    } else {
        Validation::failed(errors)
    }
}

/// Maps a validated task onto the engine's dir-job request. The starter
/// ([`crate::rclone_start_dir_job`]) stays the single enqueue path — same
/// job mirror, same `files/transfer/status` surface as the workbench.
pub fn to_dir_job_request(resolved: &ResolvedTask) -> DirJobRequest {
    let config = &resolved.config;
    DirJobRequest {
        source_connection_id: resolved.source_connection_id.clone(),
        source_path: config.source_path.clone(),
        target_connection_id: resolved.destination_connection_id.clone(),
        target_path: config.destination_path.clone(),
        dry_run: Some(config.dry_run),
        max_delete: None,
        include: if config.recursive {
            None
        } else {
            Some(vec![NON_RECURSIVE_INCLUDE.to_string()])
        },
        exclude: None,
        backup_dir: None,
        suffix: None,
        metadata: Some(false),
        update: None,
        existing: None,
        immutable: None,
        // Task knobs → engine flags (see the rclone::sync field docs).
        checksum: Some(config.checksum),
        ignore_existing: Some(!config.overwrite),
        ignore_times: Some(!config.resume),
        min_size: None,
        max_size: None,
        min_age: None,
        max_age: None,
        transfers: config.concurrency.map(|value| value.clamp(1, 32)),
        checkers: None,
        retries: None,
    }
}

/// Whether the mapped dir job carries a mirror delete phase: the sync
/// trigger with `deleteExtra` on. Everything else runs as additive copy.
pub fn is_mirror(resolved: &ResolvedTask) -> bool {
    resolved.trigger_id == TRIGGER_SYNC && resolved.config.delete_extra
}

// ---------------------------------------------------------------------------
// Run registry: active runId → job bookkeeping (cancel flag, log seq, job id)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct TaskRunRegistry {
    runs: Arc<Mutex<HashMap<String, ActiveRun>>>,
}

#[derive(Debug)]
pub struct ActiveRun {
    pub task_id: String,
    /// The transfers-panel job id once a dir job started (`None` while the
    /// pre-flight runs and for native single-file copies).
    pub job_id: Option<String>,
    pub cancelled: bool,
    pub started_at_millis: u64,
    /// `task/log` seq, monotonic per run from 1.
    seq: u64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl TaskRunRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn register(&self, task_id: &str, run_id: &str) {
        lock(&self.runs).insert(
            run_id.to_string(),
            ActiveRun {
                task_id: task_id.to_string(),
                job_id: None,
                cancelled: false,
                started_at_millis: crate::store::unix_millis_now(),
                seq: 0,
            },
        );
    }

    fn finish(&self, run_id: &str) {
        lock(&self.runs).remove(run_id);
    }

    fn attach_job(&self, run_id: &str, job_id: &str) {
        if let Some(run) = lock(&self.runs).get_mut(run_id) {
            run.job_id = Some(job_id.to_string());
        }
    }

    fn next_seq(&self, run_id: &str) -> u64 {
        match lock(&self.runs).get_mut(run_id) {
            Some(run) => {
                run.seq += 1;
                run.seq
            }
            None => 0,
        }
    }

    /// Flags the run cancelled; returns the underlying job id when the run
    /// is live. Unknown/finished runs are a no-op (idempotent stop).
    pub fn request_cancel(&self, run_id: &str) -> Option<String> {
        let mut runs = lock(&self.runs);
        let run = runs.get_mut(run_id)?;
        run.cancelled = true;
        run.job_id.clone()
    }

    fn is_cancelled(&self, run_id: &str) -> bool {
        lock(&self.runs)
            .get(run_id)
            .map(|run| run.cancelled)
            .unwrap_or(false)
    }

    fn job_id_of(&self, run_id: &str) -> Option<String> {
        lock(&self.runs)
            .get(run_id)
            .and_then(|run| run.job_id.clone())
    }

    /// Cancel fallback for `task/stop` requests that only carry `taskId`:
    /// flags the first live run of that task (single active run per task is
    /// the host's concurrency policy) and returns its job id.
    pub fn request_cancel_for_task(&self, task_id: &str) -> Option<String> {
        let mut runs = lock(&self.runs);
        let run = runs.values_mut().find(|run| run.task_id == task_id)?;
        run.cancelled = true;
        run.job_id.clone()
    }

    /// `task/status` projection for an active run: `starting` before the
    /// dir job exists, then the engine mirror's terminal state. `None` once
    /// the run is finished here (the host owns TaskRun persistence).
    pub fn status(&self, engine: &rclone::RcloneEngine, run_id: &str) -> Option<Value> {
        let runs = lock(&self.runs);
        let run = runs.get(run_id)?;
        let job_id = run.job_id.clone();
        let started_at = run.started_at_millis;
        drop(runs);
        let state = match job_id.as_deref() {
            None => "starting",
            Some(job_id) => {
                let jobs = engine
                    .jobs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match jobs.get(job_id) {
                    Some(job) if job.status.is_terminal() => match job.status {
                        crate::transfers::JobStatus::Completed => "completed",
                        crate::transfers::JobStatus::Canceled => "cancelled",
                        _ => "failed",
                    },
                    Some(_) | None => "running",
                }
            }
        };
        Some(json!({
            "state": state,
            "heartbeatAt": crate::store::format_rfc3339(crate::store::unix_millis_now() as i64),
            "restartCount": 0,
            "startedAt": crate::store::format_rfc3339(started_at as i64),
        }))
    }
}

// ---------------------------------------------------------------------------
// Outbound events (ADR §6.3 shapes)
// ---------------------------------------------------------------------------

/// Artifact metadata (ADR §5.1 `TaskArtifact` shape). Bodies never travel —
/// `size` is the only byte count, `checksum` stays `None` until a
/// whole-tree checksum becomes meaningful to report.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactValue {
    pub name: String,
    pub uri: String,
    pub content_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
}

/// `files://<connection-id>/<policy-relative-path>` — provider-local
/// artifact URI convention (ADR §5.1: "文件 URI / 路径").
pub fn artifact_uri(connection_id: &str, path: &str) -> String {
    let trimmed = path.trim().trim_matches('/');
    if trimmed.is_empty() {
        format!("files://{connection_id}")
    } else {
        format!("files://{connection_id}/{trimmed}")
    }
}

fn base_name(path: &str) -> String {
    path.trim_matches('/')
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or("/")
        .to_string()
}

fn content_type_for(path: &str) -> String {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "csv" => "text/csv",
        "gif" => "image/gif",
        "html" | "htm" => "text/html",
        "jpeg" | "jpg" => "image/jpeg",
        "json" => "application/json",
        "md" => "text/markdown",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "txt" => "text/plain",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Directory artifact: metadata only, `size` = bytes moved by THIS run.
pub fn directory_artifact(connection_id: &str, path: &str, bytes: u64) -> ArtifactValue {
    ArtifactValue {
        name: base_name(path),
        uri: artifact_uri(connection_id, path),
        content_type: "inode/directory".to_string(),
        size: Some(bytes),
        checksum: None,
    }
}

/// Single-file artifact.
pub fn file_artifact(connection_id: &str, path: &str, size: Option<u64>) -> ArtifactValue {
    ArtifactValue {
        name: base_name(path),
        uri: artifact_uri(connection_id, path),
        content_type: content_type_for(path),
        size,
        checksum: None,
    }
}

/// Progress math: percent of bytes, clamped to [0, 100]; an unknown or zero
/// total reports 0 (bytes still flow via `current`).
pub fn percent_of(current: u64, total: Option<u64>) -> f64 {
    match total {
        Some(total) if total > 0 => ((current as f64 / total as f64) * 100.0).clamp(0.0, 100.0),
        _ => 0.0,
    }
}

/// One of the fixed event emitters (`task/log` / `task/progress` /
/// `task/artifact`). Every message passes [`redact`] before it leaves, and
/// an emission failure never fails the run (the result travels in the RPC
/// response).
pub struct TaskEventSink {
    emitter: PluginEmitter,
    task_id: String,
    run_id: String,
    registry: TaskRunRegistry,
}

impl TaskEventSink {
    pub fn new(
        emitter: PluginEmitter,
        task_id: String,
        run_id: String,
        registry: TaskRunRegistry,
    ) -> Self {
        Self {
            emitter,
            task_id,
            run_id,
            registry,
        }
    }

    fn emit(&self, event: &str, mut params: Value) {
        if let Some(object) = params.as_object_mut() {
            object.insert("event".to_string(), json!(event));
            object.insert("taskId".to_string(), json!(self.task_id));
            object.insert("runId".to_string(), json!(self.run_id));
        }
        let _ = self.emitter.event(event, params);
    }

    pub fn log(&self, level: &str, message: &str) {
        self.emit(
            "task/log",
            json!({
                "seq": self.registry.next_seq(&self.run_id),
                "stream": "system",
                "level": level,
                "message": redact(message),
                "timestamp": crate::store::format_rfc3339(
                    crate::store::unix_millis_now() as i64
                ),
            }),
        );
    }

    pub fn info(&self, message: &str) {
        self.log("info", message);
    }

    pub fn warn(&self, message: &str) {
        self.log("warn", message);
    }

    pub fn error(&self, message: &str) {
        self.log("error", message);
    }

    /// ADR payload: `{ percent, current?, completed?, total? }` — current
    /// and total are BYTES, completed is the file counter when known.
    pub fn progress(
        &self,
        percent: f64,
        current: Option<u64>,
        total: Option<u64>,
        completed: Option<u64>,
    ) {
        let mut params = json!({ "percent": percent });
        let object = params.as_object_mut().expect("object literal");
        if let Some(current) = current {
            object.insert("current".to_string(), json!(current));
        }
        if let Some(total) = total {
            object.insert("total".to_string(), json!(total));
        }
        if let Some(completed) = completed {
            object.insert("completed".to_string(), json!(completed));
        }
        self.emit("task/progress", params);
    }

    pub fn artifact(&self, artifact: &ArtifactValue) {
        self.emit(
            "task/artifact",
            json!({ "artifact": serde_json::to_value(artifact).unwrap_or(Value::Null) }),
        );
    }
}

// ---------------------------------------------------------------------------
// Secret redaction (defense in depth; ADR §10.4)
// ---------------------------------------------------------------------------

/// Credential key words redacted from every outbound line. Matching is
/// case-insensitive on whole identifiers (`key` must not swallow "keynote").
const SECRET_KEY_WORDS: [&str; 19] = [
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "api-key",
    "access_key",
    "private_key",
    "authorization",
    "credential",
    // Compound keys from this plugin's own connection forms (snake_case
    // compounds never match their parts under the whole-identifier rule).
    "access_token",
    "client_secret",
    "refresh_token",
    "secret_access_key",
    "secret_key",
    "secret_id",
    "account_key",
    "security_token",
];

fn is_ident_char(byte: char) -> bool {
    byte.is_ascii_alphanumeric() || byte == '_'
}

fn mask_value_after(chars: &mut [char], mut cursor: usize) -> usize {
    while cursor < chars.len() && chars[cursor] == ' ' {
        cursor += 1;
    }
    let start = cursor;
    while cursor < chars.len()
        && !matches!(
            chars[cursor],
            ' ' | '"' | '\'' | ',' | ';' | '&' | ')' | ']'
        )
    {
        cursor += 1;
    }
    for slot in &mut chars[start..cursor] {
        *slot = '*';
    }
    cursor
}

/// Replaces the value of `<key> = value` / `<key>: value` / `key=value`
/// occurrences (case-insensitive) with `***`.
pub fn redact(text: &str) -> String {
    let mut chars: Vec<char> = text.chars().collect();
    for scheme in ["bearer", "basic"] {
        mask_scheme_token(&mut chars, scheme);
    }
    for key in SECRET_KEY_WORDS {
        mask_key_value(&mut chars, key);
    }
    chars.into_iter().collect()
}

fn mask_key_value(chars: &mut [char], key: &str) {
    let key_chars: Vec<char> = key.chars().collect();
    let lower = |c: char| c.to_ascii_lowercase();
    let mut i = 0;
    while i + key_chars.len() <= chars.len() {
        let matches_key = chars[i..i + key_chars.len()]
            .iter()
            .copied()
            .map(lower)
            .zip(key_chars.iter().copied())
            .all(|(a, b)| a == b);
        if !matches_key {
            i += 1;
            continue;
        }
        // Whole-identifier match only: "password=" yes, "passwordfile=" no.
        let prev_ok = i == 0 || !is_ident_char(chars[i - 1]);
        let after = i + key_chars.len();
        let next_ok = after >= chars.len() || !is_ident_char(chars[after]);
        if !prev_ok || !next_ok {
            i += 1;
            continue;
        }
        // CLI flag form (`--password topsecret`): a dash prefix directly
        // before the key makes the space-separated next token its value.
        let flag_form =
            i > 0 && chars[i - 1] == '-' && (i == 1 || chars[i - 2] == ' ' || chars[i - 2] == '-');
        let mut cursor = after;
        while cursor < chars.len() && chars[cursor] == ' ' {
            cursor += 1;
        }
        if cursor < chars.len() && (chars[cursor] == '=' || chars[cursor] == ':') {
            i = mask_value_after(chars, cursor + 1);
        } else if flag_form {
            i = mask_value_after(chars, after);
        } else {
            i = after;
        }
    }
}

/// `Bearer <token>` / `Basic <token>` scheme masking (space-separated form;
/// the `authorization:` label pass above already covers labeled values).
fn mask_scheme_token(chars: &mut [char], scheme: &str) {
    let scheme_chars: Vec<char> = scheme.chars().collect();
    let lower = |c: char| c.to_ascii_lowercase();
    let mut i = 0;
    while i + scheme_chars.len() < chars.len() {
        let matches_scheme = chars[i..i + scheme_chars.len()]
            .iter()
            .copied()
            .map(lower)
            .zip(scheme_chars.iter().copied())
            .all(|(a, b)| a == b);
        let prev_ok = i == 0 || !is_ident_char(chars[i - 1]);
        let after = i + scheme_chars.len();
        if matches_scheme && prev_ok && after < chars.len() && chars[after] == ' ' {
            i = mask_value_after(chars, after + 1);
        } else {
            i += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Mirror views (injectable for tests; scheduler `job_view` parity)
// ---------------------------------------------------------------------------

/// Read access to the single-file job table.
pub type JobView = Box<dyn Fn(&str) -> Option<crate::transfers::TransferJob> + Send + Sync>;
/// Read access to the dir-job projection table.
pub type SyncRecordView = Box<dyn Fn(&str) -> Option<crate::RcloneSyncRecord> + Send + Sync>;

/// Production views over the live engine mirrors.
pub fn production_views(
    engine: &Arc<rclone::RcloneEngine>,
    sync_jobs: &Arc<std::sync::Mutex<HashMap<String, crate::RcloneSyncRecord>>>,
) -> (JobView, SyncRecordView) {
    let jobs_engine = Arc::clone(engine);
    let job_view: JobView = Box::new(move |job_id| {
        jobs_engine
            .jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(job_id)
            .cloned()
    });
    let records = Arc::clone(sync_jobs);
    let sync_view: SyncRecordView = Box::new(move |job_id| {
        records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(job_id)
            .cloned()
    });
    (job_view, sync_view)
}

/// One engine job's final observation (from the shared mirrors).
#[derive(Debug, Clone)]
pub struct JobOutcome {
    pub status: crate::transfers::JobStatus,
    pub bytes: u64,
    pub files: Option<u64>,
    pub skipped: u64,
    pub total_bytes: Option<u64>,
    pub error: Option<String>,
}

/// Watches a started dir job through the mirrors until terminal / cancel /
/// deadline, emitting throttled `task/progress` on the way. Pure
/// observation — settling (remote stop + terminal-once) belongs to the
/// orchestration in [`run_dir_job`], which owns the engine Arcs.
pub async fn watch_job(
    job_id: &str,
    run_id: &str,
    registry: &TaskRunRegistry,
    sink: &TaskEventSink,
    job_view: &JobView,
    sync_view: &SyncRecordView,
) -> JobOutcome {
    let deadline = tokio::time::Instant::now() + MAX_RUN_DURATION;
    let mut throttle = crate::transfers::Throttle::default();
    loop {
        tokio::time::sleep(RUN_POLL_INTERVAL).await;
        let job = job_view(job_id);
        match job {
            Some(job) if job.status.is_terminal() => {
                let record = sync_view(job_id);
                let outcome = JobOutcome {
                    status: job.status,
                    bytes: job.transferred_bytes,
                    files: record.as_ref().map(|record| record.files_done),
                    skipped: record
                        .as_ref()
                        .map(|record| record.files_skipped)
                        .unwrap_or(0),
                    total_bytes: job.total_bytes,
                    error: job.error.clone(),
                };
                sink.progress(
                    if outcome.status == crate::transfers::JobStatus::Completed {
                        100.0
                    } else {
                        percent_of(outcome.bytes, job.total_bytes)
                    },
                    Some(outcome.bytes),
                    job.total_bytes,
                    outcome.files,
                );
                return outcome;
            }
            Some(job) => {
                // Cancel wins over everything not yet terminal.
                if registry.is_cancelled(run_id) {
                    return JobOutcome {
                        status: crate::transfers::JobStatus::Canceled,
                        bytes: job.transferred_bytes,
                        files: None,
                        skipped: 0,
                        total_bytes: job.total_bytes,
                        error: Some("cancelled by request".to_string()),
                    };
                }
                if tokio::time::Instant::now() >= deadline {
                    return JobOutcome {
                        status: crate::transfers::JobStatus::Failed,
                        bytes: job.transferred_bytes,
                        files: None,
                        skipped: 0,
                        total_bytes: job.total_bytes,
                        error: Some(format!(
                            "run exceeded its maximum duration ({MAX_RUN_DURATION:?}) and was \
                             released by the task provider; the underlying transfer may still \
                             be running — cancel it from the transfers panel"
                        )),
                    };
                }
                let record = sync_view(job_id);
                let files = record.as_ref().map(|record| record.files_done);
                if throttle.should_emit(job.transferred_bytes, job.total_bytes) {
                    sink.progress(
                        percent_of(job.transferred_bytes, job.total_bytes),
                        Some(job.transferred_bytes),
                        job.total_bytes,
                        files,
                    );
                }
            }
            None => {
                // Terminal entries persist in the mirror, so a vanished id
                // means the engine lost it (rcd restart). Confirm once more
                // after a poll beat before declaring engine loss.
                tokio::time::sleep(RUN_POLL_INTERVAL).await;
                if job_view(job_id).is_some() {
                    continue;
                }
                return JobOutcome {
                    status: crate::transfers::JobStatus::Failed,
                    bytes: 0,
                    files: None,
                    skipped: 0,
                    total_bytes: None,
                    error: Some(
                        "job record vanished from the engine (rcd restarted, or the run was \
                         removed from the transfers panel while finishing)"
                            .to_string(),
                    ),
                };
            }
        }
    }
}

/// Builds the fixed `task/execute` response value (message redacted).
pub fn execute_response(
    success: bool,
    exit_code: Option<i32>,
    message: Option<String>,
    artifacts: Vec<ArtifactValue>,
) -> Value {
    let mut response = json!({
        "success": success,
        "artifacts": artifacts
            .iter()
            .map(|artifact| serde_json::to_value(artifact).unwrap_or(Value::Null))
            .collect::<Vec<_>>(),
    });
    let object = response.as_object_mut().expect("object literal");
    if let Some(exit_code) = exit_code {
        object.insert("exitCode".to_string(), json!(exit_code));
    }
    if let Some(message) = message {
        object.insert("message".to_string(), json!(redact(&message)));
    }
    response
}

/// Describes the mapped direction for the start log line (ids + paths
/// only — no secret material).
pub fn describe_direction(resolved: &ResolvedTask) -> String {
    format!(
        "{}:{} -> {}:{}",
        resolved.source_connection_id,
        resolved.config.source_path,
        resolved.destination_connection_id,
        resolved.config.destination_path
    )
}

// ---------------------------------------------------------------------------
// Execute flow (task/execute)
// ---------------------------------------------------------------------------

/// Everything the run path needs from the plugin wiring (Arcs kept alive
/// for the `'static` on_event closures of the dir-job starter).
pub struct ExecutionContext {
    pub engine: Arc<rclone::RcloneEngine>,
    pub sync_jobs: Arc<std::sync::Mutex<HashMap<String, crate::RcloneSyncRecord>>>,
    pub registry: TaskRunRegistry,
    pub emitter: PluginEmitter,
}

/// Exit-code convention: 0 success · 1 engine failure (retryable at the
/// scheduler's discretion) · 2 invalid config (never retry). Cancelled
/// carries no exit code — the host marks the TaskRun `cancelled`.
const EXIT_SUCCESS: i32 = 0;
const EXIT_FAILED: i32 = 1;
const EXIT_INVALID_CONFIG: i32 = 2;

/// The whole `task/execute` arm: parse → register → validate → dispatch →
/// settle → respond. `Err` only for protocol-level malformed requests —
/// business failures travel in the response (`success: false`).
pub async fn execute(context: ExecutionContext, params: Value) -> Result<Value, String> {
    let request: ExecuteRequest = crate::parse(params)?;
    let run_id = request
        .run
        .as_ref()
        .and_then(|run| run.run_id.clone())
        .or_else(|| request.task.run_id.clone())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or("task/execute requires a runId (task.runId or run.runId)")?;
    let task_id = request
        .task
        .task_id
        .clone()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("{run_id}-task"));
    let attempt = request
        .run
        .as_ref()
        .and_then(|run| run.attempt)
        .unwrap_or(1);
    context.registry.register(&task_id, &run_id);
    let sink = TaskEventSink::new(
        context.emitter.clone(),
        task_id,
        run_id.clone(),
        context.registry.clone(),
    );
    // One registry entry per execute — cleared on every exit path below so
    // task/status and task/stop never see a ghost run.
    let validation = validate(
        &context.engine,
        &request.task.trigger_id,
        request.task.connection_id.as_deref(),
        request.task.config_version,
        request.task.config.as_ref(),
    );
    let Some(resolved) = validation.resolved else {
        let message = format!("invalid config: {}", validation.errors.join("; "));
        sink.error(&message);
        context.registry.finish(&run_id);
        return Ok(execute_response(
            false,
            Some(EXIT_INVALID_CONFIG),
            Some(message),
            Vec::new(),
        ));
    };
    for warning in &validation.warnings {
        sink.info(&format!("note: {warning}"));
    }
    sink.info(&format!(
        "{} started: {} (attempt {attempt}, mode {})",
        resolved.trigger_id,
        describe_direction(&resolved),
        resolved.config.mode.as_str()
    ));
    sink.info(&format!(
        "config: recursive={} deleteExtra={} overwrite={} checksum={} resume={} concurrency={} dryRun={}",
        resolved.config.recursive,
        resolved.config.delete_extra,
        resolved.config.overwrite,
        resolved.config.checksum,
        resolved.config.resume,
        resolved.config.concurrency.map_or_else(|| "engine default".to_string(), |value| value.to_string()),
        resolved.config.dry_run
    ));
    // The "retry" log line, honestly scoped: rclone retries the job
    // internally per its own default; run-level retries belong to the
    // scheduler's execution policy (this provider never retries silently).
    sink.info(
        "engine: rclone applies its own job-level retry default; run-level retries are the scheduler's policy",
    );
    let response = match resolved.config.mode {
        CopyMode::Directory => run_dir_job(&context, &sink, &run_id, &resolved, attempt).await,
        CopyMode::File => run_file_copy(&context, &sink, &run_id, &resolved, attempt).await,
    };
    context.registry.finish(&run_id);
    Ok(response)
}

/// Directory branch: sync trigger (mirror when `deleteExtra`) and
/// copy-directory both enqueue through the workbench's
/// [`crate::rclone_start_dir_job`] — the native engine, no re-implementation.
async fn run_dir_job(
    context: &ExecutionContext,
    sink: &TaskEventSink,
    run_id: &str,
    resolved: &ResolvedTask,
    attempt: u32,
) -> Value {
    let mirror = is_mirror(resolved);
    let request = to_dir_job_request(resolved);
    let job_id = match crate::rclone_start_dir_job(
        Arc::clone(&context.engine),
        Arc::clone(&context.sync_jobs),
        &request,
        mirror,
        false,
        Some(&context.emitter),
    )
    .await
    {
        Ok(job_id) => job_id,
        Err(error) => {
            let message = format!(
                "failed to start the {} job: {error}",
                if mirror { "sync" } else { "copy" }
            );
            sink.error(&message);
            return execute_response(false, Some(EXIT_FAILED), Some(message), Vec::new());
        }
    };
    context.registry.attach_job(run_id, &job_id);
    sink.info(&format!(
        "job {job_id} started ({}, engine-native rclone {})",
        if mirror {
            "mirror semantics"
        } else {
            "additive copy"
        },
        if mirror { "sync" } else { "copy" }
    ));
    let (job_view, sync_view) = production_views(&context.engine, &context.sync_jobs);
    let outcome = watch_job(
        &job_id,
        run_id,
        &context.registry,
        sink,
        &job_view,
        &sync_view,
    )
    .await;
    match outcome.status {
        crate::transfers::JobStatus::Completed => {
            let files = outcome.files.unwrap_or(0);
            sink.info(&format!(
                "complete: {files} file(s) transferred, {} byte(s)",
                outcome.bytes
            ));
            if outcome.skipped > 0 {
                sink.info(&format!(
                    "resume: {} file(s) already present with matching size/checksum — skipped",
                    outcome.skipped
                ));
            }
            if resolved.config.dry_run {
                sink.info("dry run: nothing was written");
            }
            let artifact = directory_artifact(
                &resolved.destination_connection_id,
                &resolved.config.destination_path,
                outcome.bytes,
            );
            sink.artifact(&artifact);
            execute_response(
                true,
                Some(EXIT_SUCCESS),
                Some(format!(
                    "complete: {files} file(s), {} byte(s)",
                    outcome.bytes
                )),
                vec![artifact],
            )
        }
        crate::transfers::JobStatus::Canceled => {
            // Real cancellation: stop the remote rc job and settle the
            // mirror terminal-once (the shared arm's exact sequence).
            cancel_dir_job(context, &job_id, &resolved.source_connection_id).await;
            sink.warn(&format!("cancelled after {} byte(s)", outcome.bytes));
            execute_response(
                false,
                None,
                Some("cancelled by request".to_string()),
                Vec::new(),
            )
        }
        _ => {
            let error = outcome
                .error
                .unwrap_or_else(|| "unknown engine failure".to_string());
            let message = format!(
                "failed after {} byte(s): {error} (attempt {attempt})",
                outcome.bytes
            );
            sink.error(&message);
            execute_response(false, Some(EXIT_FAILED), Some(message), Vec::new())
        }
    }
}

/// Stops the remote rc job and settles the mirror Canceled terminal-once
/// (same sequence the workbench cancel arm runs). Idempotent.
async fn cancel_dir_job(context: &ExecutionContext, job_id: &str, fallback_connection: &str) {
    let handle = {
        let records = lock(&context.sync_jobs);
        records
            .get(job_id)
            .and_then(|record| record.handle.clone())
            .map(|handle| (handle, fallback_connection.to_string()))
    };
    if let Some((handle, connection_id)) = handle {
        match context.engine.client_for_id(&connection_id).await {
            Ok(client) => {
                if let Err(error) = rclone::sync::stop_job(&client, &handle).await {
                    // Cancel-marker semantics: the poll loop settles
                    // Canceled regardless; never fail the cancel here.
                    eprintln!("[{PROVIDER_ID}] dir job stop ignored: {error}");
                }
            }
            Err(error) => eprintln!("[{PROVIDER_ID}] client for cancel unavailable: {error}"),
        }
    }
    crate::rclone_sync_terminal(
        &context.engine.jobs,
        &context.sync_jobs,
        None,
        job_id,
        crate::transfers::JobStatus::Canceled,
        None,
        None,
        None,
    );
}

/// Single-file branch (copy trigger, `mode = file`): engine-native
/// `operations/copyfile` — the same op the workbench `files/copy` uses. A
/// directory source degrades to the dir-job branch (workbench parity).
async fn run_file_copy(
    context: &ExecutionContext,
    sink: &TaskEventSink,
    run_id: &str,
    resolved: &ResolvedTask,
    attempt: u32,
) -> Value {
    let failed = |sink: &TaskEventSink, message: String| {
        sink.error(&message);
        execute_response(false, Some(EXIT_FAILED), Some(message), Vec::new())
    };
    let config = &resolved.config;
    let source_binding = match context.engine.binding(&resolved.source_connection_id) {
        Ok(binding) => binding,
        Err(error) => {
            return failed(
                sink,
                format!(
                    "source connection \"{}\" is not available: {error}",
                    resolved.source_connection_id
                ),
            );
        }
    };
    let client = match context.engine.client_for_binding(&source_binding).await {
        Ok(client) => client,
        Err(error) => return failed(sink, format!("source engine unavailable: {error}")),
    };
    let source_fs = rclone::call_fs(&source_binding);
    let entry = match rclone::ops::stat(
        &client,
        &source_fs,
        &config.source_path,
        &source_binding.root,
        source_binding.lock_to_root,
    )
    .await
    {
        Ok(entry) => entry,
        Err(error) => return failed(sink, format!("sourcePath: {error}")),
    };
    if entry.kind == "dir" {
        // Workbench `files/copy` degrades identically (sync/copy job).
        sink.info("source is a directory; degrading to a recursive directory copy");
        let mut directory = resolved.clone();
        directory.config.mode = CopyMode::Directory;
        return run_dir_job(context, sink, run_id, &directory, attempt).await;
    }
    let destination_binding = match context.engine.binding(&resolved.destination_connection_id) {
        Ok(binding) => binding,
        Err(error) => {
            return failed(
                sink,
                format!(
                    "destination connection \"{}\" is not available: {error}",
                    resolved.destination_connection_id
                ),
            );
        }
    };
    // overwrite = off: an existing destination file is a refusal, not a
    // replacement (the dir branch expresses the same via --ignore-existing).
    if !config.overwrite {
        let destination_client = match context
            .engine
            .client_for_binding(&destination_binding)
            .await
        {
            Ok(client) => client,
            Err(error) => return failed(sink, format!("destination engine unavailable: {error}")),
        };
        if rclone::ops::stat(
            &destination_client,
            &rclone::call_fs(&destination_binding),
            &config.destination_path,
            &destination_binding.root,
            destination_binding.lock_to_root,
        )
        .await
        .is_ok()
        {
            let message = format!(
                "destination \"{}\" already exists and overwrite is off",
                config.destination_path
            );
            sink.error(&message);
            return execute_response(false, Some(EXIT_FAILED), Some(message), Vec::new());
        }
    }
    if context.registry.is_cancelled(run_id) {
        sink.warn("cancelled before the copy started");
        return execute_response(
            false,
            None,
            Some("cancelled by request".to_string()),
            Vec::new(),
        );
    }
    sink.progress(0.0, Some(0), entry.size, Some(1));
    if let Err(error) = rclone::ops::copy_file(
        &client,
        &source_fs,
        &config.source_path,
        &rclone::call_fs(&destination_binding),
        &config.destination_path,
        &destination_binding.root,
        destination_binding.lock_to_root,
    )
    .await
    {
        return failed(sink, format!("copy failed: {error} (attempt {attempt})"));
    }
    if context.registry.is_cancelled(run_id) {
        // The single rc call landed while a stop raced in: the file exists
        // but the run was told to stop — report cancelled, not success.
        sink.warn("cancelled (the copy call completed while the stop arrived)");
        return execute_response(
            false,
            None,
            Some("cancelled by request".to_string()),
            Vec::new(),
        );
    }
    sink.progress(100.0, entry.size, entry.size, Some(1));
    sink.info(&format!(
        "complete: 1 file copied ({} byte(s))",
        entry
            .size
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    ));
    let artifact = file_artifact(
        &resolved.destination_connection_id,
        &config.destination_path,
        entry.size,
    );
    sink.artifact(&artifact);
    execute_response(
        true,
        Some(EXIT_SUCCESS),
        Some(format!(
            "complete: 1 file, {} byte(s)",
            entry
                .size
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".into())
        )),
        vec![artifact],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- config parsing -------------------------------------------------------

    #[test]
    fn config_defaults_are_safe_engine_semantics() {
        let config = TriggerConfig::parse(None).expect("parse");
        assert_eq!(config.mode, CopyMode::File);
        assert!(config.recursive && config.delete_extra && config.overwrite && config.resume);
        assert!(!config.checksum && !config.dry_run);
        assert_eq!(config.concurrency, None);
        assert_eq!(config.source_connection_id, None);
        assert_eq!(config.destination_connection_id, None);
    }

    #[test]
    fn config_parses_camel_case_and_ignores_unknown_keys() {
        let config = TriggerConfig::parse(Some(&json!({
            "mode": "directory",
            "sourcePath": "/data",
            "destinationPath": "/backup",
            "destinationConnectionId": "nas",
            "deleteExtra": false,
            "checksum": true,
            "concurrency": 8,
            "someFutureKey": {"nested": true}
        })))
        .expect("parse");
        assert_eq!(config.mode, CopyMode::Directory);
        assert_eq!(config.source_path, "/data");
        assert_eq!(config.destination_connection_id.as_deref(), Some("nas"));
        assert!(!config.delete_extra);
        assert!(config.checksum);
        assert_eq!(config.concurrency, Some(8));
    }

    #[test]
    fn config_rejects_bad_enums_and_non_object_config() {
        let error =
            TriggerConfig::parse(Some(&json!({ "mode": "symlink" }))).expect_err("bad mode");
        assert!(
            error.contains("file") && error.contains("directory"),
            "{error}"
        );
        let error = TriggerConfig::parse(Some(&json!("[1,2]"))).expect_err("non-object config");
        assert!(error.contains("object"), "{error}");
        let error = TriggerConfig::parse(Some(&json!({ "concurrency": "many" })))
            .expect_err("bad concurrency");
        assert!(error.contains("concurrency"), "{error}");
    }

    // -- dir job mapping (pure) -----------------------------------------------

    fn resolved(config: TriggerConfig) -> ResolvedTask {
        ResolvedTask {
            trigger_id: TRIGGER_SYNC.to_string(),
            config,
            source_connection_id: "src".to_string(),
            destination_connection_id: "src".to_string(),
        }
    }

    #[test]
    fn mapping_defaults_to_mirror_with_delta_resume() {
        let request = to_dir_job_request(&resolved(TriggerConfig {
            source_path: "/data".into(),
            destination_path: "/mirror".into(),
            ..TriggerConfig::default()
        }));
        assert_eq!(request.source_path, "/data");
        assert_eq!(request.target_path, "/mirror");
        assert_eq!(request.dry_run, Some(false));
        assert_eq!(request.checksum, Some(false));
        assert_eq!(request.ignore_existing, Some(false));
        assert_eq!(
            request.ignore_times,
            Some(false),
            "resume on → no ignore-times"
        );
        assert_eq!(request.include, None, "recursive on → no filter");
        assert_eq!(request.transfers, None);
        assert!(is_mirror(&resolved(TriggerConfig {
            source_path: "/a".into(),
            destination_path: "/b".into(),
            ..TriggerConfig::default()
        })));
    }

    #[test]
    fn mapping_carries_every_knob() {
        let request = to_dir_job_request(&resolved(TriggerConfig {
            source_path: "/data".into(),
            destination_path: "/mirror".into(),
            recursive: false,
            delete_extra: false,
            overwrite: false,
            checksum: true,
            resume: false,
            concurrency: Some(9),
            dry_run: true,
            ..TriggerConfig::default()
        }));
        assert_eq!(request.checksum, Some(true));
        assert_eq!(
            request.ignore_existing,
            Some(true),
            "overwrite off → ignore-existing"
        );
        assert_eq!(
            request.ignore_times,
            Some(true),
            "resume off → ignore-times"
        );
        assert_eq!(
            request.include,
            Some(vec![NON_RECURSIVE_INCLUDE.to_string()]),
            "recursive off → top-level filter"
        );
        assert_eq!(request.transfers, Some(9));
        assert_eq!(request.dry_run, Some(true));
        assert!(!is_mirror(&resolved(TriggerConfig {
            source_path: "/a".into(),
            destination_path: "/b".into(),
            delete_extra: false,
            ..TriggerConfig::default()
        })));
    }

    #[test]
    fn concurrency_is_clamped_into_the_engine_range() {
        let request = to_dir_job_request(&resolved(TriggerConfig {
            source_path: "/a".into(),
            destination_path: "/b".into(),
            concurrency: Some(500),
            ..TriggerConfig::default()
        }));
        assert_eq!(request.transfers, Some(32));
    }

    // -- artifacts / progress --------------------------------------------------

    #[test]
    fn artifacts_carry_metadata_only_and_never_bodies() {
        let artifact = file_artifact("conn-1", "/backup/report.pdf", Some(4u64 << 30));
        let value = serde_json::to_value(&artifact).expect("serialize");
        assert_eq!(value["name"], "report.pdf");
        assert_eq!(value["uri"], "files://conn-1/backup/report.pdf");
        assert_eq!(value["contentType"], "application/pdf");
        assert_eq!(
            value["size"],
            4u64 << 30,
            "large-file metadata stays an integer"
        );
        assert!(value.get("checksum").is_none(), "no fake checksums");
        let serialized = value.to_string();
        assert!(
            serialized.len() < 300,
            "artifact payload is metadata only: {serialized}"
        );

        let directory = directory_artifact("conn-2", "/backup/data", 7);
        let value = serde_json::to_value(&directory).expect("serialize");
        assert_eq!(value["contentType"], "inode/directory");
        assert_eq!(value["name"], "data");
        assert_eq!(value["uri"], "files://conn-2/backup/data");
    }

    #[test]
    fn artifact_uri_handles_the_connection_root() {
        assert_eq!(artifact_uri("c", "/a/b"), "files://c/a/b");
        assert_eq!(artifact_uri("c", "/"), "files://c");
        assert_eq!(artifact_uri("c", ""), "files://c");
    }

    #[test]
    fn percent_math_clamps_and_handles_unknown_totals() {
        assert_eq!(percent_of(0, Some(100)), 0.0);
        assert_eq!(percent_of(50, Some(100)), 50.0);
        assert_eq!(percent_of(200, Some(100)), 100.0);
        assert_eq!(percent_of(7, None), 0.0);
        assert_eq!(percent_of(7, Some(0)), 0.0);
    }

    #[test]
    fn execute_response_matches_the_frozen_shape() {
        let value = execute_response(
            false,
            Some(2),
            Some("invalid config: missing path".to_string()),
            Vec::new(),
        );
        assert_eq!(value["success"], false);
        assert_eq!(value["exitCode"], 2);
        assert_eq!(value["message"], "invalid config: missing path");
        assert_eq!(value["artifacts"], serde_json::json!([]));
        // Cancelled: no exitCode key at all (contract `exitCode?`).
        let cancelled = execute_response(false, None, Some("cancelled by request".into()), vec![]);
        assert!(cancelled.get("exitCode").is_none());
        assert_eq!(cancelled["message"], "cancelled by request");
    }

    // -- redaction --------------------------------------------------------------

    #[test]
    fn redaction_masks_known_credential_shapes() {
        let cases = [
            ("password=hunter2", "hunter2"),
            ("PASSWORD: hunter2", "hunter2"),
            ("token=abc123,", "abc123"),
            ("secret_access_key = wJalrXUtnFEMI", "wJalrXUtnFEMI"),
            ("access_key:AKIAIOSFODNN7", "AKIAIOSFODNN7"),
            ("api_key=sk-123;", "sk-123"),
            ("--password topsecret", "topsecret"),
            ("Bearer eyJhbGciOi", "eyJhbGciOi"),
            ("private_key=CONTENT", "CONTENT"),
        ];
        for (sample, leaked) in cases {
            let masked = redact(sample);
            assert!(!masked.contains(leaked), "{sample} → {masked}");
            assert!(masked.contains("***"), "{sample} → {masked}");
        }
    }

    #[test]
    fn redaction_leaves_prose_paths_and_identifiers_intact() {
        assert_eq!(
            redact("sync complete: 3 files, 120 bytes"),
            "sync complete: 3 files, 120 bytes"
        );
        assert_eq!(redact("/data/notes/keynote.txt"), "/data/notes/keynote.txt");
        // Identifier-adjacent words must not be swallowed…
        assert_eq!(redact("passwordfile=/x"), "passwordfile=/x");
        assert_eq!(redact("Token Bearer"), "Token Bearer");
        // Multibyte text survives (char-wise masking, no byte slicing).
        assert_eq!(redact("同步完成：3 个文件"), "同步完成：3 个文件");
    }

    // -- run registry -------------------------------------------------------------

    #[test]
    fn registry_tracks_seq_cancel_and_job_attachment() {
        let registry = TaskRunRegistry::new();
        registry.register("task-1", "run-1");
        assert_eq!(registry.job_id_of("run-1"), None);
        registry.attach_job("run-1", "job-9");
        assert_eq!(registry.job_id_of("run-1"), Some("job-9".to_string()));
        assert_eq!(
            (registry.next_seq("run-1"), registry.next_seq("run-1")),
            (1, 2)
        );
        assert_eq!(
            registry.request_cancel("run-1"),
            Some("job-9".to_string()),
            "cancel returns the job to route through the shared cancel arm"
        );
        assert!(registry.is_cancelled("run-1"));
        assert_eq!(
            registry.request_cancel("run-unknown"),
            None,
            "idempotent stop"
        );
        assert_eq!(registry.next_seq("run-unknown"), 0);
        registry.finish("run-1");
        assert_eq!(
            registry.request_cancel("run-1"),
            None,
            "finished runs stop as no-op"
        );
    }
}
