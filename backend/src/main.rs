//! `io.dbx.files` sidecar: DBX PluginServer (stdio-framed + binary channels)
//! dispatching storage operations to the OpenDAL engine.
//!
//! Structure mirrors the proven ssh-sftp sidecar: `PluginServer::new(..,
//! Framed).serve()`, a `handle` switch per method (`§8` method table), and
//! `handle_binary` for the transfer channels `files/upload/{taskId}` /
//! `files/download/{taskId}` (8-byte BE offset + <=256 KiB payloads).
//! `operationId` falls back to a local uuid on Host API 1.0 (ssh-sftp
//! main.rs:694 pattern).

#![recursion_limit = "256"]

mod archive;
mod engine;
mod local_downloads;
mod mcp;
mod model;
mod store;
mod transfers;

#[cfg(test)]
mod bench;

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
// Underscore import: only the trait's methods are needed, and the name
// `Engine` is reserved for the storage engine module below.
use base64::Engine as _;
use dbx_plugin_sdk::{
    PluginEmitter, PluginError, PluginHandler, PluginMetadata, PluginServer, PluginTransport,
    RequestContext,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::runtime::Runtime;

use engine::Engine;
use model::StoredConnection;
use store::Store;
use transfers::JobTable;

struct Plugin {
    runtime: Runtime,
    engine: Arc<Engine>,
    transfers: Arc<JobTable>,
    store: Arc<Store>,
    mcp: Arc<mcp::Mcp>,
}

impl Plugin {
    fn new() -> Result<Self, String> {
        let data_dir = Store::default_dir();
        std::fs::create_dir_all(&data_dir).map_err(|error| {
            format!(
                "Failed to create plugin data directory {}: {error}",
                data_dir.display()
            )
        })?;
        let runtime =
            Runtime::new().map_err(|error| format!("Failed to create async runtime: {error}"))?;
        let transfers = Arc::new(JobTable::new());
        let mcp = Arc::new(mcp::Mcp::new(data_dir.clone()));
        let store = Arc::new(Store::new(data_dir));
        // P-FILES ①c (X-A handover ④): hydrate the persisted transfer history
        // at startup (M3-F3-4 hook). Behavior is unchanged — `list` reads the
        // store fresh — but the in-memory mirror is now wired instead of
        // relying on the lazy first-write path.
        runtime.block_on(transfers.load_history(&store));
        Ok(Self {
            runtime,
            engine: Arc::new(Engine::new()),
            transfers,
            store,
            mcp,
        })
    }

    fn handle_request(
        &self,
        method: &str,
        params: Value,
        emitter: &PluginEmitter,
    ) -> Result<Value, String> {
        match method {
            // ------------------------------------------------------------------
            // Lifecycle (M0 §3.2)
            // ------------------------------------------------------------------
            "connection/test" => {
                let connection = StoredConnection::from_lifecycle_params(&params)?;
                let _operation_id = operation_id(&params);
                self.runtime
                    .block_on(self.engine.test(&connection))?;
                Ok(json!({
                    "success": true,
                    "message": "Storage backend reachable"
                }))
            }
            "connection/connect" => {
                let connection = StoredConnection::from_lifecycle_params(&params)?;
                self.engine.connect(connection)?;
                Ok(json!({ "success": true }))
            }
            "connection/disconnect" => {
                let connection_id = params
                    .get("connection")
                    .and_then(|value| value.get("id"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or("Missing connection id")?
                    .to_string();
                // Cancel the connection's transfer jobs first (M0 §3.2). The
                // F-C transfer layer is still a `todo!()` stub, but until jobs
                // exist the table is empty and there is genuinely nothing to
                // cancel — so a stub panic is swallowed here to keep
                // disconnect idempotent-successful per the lifecycle contract.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.runtime
                        .block_on(self.transfers.cancel_connection_jobs(&connection_id, emitter))
                }));
                self.engine.disconnect(&connection_id)?;
                Ok(json!({ "success": true }))
            }

            // ------------------------------------------------------------------
            // Browse & metadata (§8.1)
            // ------------------------------------------------------------------
            "files/list" => {
                let request: model::ListRequest = parse(params)?;
                let operator = self.engine.operator(&request.connection_id)?;
                let entries = self.runtime.block_on(engine::ops::list(
                    &operator,
                    &request.path,
                    request.recurse,
                ))?;
                Ok(json!({ "entries": entries }))
            }
            "files/listPaged" => {
                let request: model::ListPagedRequest = parse(params)?;
                let operator = self.engine.operator(&request.connection_id)?;
                let (entries, total) = self.runtime.block_on(engine::ops::list_paged(
                    &operator,
                    &request.path,
                    request.page,
                    request.page_size,
                ))?;
                Ok(json!({ "entries": entries, "total": total }))
            }
            "files/stat" => {
                let request: model::PathRequest = parse(params)?;
                let operator = self.engine.operator(&request.connection_id)?;
                let entry = self
                    .runtime
                    .block_on(engine::ops::stat(&operator, &request.path))?;
                Ok(json!({ "entry": entry }))
            }
            "files/quickPaths" => {
                let connection_id = connection_id_param(&params)?.to_string();
                let operator = self.engine.operator(&connection_id)?;
                let connection = self.engine.connection(&connection_id)?;
                let payload = self
                    .runtime
                    .block_on(engine::ops::quick_paths(&operator, &connection))?;
                Ok(payload)
            }
            "files/capabilities" => {
                let connection_id = connection_id_param(&params)?.to_string();
                let operator = self.engine.operator(&connection_id)?;
                // Single-logic projection of `info().full_capability()`
                // (F-B handover item ③: the inline copy is gone; the wire
                // shape is unchanged).
                let capabilities = engine::ops::capabilities(&operator)?;
                let mut payload =
                    serde_json::to_value(capabilities).map_err(|error| error.to_string())?;
                // 策略层只读门禁（连接表单 read_only ∥ 宿主标准 read_only）
                // 透出给前端，驱动写操作的禁用态。
                let read_only = self.engine.connection(&connection_id)?.read_only;
                if let Some(object) = payload.as_object_mut() {
                    object.insert("readOnly".to_string(), serde_json::Value::Bool(read_only));
                }
                Ok(payload)
            }
            "files/size" => {
                let request: model::PathRequest = parse(params)?;
                let operator = self.engine.operator(&request.connection_id)?;
                let (count, bytes) = self
                    .runtime
                    .block_on(engine::ops::size(&operator, &request.path))?;
                Ok(json!({ "count": count, "bytes": bytes }))
            }
            "files/publicLink" => {
                let request: model::PublicLinkRequest = parse(params)?;
                let operator = self.engine.operator(&request.connection_id)?;
                let expire_secs = request.expire_secs.unwrap_or(3600).max(1);
                let url = self.runtime.block_on(engine::ops::public_link(
                    &operator,
                    &request.path,
                    expire_secs,
                ))?;
                Ok(json!({ "url": url }))
            }

            // ------------------------------------------------------------------
            // Read/write & structure (§8.2)
            // ------------------------------------------------------------------
            "files/read" => {
                let request: model::ReadRequest = parse(params)?;
                let operator = self.engine.operator(&request.connection_id)?;
                let max_bytes = request
                    .max_bytes
                    .unwrap_or(256 * 1024)
                    .clamp(1, model::MAX_PREVIEW_BYTES as u64) as usize;
                let (data, truncated) = self.runtime.block_on(engine::ops::read(
                    &operator,
                    &request.path,
                    max_bytes,
                ))?;
                Ok(json!({
                    "dataBase64": BASE64_STANDARD.encode(data),
                    "truncated": truncated
                }))
            }
            "files/write" => {
                let request: model::WriteRequest = parse(params)?;
                let connection = self.engine.connection(&request.connection_id)?;
                ensure_writable(&connection)?;
                let operator = self.engine.operator(&request.connection_id)?;
                let data = BASE64_STANDARD
                    .decode(request.data_base64.as_bytes())
                    .map_err(|error| format!("Invalid base64 file data: {error}"))?;
                if data.len() > model::MAX_INLINE_WRITE_BYTES {
                    return Err(format!(
                        "Inline write payload of {} bytes exceeds {}; use the upload channel",
                        data.len(),
                        model::MAX_INLINE_WRITE_BYTES
                    ));
                }
                self.runtime
                    .block_on(engine::ops::write(&operator, &request.path, data))?;
                Ok(json!({ "success": true }))
            }
            "files/mkdir" => {
                let request: model::PathRequest = parse(params)?;
                let connection = self.engine.connection(&request.connection_id)?;
                ensure_writable(&connection)?;
                let operator = self.engine.operator(&request.connection_id)?;
                self.runtime
                    .block_on(engine::ops::mkdir(&operator, &request.path))?;
                Ok(json!({ "success": true }))
            }
            "files/rmdir" => {
                let request: model::PathRequest = parse(params)?;
                let connection = self.engine.connection(&request.connection_id)?;
                ensure_writable(&connection)?;
                ensure_deletable(&connection)?;
                let operator = self.engine.operator(&request.connection_id)?;
                self.runtime
                    .block_on(engine::ops::rmdir(&operator, &request.path))?;
                Ok(json!({ "success": true }))
            }
            "files/delete" => {
                let request: model::PathRequest = parse(params)?;
                let connection = self.engine.connection(&request.connection_id)?;
                ensure_deletable(&connection)?;
                let operator = self.engine.operator(&request.connection_id)?;
                self.runtime
                    .block_on(engine::ops::delete(&operator, &request.path))?;
                self.audit(&connection, method, &request.path, "ok")?;
                Ok(json!({ "success": true }))
            }
            "files/purge" => {
                let request: model::PathRequest = parse(params)?;
                let connection = self.engine.connection(&request.connection_id)?;
                ensure_deletable(&connection)?;
                refuse_root_purge(&connection, &request.path)?;
                let operator = self.engine.operator(&request.connection_id)?;
                self.runtime
                    .block_on(engine::ops::purge(&operator, &request.path))?;
                self.audit(&connection, method, &request.path, "ok")?;
                Ok(json!({ "success": true }))
            }
            "files/copy" | "files/move" => {
                let request: model::CopyMoveRequest = parse(params)?;
                let connection = self.engine.connection(&request.connection_id)?;
                ensure_writable(&connection)?;
                // `move` implicitly deletes the source after the copy (§8.2
                // degrade), so it also passes the delete gate — same rule as
                // policy::PathPolicy::check_rename.
                if method == "files/move" {
                    ensure_deletable(&connection)?;
                }
                let source_connection_id = request
                    .source_connection_id
                    .unwrap_or_else(|| request.connection_id.clone());
                let target_connection_id = request
                    .target_connection_id
                    .unwrap_or_else(|| request.connection_id.clone());
                let source = self.engine.operator(&source_connection_id)?;
                let target = self.engine.operator(&target_connection_id)?;
                // §8.2 decision tree (X-A ③): same Operator instance +
                // native capability → server-side copy/rename executed inline;
                // everything else degrades to a real async read→write job on
                // the JobTable — the response carries a pollable `jobId`
                // (previously `null`; see docs/PROGRESS-XA.zh-CN.md §2).
                let native = if method == "files/copy" {
                    engine::ops::native_copy_available(&source, &target)
                } else {
                    engine::ops::native_move_available(&source, &target)
                };
                let (transport, job_id) = if native {
                    let outcome = if method == "files/copy" {
                        self.runtime.block_on(engine::ops::copy(
                            &source,
                            &target,
                            &request.source_path,
                            &request.target_path,
                        ))?
                    } else {
                        self.runtime.block_on(engine::ops::move_path(
                            &source,
                            &target,
                            &request.source_path,
                            &request.target_path,
                        ))?
                    };
                    let transport = match outcome.transport {
                        engine::ops::CopyTransport::Native => "native",
                        engine::ops::CopyTransport::Job => "job",
                    };
                    (transport, outcome.job_id)
                } else {
                    let source_connection = self.engine.connection(&source_connection_id)?;
                    let target_connection = self.engine.connection(&target_connection_id)?;
                    let kind = if method == "files/move" {
                        transfers::DirJobKind::Move
                    } else {
                        transfers::DirJobKind::Copy
                    };
                    let job_id = self.runtime.block_on(self.transfers.enqueue_copy_job(
                        &source_connection,
                        &source,
                        &target_connection,
                        &target,
                        &request.source_path,
                        &request.target_path,
                        method == "files/move",
                        kind,
                        emitter,
                    ))?;
                    ("job", Some(job_id))
                };
                self.audit(&connection, method, &request.source_path, "ok")?;
                Ok(json!({
                    "success": true,
                    "transport": transport,
                    "jobId": job_id,
                }))
            }
            "files/rename" => {
                let request: model::RenameRequest = parse(params)?;
                let connection = self.engine.connection(&request.connection_id)?;
                ensure_writable(&connection)?;
                // Rename removes the source path (native rename or the
                // copy+delete degrade), so the delete gate applies — same
                // rule as policy::PathPolicy::check_rename.
                ensure_deletable(&connection)?;
                let operator = self.engine.operator(&request.connection_id)?;
                // P-FILES ②: OpenDAL `Operator::rename` validates FILE paths
                // only, so a directory rename degrades to a copy + source
                // delete async job (progress visible, cancellable) — the same
                // engine path as a degraded `files/move`. The response shape
                // matches copy/move (`transport`/`jobId`) so the frontend can
                // wait for the terminal state before refreshing.
                let source_is_dir = self
                    .runtime
                    .block_on(engine::ops::is_dir_path(&operator, &request.path))?;
                if source_is_dir {
                    let job_id = self.runtime.block_on(self.transfers.enqueue_copy_job(
                        &connection,
                        &operator,
                        &connection,
                        &operator,
                        &request.path,
                        &request.new_path,
                        true,
                        transfers::DirJobKind::Rename,
                        emitter,
                    ))?;
                    self.audit(&connection, method, &request.path, "ok")?;
                    return Ok(json!({
                        "success": true,
                        "transport": "job",
                        "jobId": Some(job_id),
                    }));
                }
                self.runtime.block_on(engine::ops::rename(
                    &operator,
                    &request.path,
                    &request.new_path,
                ))?;
                self.audit(&connection, method, &request.path, "ok")?;
                Ok(json!({ "success": true, "transport": "native", "jobId": Option::<String>::None }))
            }

            // ------------------------------------------------------------------
            // Archives (B-ARCHIVE route): tar / tar.gz listing + extraction.
            // zip is explicitly Phase 2; contract per PROGRESS-A-FILES §3.
            // ------------------------------------------------------------------
            "files/archiveList" => {
                let request: model::ArchiveListRequest = parse(params)?;
                let connection = self.engine.connection(&request.connection_id)?;
                let operator = self.engine.operator(&request.connection_id)?;
                // Read gate only: listing never touches storage outside the
                // archive file itself (root whitelist via the policy layer).
                let gate = engine::ops::Gate::from_connection(&connection);
                let archive_path = gate.readable_path(&request.path, false)?;
                let raw = self
                    .runtime
                    .block_on(archive::read_archive(&operator, &archive_path))?;
                let entries = archive::list_entries(&raw, &archive::DEFAULT_LIMITS)?;
                let total = entries.len() as u64;
                let page = request.page.unwrap_or(1).max(1);
                let page_size = request.page_size.unwrap_or(200).clamp(1, 1000);
                let (start, end) = archive::paginate(total, page, page_size);
                Ok(json!({ "entries": &entries[start..end], "total": total }))
            }
            "files/extract" => {
                let request: model::ExtractRequest = parse(params)?;
                let connection = self.engine.connection(&request.connection_id)?;
                // Extract writes the target tree but never deletes the source
                // archive → read_only gate applies, allow_delete does not.
                ensure_writable(&connection)?;
                let operator = self.engine.operator(&request.connection_id)?;
                let gate = engine::ops::Gate::from_connection(&connection);
                let archive_path = gate.readable_path(&request.path, false)?;
                let target_path = gate.ensure_writable_path(&request.target_path, true)?;
                let raw = self
                    .runtime
                    .block_on(archive::read_archive(&operator, &archive_path))?;
                let plan = archive::extract_entries(&raw, &archive::DEFAULT_LIMITS)?;
                let payload_bytes: u64 = plan.iter().map(|entry| entry.size).sum();
                if plan.len() <= archive::MAX_SYNC_EXTRACT_ENTRIES
                    && payload_bytes <= archive::MAX_SYNC_EXTRACT_BYTES
                {
                    // Small package: run synchronously and report `{success}`.
                    let never_canceled = std::sync::atomic::AtomicBool::new(false);
                    self.runtime
                        .block_on(archive::write_entries(
                            &operator,
                            plan,
                            &target_path,
                            &never_canceled,
                            |_, _| {},
                        ))?;
                    self.audit(&connection, method, &request.path, "ok")?;
                    return Ok(json!({
                        "success": true,
                        "transport": "native",
                        "jobId": Option::<String>::None,
                    }));
                }
                // Big package: degrade to a real async job (progress/cancel/
                // status reuse the dir-job table, same semantics as copy/move).
                let job_id = self
                    .runtime
                    .block_on(self.transfers.enqueue_extract_job(
                        &connection,
                        &operator,
                        &archive_path,
                        &target_path,
                        emitter,
                    ))?;
                self.audit(&connection, method, &request.path, "ok")?;
                Ok(json!({
                    "success": true,
                    "transport": "job",
                    "jobId": job_id,
                }))
            }
            "files/compress" => {
                let request: model::CompressRequest = parse(params)?;
                let connection = self.engine.connection(&request.connection_id)?;
                ensure_writable(&connection)?;
                let operator = self.engine.operator(&request.connection_id)?;
                let gate = engine::ops::Gate::from_connection(&connection);
                if request.paths.is_empty() {
                    return Err("paths must not be empty".to_string());
                }
                // Archive file (not a directory): no trailing-slash rewrite.
                let target_path = gate.ensure_writable_path(&request.target_path, false)?;
                let gzip = if target_path.to_lowercase().ends_with(".tar.gz")
                    || target_path.to_lowercase().ends_with(".tgz")
                {
                    true
                } else if target_path.to_lowercase().ends_with(".tar") {
                    false
                } else {
                    return Err(format!(
                        "Archive target '{target_path}' must end with .tar, .tar.gz or .tgz"
                    ));
                };
                // Refuse to overwrite: an existing target is never clobbered
                // by a compression run (the caller picks another name).
                if self
                    .runtime
                    .block_on(operator.stat(&target_path))
                    .is_ok()
                {
                    return Err(format!("Archive target '{target_path}' already exists"));
                }
                for source in &request.paths {
                    gate.readable_path(source, false)?;
                }
                let plan = self
                    .runtime
                    .block_on(archive::plan_compress(&operator, &request.paths))?;
                let payload_bytes: u64 = plan.iter().map(|entry| entry.size).sum();
                if plan.len() <= archive::MAX_SYNC_EXTRACT_ENTRIES
                    && payload_bytes <= archive::MAX_SYNC_EXTRACT_BYTES
                {
                    let data = self
                        .runtime
                        .block_on(archive::build_archive(&operator, &plan, gzip))?;
                    self.runtime
                        .block_on(operator.write(&target_path, data))
                        .map_err(|error| {
                            format!("Failed to write archive '{target_path}': {error}")
                        })?;
                    self.audit(&connection, method, &request.target_path, "ok")?;
                    return Ok(json!({
                        "success": true,
                        "transport": "native",
                        "jobId": Option::<String>::None,
                    }));
                }
                let job_id = self
                    .runtime
                    .block_on(self.transfers.enqueue_compress_job(
                        &connection,
                        &operator,
                        plan,
                        &target_path,
                        gzip,
                        emitter,
                    ))?;
                self.audit(&connection, method, &request.target_path, "ok")?;
                Ok(json!({
                    "success": true,
                    "transport": "job",
                    "jobId": job_id,
                }))
            }

            // ------------------------------------------------------------------
            // Large transfers over binary channels (§8.3)
            // ------------------------------------------------------------------
            "files/upload/start" => {
                let request: model::UploadStartRequest = parse(params)?;
                let connection = self.engine.connection(&request.connection_id)?;
                ensure_writable(&connection)?;
                let operator = self.engine.operator(&request.connection_id)?;
                let task_id = self.runtime.block_on(self.transfers.start_upload(
                    &connection,
                    &operator,
                    &request.remote_path,
                    request.size,
                    emitter,
                ))?;
                Ok(json!({ "taskId": task_id }))
            }
            "files/upload/finish" => {
                let request: model::TaskRequest = parse(params)?;
                self.runtime
                    .block_on(self.transfers.finish_upload(&request.task_id, emitter))?;
                Ok(json!({ "success": true }))
            }
            "files/download/start" => {
                let request: model::DownloadStartRequest = parse(params)?;
                let connection = self.engine.connection(&request.connection_id)?;
                let operator = self.engine.operator(&request.connection_id)?;
                let (task_id, size) = self.runtime.block_on(self.transfers.start_download(
                    &connection,
                    &operator,
                    &request.remote_path,
                    request.save_to_local,
                    request.download_dir.as_deref(),
                    emitter,
                ))?;
                Ok(json!({ "taskId": task_id, "size": size }))
            }
            "files/download/finish" => {
                let request: model::TaskRequest = parse(params)?;
                // saveToLocal 完成的下载在此改名落盘并带回 localPath（历史同
                // 步记录）；web/docker 等本地落盘关闭时为 None。
                let local_path = self
                    .runtime
                    .block_on(self.transfers.finish_download(&request.task_id, emitter))?;
                let mut response = json!({ "success": true, "taskId": request.task_id });
                if let Some(local_path) = local_path {
                    response["localPath"] = json!(local_path);
                }
                Ok(response)
            }

            // ------------------------------------------------------------------
            // Directory sync & job queries (§8.4)
            // ------------------------------------------------------------------
            "files/syncDir" | "files/copyDir" => {
                let request: model::DirJobRequest = parse(params)?;
                let source = self.engine.connection(&request.source_connection_id)?;
                let target = self.engine.connection(&request.target_connection_id)?;
                let source_operator = self.engine.operator(&request.source_connection_id)?;
                let target_operator = self.engine.operator(&request.target_connection_id)?;
                let job_id = self.runtime.block_on(self.transfers.enqueue_dir_job(
                    &source,
                    &source_operator,
                    &target,
                    &target_operator,
                    &request.source_path,
                    &request.target_path,
                    method == "files/syncDir",
                    emitter,
                ))?;
                Ok(json!({ "jobId": job_id }))
            }
            "files/transfers/list" => {
                let request: model::TransfersListRequest = parse(params)?;
                // P-FILES ①a: unified view — single-file jobs + history +
                // dir jobs (syncDir/copyDir/degraded copy/move/rename).
                let jobs = self.runtime.block_on(self.transfers.list_merged(
                    &self.store,
                    request.connection_id.as_deref(),
                ))?;
                Ok(json!({ "jobs": jobs }))
            }
            "files/transfers/clear" => {
                // P-FILES ⑥: drop finished transfer history (all connections
                // or scoped by optional connectionId). Queued/running jobs
                // are never touched.
                let request: model::TransfersListRequest = parse(params)?;
                let cleared = self
                    .runtime
                    .block_on(self.transfers.clear(&self.store, request.connection_id.as_deref()))?;
                Ok(json!({ "cleared": cleared }))
            }
            "files/transfers/delete" => {
                // 传输面板单条删除：按 taskId 移除已结束的记录（内存 + 持久化
                // transfers.json）；活动任务拒绝，先取消再删。
                let request: model::TaskRequest = parse(params)?;
                let removed = self
                    .runtime
                    .block_on(self.transfers.delete_record(&self.store, &request.task_id))?;
                Ok(json!({ "removed": removed }))
            }
            // 本机落盘能力探测：桌面端 sidecar 可直接把下载写进本机下载目录
            // （完成后 localPath 进入传输历史，面板提供 reveal/open）；web/
            // docker 模式探测失败或 canSaveLocal=false 时前端回退宿主
            // fileTransfer 保存或浏览器 <a download>。
            "files/local/capabilities" => {
                let data_dir = Store::default_dir();
                let downloads_dir = local_downloads::downloads_base_dir(
                    None,
                    |key| std::env::var_os(key),
                    &data_dir,
                );
                Ok(json!({
                    "canSaveLocal": local_downloads::can_save_local(|key| std::env::var_os(key)),
                    "downloadsDir": downloads_dir.to_string_lossy(),
                    "platform": local_downloads::platform_name(),
                }))
            }
            // 在文件管理器中定位已完成的下载。只允许 reveal 传输历史里记录过
            // 的 localPath，不能成为任意路径打开原语。
            "files/local/reveal" => {
                let path = params
                    .get("path")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or("Missing path")?;
                let history = self.store.load_transfers();
                local_downloads::reveal_validated(&history, std::path::Path::new(path))?;
                Ok(json!({ "success": true }))
            }
            // 在默认应用中打开已完成的本机下载；同样只允许打开传输历史中记录
            // 过的路径，避免把这个按钮变成任意本机路径执行入口。
            "files/local/open" => {
                let path = params
                    .get("path")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or("Missing path")?;
                let history = self.store.load_transfers();
                local_downloads::open_validated(&history, std::path::Path::new(path))?;
                Ok(json!({ "success": true }))
            }
            "files/transfer/status" => {
                let request: model::JobRequest = parse(params)?;
                if let Some(job) = self
                    .runtime
                    .block_on(self.transfers.status(&request.job_id))?
                {
                    return Ok(json!({ "job": job, "kind": "transfer" }));
                }
                let dir_job = self
                    .runtime
                    .block_on(self.transfers.dir_status(&request.job_id))?
                    .ok_or(format!("Unknown jobId '{}'", request.job_id))?;
                Ok(json!({ "job": dir_job, "kind": "dirJob" }))
            }
            "files/transfer/cancel" => {
                let request: model::TaskRequest = parse(params)?;
                self.runtime
                    .block_on(self.transfers.cancel(&request.task_id, emitter))?;
                Ok(json!({ "success": true }))
            }
            "files/audit/list" => audit_list_response(&self.store, &params),

            // ------------------------------------------------------------------
            // MCP tool surface (shared/IMPL_PLAN_PLUGIN_MCP §2; ssh mcp.rs
            // skeleton parity): discovery / execution / settings, plus the
            // frontend-side intent report channel (design §1).
            // ------------------------------------------------------------------
            "mcp/tools" => Ok(self.mcp.tool_definitions(&self.engine, &params)),
            "mcp/call" => Ok(self.runtime.block_on(self.mcp.call(
                &self.engine,
                &self.transfers,
                &self.store,
                emitter,
                &params,
            ))?),
            "mcp/settings/get" => Ok(self.mcp.settings_get()),
            "mcp/settings/set" => self.mcp.settings_set(&params),
            "files/ui/state/report" => self.mcp.report(&params),
            _ => Err(format!("Method not found: {method}")),
        }
    }

    /// Appends an audit line for a completed write-ish operation. Failures to
    /// audit are logged to stderr but never fail the operation itself.
    fn audit(
        &self,
        connection: &StoredConnection,
        action: &str,
        target: &str,
        result: &str,
    ) -> Result<(), String> {
        if let Err(error) = self.store.append_audit(store::AuditRecord {
            time: store::format_rfc3339(store::unix_millis_now() as i64),
            connection_id: connection.id.clone(),
            action: action.to_string(),
            target: target.to_string(),
            result: result.to_string(),
            // Workbench-originated writes carry no source marker; MCP writes
            // are tagged `source:"mcp"` in the mcp module (design §4).
            source: None,
        }) {
            eprintln!("[io.dbx.files] audit write failed: {error}");
        }
        Ok(())
    }
}

