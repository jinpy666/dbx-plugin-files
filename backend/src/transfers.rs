//! Async transfer jobs (implementation doc §7/§8.3/§8.4).
//!
//! Semantics:
//! - state machine `queued → running → completed | failed | canceled`;
//! - global concurrency cap of 3 (`tokio::sync::Semaphore`) and
//!   per-`connectionId` FIFO serialization for spawned jobs (download pumps
//!   and syncDir/copyDir traversals; uploads are host-driven by frame order);
//! - progress events `files/transfer/progress` throttled to >= 200 ms OR
//!   >= 1% delta (`model::PROGRESS_INTERVAL_MS` / `PROGRESS_MIN_DELTA`); state
//!   transitions always emit;
//! - finished history persisted to `store/transfers.json` (ring cap 200,
//!   `model::TRANSFER_HISTORY_LIMIT`) via a lazily attached `Store` (the
//!   constructor stays side-effect free);
//! - binary channel framing aligned with ssh-sftp: payload = 8-byte BE
//!   offset + chunk (<= 256 KiB, `model::TRANSFER_CHUNK_SIZE`); upload
//!   offsets are validated in-band (misaligned chunk = hard error), size
//!   verification happens at `finish`.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dbx_plugin_sdk::PluginEmitter;
use opendal::Writer;
use tokio::sync::Mutex;

use crate::engine::transfer as slot;
use crate::model::{
    StoredConnection, PROGRESS_INTERVAL_MS, PROGRESS_MIN_DELTA, TRANSFER_CHUNK_SIZE,
    TRANSFER_HISTORY_LIMIT,
};
use crate::store::{self, Store};

/// issue#6-6：`finish_upload` 等待在途上传帧的采样间隔与停滞判定窗口。
/// 宿主 sendBinary 是即发即忘队列，`files/upload/finish` 可能越过仍在排队的
/// 帧；只要字节数持续前进就继续等，停滞 `UPLOAD_FINISH_STALL_TICKS` 轮
/// （40 × 250ms = 10s）才判不完整。
const UPLOAD_FINISH_TICK: Duration = Duration::from_millis(250);
const UPLOAD_FINISH_STALL_TICKS: u32 = 40;

/// Direction of a transfer job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferKind {
    Upload,
    Download,
}

/// Job lifecycle state (§7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Canceled,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Completed => "completed",
            JobStatus::Failed => "failed",
            JobStatus::Canceled => "canceled",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobStatus::Completed | JobStatus::Failed | JobStatus::Canceled
        )
    }
}

/// One transfer job record — also the payload of `files/transfer/status` and
/// an element of `files/transfers/list` (camelCase over the wire).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferJob {
    /// Same id as the binary-channel taskId for single-file transfers.
    pub task_id: String,
    pub connection_id: String,
    pub kind: TransferKind,
    /// Remote path (upload target / download source).
    pub remote_path: String,
    /// Expected size (upload) or stat'ed size (download); `None` when unknown.
    pub total_bytes: Option<u64>,
    pub transferred_bytes: u64,
    pub status: JobStatus,
    /// Failure detail when `status == Failed` (never contains credentials).
    pub error: Option<String>,
    /// Unix epoch millis.
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
}

/// What triggered a dir-table job. Pure display/routing metadata: the engine
/// branches on `sync` / `delete_source`, the UI renders the kind label and
/// `files/transfers/list` needs it to unify dir jobs with single-file jobs
/// (P-FILES ①a).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DirJobKind {
    SyncDir,
    CopyDir,
    /// Degraded `files/copy` (read→write job).
    Copy,
    /// Degraded `files/move` and degraded directory rename (`files/rename` on
    /// a dir): copy succeeds first, then the source is deleted.
    Move,
    /// Degraded directory rename (copy + delete of the source dir).
    Rename,
    /// Archive extraction (`files/extract` on a big package): tar/tar.gz
    /// entries are read from the source archive and written under the
    /// target directory (B-ARCHIVE route).
    Extract,
    /// Archive creation (`files/compress` beyond the synchronous inline
    /// budget): planned sources are streamed into one tar / tar.gz file
    /// (P-FILES round 13).
    Compress,
}

/// Directory sync/copy job record (§7 self-built traversal job).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirJob {
    pub job_id: String,
    pub source_connection_id: String,
    pub source_path: String,
    pub target_connection_id: String,
    pub target_path: String,
    /// `syncDir` additionally deletes extras on the target; `copyDir` never.
    pub sync: bool,
    /// X-A ③: degraded `files/move` — the source tree is deleted after the
    /// copy succeeded (never for `files/copy`, `copyDir`, `syncDir`).
    pub delete_source: bool,
    /// Triggering operation (UI label; P-FILES ①a).
    pub kind: DirJobKind,
    pub files_done: u64,
    pub files_total: Option<u64>,
    pub bytes_done: u64,
    pub bytes_total: Option<u64>,
    pub status: JobStatus,
    pub error: Option<String>,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
}

// ---------------------------------------------------------------------------
// Internal slot/handle state
// ---------------------------------------------------------------------------

/// Host-driven upload slot: the OpenDAL writer plus the expected position.
struct UploadSlot {
    #[allow(dead_code)]
    connection_id: String,
    declared_size: u64,
    received: u64,
    writer: Writer,
    throttle: Throttle,
}

/// Download slot: cancellation is cooperative — the pump checks the flag
/// between chunks (CancellationToken semantics via an atomic flag; no new
/// crate dependency).
struct DownloadSlot {
    connection_id: String,
    remote_path: String,
    #[allow(dead_code)]
    size: u64,
    cancel: Arc<AtomicBool>,
}

/// Progress throttle: emit when >= 200 ms elapsed OR >= 1% fraction changed;
/// the first observation always emits.
#[derive(Default)]
pub(crate) struct Throttle {
    last_ms: Option<u64>,
    last_fraction: f64,
}

impl Throttle {
    pub(crate) fn should_emit(&mut self, transferred: u64, total: Option<u64>) -> bool {
        let now = store::unix_millis_now();
        let fraction = match total {
            Some(total) if total > 0 => (transferred as f64 / total as f64).min(1.0),
            _ => 0.0,
        };
        let due = match self.last_ms {
            None => true,
            Some(last_ms) => {
                now.saturating_sub(last_ms) >= PROGRESS_INTERVAL_MS
                    || (fraction - self.last_fraction).abs() >= PROGRESS_MIN_DELTA
            }
        };
        if due {
            self.last_ms = Some(now);
            self.last_fraction = fraction;
        }
        due
    }
}

/// Shared registry captured by spawned pump/traversal tasks.
struct Inner {
    jobs: Mutex<HashMap<String, TransferJob>>,
    dir_jobs: Mutex<HashMap<String, DirJob>>,
    uploads: Mutex<HashMap<String, UploadSlot>>,
    downloads: Mutex<HashMap<String, DownloadSlot>>,
    /// Cooperative cancel flags for directory jobs, keyed by jobId.
    dir_controls: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// Per-connection FIFO serialization for spawned jobs (§7).
    conn_locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Global concurrency cap (§7: 3 concurrent jobs).
    semaphore: Arc<tokio::sync::Semaphore>,
    /// Lazily attached store for history persistence (`transfers.json`).
    store: tokio::sync::OnceCell<Arc<Store>>,
    /// In-memory mirror of the persisted history (hydration + tests).
    history: std::sync::Mutex<Vec<store::TransferRecord>>,
}

impl Inner {
    fn conn_lock(&self, connection_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .conn_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks
            .entry(connection_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Moves a single-file job to a terminal state exactly once, persists the
    /// history record and emits the final progress event. Returns `true` when
    /// this call performed the transition.
    async fn complete_job(
        &self,
        task_id: &str,
        status: JobStatus,
        error: Option<String>,
        emitter: &PluginEmitter,
    ) -> bool {
        let job = {
            let mut jobs = self.jobs.lock().await;
            let Some(job) = jobs.get_mut(task_id) else {
                return false;
            };
            if job.status.is_terminal() {
                return false;
            }
            job.status = status;
            job.error = error;
            job.finished_at = Some(store::unix_millis_now());
            job.clone()
        };
        self.record_history(&job).await;
        emit_job_progress(emitter, &job);
        true
    }

    /// Dir-job twin of [`complete_job`]; also drops the cancel control.
    async fn complete_dir_job(
        &self,
        job_id: &str,
        status: JobStatus,
        error: Option<String>,
        emitter: &PluginEmitter,
    ) -> bool {
        let job = {
            let mut dir_jobs = self.dir_jobs.lock().await;
            let Some(job) = dir_jobs.get_mut(job_id) else {
                return false;
            };
            if job.status.is_terminal() {
                return false;
            }
            job.status = status;
            job.error = error;
            job.finished_at = Some(store::unix_millis_now());
            job.clone()
        };
        self.dir_controls.lock().await.remove(job_id);
        emit_dir_progress(emitter, &job);
        true
    }

    /// Persists one finished transfer (ring cap 200). The store is attached
    /// lazily so `JobTable::new()` stays side-effect free; write failures are
    /// logged and never fail the transfer itself.
    async fn record_history(&self, job: &TransferJob) {
        let record = store::TransferRecord {
            task_id: job.task_id.clone(),
            connection_id: job.connection_id.clone(),
            kind: match job.kind {
                TransferKind::Upload => "upload",
                TransferKind::Download => "download",
            }
            .to_string(),
            remote_path: job.remote_path.clone(),
            total_bytes: job.total_bytes,
            transferred_bytes: job.transferred_bytes,
            status: job.status.as_str().to_string(),
            error: job.error.clone(),
            started_at: job.started_at,
            finished_at: job.finished_at,
        };
        {
            let mut cache = self
                .history
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cache.push(record.clone());
            let overflow = cache.len().saturating_sub(TRANSFER_HISTORY_LIMIT);
            cache.drain(..overflow);
        }
        let store = self
            .store
            .get_or_init(|| async { Arc::new(Store::new(Store::default_dir())) })
            .await;
        if let Err(error) = store.record_transfer(record) {
            eprintln!("[io.dbx.files] transfer history write failed: {error}");
        }
    }

    /// Marks a queued job `running` (no-op otherwise).
    async fn mark_running(&self, task_id: &str) {
        if let Some(job) = self.jobs.lock().await.get_mut(task_id) {
            if job.status == JobStatus::Queued {
                job.status = JobStatus::Running;
            }
        }
    }

    async fn job_is_terminal(&self, task_id: &str) -> bool {
        self.jobs
            .lock()
            .await
            .get(task_id)
            .map(|job| job.status.is_terminal())
            .unwrap_or(false)
    }

    async fn job_exists(&self, task_id: &str) -> bool {
        self.jobs.lock().await.contains_key(task_id)
    }
}

/// Shared job registry handed to `main.rs`.
pub struct JobTable {
    inner: Arc<Inner>,
}

impl JobTable {
    /// Constructs the empty registry with the global semaphore (permits = 3).
    /// No side effects: the history store is attached lazily on first record.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                jobs: Mutex::new(HashMap::new()),
                dir_jobs: Mutex::new(HashMap::new()),
                uploads: Mutex::new(HashMap::new()),
                downloads: Mutex::new(HashMap::new()),
                dir_controls: Mutex::new(HashMap::new()),
                conn_locks: std::sync::Mutex::new(HashMap::new()),
                semaphore: Arc::new(tokio::sync::Semaphore::new(3)),
                store: tokio::sync::OnceCell::new(),
                history: std::sync::Mutex::new(Vec::new()),
            }),
        }
    }
}

