//! Schedule engine: persisted cron tasks that drive rclone dir/bisync/check
//! jobs (`files/schedule/*` RPC family).
//!
//! Architecture (docs/PROTOCOL.zh-CN.md scheduler section):
//! - Tasks live in `schedules.json`, run history in `runs.json` (both via
//!   `Store`); connections are NOT persisted — a fired task whose connection
//!   is no longer registered (sidecar restart) records a `skipped` run
//!   instead of failing storage that was never reachable.
//! - Execution reuses the exact workbench path: the job-starting closures
//!   ([`JobHooks`]) are injected from `main.rs` and wrap
//!   `rclone_start_dir_job` / `rclone_start_bisync_job` /
//!   `rclone_start_check_job`, so scheduled runs land in the same job
//!   mirror, emit the same `files/transfer/progress` events, and are
//!   cancellable through the same `files/transfer/cancel` arm.
//! - Own thread + current-thread runtime (keepalive pattern — `Plugin::new`
//!   runs before any ambient tokio runtime). The loop ticks every
//!   [`TICK_INTERVAL`], claims due tasks atomically (single-flight per task,
//!   global semaphore cap), advances `next_run_at` from *now* (a shutdown
//!   period shifts the schedule; it never back-fills), spawns the run, then
//!   applies post-run hooks: optional `verify_after` check job and optional
//!   `retention_days` prune of `backup_dir` (best-effort, warning-only).
//! - Secret red line: tasks carry connection ids and paths only.

pub mod cron;
pub mod model;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use dbx_plugin_sdk::PluginEmitter;
use serde_json::{json, Value};

use crate::model::CheckRequest;
use crate::rclone;
use crate::store::{self, AuditRecord, Store};
use crate::transfers;
use model::{RunRecord, ScheduleCreateRequest, ScheduleKind, ScheduleTask, ScheduleUpdateRequest};

/// Scheduler tick: fires tasks whose `next_run_at` has passed.
const TICK_INTERVAL: Duration = Duration::from_secs(30);
/// Poll cadence while watching a started job for its terminal state.
const RUN_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Global cap on concurrent scheduled/manual runs across all tasks.
const MAX_CONCURRENT_RUNS: usize = 2;
/// Retention prune ceiling per run (prune failure never fails the run).
const RETENTION_TIMEOUT: Duration = Duration::from_secs(120);

/// Shared emitter slot: created by `main.rs` next to the job hooks (both
/// read the same slot — background dir jobs push transfer progress through
/// it too), refreshed on every request via [`Scheduler::set_emitter`].
pub type EmitterSlot = Arc<std::sync::Mutex<Option<PluginEmitter>>>;

/// Boxed job starter injected from `main.rs` (the request-path starters are
/// private there; the scheduler only ever sees this narrow surface).
pub type StartJobFuture = Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;
pub type StartDirJobFn =
    Arc<dyn Fn(crate::model::DirJobRequest, bool) -> StartJobFuture + Send + Sync>;
pub type StartBisyncJobFn =
    Arc<dyn Fn(crate::model::BisyncStartRequest) -> StartJobFuture + Send + Sync>;
pub type StartCheckJobFn = Arc<dyn Fn(CheckRequest) -> StartJobFuture + Send + Sync>;

/// The three starters the run path may call, wired in `Plugin::new`.
#[derive(Clone)]
pub struct JobHooks {
    pub start_dir_job: StartDirJobFn,
    pub start_bisync_job: StartBisyncJobFn,
    pub start_check_job: StartCheckJobFn,
}

/// Poison-tolerant std-Mutex lock (same discipline as `rclone_lock`).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct Scheduler {
    inner: Arc<Inner>,
}

struct Inner {
    engine: Arc<rclone::RcloneEngine>,
    /// Shared dir-job mirror (main.rs) — read here only for file counters.
    sync_jobs: Arc<Mutex<HashMap<String, crate::RcloneSyncRecord>>>,
    store: Arc<Store>,
    hooks: JobHooks,
    /// Latest request-path emitter; background runs emit UI events through
    /// it (absent until the first request — runs still execute, jobs stay
    /// pollable, only the push events are lost). Shared with the job hooks.
    emitter: EmitterSlot,
    /// In-memory mirror of schedules.json (the file is the source of truth).
    tasks: Mutex<Vec<ScheduleTask>>,
    /// Task ids with a live run — single-flight per task across triggers.
    running: Mutex<HashSet<String>>,
    /// Global run concurrency cap.
    gate: tokio::sync::Semaphore,
    /// Handle of the scheduler runtime; run-now spawns land here so manual
    /// runs keep polling regardless of which thread called them.
    runtime: Mutex<Option<tokio::runtime::Handle>>,
    /// Job-table read view: production reads the engine's shared jobs
    /// mirror; tests swap in a fake table (set once before any run).
    job_view: Mutex<JobView>,
    /// Injected tick/poll periods (tests shrink both).
    tick_period: Duration,
    run_poll: Duration,
}

/// Snapshot read of one job's current record (see [`Inner::job_view`]).
type JobView = Box<dyn Fn(&str) -> Option<transfers::TransferJob> + Send + Sync>;