/// `files/audit/list` (UI parity round 3): read-only projection of the
/// store's `audit.jsonl` trail for AuditPanel.vue. Response shape is owned
/// by the panel's parser: `{ entries: [{ at, action, connectionId, path,
/// result }] }`, newest first. `limit` (default 100, clamped to 1..=1000)
/// caps the slice; an optional non-empty `connectionId` filters per
/// connection. Credential red line: `AuditRecord` carries paths only
/// (store.rs), so no secret field can ever appear here.
fn audit_list_response(store: &Store, params: &Value) -> Result<Value, String> {
    let limit = match params.get("limit") {
        None | Some(Value::Null) => 100usize,
        Some(value) => value
            .as_u64()
            .ok_or("Invalid request parameters: limit must be a non-negative integer")?
            .clamp(1, 1000) as usize,
    };
    let connection_filter = params
        .get("connectionId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let mut records = store.read_audit();
    if let Some(id) = connection_filter {
        records.retain(|record| &record.connection_id == id);
    }
    // The trail is stored oldest-first; the panel wants the most recent
    // operations on top.
    records.reverse();
    records.truncate(limit);
    let entries = records
        .into_iter()
        .map(|record| {
            let mut entry = json!({
                "at": record.time,
                "action": record.action,
                "connectionId": record.connection_id,
                "path": record.target,
                "result": record.result,
            });
            // MCP-originated writes carry `source:"mcp"` (MCP design §4);
            // legacy lines and workbench writes omit the field.
            if let Some(source) = record.source {
                entry["source"] = json!(source);
            }
            entry
        })
        .collect::<Vec<_>>();
    Ok(json!({ "entries": entries }))
}

fn ensure_writable(connection: &StoredConnection) -> Result<(), String> {
    if connection.read_only {
        Err("Connection is read-only; write operations are rejected".to_string())
    } else {
        Ok(())
    }
}

/// Unified §9 delete gate (stricter semantics, X-A consolidation): `read_only`
/// rejects every mutating operation — delete/purge/rmdir included — and
/// `allow_delete` independently rejects delete-class ops. Same rule as
/// `policy::PathPolicy::check_delete` / `Gate::ensure_deletable_path`, so
/// main.rs and the policy layer share one semantic (F-B handover item ①).
fn ensure_deletable(connection: &StoredConnection) -> Result<(), String> {
    if connection.read_only {
        Err("Connection is read-only; delete operations are rejected".to_string())
    } else if !connection.allow_delete {
        Err("Connection disallows delete operations (allow_delete=false)".to_string())
    } else {
        Ok(())
    }
}

/// Caller-side root-purge guard (§8.2: `files/purge` must refuse the
/// connection root and `/`). `engine/ops.rs::purge` re-checks once F-B fills
/// the stub — defense in depth, exactly as the F-B contract comment requires.
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

impl PluginHandler for Plugin {
    fn handle(
        &self,
        _context: RequestContext,
        method: &str,
        params: Value,
        emitter: &PluginEmitter,
    ) -> Result<Value, PluginError> {
        // Panic safety net: until F-B/F-C land, the ops/transfer layers are
        // `todo!()` contract stubs, and any future engine panic must never
        // kill the request task silently (the host would then hang waiting
        // for a response frame that is never sent). Convert panics into a
        // business error. The "Method not found" phrasing preserves the smoke
        // suite's SKIP semantics for not-yet-implemented methods; once F-B/F-C
        // fill the stubs the branch is unreachable in normal operation.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.handle_request(method, params, emitter)
        }));
        match outcome {
            Ok(result) => result.map_err(to_plugin_error),
            Err(panic) => Err(to_plugin_error(format!(
                "Method not found: {method} is not implemented yet (sidecar panic: {})",
                panic_message(&panic)
            ))),
        }
    }

    fn handle_binary(
        &self,
        channel: &str,
        data: Vec<u8>,
        emitter: &PluginEmitter,
    ) -> Result<(), PluginError> {
        // Host → sidecar upload: `files/upload/{taskId}` frames carry an
        // 8-byte BE offset + payload chunk (ssh-sftp main.rs:666-671 shape).
        if let Some(task_id) = channel.strip_prefix("files/upload/") {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.runtime
                    .block_on(self.transfers.append_upload(task_id, &data, emitter))
            }));
            return match outcome {
                Ok(result) => result.map_err(to_plugin_error),
                Err(panic) => Err(to_plugin_error(format!(
                    "Upload channel not implemented yet (sidecar panic: {})",
                    panic_message(&panic)
                ))),
            };
        }
        // `files/download/{taskId}` frames only travel sidecar → host and are
        // pushed by the download pump; incoming frames on that prefix are a
        // protocol violation.
        Err(PluginError::new(
            -32601,
            format!("Unknown binary channel: {channel}"),
        ))
    }
}