impl Default for JobTable {
    fn default() -> Self {
        Self::new()
    }
}

impl JobTable {
    /// `files/upload/start` (§8.3): `remotePath`, `size` → `{taskId}`.
    ///
    /// Creates the job (`queued`), allocates the OpenDAL writer
    /// (`chunk(4MiB).concurrent(4)`), and registers it so binary frames on
    /// `files/upload/{taskId}` land in the right slot. Fails if the
    /// connection is unknown or `read_only`.
    pub async fn start_upload(
        &self,
        connection: &StoredConnection,
        operator: &opendal::Operator,
        remote_path: &str,
        size: u64,
        emitter: &PluginEmitter,
    ) -> Result<String, String> {
        if connection.read_only {
            return Err("Connection is read-only; write operations are rejected".to_string());
        }
        let writer = slot::open_upload_writer(operator, remote_path).await?;
        let task_id = uuid::Uuid::new_v4().to_string();
        let now = store::unix_millis_now();
        let job = TransferJob {
            task_id: task_id.clone(),
            connection_id: connection.id.clone(),
            kind: TransferKind::Upload,
            remote_path: remote_path.to_string(),
            total_bytes: Some(size),
            transferred_bytes: 0,
            status: JobStatus::Queued,
            error: None,
            started_at: Some(now),
            finished_at: None,
        };
        self.inner
            .jobs
            .lock()
            .await
            .insert(task_id.clone(), job.clone());
        self.inner.uploads.lock().await.insert(
            task_id.clone(),
            UploadSlot {
                connection_id: connection.id.clone(),
                declared_size: size,
                received: 0,
                writer,
                throttle: Throttle::default(),
            },
        );
        emit_job_progress(emitter, &job);
        Ok(task_id)
    }

    /// Binary-channel ingest for `files/upload/{taskId}` (§8.3).
    ///
    /// `data` = 8-byte BE offset + payload (<= 256 KiB). The offset must
    /// match the expected writer position exactly — a misaligned frame is a
    /// hard error (aligned with ssh-sftp `append_upload`); the chunk bytes are
    /// forwarded to the OpenDAL writer. Emits throttled progress.
    pub async fn append_upload(
        &self,
        task_id: &str,
        data: &[u8],
        emitter: &PluginEmitter,
    ) -> Result<(), String> {
        let (offset, payload) = parse_upload_frame(data)?;
        let (transferred, total, emit) = {
            let mut uploads = self.inner.uploads.lock().await;
            let Some(upload) = uploads.get_mut(task_id) else {
                return Err("Upload task was not found".to_string());
            };
            if upload.received != offset {
                return Err(format!(
                    "Upload offset mismatch: expected {}, received {offset}",
                    upload.received
                ));
            }
            if upload
                .received
                .saturating_add(payload.len() as u64)
                > upload.declared_size
            {
                return Err("Upload exceeds the declared file size".to_string());
            }
            if let Err(error) = upload.writer.write(payload.to_vec()).await {
                uploads.remove(task_id);
                let detail = format!("Upload write failed: {error}");
                self.inner
                    .complete_job(task_id, JobStatus::Failed, Some(detail.clone()), emitter)
                    .await;
                return Err(detail);
            }
            upload.received = upload.received.saturating_add(payload.len() as u64);
            let emit = upload
                .throttle
                .should_emit(upload.received, Some(upload.declared_size));
            (upload.received, Some(upload.declared_size), emit)
        };
        {
            let mut jobs = self.inner.jobs.lock().await;
            if let Some(job) = jobs.get_mut(task_id) {
                job.transferred_bytes = transferred;
                job.status = JobStatus::Running;
            }
        }
        if emit {
            let mut event = progress_event(task_id, transferred, total);
            if let Some(object) = event.as_object_mut() {
                object.insert("size".into(), serde_json::json!(total));
                object.insert(
                    "state".into(),
                    serde_json::json!(JobStatus::Running.as_str()),
                );
            }
            let _ = emitter.event("files/transfer/progress", event);
        }
        Ok(())
    }

    /// `files/upload/finish` (§8.3): closes the writer, verifies the written
    /// size matches the declared `size`, marks the job `completed`, and
    /// records it into the history store.
    pub async fn finish_upload(
        &self,
        task_id: &str,
        emitter: &PluginEmitter,
    ) -> Result<(), String> {
        // issue#6-3：终态 job 不再一律幂等成功——只有 Completed 才返回 Ok；
        // Failed/Canceled 把存储的错误带回去，宿主侧 finish 如实落失败
        // （此前写中途失败后再 finish 会拿到 Ok，前端标 completed 并提示
        // 「已上传」，形成假成功）。
        if self.inner.job_is_terminal(task_id).await {
            if let Some(job) = self.status(task_id).await? {
                return Self::terminal_finish_result(&job);
            }
            return Err("Upload task was not found".to_string());
        }
        let mut slot = {
            let mut uploads = self.inner.uploads.lock().await;
            if !uploads.contains_key(task_id) {
                return Err("Upload task was not found".to_string());
            }
            // issue#6-6：finish 可能越过宿主仍在排队的在途帧，received 暂时
            // 小于声明值。这里等待在途帧落地：字节数仍在前进就继续等，停滞
            // 超过 UPLOAD_FINISH_STALL_TICKS 才判不完整（真丢帧时也只多等
            // 10s，而不是把排队中的合法上传误杀）。
            let mut last = uploads.get(task_id).map(|s| s.received).unwrap_or(0);
            let mut stall_ticks: u32 = 0;
            while uploads
                .get(task_id)
                .map(|s| s.received < s.declared_size)
                .unwrap_or(false)
            {
                if stall_ticks >= UPLOAD_FINISH_STALL_TICKS {
                    break;
                }
                drop(uploads);
                tokio::time::sleep(UPLOAD_FINISH_TICK).await;
                uploads = self.inner.uploads.lock().await;
                let Some(current) = uploads.get(task_id) else {
                    // 等待期间写失败：append_upload 已摘除 slot 并落 Failed，
                    // 带出存储的错误而不是笼统的 not found。
                    let stored = self
                        .status(task_id)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|job| job.error);
                    return Err(stored.unwrap_or_else(|| "Upload task was not found".to_string()));
                };
                if current.received == last {
                    stall_ticks += 1;
                } else {
                    stall_ticks = 0;
                    last = current.received;
                }
            }
            uploads.remove(task_id).expect("slot checked above")
        };
        if slot.received != slot.declared_size {
            let detail = format!(
                "Upload is incomplete: expected {}, received {}",
                slot.declared_size, slot.received
            );
            self.inner
                .complete_job(task_id, JobStatus::Failed, Some(detail.clone()), emitter)
                .await;
            return Err(detail);
        }
        let closed = slot.writer.close().await;
        match closed {
            Err(error) => {
                let detail = format!("Failed to close upload writer: {error}");
                self.inner
                    .complete_job(task_id, JobStatus::Failed, Some(detail.clone()), emitter)
                    .await;
                Err(detail)
            }
            Ok(metadata) => {
                let written = metadata.content_length();
                if written > 0 && written != slot.declared_size {
                    let detail = format!(
                        "Upload size mismatch: declared {}, backend stored {written}",
                        slot.declared_size
                    );
                    self.inner
                        .complete_job(task_id, JobStatus::Failed, Some(detail.clone()), emitter)
                        .await;
                    return Err(detail);
                }
                self.inner
                    .complete_job(task_id, JobStatus::Completed, None, emitter)
                    .await;
                Ok(())
            }
        }
    }