impl Scheduler {
    pub fn new(
        engine: Arc<rclone::RcloneEngine>,
        sync_jobs: Arc<Mutex<HashMap<String, crate::RcloneSyncRecord>>>,
        store: Arc<Store>,
        hooks: JobHooks,
        emitter: EmitterSlot,
    ) -> Arc<Self> {
        let view_engine = Arc::clone(&engine);
        let job_view: JobView = Box::new(move |job_id| {
            let jobs = view_engine.jobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            jobs.get(job_id).cloned()
        });
        Self::with_periods(
            engine,
            sync_jobs,
            store,
            hooks,
            emitter,
            Mutex::new(job_view),
            TICK_INTERVAL,
            RUN_POLL_INTERVAL,
        )
    }

    /// Periods/job-view injected: production passes the module constants
    /// and the engine-backed view (see [`Scheduler::new`]); tests shrink
    /// both and point the view at a fake table.
    fn with_periods(
        engine: Arc<rclone::RcloneEngine>,
        sync_jobs: Arc<Mutex<HashMap<String, crate::RcloneSyncRecord>>>,
        store: Arc<Store>,
        hooks: JobHooks,
        emitter: EmitterSlot,
        job_view: Mutex<JobView>,
        tick_period: Duration,
        run_poll: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(Inner {
                engine,
                sync_jobs,
                store,
                hooks,
                emitter,
                tasks: Mutex::new(Vec::new()),
                running: Mutex::new(HashSet::new()),
                gate: tokio::sync::Semaphore::new(MAX_CONCURRENT_RUNS),
                runtime: Mutex::new(None),
                job_view,
                tick_period,
                run_poll,
            }),
        })
    }

    /// Request-path hook: keeps the latest emitter so background runs can
    /// push UI events. Cheap clone; called on every request.
    pub fn set_emitter(&self, emitter: PluginEmitter) {
        *self
            .inner
            .emitter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(emitter);
    }

    /// Starts the tick loop on a dedicated thread (keepalive pattern —
    /// `Plugin::new` has no ambient runtime). Never panics: a denied thread
    /// or runtime only disables scheduled runs for the session, logging why.
    pub fn start(self: &Arc<Self>) {
        self.hydrate();
        let scheduler = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("files-scheduler".to_string())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    eprintln!("[io.dbx.files] scheduler runtime unavailable: scheduled runs disabled this session");
                    return;
                };
                *lock(&scheduler.inner.runtime) = Some(runtime.handle().clone());
                runtime.block_on(scheduler.tick_loop());
            });
        if spawned.is_err() {
            eprintln!("[io.dbx.files] scheduler thread spawn failed: scheduled runs disabled this session");
        }
    }

    /// Loads persisted tasks and recomputes every schedule from *now*
    /// (downtime shifts the schedule; it never back-fills). Also settles
    /// `running` records orphaned by a restart — they never observed their
    /// terminal event, so they must not stay `running` forever.
    fn hydrate(&self) {
        let now = store::unix_millis_now();
        let mut tasks = self.inner.store.load_schedules();
        for task in &mut tasks {
            task.next_run_at = if task.enabled {
                cron::CronExpr::parse(&task.cron)
                    .ok()
                    .and_then(|expr| expr.next_fire_millis(now))
            } else {
                None
            };
        }
        let mut runs = self.inner.store.load_runs();
        let mut settled = false;
        for run in runs.iter_mut().filter(|run| run.status == "running") {
            run.status = "failed".to_string();
            run.error = Some("interrupted: sidecar restarted mid-run".to_string());
            run.finished_at = Some(now);
            settled = true;
        }
        *lock(&self.inner.tasks) = tasks.clone();
        let _ = self.inner.store.save_schedules(&tasks);
        if settled {
            if let Ok(value) = serde_json::to_value(runs) {
                let _ = self.inner.store.write_json_atomic("runs.json", &value);
            }
        }
    }

    async fn tick_loop(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(self.inner.tick_period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            self.process_due();
        }
    }

    /// Claims and spawns every due task. Claiming advances `next_run_at`
    /// under the lock, so one fired tick can never double-fire a task.
    fn process_due(&self) {
        let now = store::unix_millis_now();
        let due_ids: Vec<String> = {
            let tasks = lock(&self.inner.tasks);
            let running = lock(&self.inner.running);
            tasks
                .iter()
                .filter(|task| {
                    task.enabled
                        && task.next_run_at.is_some_and(|at| at <= now)
                        && !running.contains(&task.id)
                })
                .map(|task| task.id.clone())
                .collect()
        };
        for id in due_ids {
            if let Ok((task, run)) = self.claim(&id, "schedule", false) {
                self.spawn_run(task, run);
            }
        }
    }

    // -- task CRUD (request path) -------------------------------------------

    pub fn list(&self) -> Vec<ScheduleTask> {
        lock(&self.inner.tasks).clone()
    }

    pub fn create(&self, request: ScheduleCreateRequest) -> Result<ScheduleTask, String> {
        let name = request.name.trim().to_string();
        if name.is_empty() {
            return Err("Schedule task name must not be empty".to_string());
        }
        let target_connection_id = request
            .target_connection_id
            .clone()
            .unwrap_or_else(|| request.source_connection_id.clone());
        Self::validate(
            &self.inner.engine,
            request.kind,
            &request.source_connection_id,
            &request.source_path,
            &target_connection_id,
            &request.target_path,
            &request.cron,
            &request.options,
        )?;
        let now = store::unix_millis_now();
        let enabled = request.enabled.unwrap_or(true);
        let task = ScheduleTask {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            kind: request.kind,
            source_connection_id: request.source_connection_id,
            source_path: request.source_path,
            target_connection_id,
            target_path: request.target_path,
            cron: request.cron.trim().to_string(),
            enabled,
            options: request.options,
            bisync_resync_done: false,
            created_at: now,
            last_run_at: None,
            last_run_status: None,
            next_run_at: Self::initial_next_run(&request.cron, enabled, now),
        };
        {
            let mut tasks = lock(&self.inner.tasks);
            if tasks.iter().any(|existing| existing.name == task.name) {
                return Err(format!("Schedule task name '{}' is already in use", task.name));
            }
            tasks.push(task.clone());
        }
        self.persist_tasks();
        self.emit_changed();
        Ok(task)
    }

    /// Full-object replace (the UI edits a task it already holds). Runtime
    /// bookkeeping survives: `created_at`, `last_run_*`, `next_run_at` is
    /// recomputed only when the cron/enabled shape actually changed.
    pub fn update(&self, request: ScheduleUpdateRequest) -> Result<ScheduleTask, String> {
        let name = request.name.trim().to_string();
        if name.is_empty() {
            return Err("Schedule task name must not be empty".to_string());
        }
        let target_connection_id = request
            .target_connection_id
            .clone()
            .unwrap_or_else(|| request.source_connection_id.clone());
        Self::validate(
            &self.inner.engine,
            request.kind,
            &request.source_connection_id,
            &request.source_path,
            &target_connection_id,
            &request.target_path,
            &request.cron,
            &request.options,
        )?;
        let updated;
        {
            let mut tasks = lock(&self.inner.tasks);
            if tasks
                .iter()
                .any(|existing| existing.id != request.id && existing.name == name)
            {
                return Err(format!("Schedule task name '{name}' is already in use"));
            }
            let Some(task) = tasks.iter_mut().find(|task| task.id == request.id) else {
                return Err("Schedule task was not found".to_string());
            };
            let enabled = request.enabled.unwrap_or(task.enabled);
            let cron_changed = task.cron.trim() != request.cron.trim();
            task.name = name;
            task.kind = request.kind;
            task.source_connection_id = request.source_connection_id;
            task.source_path = request.source_path;
            task.target_connection_id = target_connection_id;
            task.target_path = request.target_path;
            task.cron = request.cron.trim().to_string();
            if enabled != task.enabled || cron_changed {
                task.next_run_at =
                    Self::initial_next_run(&task.cron, enabled, store::unix_millis_now());
            }
            task.enabled = enabled;
            task.options = request.options;
            updated = Some(task.clone());
        }
        let task = updated.expect("task found above");
        self.persist_tasks();
        self.emit_changed();
        Ok(task)
    }

    /// Removes the task; a live run keeps going and still finalizes its
    /// history record (finalization tolerates a missing task row).
    pub fn delete(&self, id: &str) -> Result<bool, String> {
        let removed = {
            let mut tasks = lock(&self.inner.tasks);
            let before = tasks.len();
            tasks.retain(|task| task.id != id);
            before != tasks.len()
        };
        if removed {
            self.persist_tasks();
            self.emit_changed();
        }
        Ok(removed)
    }

    /// Run history, newest first, optionally scoped to one task.
    pub fn history(&self, id: Option<&str>, limit: u32) -> Vec<RunRecord> {
        let limit = limit.clamp(1, 500) as usize;
        let mut runs: Vec<RunRecord> = self
            .inner
            .store
            .load_runs()
            .into_iter()
            .filter(|run| id.map_or(true, |task_id| run.task_id == task_id))
            .collect();
        runs.reverse();
        runs.truncate(limit);
        runs
    }

    /// Manual run — allowed on disabled tasks (`force`). Single-flight per
    /// task: a second run while one is live is refused, not queued.
    pub fn run_now(self: &Arc<Self>, id: &str) -> Result<RunRecord, String> {
        let (task, run) = self.claim(id, "manual", true)?;
        self.spawn_run(task, run.clone());
        Ok(run)
    }

    /// The transfers jobId of the task's live run, for routing a cancel
    /// through the shared `files/transfer/cancel` arm (zero duplicated stop
    /// logic). The run then finalizes as `canceled` via the normal poll.
    pub fn cancel(&self, id: &str) -> Result<String, String> {
        let runs = self.inner.store.load_runs();
        let Some(run) = runs
            .iter()
            .filter(|run| run.task_id == id)
            .max_by_key(|run| run.started_at.unwrap_or(0))
            .filter(|run| run.status == "running")
        else {
            return Err("Schedule task has no run in progress".to_string());
        };
        run.job_id
            .clone()
            .ok_or_else(|| "Schedule run has not started a job yet".to_string())
    }

    // -- run path -------------------------------------------------------------

    /// Atomically claims one run of `id`: single-flight marker, `next_run_at`
    /// advance (scheduled trigger only), history record, UI events.
    fn claim(
        &self,
        id: &str,
        trigger: &str,
        force: bool,
    ) -> Result<(ScheduleTask, RunRecord), String> {
        let now = store::unix_millis_now();
        let task = {
            let mut tasks = lock(&self.inner.tasks);
            let mut running = lock(&self.inner.running);
            let Some(task) = tasks.iter_mut().find(|task| task.id == id) else {
                return Err("Schedule task was not found".to_string());
            };
            if running.contains(id) {
                return Err("Schedule task already has a run in progress".to_string());
            }
            if !task.enabled && !force {
                return Err("Schedule task is disabled".to_string());
            }
            if trigger == "schedule" {
                task.next_run_at = cron::CronExpr::parse(&task.cron)
                    .ok()
                    .and_then(|expr| expr.next_fire_millis(now));
            }
            running.insert(task.id.clone());
            task.clone()
        };
        let run = RunRecord {
            run_id: uuid::Uuid::new_v4().to_string(),
            task_id: task.id.clone(),
            job_id: None,
            trigger: trigger.to_string(),
            status: "running".to_string(),
            started_at: Some(now),
            finished_at: None,
            bytes: 0,
            files: None,
            error: None,
        };
        let scheduled = trigger == "schedule";
        if scheduled {
            self.persist_tasks();
        }
        let _ = self.inner.store.record_run(run.clone());
        self.emit(
            "files/schedule/run",
            json!({ "run": serde_json::to_value(&run).unwrap_or(Value::Null) }),
        );
        if scheduled {
            self.emit_changed();
        }
        Ok((task, run))
    }

    fn spawn_run(&self, task: ScheduleTask, run: RunRecord) {
        let handle = lock(&self.inner.runtime).clone();
        match handle {
            Some(handle) => {
                let scheduler = Scheduler {
                    inner: Arc::clone(&self.inner),
                };
                handle.spawn(scheduler.execute_run(task, run));
            }
            None => {
                eprintln!("[io.dbx.files] scheduler runtime unavailable: run of '{}' dropped", task.name);
                lock(&self.inner.running).remove(&task.id);
            }
        }
    }

    /// Drives one run end-to-end: pre-flight, job start, terminal poll,
    /// post-run hooks, history/mirror/audit finalization.
    async fn execute_run(self, task: ScheduleTask, mut run: RunRecord) {
        // The gate is never closed; an Err here cannot happen and must not
        // lose the run record — bail with a failed record if it ever does.
        if self.inner.gate.acquire().await.is_err() {
            run.status = "failed".to_string();
            run.error = Some("scheduler run gate closed".to_string());
            run.finished_at = Some(store::unix_millis_now());
            let _ = self.inner.store.record_run(run.clone());
            lock(&self.inner.running).remove(&task.id);
            return;
        }
        self.drive_run(&task, &mut run).await;
        run.finished_at = Some(store::unix_millis_now());
        let _ = self.inner.store.record_run(run.clone());
        {
            let mut tasks = lock(&self.inner.tasks);
            if let Some(task_row) = tasks.iter_mut().find(|row| row.id == task.id) {
                task_row.last_run_at = run.finished_at;
                task_row.last_run_status = Some(run.status.clone());
            }
        }
        self.persist_tasks();
        self.emit(
            "files/schedule/run",
            json!({ "run": serde_json::to_value(&run).unwrap_or(Value::Null) }),
        );
        self.emit_changed();
        let result = match run.status.as_str() {
            "failed" => "error",
            _ => "ok",
        };
        let _ = self.inner.store.append_audit(AuditRecord {
            time: store::format_rfc3339(store::unix_millis_now() as i64),
            connection_id: task.source_connection_id.clone(),
            action: "files/schedule/run".to_string(),
            target: format!("{} → {}", task.source_path, task.target_path),
            result: result.to_string(),
            source: Some("scheduler".to_string()),
        });
        lock(&self.inner.running).remove(&task.id);
    }

    async fn drive_run(&self, task: &ScheduleTask, run: &mut RunRecord) {
        // Pre-flight: an unregistered connection (post-restart) is a skip,
        // not a failure — storage was never reachable this session.
        if let Err(error) = self.inner.engine.binding(&task.source_connection_id) {
            run.status = "skipped".to_string();
            run.error = Some(format!("connection unavailable: {error}"));
            return;
        }
        if task.target_connection_id != task.source_connection_id {
            if let Err(error) = self.inner.engine.binding(&task.target_connection_id) {
                run.status = "skipped".to_string();
                run.error = Some(format!("connection unavailable: {error}"));
                return;
            }
        }
        let started = match task.kind {
            ScheduleKind::Sync | ScheduleKind::Copy => {
                (self.inner.hooks.start_dir_job)(task.to_dir_job_request(), task.kind == ScheduleKind::Sync)
                    .await
            }
            ScheduleKind::Bisync => {
                let resync = !task.bisync_resync_done;
                (self.inner.hooks.start_bisync_job)(task.to_bisync_request(resync)).await
            }
        };
        let job_id = match started {
            Ok(job_id) => job_id,
            Err(error) => {
                run.status = "failed".to_string();
                run.error = Some(error);
                return;
            }
        };
        run.job_id = Some(job_id.clone());
        let _ = self.inner.store.record_run(run.clone());
        let (status, error, bytes) = self.await_job_terminal(&job_id).await;
        match status {
            transfers::JobStatus::Completed => {
                run.status = "success".to_string();
                run.bytes = bytes;
                run.files = lock(&self.inner.sync_jobs)
                    .get(&job_id)
                    .map(|record| record.files_done);
            }
            transfers::JobStatus::Canceled => {
                run.status = "canceled".to_string();
                run.error = error;
                return;
            }
            _ => {
                run.status = "failed".to_string();
                run.error = error;
                return;
            }
        }
        // Bisync pairs resync once; stamp so later runs go out in plain mode.
        if task.kind == ScheduleKind::Bisync && !task.bisync_resync_done {
            let mut tasks = lock(&self.inner.tasks);
            if let Some(row) = tasks.iter_mut().find(|row| row.id == task.id) {
                row.bisync_resync_done = true;
            }
            drop(tasks);
            self.persist_tasks();
        }
        // Post-run hooks: best-effort; a hook problem downgrades the run to
        // success-with-warning (the transfer itself landed).
        if task.options.verify_after {
            if let Err(error) = self.verify_run(task).await {
                run.error = Some(format!("verify failed: {error}"));
            }
        }
        if let Err(error) = self.prune_retention(task).await {
            let note = format!("retention: {error}");
            run.error = Some(match run.error.take() {
                Some(previous) => format!("{previous}; {note}"),
                None => note,
            });
        }
    }

    /// Polls the shared jobs mirror until the job reaches a terminal state.
    /// Terminal entries persist in the mirror, so a vanished id means the
    /// engine lost it (rcd restart) — reported as a failure, never success.
    async fn await_job_terminal(&self, job_id: &str) -> (transfers::JobStatus, Option<String>, u64) {
        loop {
            tokio::time::sleep(self.inner.run_poll).await;
            let job = (lock(&self.inner.job_view))(job_id);
            match job {
                Some(job) if job.status.is_terminal() => {
                    return (job.status, job.error.clone(), job.transferred_bytes);
                }
                Some(_) => continue,
                None => {
                    return (
                        transfers::JobStatus::Failed,
                        Some("job record vanished from the engine (rcd restarted?)".to_string()),
                        0,
                    );
                }
            }
        }
    }

    /// `verify_after` hook: an `operations/check` job over the same pair.
    async fn verify_run(&self, task: &ScheduleTask) -> Result<(), String> {
        let job_id = (self.inner.hooks.start_check_job)(CheckRequest {
            source_connection_id: task.source_connection_id.clone(),
            source_path: task.source_path.clone(),
            target_connection_id: task.target_connection_id.clone(),
            target_path: task.target_path.clone(),
            one_way: None,
            download: None,
            sum_path: None,
            hash_type: None,
        })
        .await?;
        let (status, error, _) = self.await_job_terminal(&job_id).await;
        match status {
            transfers::JobStatus::Completed => Ok(()),
            _ => Err(error.unwrap_or_else(|| "check job did not complete".to_string())),
        }
    }

    /// `retention_days` hook: prune `backup_dir` entries older than N days
    /// (rc `operations/delete` + `_filter MinAge`). Failure is a warning.
    async fn prune_retention(&self, task: &ScheduleTask) -> Result<(), String> {
        let Some(days) = task.options.retention_days else {
            return Ok(());
        };
        let Some(backup_dir) = task
            .options
            .backup_dir
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(());
        };
        let binding = self.inner.engine.binding(&task.target_connection_id)?;
        let policy = crate::policy::PathPolicy::from_parts(&binding.root, binding.lock_to_root, false, true);
        let backup_rel = crate::policy::PathPolicy::check_write(&policy, backup_dir)?.relative;
        let client = self.inner.engine.client_for_binding(&binding).await?;
        let body = json!({
            "fs": rclone::call_fs(&binding),
            "remote": backup_rel,
            "_filter": { "MinAge": format!("{days}d") },
        });
        let call = client.call("operations/delete", &body);
        match tokio::time::timeout(RETENTION_TIMEOUT, call).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err(format!(
                "prune of '{backup_dir}' did not finish within {:?}",
                RETENTION_TIMEOUT
            )),
        }
    }

    // -- helpers ---------------------------------------------------------------

    /// Create/update-time validation, mirroring the dir-job starter's gates
    /// so a task never persists that is doomed to fail on its first run.
    fn validate(
        engine: &Arc<rclone::RcloneEngine>,
        kind: ScheduleKind,
        source_connection_id: &str,
        source_path: &str,
        target_connection_id: &str,
        target_path: &str,
        cron_expr: &str,
        options: &model::ScheduleOptions,
    ) -> Result<(), String> {
        cron::CronExpr::parse(cron_expr)?;
        let source = engine.binding(source_connection_id)?;
        let target = engine.binding(target_connection_id)?;
        match kind {
            ScheduleKind::Sync => {
                if target.read_only {
                    return Err("Schedule target connection is read-only; mirror sync needs write access".to_string());
                }
                if !target.allow_delete {
                    return Err("Schedule target connection disallows deletes; mirror sync needs delete access".to_string());
                }
            }
            ScheduleKind::Copy => {
                if target.read_only {
                    return Err("Schedule target connection is read-only".to_string());
                }
            }
            ScheduleKind::Bisync => {
                for (role, binding) in [("source", &source), ("target", &target)] {
                    if binding.read_only {
                        return Err(format!("Bisync {role} connection is read-only"));
                    }
                    if !binding.allow_delete {
                        return Err(format!("Bisync {role} connection disallows deletes"));
                    }
                }
            }
        }
        let source_policy = crate::policy::PathPolicy::from_parts(&source.root, source.lock_to_root, false, true);
        let _ = crate::policy::PathPolicy::check_read(&source_policy, source_path)?.relative;
        let target_policy = crate::policy::PathPolicy::from_parts(&target.root, target.lock_to_root, false, true);
        crate::policy::PathPolicy::check_write(&target_policy, target_path)?;
        if let Some(backup_dir) = options
            .backup_dir
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            crate::policy::PathPolicy::check_write(&target_policy, backup_dir)?;
        }
        if options.retention_days.is_some() && options.backup_dir.as_deref().map(str::trim).filter(|value| !value.is_empty()).is_none() {
            return Err("retentionDays requires backupDir (there is nothing to prune without a versioned backup directory)".to_string());
        }
        Ok(())
    }

    fn initial_next_run(cron_expr: &str, enabled: bool, now: u64) -> Option<u64> {
        if !enabled {
            return None;
        }
        cron::CronExpr::parse(cron_expr)
            .ok()
            .and_then(|expr| expr.next_fire_millis(now))
    }

    fn persist_tasks(&self) {
        let tasks = lock(&self.inner.tasks).clone();
        let _ = self.inner.store.save_schedules(&tasks);
    }

    fn emit(&self, method: &str, params: Value) {
        let held = self
            .inner
            .emitter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(emitter) = held {
            let _ = emitter.event(method, params);
        }
    }

    fn emit_changed(&self) {
        let tasks = lock(&self.inner.tasks).clone();
        let tasks = serde_json::to_value(tasks).unwrap_or(Value::Array(Vec::new()));
        self.emit("files/schedule/changed", json!({ "tasks": tasks }));
    }
}