/// Extracts a printable message from a panic payload (todo!() panics carry a
/// formatted `String`; manual panics may carry a `&'static str`).
fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = panic.downcast_ref::<&'static str>() {
        (*text).to_string()
    } else if let Some(text) = panic.downcast_ref::<String>() {
        text.clone()
    } else {
        "unknown panic".to_string()
    }
}

fn parse<T: DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| format!("Invalid request parameters: {error}"))
}

fn connection_id_param(params: &Value) -> Result<&str, String> {
    params
        .get("connectionId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Missing connectionId".to_string())
}

/// Host API 1.1 passes `operationId` to correlate connection lifecycle calls;
/// on Host API 1.0 it is absent, so a locally generated id is used instead.
/// The id only needs to stay stable between a challenge prompt and its resolve.
fn operation_id(params: &Value) -> String {
    params
        .get("operationId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

fn to_plugin_error(error: String) -> PluginError {
    PluginError::new(-32000, error)
}

fn main() -> std::io::Result<()> {
    // Standalone MCP stdio server mode (`--mcp`, plugin-MCP design §0.2/§5
    // stdio row): serve the MCP tool surface over newline-delimited JSON-RPC
    // instead of the DBX framed protocol. The two modes are mutually
    // exclusive in one process — `--mcp` returns before `PluginServer::serve()`
    // ever starts (ssh plugin `run_mcp_stdio` parity).
    if wants_stdio_mode(std::env::args()) {
        return mcp::run_mcp_stdio(store::Store::default_dir());
    }
    let plugin = Plugin::new().map_err(std::io::Error::other)?;
    let metadata = PluginMetadata::new("io.dbx.files", env!("CARGO_PKG_VERSION"))
        .with_capability("connections")
        .with_capability("events")
        .with_capability("binary")
        .with_capability("filesystem");
    PluginServer::new(metadata, plugin)
        .transport(PluginTransport::Framed)
        .worker_threads(4)
        .serve()
}

/// Pure argument check (unit-tested): `--mcp` anywhere in the argument list
/// selects the standalone stdio MCP server; the DBX framed protocol stays the
/// default and never runs in the same process.
fn wants_stdio_mode<I: Iterator<Item = String>>(mut args: I) -> bool {
    args.any(|arg| arg == "--mcp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wants_stdio_mode_matches_only_the_exact_flag() {
        // `--mcp` anywhere in the argument list selects the stdio server …
        assert!(wants_stdio_mode(["--mcp"].into_iter().map(String::from)));
        assert!(wants_stdio_mode(
            ["--verbose", "--mcp", "extra"].into_iter().map(String::from)
        ));
        // … while the plain invocation (and lookalikes) stay on the framed
        // protocol: the modes never run in the same process.
        assert!(!wants_stdio_mode(Vec::<String>::new().into_iter()));
        assert!(!wants_stdio_mode(["--mcpfoo", "-mcp"].into_iter().map(String::from)));
    }

    #[test]
    fn operation_id_falls_back_to_uuid() {
        let with_id = operation_id(&json!({ "operationId": "op-1" }));
        assert_eq!(with_id, "op-1");
        let fallback = operation_id(&json!({}));
        assert_eq!(fallback.len(), 36, "uuid v4 shape: {fallback}");
        let empty = operation_id(&json!({ "operationId": "" }));
        assert_eq!(empty.len(), 36);
    }

    #[test]
    fn read_only_gate_blocks_writes_and_deletes() {
        let connection = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "c",
                "external_config": { "protocol": "fs", "read_only": true }
            }
        }))
        .unwrap();
        assert!(ensure_writable(&connection).is_err());
        assert!(
            ensure_deletable(&connection).is_err(),
            "read_only rejects every mutating operation, deletes included (§9 stricter semantics)"
        );
    }

    #[test]
    fn allow_delete_gate_blocks_deletes() {
        let connection = StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "c",
                "external_config": { "protocol": "fs", "allow_delete": false }
            }
        }))
        .unwrap();
        assert!(ensure_deletable(&connection).is_err());
        assert!(ensure_writable(&connection).is_ok());
    }

    /// X-A ① alignment test: the main.rs gates and the policy layer
    /// (`policy::PathPolicy`) must decide identically across the whole
    /// `read_only` × `allow_delete` matrix (§9 stricter semantics —
    /// `read_only` rejects deletes too).
    #[test]
    fn gates_match_policy_layer_semantics() {
        use engine::ops::policy::PathPolicy;
        for read_only in [false, true] {
            for allow_delete in [false, true] {
                let connection =
                    StoredConnection::from_lifecycle_params(&json!({
                        "connection": {
                            "id": "c",
                            "external_config": {
                                "protocol": "fs",
                                "read_only": read_only,
                                "allow_delete": allow_delete
                            }
                        }
                    }))
                    .unwrap();
                let policy = PathPolicy::from_parts(
                    &connection.root,
                    connection.lock_to_root,
                    connection.read_only,
                    connection.allow_delete,
                );
                assert_eq!(
                    ensure_writable(&connection).is_ok(),
                    policy.check_write("a/b.txt").is_ok(),
                    "write gate mismatch: read_only={read_only} allow_delete={allow_delete}"
                );
                assert_eq!(
                    ensure_deletable(&connection).is_ok(),
                    policy.check_delete("a/b.txt").is_ok(),
                    "delete gate mismatch: read_only={read_only} allow_delete={allow_delete}"
                );
            }
        }
    }

    #[test]
    fn connection_id_param_validates() {
        assert!(connection_id_param(&json!({ "connectionId": "x" })).is_ok());
        assert!(connection_id_param(&json!({})).is_err());
    }

    // -- files/audit/list ----------------------------------------------------

    use crate::store::{format_rfc3339, AuditRecord};

    fn seed_audit(store: &Store, records: &[(&str, &str, &str, &str)]) {
        for (time_offset, connection, action, target) in records {
            store
                .append_audit(AuditRecord {
                    time: format_rfc3339(1_700_000_000_000 + time_offset.parse::<i64>().unwrap()),
                    connection_id: (*connection).into(),
                    action: (*action).into(),
                    target: (*target).into(),
                    result: "ok".into(),
                    source: None,
                })
                .unwrap();
        }
    }

    #[test]
    fn audit_list_empty_store_returns_empty_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        let response = audit_list_response(&store, &json!({ "limit": 100 })).unwrap();
        assert_eq!(response, json!({ "entries": [] }));
    }

    #[test]
    fn audit_list_maps_camel_case_panel_shape_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        seed_audit(
            &store,
            &[
                ("0", "c1", "files/purge", "/data/old"),
                ("1000", "c1", "files/write", "/data/new.txt"),
            ],
        );
        let response = audit_list_response(&store, &json!({})).unwrap();
        let entries = response["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        // Newest (last appended) first; field names match AuditPanel.vue.
        assert_eq!(entries[0]["at"], "2023-11-14T22:13:21.000Z");
        assert_eq!(entries[0]["action"], "files/write");
        assert_eq!(entries[0]["connectionId"], "c1");
        assert_eq!(entries[0]["path"], "/data/new.txt");
        assert_eq!(entries[0]["result"], "ok");
        assert_eq!(entries[1]["path"], "/data/old");
        // No secret-shaped key may leak into the projection.
        let raw = serde_json::to_string(&response).unwrap().to_ascii_lowercase();
        assert!(!raw.contains("password") && !raw.contains("secret") && !raw.contains("token"));
    }

    #[test]
    fn audit_list_limit_trims_and_filters_by_connection() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        seed_audit(
            &store,
            &[
                ("0", "c1", "files/purge", "/a"),
                ("1000", "c2", "files/purge", "/b"),
                ("2000", "c1", "files/write", "/c"),
                ("3000", "c1", "files/delete", "/d"),
            ],
        );
        // limit caps to the newest N entries.
        let trimmed = audit_list_response(&store, &json!({ "limit": 2 })).unwrap();
        let paths: Vec<&str> = trimmed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["/d", "/c"], "newest-first, trimmed to limit");

        // connectionId filter keeps only that connection's records.
        let filtered =
            audit_list_response(&store, &json!({ "connectionId": "c2" })).unwrap();
        let entries = filtered["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["path"], "/b");
        assert_eq!(entries[0]["connectionId"], "c2");
    }

    #[test]
    fn audit_list_rejects_bad_limit_values() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        for bad in [json!(-1), json!("x"), json!(1.5)] {
            let error = audit_list_response(&store, &json!({ "limit": bad }))
                .expect_err("negative/non-integer limit must be an explicit error");
            assert!(error.contains("limit"), "{bad}: {error}");
        }
        // Explicit null behaves like the default.
        let response = audit_list_response(&store, &json!({ "limit": null })).unwrap();
        assert_eq!(response, json!({ "entries": [] }));
    }

    #[test]
    fn root_purge_guard_refuses_root_and_slash() {
        fn connect(external_config: serde_json::Value) -> StoredConnection {
            StoredConnection::from_lifecycle_params(&json!({
                "connection": { "id": "c", "external_config": external_config }
            }))
            .unwrap()
        }
        // `/` is always refused, with or without a configured root.
        let no_root = connect(json!({ "protocol": "fs" }));
        assert!(refuse_root_purge(&no_root, "/").is_err());
        assert!(refuse_root_purge(&no_root, "").is_err());
        assert!(refuse_root_purge(&no_root, "/data/old").is_ok());

        // The configured root itself is refused regardless of spelling.
        let rooted = connect(json!({ "protocol": "fs", "root": "/srv/data" }));
        for path in ["/srv/data", "/srv/data/", "srv/data"] {
            let error = refuse_root_purge(&rooted, path).unwrap_err();
            assert!(error.contains("root"), "{path}: {error}");
        }
        assert!(refuse_root_purge(&rooted, "/srv/data/child").is_ok());
    }
}