/// issue#6-3：终态 finish 的语义映射（纯函数便于单测）。Completed 幂等成功；
/// Failed 带出存储的错误；Canceled 如实报取消。
fn terminal_finish_result(job: &TransferJob) -> Result<(), String> {
    match job.status {
        JobStatus::Completed => Ok(()),
        JobStatus::Canceled => Err("Upload was canceled".to_string()),
        _ => Err(job.error.clone().unwrap_or_else(|| "Upload failed".to_string())),
    }
}

    /// `files/download/start` (§8.3): `remotePath` → `{taskId, size}`.
    ///
    /// Opens an OpenDAL reader (prefetch), stats the size, and spawns the
    /// pump task that pushes `files/download/{taskId}` binary frames
    /// (8-byte BE offset + <= 256 KiB payload, sidecar → host direction) via
    /// `emitter.binary`. The pump honours cancellation.
    pub async fn start_download(
        &self,
        connection: &StoredConnection,
        operator: &opendal::Operator,
        remote_path: &str,
        emitter: &PluginEmitter,
    ) -> Result<(String, u64), String> {
        let (reader, size) = slot::open_download_reader(operator, remote_path).await?;
        let task_id = uuid::Uuid::new_v4().to_string();
        let now = store::unix_millis_now();
        let job = TransferJob {
            task_id: task_id.clone(),
            connection_id: connection.id.clone(),
            kind: TransferKind::Download,
            remote_path: remote_path.to_string(),
            total_bytes: Some(size),
            transferred_bytes: 0,
            status: JobStatus::Queued,
            error: None,
            started_at: Some(now),
            finished_at: None,
        };
        self.inner
            .jobs
            .lock()
            .await
            .insert(task_id.clone(), job.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        self.inner.downloads.lock().await.insert(
            task_id.clone(),
            DownloadSlot {
                connection_id: connection.id.clone(),
                remote_path: remote_path.to_string(),
                size,
                cancel: cancel.clone(),
            },
        );
        emit_job_progress(emitter, &job);
        tokio::spawn(download_pump(
            self.inner.clone(),
            task_id.clone(),
            reader,
            size,
            connection.id.clone(),
            cancel,
            emitter.clone(),
        ));
        Ok((task_id, size))
    }

    /// `files/download/finish` (§8.3): releases the reader and marks the job
    /// completed. Safe to call twice (idempotent).
    pub async fn finish_download(
        &self,
        task_id: &str,
        emitter: &PluginEmitter,
    ) -> Result<(), String> {
        let slot = self.inner.downloads.lock().await.remove(task_id);
        let transitioned = self
            .inner
            .complete_job(task_id, JobStatus::Completed, None, emitter)
            .await;
        if slot.is_none() && !transitioned && !self.inner.job_exists(task_id).await {
            return Err(format!("Download task '{task_id}' was not found"));
        }
        Ok(())
    }

    /// `files/transfer/cancel` (§8.3): cancels a single-file job (`taskId`)
    /// or a directory job (id shared namespace permitted: try both tables).
    /// Cancellation is cooperative: the pump/writer loop checks the cancel
    /// flag between chunks; an upload being cancelled discards its writer.
    pub async fn cancel(&self, task_id: &str, emitter: &PluginEmitter) -> Result<(), String> {
        // Upload: abort the writer and drop the slot.
        let upload = self.inner.uploads.lock().await.remove(task_id);
        if let Some(mut slot) = upload {
            let _ = slot.writer.abort().await;
            self.inner
                .complete_job(task_id, JobStatus::Canceled, None, emitter)
                .await;
            return Ok(());
        }
        // Download: flag the pump; it bails between chunks.
        let download = self.inner.downloads.lock().await.remove(task_id);
        if let Some(slot) = download {
            slot.cancel.store(true, Ordering::Release);
            self.inner
                .complete_job(task_id, JobStatus::Canceled, None, emitter)
                .await;
            return Ok(());
        }
        // Directory job: flag + mark canceled (the traversal checks per file).
        let control = self.inner.dir_controls.lock().await.get(task_id).cloned();
        if let Some(flag) = control {
            flag.store(true, Ordering::Release);
            self.inner
                .complete_dir_job(task_id, JobStatus::Canceled, None, emitter)
                .await;
            return Ok(());
        }
        Err("Transfer task was not found".to_string())
    }

    /// `files/transfers/list` (§8.4): in-progress jobs plus persisted history
    /// for `connectionId` (or all connections when `None`).
    pub async fn list(
        &self,
        store: &crate::store::Store,
        connection_id: Option<&str>,
    ) -> Result<Vec<TransferJob>, String> {
        let mut result = Vec::new();
        {
            let jobs = self.inner.jobs.lock().await;
            for job in jobs.values() {
                if connection_id
                    .map(|id| job.connection_id == id)
                    .unwrap_or(true)
                {
                    result.push(job.clone());
                }
            }
        }
        let live: HashSet<String> = result.iter().map(|job| job.task_id.clone()).collect();
        for record in store.load_transfers() {
            if let Some(id) = connection_id {
                if record.connection_id != id {
                    continue;
                }
            }
            if live.contains(&record.task_id) {
                continue;
            }
            result.push(TransferJob {
                task_id: record.task_id,
                connection_id: record.connection_id,
                kind: if record.kind == "download" {
                    TransferKind::Download
                } else {
                    TransferKind::Upload
                },
                remote_path: record.remote_path,
                total_bytes: record.total_bytes,
                transferred_bytes: record.transferred_bytes,
                status: match record.status.as_str() {
                    "failed" => JobStatus::Failed,
                    "canceled" => JobStatus::Canceled,
                    _ => JobStatus::Completed,
                },
                error: record.error,
                started_at: record.started_at,
                finished_at: record.finished_at,
            });
        }
        Ok(result)
    }

    /// P-FILES ①a: unified `files/transfers/list` view (§8.4). Merges the
    /// single-file jobs + persisted history ([`Self::list`]) with the dir-job
    /// table (syncDir/copyDir and the degraded copy/move/rename jobs), so a
    /// degraded `files/copy|move|rename` with `transport: "job"` is visible
    /// in the same list the frontend polls. Dir jobs are serialized as their
    /// camelCase DirJob shape (jobId/kind/status/bytes*/files*); the
    /// connection filter matches source OR target connection id.
    pub async fn list_merged(
        &self,
        store: &crate::store::Store,
        connection_id: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, String> {
        let mut result: Vec<serde_json::Value> = Vec::new();
        for job in self.list(store, connection_id).await? {
            result.push(serde_json::to_value(job).map_err(|error| error.to_string())?);
        }
        let dir_jobs = self.inner.dir_jobs.lock().await;
        let mut matched: Vec<&DirJob> = dir_jobs
            .values()
            .filter(|job| {
                connection_id
                    .map(|id| job.source_connection_id == id || job.target_connection_id == id)
                    .unwrap_or(true)
            })
            .collect();
        // Deterministic order: oldest start first (stable enough for the UI,
        // which re-sorts by activity/update time anyway).
        matched.sort_by_key(|job| job.started_at.unwrap_or(0));
        for job in matched {
            result.push(serde_json::to_value(job).map_err(|error| error.to_string())?);
        }
        Ok(result)
    }

    /// `files/transfer/status` (§8.4): `jobId` → `{job}` for directory jobs
    /// (and single-file jobs by taskId).
    pub async fn status(&self, job_id: &str) -> Result<Option<TransferJob>, String> {
        Ok(self.inner.jobs.lock().await.get(job_id).cloned())
    }

    /// P-FILES ⑥: `files/transfers/clear` — drop finished (completed/failed/
    /// canceled) transfer history so the panel's history list can be emptied.
    /// Queued/running jobs are never touched. Covers all three history
    /// surfaces: single-file jobs table, dir-job table and the persisted
    /// `transfers.json` records. Returns the number of removed entries.
    pub async fn clear(
        &self,
        store: &crate::store::Store,
        connection_id: Option<&str>,
    ) -> Result<u64, String> {
        let mut removed: u64 = 0;
        {
            let mut jobs = self.inner.jobs.lock().await;
            let drop_ids: Vec<String> = jobs
                .iter()
                .filter(|(_, job)| {
                    job.status.is_terminal()
                        && connection_id
                            .map(|id| job.connection_id == id)
                            .unwrap_or(true)
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in drop_ids {
                jobs.remove(&id);
                removed += 1;
            }
        }
        {
            let mut dir_jobs = self.inner.dir_jobs.lock().await;
            let drop_ids: Vec<String> = dir_jobs
                .iter()
                .filter(|(_, job)| {
                    job.status.is_terminal()
                        && connection_id
                            .map(|id| job.source_connection_id == id || job.target_connection_id == id)
                            .unwrap_or(true)
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in drop_ids {
                dir_jobs.remove(&id);
                removed += 1;
            }
        }
        removed += store.clear_transfers(connection_id)? as u64;
        Ok(removed)
    }

    /// Directory-job status projection for `files/transfer/status` when the
    /// id refers to a syncDir/copyDir job.
    pub async fn dir_status(&self, job_id: &str) -> Result<Option<DirJob>, String> {
        Ok(self.inner.dir_jobs.lock().await.get(job_id).cloned())
    }

    /// `files/syncDir` / `files/copyDir` (§8.4): enqueues the traversal job
    /// and returns `{jobId}` immediately. `sync = true` deletes target extras
    /// (rclone-sync semantics; refuse when the target disallows delete).
    pub async fn enqueue_dir_job(
        &self,
        source: &StoredConnection,
        source_operator: &opendal::Operator,
        target: &StoredConnection,
        target_operator: &opendal::Operator,
        source_path: &str,
        target_path: &str,
        sync: bool,
        emitter: &PluginEmitter,
    ) -> Result<String, String> {
        validate_dir_job_gates(target, sync)?;
        let job_id = uuid::Uuid::new_v4().to_string();
        let now = store::unix_millis_now();
        let job = DirJob {
            job_id: job_id.clone(),
            source_connection_id: source.id.clone(),
            source_path: source_path.to_string(),
        target_connection_id: target.id.clone(),
        target_path: target_path.to_string(),
        sync,
        delete_source: false,
        kind: if sync { DirJobKind::SyncDir } else { DirJobKind::CopyDir },
            files_done: 0,
            files_total: None,
            bytes_done: 0,
            bytes_total: None,
            status: JobStatus::Queued,
            error: None,
            started_at: Some(now),
            finished_at: None,
        };
        self.inner
            .dir_jobs
            .lock()
            .await
            .insert(job_id.clone(), job.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        self.inner
            .dir_controls
            .lock()
            .await
            .insert(job_id.clone(), cancel.clone());
        emit_dir_progress(emitter, &job);
        tokio::spawn(run_dir_job(
            self.inner.clone(),
            job_id.clone(),
            source_operator.clone(),
            target_operator.clone(),
            source.id.clone(),
            target.id.clone(),
            source_path.to_string(),
            target_path.to_string(),
            sync,
            // syncDir/copyDir never delete their source.
            false,
            cancel,
            emitter.clone(),
        ));
        Ok(job_id)
    }

    /// X-A ③ (F-B handover) / P-FILES ②: degraded cross-path copy with an
    /// optional source delete, as a real async job. main.rs routes here when
    /// the two sides are not the same Operator instance or the backend lacks
    /// the native copy/rename capability; the call returns `{jobId}`
    /// immediately and the streaming read→write work runs on the dir-job
    /// table (progress events + `files/transfer/status` + `files/transfers/list`).
    /// `delete_source` implements the move semantics — the source is deleted
    /// only after the copy succeeded. `kind` records the triggering method
    /// (`copy`/`move`/`rename`) for the unified list view.
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_copy_job(
        &self,
        source: &StoredConnection,
        source_operator: &opendal::Operator,
        target: &StoredConnection,
        target_operator: &opendal::Operator,
        source_path: &str,
        target_path: &str,
        delete_source: bool,
        kind: DirJobKind,
        emitter: &PluginEmitter,
    ) -> Result<String, String> {
        // Target gates: read_only rejects the write; the delete side of a
        // move was already gated by the caller (main.rs ensure_deletable).
        validate_dir_job_gates(target, false)?;
        if source_path.trim().trim_matches('/').is_empty() {
            return Err(
                "Cannot copy or move the connection root '/'; copy a subpath instead"
                    .to_string(),
            );
        }
        let job_id = uuid::Uuid::new_v4().to_string();
        let now = store::unix_millis_now();
        let job = DirJob {
            job_id: job_id.clone(),
            source_connection_id: source.id.clone(),
            source_path: source_path.to_string(),
            target_connection_id: target.id.clone(),
            target_path: target_path.to_string(),
            sync: false,
            delete_source,
            kind,
            files_done: 0,
            files_total: None,
            bytes_done: 0,
            bytes_total: None,
            status: JobStatus::Queued,
            error: None,
            started_at: Some(now),
            finished_at: None,
        };
        self.inner
            .dir_jobs
            .lock()
            .await
            .insert(job_id.clone(), job.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        self.inner
            .dir_controls
            .lock()
            .await
            .insert(job_id.clone(), cancel.clone());
        emit_dir_progress(emitter, &job);
        tokio::spawn(run_dir_job(
            self.inner.clone(),
            job_id.clone(),
            source_operator.clone(),
            target_operator.clone(),
            source.id.clone(),
            target.id.clone(),
            source_path.to_string(),
            target_path.to_string(),
            false,
            delete_source,
            cancel,
            emitter.clone(),
        ));
        Ok(job_id)
    }

    /// B-ARCHIVE: `files/extract` degrade for archives beyond the synchronous
    /// inline budget (> 10 files or > 8 MiB payload). Enqueues an
    /// [`DirJobKind::Extract`] job on the dir-job table (same progress
    /// events / cancel / status semantics as copy/move) and returns the
    /// `jobId` immediately. The archive is re-read from storage inside the
    /// spawned task; gates are checked here so a rejected request never
    /// creates a job.
    pub async fn enqueue_extract_job(
        &self,
        connection: &StoredConnection,
        operator: &opendal::Operator,
        archive_path: &str,
        target_path: &str,
        emitter: &PluginEmitter,
    ) -> Result<String, String> {
        // Extract only writes the target (it never deletes the source
        // archive), so the delete gate does not apply — same rule as the
        // inline path in main.rs.
        validate_dir_job_gates(connection, false)?;
        let job_id = uuid::Uuid::new_v4().to_string();
        let now = store::unix_millis_now();
        let job = DirJob {
            job_id: job_id.clone(),
            source_connection_id: connection.id.clone(),
            source_path: archive_path.to_string(),
            target_connection_id: connection.id.clone(),
            target_path: target_path.to_string(),
            sync: false,
            delete_source: false,
            kind: DirJobKind::Extract,
            files_done: 0,
            files_total: None,
            bytes_done: 0,
            bytes_total: None,
            status: JobStatus::Queued,
            error: None,
            started_at: Some(now),
            finished_at: None,
        };
        self.inner
            .dir_jobs
            .lock()
            .await
            .insert(job_id.clone(), job.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        self.inner
            .dir_controls
            .lock()
            .await
            .insert(job_id.clone(), cancel.clone());
        emit_dir_progress(emitter, &job);
        tokio::spawn(run_extract_job(
            self.inner.clone(),
            job_id.clone(),
            connection.id.clone(),
            operator.clone(),
            archive_path.to_string(),
            target_path.to_string(),
            cancel,
            emitter.clone(),
        ));
        Ok(job_id)
    }

    /// `files/compress` degrade for sources beyond the synchronous inline
    /// budget (> 10 files or > 8 MiB payload). Enqueues a
    /// [`DirJobKind::Compress`] job on the dir-job table (same progress /
    /// cancel / status semantics as copy/move) and returns the `jobId`
    /// immediately. The plan (sources, archive paths, budgets) is validated
    /// by the caller; payloads are re-read from storage inside the spawned
    /// task.
    pub async fn enqueue_compress_job(
        &self,
        connection: &StoredConnection,
        operator: &opendal::Operator,
        plan: Vec<crate::archive::TarPlanEntry>,
        target_path: &str,
        gzip: bool,
        emitter: &PluginEmitter,
    ) -> Result<String, String> {
        validate_dir_job_gates(connection, false)?;
        let job_id = uuid::Uuid::new_v4().to_string();
        let now = store::unix_millis_now();
        let job = DirJob {
            job_id: job_id.clone(),
            source_connection_id: connection.id.clone(),
            source_path: plan
                .first()
                .map(|entry| entry.source_path.clone())
                .unwrap_or_else(|| target_path.to_string()),
            target_connection_id: connection.id.clone(),
            target_path: target_path.to_string(),
            sync: false,
            delete_source: false,
            kind: DirJobKind::Compress,
            files_done: 0,
            files_total: None,
            bytes_done: 0,
            bytes_total: None,
            status: JobStatus::Queued,
            error: None,
            started_at: Some(now),
            finished_at: None,
        };
        self.inner
            .dir_jobs
            .lock()
            .await
            .insert(job_id.clone(), job.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        self.inner
            .dir_controls
            .lock()
            .await
            .insert(job_id.clone(), cancel.clone());
        emit_dir_progress(emitter, &job);
        tokio::spawn(run_compress_job(
            self.inner.clone(),
            job_id.clone(),
            connection.id.clone(),
            operator.clone(),
            plan,
            target_path.to_string(),
            gzip,
            cancel,
            emitter.clone(),
        ));
        Ok(job_id)
    }

    /// Cancels every job of a connection; called by `connection/disconnect`
    /// (M0 §3.2) before the Operator is dropped.
    pub async fn cancel_connection_jobs(&self, connection_id: &str, emitter: &PluginEmitter) {
        let upload_ids: Vec<String> = {
            let uploads = self.inner.uploads.lock().await;
            uploads
                .iter()
                .filter(|(_, slot)| slot.connection_id == connection_id)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for task_id in upload_ids {
            let _ = self.cancel(&task_id, emitter).await;
        }
        let download_ids: Vec<String> = {
            let downloads = self.inner.downloads.lock().await;
            downloads
                .iter()
                .filter(|(_, slot)| slot.connection_id == connection_id)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for task_id in download_ids {
            let _ = self.cancel(&task_id, emitter).await;
        }
        let dir_ids: Vec<String> = {
            let dir_jobs = self.inner.dir_jobs.lock().await;
            dir_jobs
                .iter()
                .filter(|(_, job)| {
                    !job.status.is_terminal()
                        && (job.source_connection_id == connection_id
                            || job.target_connection_id == connection_id)
                })
                .map(|(id, _)| id.clone())
                .collect()
        };
        for job_id in dir_ids {
            let _ = self.cancel(&job_id, emitter).await;
        }
    }

    /// Loads persisted finished transfers into the history view (startup,
    /// M3-F3-4 hook). Hydrates the in-memory mirror; `list` always reads the
    /// store fresh, so this is a warm-up for future in-memory queries.
    pub async fn load_history(&self, store: &crate::store::Store) {
        let records = store.load_transfers();
        let mut cache = self
            .inner
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *cache = records;
    }
}

/// Gate check for syncDir/copyDir before any job is enqueued: the target must
/// be writable, and `sync` additionally requires `allow_delete` (§8.4).
fn validate_dir_job_gates(target: &StoredConnection, sync: bool) -> Result<(), String> {
    if target.read_only {
        return Err("Connection is read-only; write operations are rejected".to_string());
    }
    if sync && !target.allow_delete {
        return Err("Connection disallows delete operations (allow_delete=false)".to_string());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Spawned tasks
// ---------------------------------------------------------------------------

/// Download pump (§8.3): pushes the whole file through the binary channel in
/// `TRANSFER_CHUNK_SIZE` slices, FIFO per connection, capped by the global
/// semaphore. Honors the cooperative cancel flag between chunks.
async fn download_pump(
    inner: Arc<Inner>,
    task_id: String,
    reader: opendal::Reader,
    size: u64,
    connection_id: String,
    cancel: Arc<AtomicBool>,
    emitter: PluginEmitter,
) {
    // Per-connection FIFO, then the global cap (§7). Every spawned job
    // acquires in this order, so the two-phase wait cannot cycle.
    let fifo = inner.conn_lock(&connection_id).clone();
    let _fifo_guard = fifo.lock().await;
    let Ok(_permit) = inner.semaphore.acquire().await else {
        return;
    };
    if cancel.load(Ordering::Acquire) {
        inner
            .complete_job(&task_id, JobStatus::Canceled, None, &emitter)
            .await;
        return;
    }
    inner.mark_running(&task_id).await;
    emit_running(&emitter, &task_id);
    let channel = format!("files/download/{task_id}");
    let mut offset = 0u64;
    let mut throttle = Throttle::default();
    while offset < size {
        if cancel.load(Ordering::Acquire) {
            inner
                .complete_job(&task_id, JobStatus::Canceled, None, &emitter)
                .await;
            return;
        }
        match slot::read_chunk(
            &reader,
            offset,
            offset.saturating_add(TRANSFER_CHUNK_SIZE as u64).min(size),
        )
        .await
        {
            Err(error) => {
                inner
                    .complete_job(&task_id, JobStatus::Failed, Some(error), &emitter)
                    .await;
                return;
            }
            Ok(bytes) if bytes.is_empty() => break, // backend file shorter than stat
            Ok(bytes) => {
                let payload = slot::frame(offset, &bytes);
                if let Err(error) = emitter.binary(&channel, &payload) {
                    let detail = format!("Failed to push download frame: {error:?}");
                    inner
                        .complete_job(&task_id, JobStatus::Failed, Some(detail), &emitter)
                        .await;
                    return;
                }
                offset = offset.saturating_add(bytes.len() as u64);
                if let Some(job) = inner.jobs.lock().await.get_mut(&task_id) {
                    job.transferred_bytes = offset;
                }
                if throttle.should_emit(offset, Some(size)) {
                    emit_running_at(&emitter, &task_id, offset, Some(size));
                }
            }
        }
    }
    inner
        .complete_job(&task_id, JobStatus::Completed, None, &emitter)
        .await;
}

/// syncDir/copyDir traversal (§7) and the X-A degraded copy/move job (§8.2):
/// source walk (directory subtree, or the single file itself) → per-file
/// native copy (same connection + capability) or streamed read→write →
/// throttled `{filesDone, filesTotal, bytesDone, bytesTotal}` progress;
/// `sync` deletes target extras (extra files; emptied directories are left in
/// place); `delete_source` (degraded move) removes the source only after the
/// copy succeeded.
#[allow(clippy::too_many_arguments)]
async fn run_dir_job(
    inner: Arc<Inner>,
    job_id: String,
    source_operator: opendal::Operator,
    target_operator: opendal::Operator,
    source_connection_id: String,
    target_connection_id: String,
    source_path: String,
    target_path: String,
    sync: bool,
    delete_source: bool,
    cancel: Arc<AtomicBool>,
    emitter: PluginEmitter,
) {
    // Per-connection FIFO in sorted id order (deterministic order → no lock
    // cycles), held for the whole traversal, then the global cap.
    let mut lock_ids = vec![source_connection_id.clone(), target_connection_id.clone()];
    lock_ids.sort();
    lock_ids.dedup();
    let mut fifo_locks = Vec::new();
    for id in &lock_ids {
        fifo_locks.push(inner.conn_lock(id));
    }
    let mut fifo_guards = Vec::new();
    for lock in &fifo_locks {
        fifo_guards.push(lock.lock().await);
    }
    let Ok(_permit) = inner.semaphore.acquire().await else {
        return;
    };
    if cancel.load(Ordering::Acquire) {
        inner
            .complete_dir_job(&job_id, JobStatus::Canceled, None, &emitter)
            .await;
        return;
    }
    if let Some(job) = inner.dir_jobs.lock().await.get_mut(&job_id) {
        if job.status == JobStatus::Queued {
            job.status = JobStatus::Running;
        }
    }
    emit_dir_running(&emitter, &job_id);

    // Walk the source once so filesTotal/bytesTotal are known upfront. A
    // single-file source (degraded `files/copy`/`files/move` with a file
    // path) plans one entry whose relative part is empty — `join_target`
    // then maps it onto the target path verbatim (cp semantics).
    let source_is_dir = match crate::engine::ops::is_dir_path(&source_operator, &source_path).await
    {
        Ok(flag) => flag,
        Err(error) => {
            inner
                .complete_dir_job(&job_id, JobStatus::Failed, Some(error), &emitter)
                .await;
            return;
        }
    };
    let plan: Vec<slot::WalkedFile> = if source_is_dir {
        match slot::walk_files(&source_operator, &source_path).await {
            Ok(files) => files,
            Err(error) => {
                inner
                    .complete_dir_job(&job_id, JobStatus::Failed, Some(error), &emitter)
                    .await;
                return;
            }
        }
    } else {
        match source_operator.stat(&source_path).await {
            Ok(metadata) => vec![(String::new(), metadata.content_length())],
            Err(error) => {
                let error = format!("Failed to stat '{source_path}': {error}");
                inner
                    .complete_dir_job(&job_id, JobStatus::Failed, Some(error), &emitter)
                    .await;
                return;
            }
        }
    };
    let bytes_total: u64 = plan.iter().map(|(_, size)| size).sum();
    {
        let mut dir_jobs = inner.dir_jobs.lock().await;
        if let Some(job) = dir_jobs.get_mut(&job_id) {
            job.files_total = Some(plan.len() as u64);
            job.bytes_total = Some(bytes_total);
        }
    }
    emit_dir_running(&emitter, &job_id);

    let native_copy = source_connection_id == target_connection_id
        && source_operator.info().full_capability().copy;

    // Ensure the target directory itself exists (mkdir -p semantics). Only
    // for directory sources: a single-file copy/move targets a file path, and
    // creating a dir marker there would shadow the file on prefix backends
    // (memory/s3) — the file's parent dirs are created per-plan-entry below.
    let target_base = target_path.trim().trim_matches('/').to_string();
    if source_is_dir && !target_base.is_empty() {
        if let Err(error) = target_operator
            .create_dir(&format!("/{target_base}/"))
            .await
            .map_err(|error| format!("Failed to create target dir '{target_base}': {error}"))
        {
            inner
                .complete_dir_job(&job_id, JobStatus::Failed, Some(error), &emitter)
                .await;
            return;
        }
    }

    let mut throttle = Throttle::default();
    let mut files_done = 0u64;
    let mut bytes_done = 0u64;
    let mut created_dirs: HashSet<String> = HashSet::new();

    for (relative, size) in &plan {
        if cancel.load(Ordering::Acquire) {
            inner
                .complete_dir_job(&job_id, JobStatus::Canceled, None, &emitter)
                .await;
            return;
        }
        let target_file = join_target(&target_path, relative);
        // `relative` is relative to the source dir; reading (and native copy)
        // needs the path in source-connection coordinates.
        let source_file = join_target(&source_path, relative);
        let parent = slot::parent_dir(&target_file);
        if !parent.is_empty() && created_dirs.insert(parent.clone()) {
            if let Err(error) = target_operator
                .create_dir(&format!("/{parent}/"))
                .await
                .map_err(|error| format!("Failed to create dir '{parent}': {error}"))
            {
                inner
                    .complete_dir_job(&job_id, JobStatus::Failed, Some(error), &emitter)
                    .await;
                return;
            }
        }
        let outcome = if native_copy {
            source_operator
                .copy(&source_file, &target_file)
                .await
                .map(|_| ())
                .map_err(|error| format!("Native copy of '{source_file}' failed: {error}"))
        } else {
            stream_copy(&source_operator, &target_operator, &source_file, &target_file).await
        };
        match outcome {
            Err(error) => {
                inner
                    .complete_dir_job(&job_id, JobStatus::Failed, Some(error), &emitter)
                    .await;
                return;
            }
            Ok(()) => {
                files_done += 1;
                bytes_done += size;
                {
                    let mut dir_jobs = inner.dir_jobs.lock().await;
                    if let Some(job) = dir_jobs.get_mut(&job_id) {
                        job.files_done = files_done;
                        job.bytes_done = bytes_done;
                    }
                }
                if throttle.should_emit(bytes_done, Some(bytes_total)) {
                    if let Some(job) = inner.dir_jobs.lock().await.get(&job_id).cloned() {
                        emit_dir_progress(&emitter, &job);
                    }
                }
            }
        }
    }

    // Degraded move (§8.2): delete the source only after the copy succeeded.
    if delete_source {
        if cancel.load(Ordering::Acquire) {
            inner
                .complete_dir_job(&job_id, JobStatus::Canceled, None, &emitter)
                .await;
            return;
        }
        let source_trimmed = source_path.trim().trim_matches('/');
        let outcome = if source_is_dir {
            // Per-entry precise deletion. OpenDAL 0.57's recursive delete on
            // some backends (memory) matches by bare path prefix, wiping
            // sibling paths sharing the source name prefix (e.g. deleting
            // `d1` also removed `d1-renamed`) — covered by the
            // `rename_dir_degrade` smoke scenario and the
            // `delete_source_precisely_spares_prefix_siblings` unit test.
            let deletions = plan_source_deletions(&source_path, &plan);
            let mut failure: Option<String> = None;
            for path in &deletions {
                if let Err(error) = source_operator.delete(path).await {
                    failure = Some(format!(
                        "Copied '{source_trimmed}' but failed to delete '{path}': {error}"
                    ));
                    break;
                }
            }
            match failure {
                Some(error) => Err(error),
                None => Ok(()),
            }
        } else {
            source_operator
                .delete(source_trimmed)
                .await
                .map(|_| ())
                .map_err(|error| {
                    format!("Copied '{source_trimmed}' but failed to delete the source: {error}")
                })
        };
        if let Err(error) = outcome {
            inner
                .complete_dir_job(&job_id, JobStatus::Failed, Some(error), &emitter)
                .await;
            return;
        }
    }

    // syncDir: delete target extras (rclone-sync semantics).
    if sync {
        let target_files = match slot::walk_files(&target_operator, &target_path).await {
            Ok(files) => files,
            Err(error) => {
                inner
                    .complete_dir_job(&job_id, JobStatus::Failed, Some(error), &emitter)
                    .await;
                return;
            }
        };
        let source_set: HashSet<&String> = plan.iter().map(|(path, _)| path).collect();
        for (relative, _) in target_files {
            if source_set.contains(&relative) {
                continue;
            }
            if cancel.load(Ordering::Acquire) {
                inner
                    .complete_dir_job(&job_id, JobStatus::Canceled, None, &emitter)
                    .await;
                return;
            }
            let extra = join_target(&target_path, &relative);
            if let Err(error) = target_operator.delete(&extra).await.map_err(|error| {
                format!("Failed to delete extra target file '{extra}': {error}")
            }) {
                inner
                    .complete_dir_job(&job_id, JobStatus::Failed, Some(error), &emitter)
                    .await;
                return;
            }
        }
    }

    inner
        .complete_dir_job(&job_id, JobStatus::Completed, None, &emitter)
        .await;
}

/// B-ARCHIVE: extraction runner for the degraded `files/extract` transport.
/// Reads the archive from storage, plans (sanitize + bomb guards — the same
/// functions the inline path uses), then writes entry-by-entry under the
/// target directory with throttled `{filesDone, filesTotal, bytesDone,
/// bytesTotal}` progress and cooperative cancel between entries.
async fn run_extract_job(
    inner: Arc<Inner>,
    job_id: String,
    connection_id: String,
    operator: opendal::Operator,
    archive_path: String,
    target_path: String,
    cancel: Arc<AtomicBool>,
    emitter: PluginEmitter,
) {
    let fifo = inner.conn_lock(&connection_id).clone();
    let _fifo_guard = fifo.lock().await;
    let Ok(_permit) = inner.semaphore.acquire().await else {
        return;
    };
    if cancel.load(Ordering::Acquire) {
        inner
            .complete_dir_job(&job_id, JobStatus::Canceled, None, &emitter)
            .await;
        return;
    }
    if let Some(job) = inner.dir_jobs.lock().await.get_mut(&job_id) {
        if job.status == JobStatus::Queued {
            job.status = JobStatus::Running;
        }
    }
    emit_dir_running(&emitter, &job_id);

    // Read + plan once so filesTotal/bytesTotal are known upfront; a parse
    // failure (corrupt archive, zip, zip-slip entry) fails the job.
    let outcome = async {
        let raw = crate::archive::read_archive(&operator, &archive_path).await?;
        let plan = crate::archive::extract_entries(&raw, &crate::archive::DEFAULT_LIMITS)?;
        Ok::<Vec<crate::archive::ExtractEntry>, String>(plan)
    }
    .await;
    let plan = match outcome {
        Ok(plan) => plan,
        Err(error) => {
            inner
                .complete_dir_job(&job_id, JobStatus::Failed, Some(error), &emitter)
                .await;
            return;
        }
    };
    let bytes_total: u64 = plan.iter().map(|entry| entry.size).sum();
    {
        let mut dir_jobs = inner.dir_jobs.lock().await;
        if let Some(job) = dir_jobs.get_mut(&job_id) {
            job.files_total = Some(plan.len() as u64);
            job.bytes_total = Some(bytes_total);
        }
    }
    emit_dir_running(&emitter, &job_id);

    // Per-entry write loop (mirrors run_dir_job): short table locks between
    // entries so `files/transfer/status` stays responsive, throttled progress
    // events, cooperative cancel.
    let base = target_path.trim().trim_matches('/').to_string();
    if !base.is_empty() {
        if let Err(error) = operator
            .create_dir(&format!("/{base}/"))
            .await
            .map_err(|error| format!("Failed to create target directory '{base}': {error}"))
        {
            inner
                .complete_dir_job(&job_id, JobStatus::Failed, Some(error), &emitter)
                .await;
            return;
        }
    }
    let mut throttle = Throttle::default();
    let mut files_done = 0u64;
    let mut bytes_done = 0u64;
    let mut created_dirs: HashSet<String> = HashSet::new();
    for entry in &plan {
        if cancel.load(Ordering::Acquire) {
            inner
                .complete_dir_job(&job_id, JobStatus::Canceled, None, &emitter)
                .await;
            return;
        }
        let target_file = if base.is_empty() {
            entry.path.clone()
        } else {
            format!("{base}/{}", entry.path)
        };
        let parent = match target_file.rfind('/') {
            Some(index) => target_file[..index].to_string(),
            None => String::new(),
        };
        if !parent.is_empty() && created_dirs.insert(parent.clone()) {
            if let Err(error) = operator
                .create_dir(&format!("/{parent}/"))
                .await
                .map_err(|error| {
                    format!("Failed to create directory '{parent}' during extract: {error}")
                })
            {
                inner
                    .complete_dir_job(&job_id, JobStatus::Failed, Some(error), &emitter)
                    .await;
                return;
            }
        }
        if let Err(error) = operator
            .write(&target_file, entry.data.clone())
            .await
            .map_err(|error| format!("Failed to write extracted file '{target_file}': {error}"))
        {
            inner
                .complete_dir_job(&job_id, JobStatus::Failed, Some(error), &emitter)
                .await;
            return;
        }
        files_done += 1;
        bytes_done += entry.size;
        {
            let mut dir_jobs = inner.dir_jobs.lock().await;
            if let Some(job) = dir_jobs.get_mut(&job_id) {
                job.files_done = files_done;
                job.bytes_done = bytes_done;
            }
        }
        if throttle.should_emit(bytes_done, Some(bytes_total)) {
            if let Some(job) = inner.dir_jobs.lock().await.get(&job_id).cloned() {
                emit_dir_progress(&emitter, &job);
            }
        }
    }
    if cancel.load(Ordering::Acquire) {
        inner
            .complete_dir_job(&job_id, JobStatus::Canceled, None, &emitter)
            .await;
        return;
    }
    inner
        .complete_dir_job(&job_id, JobStatus::Completed, None, &emitter)
        .await;
}

/// Streams the planned sources into one tar / tar.gz archive (job path of
/// `files/compress`). Mirrors [`run_extract_job`]: plan totals upfront,
/// per-entry table updates with short locks, throttled progress events and
/// cooperative cancel between chunks. `gzip` wraps the tar stream in stored
/// (uncompressed) DEFLATE blocks — see `archive::deflate_stored_blocks`.
async fn run_compress_job(
    inner: Arc<Inner>,
    job_id: String,
    connection_id: String,
    operator: opendal::Operator,
    plan: Vec<crate::archive::TarPlanEntry>,
    target_path: String,
    gzip: bool,
    cancel: Arc<AtomicBool>,
    emitter: PluginEmitter,
) {
    let fifo = inner.conn_lock(&connection_id).clone();
    let _fifo_guard = fifo.lock().await;
    let Ok(_permit) = inner.semaphore.acquire().await else {
        return;
    };
    if cancel.load(Ordering::Acquire) {
        inner
            .complete_dir_job(&job_id, JobStatus::Canceled, None, &emitter)
            .await;
        return;
    }
    if let Some(job) = inner.dir_jobs.lock().await.get_mut(&job_id) {
        if job.status == JobStatus::Queued {
            job.status = JobStatus::Running;
        }
    }
    emit_dir_running(&emitter, &job_id);

    let bytes_total: u64 = plan.iter().map(|entry| entry.size).sum();
    {
        let mut dir_jobs = inner.dir_jobs.lock().await;
        if let Some(job) = dir_jobs.get_mut(&job_id) {
            job.files_total = Some(plan.len() as u64);
            job.bytes_total = Some(bytes_total);
        }
    }
    emit_dir_running(&emitter, &job_id);

    let mut throttle = Throttle::default();
    let mut files_done = 0u64;
    let mut bytes_done = 0u64;
    let mut crc: u32 = 0xFFFF_FFFF;
    let mut archived_len: u64 = 0;
    let outcome = async {
        let mut writer = operator
            .writer(&target_path)
            .await
            .map_err(|error| format!("Failed to open writer for '{target_path}': {error}"))?;
        macro_rules! emit {
            ($bytes:expr) => {{
                let bytes: &[u8] = &$bytes;
                if !bytes.is_empty() {
                    if gzip {
                        crc = crate::archive::crc32_update(crc, bytes);
                        archived_len += bytes.len() as u64;
                    }
                    writer
                        .write(bytes.to_vec())
                        .await
                        .map_err(|error| format!("Failed to write archive chunk: {error}"))?;
                }
            }};
        }
        if gzip {
            emit!(crate::archive::GZIP_HEADER);
        }
        for entry in &plan {
            if cancel.load(Ordering::Acquire) {
                return Err("compression canceled".to_string());
            }
            if entry.size == 0 {
                // Zero-size walk reports are ambiguous (empty file vs. a
                // backend that does not report lengths) — read once so the
                // header carries the real byte count.
                let data = operator
                    .read(&entry.source_path)
                    .await
                    .map_err(|error| format!("Failed to read '{}': {error}", entry.source_path))?
                    .to_vec();
                emit!(crate::archive::tar_entry_header(&entry.archive_path, data.len() as u64)?);
                emit!(data);
                emit!(vec![0u8; data.len().div_ceil(512) * 512 - data.len()]);
            } else {
                emit!(crate::archive::tar_entry_header(&entry.archive_path, entry.size)?);
                let (reader, size) = slot::open_download_reader(&operator, &entry.source_path).await?;
                let mut offset = 0u64;
                while offset < size {
                    if cancel.load(Ordering::Acquire) {
                        return Err("compression canceled".to_string());
                    }
                    let end = offset.saturating_add(TRANSFER_CHUNK_SIZE as u64).min(size);
                    let chunk = slot::read_chunk(&reader, offset, end).await?;
                    if chunk.is_empty() {
                        break;
                    }
                    offset += chunk.len() as u64;
                    bytes_done += chunk.len() as u64;
                    emit!(chunk);
                }
                let padding = (entry.size as usize).div_ceil(512) * 512 - entry.size as usize;
                emit!(vec![0u8; padding]);
            }
            files_done += 1;
            {
                let mut dir_jobs = inner.dir_jobs.lock().await;
                if let Some(job) = dir_jobs.get_mut(&job_id) {
                    job.files_done = files_done;
                    job.bytes_done = bytes_done;
                }
            }
            if throttle.should_emit(bytes_done, Some(bytes_total)) {
                if let Some(job) = inner.dir_jobs.lock().await.get(&job_id).cloned() {
                    emit_dir_progress(&emitter, &job);
                }
            }
        }
        emit!(crate::archive::TAR_END);
        if gzip {
            // Empty final DEFLATE block terminates the stream; see
            // `archive::deflate_stored_blocks`.
            writer
                .write(crate::archive::deflate_stored_blocks(&[], true))
                .await
                .map_err(|error| format!("Failed to write archive chunk: {error}"))?;
            writer
                .write(crate::archive::gzip_trailer(!crc, archived_len).to_vec())
                .await
                .map_err(|error| format!("Failed to write archive chunk: {error}"))?;
        }
        writer
            .close()
            .await
            .map_err(|error| format!("Failed to close writer for '{target_path}': {error}"))?;
        Ok::<(), String>(())
    }
    .await;
    match outcome {
        Ok(()) if cancel.load(Ordering::Acquire) => {
            inner
                .complete_dir_job(&job_id, JobStatus::Canceled, None, &emitter)
                .await;
        }
        Ok(()) => {
            inner
                .complete_dir_job(&job_id, JobStatus::Completed, None, &emitter)
                .await;
        }
        Err(_) if cancel.load(Ordering::Acquire) => {
            inner
                .complete_dir_job(&job_id, JobStatus::Canceled, None, &emitter)
                .await;
        }
        Err(error) => {
            inner
                .complete_dir_job(&job_id, JobStatus::Failed, Some(error), &emitter)
                .await;
        }
    }
}

/// Streams one file `source → target` in `TRANSFER_CHUNK_SIZE` slices.
async fn stream_copy(
    source_operator: &opendal::Operator,
    target_operator: &opendal::Operator,
    source_path: &str,
    target_path: &str,
) -> Result<(), String> {
    let (reader, size) = slot::open_download_reader(source_operator, source_path).await?;
    let mut writer = target_operator
        .writer(target_path)
        .await
        .map_err(|error| format!("Failed to open writer for '{target_path}': {error}"))?;
    let mut offset = 0u64;
    while offset < size {
        let end = offset.saturating_add(TRANSFER_CHUNK_SIZE as u64).min(size);
        let chunk = slot::read_chunk(&reader, offset, end).await?;
        if chunk.is_empty() {
            break;
        }
        offset += chunk.len() as u64;
        writer
            .write(chunk)
            .await
            .map_err(|error| format!("Write to '{target_path}' failed: {error}"))?;
    }
    writer
        .close()
        .await
        .map_err(|error| format!("Failed to close writer for '{target_path}': {error}"))?;
    Ok(())
}

/// Appends `relative` (OpenDAL-relative) onto a user-facing target dir path.
/// An empty `relative` (single-file copy/move plan) maps onto the trimmed
/// base itself instead of appending a trailing `/`.
/// Deletion plan for a degraded dir move/rename: the copied files first,
/// then their (now empty) directory markers deepest-first, then the source
/// dir marker last. Precise per-entry deletes instead of a recursive delete:
/// OpenDAL 0.57's recursive delete matches by bare path prefix on some
/// backends and would wipe prefix siblings (e.g. `d1-renamed` for `d1`).
fn plan_source_deletions(source_path: &str, plan: &[(String, u64)]) -> Vec<String> {
    let source_trimmed = source_path.trim().trim_matches('/');
    let mut deletions: Vec<String> = plan
        .iter()
        .map(|(relative, _)| join_target(source_path, relative))
        .collect();
    let mut markers: Vec<String> = plan
        .iter()
        .flat_map(|(relative, _)| {
            let mut dirs = Vec::new();
            let mut current = relative.as_str();
            while let Some(idx) = current.rfind('/') {
                current = &current[..idx];
                dirs.push(join_target(source_path, current));
            }
            dirs
        })
        .collect();
    markers.sort();
    markers.dedup();
    markers.reverse(); // sub-dirs deepest first …
    markers.push(format!("{source_trimmed}/")); // … source dir last
    deletions.extend(markers); // order: files, sub-dirs, source dir
    deletions
}

fn join_target(target_path: &str, relative: &str) -> String {
    let base = target_path.trim().trim_matches('/');
    match (base.is_empty(), relative.is_empty()) {
        (true, _) => relative.to_string(),
        (_, true) => base.to_string(),
        (false, false) => format!("{base}/{relative}"),
    }
}

// ---------------------------------------------------------------------------
// Progress payloads
// ---------------------------------------------------------------------------

/// `files/transfer/progress` payload for a single-file job: the frozen
/// `{taskId, transferred, total}` core plus size/state/kind context for the
/// UI. Emission is best-effort; failures never fail the transfer. Payloads
/// carry paths/ids only — never credentials.
fn emit_job_progress(emitter: &PluginEmitter, job: &TransferJob) {
    let mut event = progress_event(&job.task_id, job.transferred_bytes, job.total_bytes);
    if let Some(object) = event.as_object_mut() {
        object.insert("size".into(), serde_json::json!(job.total_bytes));
        object.insert("state".into(), serde_json::json!(job.status.as_str()));
        object.insert(
            "kind".into(),
            serde_json::json!(match job.kind {
                TransferKind::Upload => "upload",
                TransferKind::Download => "download",
            }),
        );
        object.insert("connectionId".into(), serde_json::json!(job.connection_id));
        object.insert("remotePath".into(), serde_json::json!(job.remote_path));
        if let Some(error) = &job.error {
            object.insert("error".into(), serde_json::json!(error));
        }
    }
    let _ = emitter.event("files/transfer/progress", event);
}

fn emit_running(emitter: &PluginEmitter, task_id: &str) {
    let mut event = progress_event(task_id, 0, None);
    if let Some(object) = event.as_object_mut() {
        object.insert(
            "state".into(),
            serde_json::json!(JobStatus::Running.as_str()),
        );
    }
    let _ = emitter.event("files/transfer/progress", event);
}

fn emit_running_at(emitter: &PluginEmitter, task_id: &str, transferred: u64, total: Option<u64>) {
    let mut event = progress_event(task_id, transferred, total);
    if let Some(object) = event.as_object_mut() {
        object.insert("size".into(), serde_json::json!(total));
        object.insert(
            "state".into(),
            serde_json::json!(JobStatus::Running.as_str()),
        );
    }
    let _ = emitter.event("files/transfer/progress", event);
}

/// Directory-job progress payload (`filesDone, filesTotal, bytesDone,
/// bytesTotal` per §7) plus state context.
fn emit_dir_progress(emitter: &PluginEmitter, job: &DirJob) {
    let mut event = dir_progress_event(job);
    if let Some(object) = event.as_object_mut() {
        object.insert("state".into(), serde_json::json!(job.status.as_str()));
        object.insert("sync".into(), serde_json::json!(job.sync));
        // P-FILES ①a/①b: carry the triggering kind so event-driven UI can
        // classify the job without a status round-trip.
        object.insert("kind".into(), serde_json::json!(job.kind));
        object.insert("remotePath".into(), serde_json::json!(job.source_path));
        if let Some(error) = &job.error {
            object.insert("error".into(), serde_json::json!(error));
        }
    }
    let _ = emitter.event("files/transfer/progress", event);
}

fn emit_dir_running(emitter: &PluginEmitter, job_id: &str) {
    let _ = emitter.event(
        "files/transfer/progress",
        serde_json::json!({ "jobId": job_id, "state": JobStatus::Running.as_str() }),
    );
}

/// Progress event helper shared by all job types: builds the
/// `files/transfer/progress` payload. Throttling state (last emitted ts /
/// fraction) lives with each job — F-C owns the exact bookkeeping.
pub fn progress_event(task_id: &str, transferred: u64, total: Option<u64>) -> serde_json::Value {
    serde_json::json!({
        "taskId": task_id,
        "transferred": transferred,
        "total": total,
    })
}

/// Directory-job progress payload (`filesDone, filesTotal, bytesDone,
/// bytesTotal` per §7).
pub fn dir_progress_event(job: &DirJob) -> serde_json::Value {
    serde_json::json!({
        "jobId": job.job_id,
        "filesDone": job.files_done,
        "filesTotal": job.files_total,
        "bytesDone": job.bytes_done,
        "bytesTotal": job.bytes_total,
    })
}

/// Decodes an incoming upload frame into `(offset, payload)`; enforces the
/// 8-byte BE prefix and the 256 KiB chunk cap. Shared by `append_upload`.
pub fn parse_upload_frame(data: &[u8]) -> Result<(u64, &[u8]), String> {
    if data.len() < 8 {
        return Err("Upload frame is missing its 8-byte offset".to_string());
    }
    let offset = u64::from_be_bytes(data[..8].try_into().unwrap());
    let payload = &data[8..];
    if payload.len() > crate::model::TRANSFER_CHUNK_SIZE {
        return Err(format!(
            "Upload chunk of {} bytes exceeds the {} byte limit",
            payload.len(),
            crate::model::TRANSFER_CHUNK_SIZE
        ));
    }
    Ok((offset, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_upload_frames_with_offset_prefix() {
        let mut frame = 42u64.to_be_bytes().to_vec();
        frame.extend_from_slice(&vec![0u8; 16]);
        let (offset, payload) = parse_upload_frame(&frame).unwrap();
        assert_eq!(offset, 42);
        assert_eq!(payload.len(), 16);
    }

    #[test]
    fn rejects_oversized_and_short_frames() {
        assert!(parse_upload_frame(&[0, 0]).is_err());
        let oversized = vec![0u8; 8 + crate::model::TRANSFER_CHUNK_SIZE + 1];
        assert!(parse_upload_frame(&oversized).is_err());
    }

    #[test]
    fn progress_payloads_are_camel_case() {
        let event = progress_event("t1", 128, Some(256));
        assert!(event["taskId"] == "t1");
        let dir_job = DirJob {
            job_id: "j1".into(),
            source_connection_id: "a".into(),
            source_path: "/s".into(),
            target_connection_id: "b".into(),
            target_path: "/t".into(),
            sync: false,
            delete_source: false,
            kind: DirJobKind::Copy,
            files_done: 1,
            files_total: Some(2),
            bytes_done: 10,
            bytes_total: Some(20),
            status: JobStatus::Running,
            error: None,
            started_at: None,
            finished_at: None,
        };
        let event = dir_progress_event(&dir_job);
        assert!(event["filesTotal"] == 2);
        assert!(event["bytesDone"] == 10);
    }

    #[test]
    fn job_status_strings_match_contract() {
        assert_eq!(JobStatus::Queued.as_str(), "queued");
        assert_eq!(JobStatus::Canceled.as_str(), "canceled");
    }

    #[test]
    fn terminal_finish_maps_failed_and_canceled_to_errors() {
        // issue#6-3：终态 finish 不得一律幂等成功——Failed 带出存储的错误，
        // Canceled 如实报取消；只有 Completed 才 Ok（此前 Failed 再 finish
        // 返回 Ok，前端标 completed 并提示「已上传」，形成假成功）。
        let build = |status: JobStatus, error: Option<&str>| TransferJob {
            task_id: "t".into(),
            connection_id: "c".into(),
            kind: TransferKind::Upload,
            remote_path: "/x.bin".into(),
            total_bytes: Some(100),
            transferred_bytes: 5,
            status,
            error: error.map(String::from),
            started_at: None,
            finished_at: Some(1),
        };
        assert_eq!(
            JobTable::terminal_finish_result(&build(JobStatus::Failed, Some("Upload write failed: boom"))),
            Err("Upload write failed: boom".to_string())
        );
        assert_eq!(
            JobTable::terminal_finish_result(&build(JobStatus::Failed, None)),
            Err("Upload failed".to_string())
        );
        assert_eq!(
            JobTable::terminal_finish_result(&build(JobStatus::Canceled, None)),
            Err("Upload was canceled".to_string())
        );
        assert_eq!(JobTable::terminal_finish_result(&build(JobStatus::Completed, None)), Ok(()));
    }

    #[test]
    fn job_table_constructs_without_side_effects() {
        let table = JobTable::new();
        let _ = table; // constructor must not spawn tasks or touch the store
    }

    #[test]
    fn throttle_emits_first_and_on_delta() {
        let mut throttle = Throttle::default();
        assert!(throttle.should_emit(0, Some(1000)), "first emits");
        // 10% delta always emits regardless of elapsed time.
        assert!(throttle.should_emit(100, Some(1000)), ">=1% delta emits");
        // 0.1% delta without 200ms elapsed: suppressed (the interval path).
        assert!(
            !throttle.should_emit(101, Some(1000)),
            "sub-delta sub-interval update is throttled"
        );
        // Crossing another full 1% emits again.
        assert!(throttle.should_emit(111, Some(1000)), ">=1% delta emits");
        // No size known: fraction stays 0 → only the interval path can emit.
        let _ = throttle.should_emit(5, None);
    }

    #[test]
    fn delete_source_precisely_spares_prefix_siblings() {
        // Regression anchor for the `rename_dir_degrade` smoke scenario:
        // deleting `/d1` must not touch `/d1-renamed` (OpenDAL 0.57
        // recursive delete matches by bare prefix). Files first, empty
        // markers deepest-first, source dir marker last.
        let plan = vec![
            ("inner.txt".to_string(), 1),
            ("sub/deep.txt".to_string(), 2),
        ];
        let deletions = plan_source_deletions("/d1", &plan);
        assert_eq!(
            deletions,
            vec![
                "d1/inner.txt",
                "d1/sub/deep.txt",
                "d1/sub",   // deepest dir marker first …
                "d1/",      // … source dir marker last
            ]
        );
        // No sibling of "d1" appears anywhere in the plan.
        assert!(deletions.iter().all(|p| p == "d1" || p.starts_with("d1/")));
    }

    #[test]
    fn join_target_keeps_relative_paths() {
        assert_eq!(join_target("/backup", "a/b.txt"), "backup/a/b.txt");
        assert_eq!(join_target("/", "a.txt"), "a.txt");
        assert_eq!(join_target("dest/", "x"), "dest/x");
        // Single-file copy/move plan: empty relative maps onto the base.
        assert_eq!(join_target("/dst/copy.txt", ""), "dst/copy.txt");
        assert_eq!(join_target("/s/f.txt", ""), "s/f.txt");
    }

    #[test]
    fn dir_job_gates_block_read_only_and_delete_disallowed_targets() {
        fn connect(config: serde_json::Value) -> StoredConnection {
            StoredConnection::from_lifecycle_params(&json!({
                "connection": { "id": "t", "external_config": config }
            }))
            .unwrap()
        }
        let read_only = connect(json!({ "protocol": "fs", "read_only": true }));
        assert!(validate_dir_job_gates(&read_only, false).is_err());
        assert!(validate_dir_job_gates(&read_only, true).is_err());

        let no_delete = connect(json!({ "protocol": "fs", "allow_delete": false }));
        assert!(validate_dir_job_gates(&no_delete, false).is_ok());
        assert!(validate_dir_job_gates(&no_delete, true).is_err());

        let writable = connect(json!({ "protocol": "fs" }));
        assert!(validate_dir_job_gates(&writable, false).is_ok());
        assert!(validate_dir_job_gates(&writable, true).is_ok());
    }

    #[test]
    fn list_merges_live_jobs_with_store_history() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::new(dir.path().to_path_buf());
            store
                .record_transfer(store::TransferRecord {
                    task_id: "hist-1".into(),
                    connection_id: "c1".into(),
                    kind: "download".into(),
                    remote_path: "/old.bin".into(),
                    total_bytes: Some(10),
                    transferred_bytes: 10,
                    status: "completed".into(),
                    error: None,
                    started_at: None,
                    finished_at: None,
                })
                .unwrap();

            let table = JobTable::new();
            table
                .inner
                .jobs
                .lock()
                .await
                .insert(
                    "live-1".to_string(),
                    TransferJob {
                        task_id: "live-1".into(),
                        connection_id: "c1".into(),
                        kind: TransferKind::Upload,
                        remote_path: "/live.bin".into(),
                        total_bytes: Some(5),
                        transferred_bytes: 2,
                        status: JobStatus::Running,
                        error: None,
                        started_at: None,
                        finished_at: None,
                    },
                );
            // record the same id in history to prove the live map wins
            store
                .record_transfer(store::TransferRecord {
                    task_id: "live-1".into(),
                    connection_id: "c1".into(),
                    kind: "upload".into(),
                    remote_path: "/live.bin".into(),
                    total_bytes: Some(5),
                    transferred_bytes: 5,
                    status: "completed".into(),
                    error: None,
                    started_at: None,
                    finished_at: None,
                })
                .unwrap();

            let jobs = table.list(&store, Some("c1")).await.unwrap();
            assert_eq!(jobs.len(), 2, "live + history, deduped: {jobs:?}");
            assert!(jobs.iter().any(|job| job.task_id == "hist-1"));
            let live = jobs.iter().find(|job| job.task_id == "live-1").unwrap();
            assert_eq!(live.status, JobStatus::Running, "live entry wins");

            let filtered = table.list(&store, Some("other")).await.unwrap();
            assert!(filtered.is_empty(), "connection filter applies: {filtered:?}");
        });
    }

    /// P-FILES ①a: the unified list view must expose dir jobs (syncDir/
    /// copyDir and the degraded copy/move/rename) alongside single-file jobs
    /// and history, with the connection filter matching source OR target.
    #[test]
    fn list_merged_includes_dir_jobs_with_kind_and_connection_filter() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::new(dir.path().to_path_buf());
            let table = JobTable::new();
            let mk = |job_id: &str, source: &str, target: &str, kind: DirJobKind, sync: bool, delete: bool| DirJob {
                job_id: job_id.into(),
                source_connection_id: source.into(),
                source_path: "/s".into(),
                target_connection_id: target.into(),
                target_path: "/t".into(),
                sync,
                delete_source: delete,
                kind,
                files_done: 0,
                files_total: None,
                bytes_done: 0,
                bytes_total: None,
                status: JobStatus::Running,
                error: None,
                started_at: Some(42),
                finished_at: None,
            };
            {
                let mut dir_jobs = table.inner.dir_jobs.lock().await;
                dir_jobs.insert("sync-1".into(), mk("sync-1", "c1", "c2", DirJobKind::SyncDir, true, false));
                dir_jobs.insert("copy-1".into(), mk("copy-1", "c1", "c1", DirJobKind::Copy, false, false));
                dir_jobs.insert("move-1".into(), mk("move-1", "c2", "c2", DirJobKind::Move, false, true));
                dir_jobs.insert("rename-1".into(), mk("rename-1", "c3", "c3", DirJobKind::Rename, false, true));
            }

            let all = table.list_merged(&store, None).await.unwrap();
            assert_eq!(all.len(), 4, "all dir jobs visible: {all:?}");
            let by_id = |items: &[serde_json::Value], id: &str| {
                items
                    .iter()
                    .find(|item| item["jobId"] == id)
                    .cloned()
                    .unwrap_or_else(|| panic!("missing {id} in {items:?}"))
            };
            assert_eq!(by_id(&all, "sync-1")["kind"], "syncDir");
            assert_eq!(by_id(&all, "copy-1")["kind"], "copy");
            assert_eq!(by_id(&all, "move-1")["kind"], "move");
            assert_eq!(by_id(&all, "rename-1")["kind"], "rename");
            // camelCase + §7 progress fields over the wire.
            let sync_item = by_id(&all, "sync-1");
            assert!(sync_item["sourceConnectionId"] == "c1" && sync_item["targetPath"] == "/t");
            assert!(sync_item["status"] == "running" && sync_item["bytesDone"] == 0);

            // Filter matches the source OR the target connection id.
            let c1 = table.list_merged(&store, Some("c1")).await.unwrap();
            assert_eq!(c1.len(), 2, "source-side filter: {c1:?}");
            assert!(c1.iter().all(|item| item["sourceConnectionId"] == "c1" || item["targetConnectionId"] == "c1"));
            let c3 = table.list_merged(&store, Some("c3")).await.unwrap();
            assert_eq!(c3.len(), 1, "target-side filter: {c3:?}");
            assert_eq!(c3[0]["jobId"], "rename-1");

            // Single-file jobs and history flow through the same view.
            table
                .inner
                .jobs
                .lock()
                .await
                .insert(
                    "live-1".to_string(),
                    TransferJob {
                        task_id: "live-1".into(),
                        connection_id: "c1".into(),
                        kind: TransferKind::Upload,
                        remote_path: "/live.bin".into(),
                        total_bytes: Some(5),
                        transferred_bytes: 2,
                        status: JobStatus::Running,
                        error: None,
                        started_at: None,
                        finished_at: None,
                    },
                );
            let mixed = table.list_merged(&store, Some("c1")).await.unwrap();
            assert_eq!(mixed.len(), 3, "single-file + dir jobs merged: {mixed:?}");
            assert!(mixed.iter().any(|item| item["taskId"] == "live-1"));
        });
    }

    /// P-FILES ⑥: `files/transfers/clear` drops finished jobs/history only;
    /// queued/running entries survive; optional connectionId scopes the wipe.
    #[test]
    fn clear_drops_finished_history_and_keeps_active_jobs() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::new(dir.path().to_path_buf());
            let table = JobTable::new();

            let mk_job = |task_id: &str, connection: &str, status: JobStatus| TransferJob {
                task_id: task_id.into(),
                connection_id: connection.into(),
                kind: TransferKind::Upload,
                remote_path: format!("/{task_id}.bin"),
                total_bytes: Some(5),
                transferred_bytes: 5,
                status,
                error: None,
                started_at: None,
                finished_at: None,
            };
            {
                let mut jobs = table.inner.jobs.lock().await;
                jobs.insert("done-1".into(), mk_job("done-1", "c1", JobStatus::Completed));
                jobs.insert("fail-1".into(), mk_job("fail-1", "c2", JobStatus::Failed));
                jobs.insert("run-1".into(), mk_job("run-1", "c1", JobStatus::Running));
            }
            {
                let mut dir_jobs = table.inner.dir_jobs.lock().await;
                let mut finished = DirJob {
                    job_id: "dir-1".into(),
                    source_connection_id: "c1".into(),
                    source_path: "/s".into(),
                    target_connection_id: "c2".into(),
                    target_path: "/t".into(),
                    sync: false,
                    delete_source: false,
                    kind: DirJobKind::CopyDir,
                    files_done: 1,
                    files_total: Some(1),
                    bytes_done: 5,
                    bytes_total: Some(5),
                    status: JobStatus::Completed,
                    error: None,
                    started_at: Some(42),
                    finished_at: Some(99),
                };
                dir_jobs.insert("dir-1".into(), finished.clone());
                finished.job_id = "dir-run".into();
                finished.status = JobStatus::Running;
                dir_jobs.insert("dir-run".into(), finished);
            }
            for (task_id, connection) in [("hist-1", "c1"), ("hist-2", "c3")] {
                store
                    .record_transfer(store::TransferRecord {
                        task_id: task_id.into(),
                        connection_id: connection.into(),
                        kind: "upload".into(),
                        remote_path: format!("/{task_id}.bin"),
                        total_bytes: Some(5),
                        transferred_bytes: 5,
                        status: "completed".into(),
                        error: None,
                        started_at: None,
                        finished_at: None,
                    })
                    .unwrap();
            }

            // Scoped clear: only c1 surfaces.
            let cleared = table.clear(&store, Some("c1")).await.unwrap();
            assert_eq!(cleared, 3, "done-1 + dir-1 + hist-1: {cleared}");
            let jobs = table.inner.jobs.lock().await;
            assert!(jobs.contains_key("fail-1") && jobs.contains_key("run-1"));
            assert!(!jobs.contains_key("done-1"));
            drop(jobs);
            assert!(!table.inner.dir_jobs.lock().await.contains_key("dir-1"));
            assert!(table.inner.dir_jobs.lock().await.contains_key("dir-run"));
            let history: Vec<String> = store
                .load_transfers()
                .into_iter()
                .map(|record| record.task_id)
                .collect();
            assert_eq!(history, vec!["hist-2".to_string()], "c3 history kept");

            // Unscoped clear removes the rest of the finished entries.
            let cleared_all = table.clear(&store, None).await.unwrap();
            assert_eq!(cleared_all, 2, "fail-1 + hist-2");
            assert!(store.load_transfers().is_empty());
            let remaining = table.inner.jobs.lock().await;
            assert_eq!(remaining.len(), 1, "running job survives: {remaining:?}");
            assert_eq!(remaining["run-1"].status, JobStatus::Running);
        });
    }
}