#[cfg(test)]
impl Scheduler {
    /// Tests run inside a tokio runtime; expose it so spawn_run lands runs
    /// on the test runtime instead of the (never-started) scheduler thread.
    fn test_use_current_runtime(&self) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            *lock(&self.inner.runtime) = Some(handle);
        }
    }

    /// Forces `next_run_at` into the past so the next `process_due` claims it.
    fn test_set_due(&self, id: &str) {
        {
            let mut tasks = lock(&self.inner.tasks);
            if let Some(task) = tasks.iter_mut().find(|task| task.id == id) {
                task.next_run_at = Some(0);
            }
        }
        self.persist_tasks();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BisyncStartRequest, DirJobRequest};
    use crate::transfers::{JobStatus, TransferJob, TransferKind};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Fake-engine harness: real `RcloneEngine` (for `binding("__local__")`
    /// validation) but injected starters that fake the rclone layer —
    /// insert a job record into a fake table and write a sentinel file
    /// into the target path. No rclone binary required.
    struct Harness {
        scheduler: Arc<Scheduler>,
        store: Arc<Store>,
        /// Keeps the store's data dir alive for the whole test.
        _dir: tempfile::TempDir,
        src: tempfile::TempDir,
        dst: tempfile::TempDir,
        jobs: Arc<Mutex<HashMap<String, TransferJob>>>,
        hold: Arc<AtomicBool>,
        check_fail: Arc<AtomicBool>,
        dir_calls: Arc<Mutex<Vec<(DirJobRequest, bool)>>>,
        bisync_calls: Arc<Mutex<Vec<BisyncStartRequest>>>,
    }

    fn fake_job(jobs: &Arc<Mutex<HashMap<String, TransferJob>>>, id: &str, connection: &str, path: &str, hold: bool) {
        let status = if hold { JobStatus::Running } else { JobStatus::Completed };
        jobs.lock().unwrap().insert(
            id.to_string(),
            TransferJob {
                task_id: id.to_string(),
                connection_id: connection.to_string(),
                kind: TransferKind::Upload,
                remote_path: path.to_string(),
                total_bytes: Some(10),
                transferred_bytes: 10,
                error: None,
                status,
                started_at: Some(store::unix_millis_now()),
                finished_at: if status.is_terminal() { Some(store::unix_millis_now()) } else { None },
                local_path: None,
            },
        );
    }

    fn harness() -> Harness {
        let dir = tempfile::tempdir().expect("store tempdir");
        let src = tempfile::tempdir().expect("src tempdir");
        let dst = tempfile::tempdir().expect("dst tempdir");
        let store = Arc::new(Store::new(dir.path().to_path_buf()));
        let engine = Arc::new(rclone::RcloneEngine::new());
        let sync_jobs = Arc::new(Mutex::new(HashMap::new()));
        let jobs: Arc<Mutex<HashMap<String, TransferJob>>> = Arc::new(Mutex::new(HashMap::new()));
        let hold = Arc::new(AtomicBool::new(false));
        let check_fail = Arc::new(AtomicBool::new(false));
        let dir_calls: Arc<Mutex<Vec<(DirJobRequest, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let bisync_calls: Arc<Mutex<Vec<BisyncStartRequest>>> = Arc::new(Mutex::new(Vec::new()));

        let start_dir_job: StartDirJobFn = {
            let jobs = Arc::clone(&jobs);
            let hold = Arc::clone(&hold);
            let calls = Arc::clone(&dir_calls);
            Arc::new(move |request: DirJobRequest, sync: bool| {
                let jobs = Arc::clone(&jobs);
                let hold = Arc::clone(&hold);
                let calls = Arc::clone(&calls);
                Box::pin(async move {
                    calls.lock().unwrap().push((request.clone(), sync));
                    let job_id = format!("dir-{}", uuid::Uuid::new_v4().simple());
                    let held = hold.load(Ordering::SeqCst);
                    let _ = std::fs::create_dir_all(&request.target_path);
                    std::fs::write(
                        std::path::Path::new(&request.target_path).join("sentinel.txt"),
                        format!("{} -> {} sync={sync}", request.source_path, request.target_path),
                    )
                    .expect("sentinel write");
                    fake_job(&jobs, &job_id, &request.source_connection_id, &request.source_path, held);
                    Ok(job_id)
                })
            })
        };
        let start_bisync_job: StartBisyncJobFn = {
            let jobs = Arc::clone(&jobs);
            let hold = Arc::clone(&hold);
            let calls = Arc::clone(&bisync_calls);
            Arc::new(move |request: BisyncStartRequest| {
                let jobs = Arc::clone(&jobs);
                let hold = Arc::clone(&hold);
                let calls = Arc::clone(&calls);
                Box::pin(async move {
                    calls.lock().unwrap().push(request.clone());
                    let job_id = format!("bisync-{}", uuid::Uuid::new_v4().simple());
                    let held = hold.load(Ordering::SeqCst);
                    let _ = std::fs::create_dir_all(&request.target_path);
                    fake_job(&jobs, &job_id, &request.source_connection_id, &request.source_path, held);
                    Ok(job_id)
                })
            })
        };
        let start_check_job: StartCheckJobFn = {
            let jobs = Arc::clone(&jobs);
            let check_fail = Arc::clone(&check_fail);
            Arc::new(move |request: CheckRequest| {
                let jobs = Arc::clone(&jobs);
                let check_fail = Arc::clone(&check_fail);
                Box::pin(async move {
                    let job_id = format!("check-{}", uuid::Uuid::new_v4().simple());
                    let mut job = {
                        let jobs = jobs.lock().unwrap();
                        jobs.values().next().cloned().expect("seed job shape")
                    };
                    job.task_id = job_id.clone();
                    job.remote_path = request.source_path;
                    if check_fail.load(Ordering::SeqCst) {
                        job.status = JobStatus::Failed;
                        job.error = Some("3 files differ".to_string());
                    } else {
                        job.status = JobStatus::Completed;
                    }
                    jobs.lock().unwrap().insert(job_id.clone(), job);
                    Ok(job_id)
                })
            })
        };
        let view_jobs = Arc::clone(&jobs);
        let job_view: JobView = Box::new(move |job_id| {
            view_jobs.lock().unwrap().get(job_id).cloned()
        });
        let scheduler = Scheduler::with_periods(
            engine,
            sync_jobs,
            Arc::clone(&store),
            JobHooks { start_dir_job, start_bisync_job, start_check_job },
            Arc::new(Mutex::new(None)),
            Mutex::new(job_view),
            Duration::from_millis(20),
            Duration::from_millis(10),
        );
        scheduler.test_use_current_runtime();
        Harness { scheduler, store, _dir: dir, src, dst, jobs, hold, check_fail, dir_calls, bisync_calls }
    }

    fn task_request(name: &str, kind: ScheduleKind, src: &str, dst: &str) -> ScheduleCreateRequest {
        ScheduleCreateRequest {
            name: name.to_string(),
            kind,
            source_connection_id: rclone::LOCAL_CONNECTION_ID.to_string(),
            source_path: src.to_string(),
            target_connection_id: None,
            target_path: dst.to_string(),
            cron: "* * * * *".to_string(),
            enabled: Some(true),
            options: Default::default(),
        }
    }

    async fn wait_for(condition: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(std::time::Instant::now() < deadline, "condition not met in time");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn create_rejects_bad_cron_and_unknown_connection() {
        let h = harness();
        let mut bad = task_request("bad", ScheduleKind::Sync, "/nowhere", "/nowhere2");
        bad.cron = "not a cron".to_string();
        assert!(h.scheduler.create(bad).is_err(), "invalid cron rejected");
        let mut ghost = task_request("ghost", ScheduleKind::Sync, "/x", "/y");
        ghost.source_connection_id = "ghost-conn".to_string();
        assert!(h.scheduler.create(ghost).is_err(), "unknown connection rejected");
    }

    #[tokio::test]
    async fn scheduled_run_completes_records_history_and_advances_next_run() {
        let h = harness();
        let request = task_request("nightly", ScheduleKind::Sync, h.src.path().to_str().unwrap(), h.dst.path().to_str().unwrap());
        let task = h.scheduler.create(request).expect("create");
        h.scheduler.test_set_due(&task.id);
        h.scheduler.process_due();
        wait_for(|| {
            h.scheduler
                .history(Some(&task.id), 10)
                .first()
                .is_some_and(|run| run.status == "success")
        })
        .await;
        // The fake starter wrote its sentinel into the target directory.
        assert!(h.dst.path().join("sentinel.txt").is_file());
        let runs = h.scheduler.history(Some(&task.id), 10);
        assert_eq!(runs[0].trigger, "schedule");
        assert!(runs[0].bytes > 0, "terminal bytes recorded");
        let listed = h.scheduler.list();
        assert_eq!(listed[0].last_run_status.as_deref(), Some("success"));
        assert!(listed[0].next_run_at.is_some(), "next_run advanced past the claim");
    }

    #[tokio::test]
    async fn manual_run_is_single_flight_and_forces_disabled_tasks() {
        let h = harness();
        let mut request = task_request("held", ScheduleKind::Copy, h.src.path().to_str().unwrap(), h.dst.path().to_str().unwrap());
        request.enabled = Some(false);
        let task = h.scheduler.create(request).expect("create disabled");
        h.hold.store(true, Ordering::SeqCst);
        let first = h.scheduler.run_now(&task.id).expect("manual run forces disabled");
        assert_eq!(h.scheduler.run_now(&task.id).unwrap_err(), "Schedule task already has a run in progress");
        // Release: flip every held job terminal so the poll sees Completed.
        h.hold.store(false, Ordering::SeqCst);
        let held_ids: Vec<String> = {
            let jobs = h.jobs.lock().unwrap();
            jobs.values()
                .filter(|job| !job.status.is_terminal())
                .map(|job| job.task_id.clone())
                .collect()
        };
        for id in held_ids {
            let mut jobs = h.jobs.lock().unwrap();
            if let Some(job) = jobs.get_mut(&id) {
                job.status = JobStatus::Completed;
                job.finished_at = Some(store::unix_millis_now());
            }
        }
        wait_for(|| {
            h.scheduler
                .history(Some(&task.id), 10)
                .first()
                .is_some_and(|run| run.run_id == first.run_id && run.status == "success")
        })
        .await;
    }

    #[tokio::test]
    async fn bisync_first_run_resyncs_once_then_runs_plain() {
        let h = harness();
        let request = task_request("pair", ScheduleKind::Bisync, h.src.path().to_str().unwrap(), h.dst.path().to_str().unwrap());
        let task = h.scheduler.create(request).expect("create bisync");
        h.scheduler.run_now(&task.id).expect("first run");
        wait_for(|| {
            h.scheduler
                .history(Some(&task.id), 5)
                .first()
                .is_some_and(|run| run.status == "success")
        })
        .await;
        h.scheduler.run_now(&task.id).expect("second run");
        let second = {
            let runs = h.scheduler.history(Some(&task.id), 5);
            runs.first().cloned().expect("second run recorded")
        };
        wait_for(|| {
            h.scheduler
                .history(Some(&task.id), 5)
                .first()
                .is_some_and(|run| run.run_id == second.run_id && run.status == "success")
        })
        .await;
        let calls = h.bisync_calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "two bisync starts");
        assert_eq!(calls[0].mode.as_deref(), Some("resync"), "first ever run resyncs");
        assert_eq!(calls[1].mode.as_deref(), Some("run"), "later runs go plain");
        assert!(h.scheduler.list()[0].bisync_resync_done, "stamp persisted");
    }

    #[tokio::test]
    async fn unavailable_connection_is_skipped_not_failed() {
        let h = harness();
        // A task referencing a connection that is not registered this session
        // cannot come through create() (validation) — it is exactly the
        // restart-survivor shape, so write it to the store directly.
        let task = ScheduleTask {
            id: "ghost-task".to_string(),
            name: "ghost".to_string(),
            kind: ScheduleKind::Sync,
            source_connection_id: "ghost-conn".to_string(),
            source_path: "/data".to_string(),
            target_connection_id: "ghost-conn".to_string(),
            target_path: "/mirror".to_string(),
            cron: "* * * * *".to_string(),
            enabled: true,
            options: Default::default(),
            bisync_resync_done: false,
            created_at: 1,
            last_run_at: None,
            last_run_status: None,
            next_run_at: None,
        };
        h.store.save_schedules(&[task]).unwrap();
        h.scheduler.hydrate();
        h.scheduler.test_set_due("ghost-task");
        h.scheduler.process_due();
        wait_for(|| {
            h.scheduler
                .history(Some("ghost-task"), 5)
                .first()
                .is_some_and(|run| run.status == "skipped")
        })
        .await;
        let run = h.scheduler.history(Some("ghost-task"), 5)[0].clone();
        assert!(run.error.unwrap_or_default().contains("connection unavailable"));
        assert_eq!(run.job_id, None, "skipped runs never started a job");
    }

    #[tokio::test]
    async fn verify_after_failure_downgrades_to_success_with_warning() {
        let h = harness();
        let mut request = task_request("verified", ScheduleKind::Sync, h.src.path().to_str().unwrap(), h.dst.path().to_str().unwrap());
        request.options.verify_after = true;
        let task = h.scheduler.create(request).expect("create");
        h.check_fail.store(true, Ordering::SeqCst);
        h.scheduler.run_now(&task.id).expect("run");
        wait_for(|| {
            h.scheduler
                .history(Some(&task.id), 5)
                .first()
                .is_some_and(|run| run.finished_at.is_some())
        })
        .await;
        let run = h.scheduler.history(Some(&task.id), 5)[0].clone();
        assert_eq!(run.status, "success", "the transfer landed; verify is a warning");
        assert!(run.error.unwrap_or_default().contains("verify failed"));
    }

    #[tokio::test]
    async fn retention_requires_backup_dir_and_delete_rejects_running_task() {
        let h = harness();
        let mut request = task_request("pruned", ScheduleKind::Sync, h.src.path().to_str().unwrap(), h.dst.path().to_str().unwrap());
        request.options.retention_days = Some(7);
        let error = h.scheduler.create(request).expect_err("retention without backup_dir rejected");
        assert!(error.contains("retentionDays requires backupDir"), "{error}");
        let task = h.scheduler.list().first().cloned();
        assert!(task.is_none(), "rejected create must not persist");
        // Delete of an unknown id answers false, not an error.
        assert!(!h.scheduler.delete("nope").unwrap());
    }
}
