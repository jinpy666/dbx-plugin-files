//! `io.dbx.files` sidecar: DBX PluginServer (stdio-framed + binary channels)
//! dispatching storage operations to the rclone engine (`rclone rcd` + rc
//! HTTP API).
//!
//! Structure mirrors the proven ssh-sftp sidecar: `PluginServer::new(..,
//! Framed).serve()`, a `handle` switch per method (`§8` method table), and
//! `handle_binary` for the transfer channels `files/upload/{taskId}` /
//! `files/download/{taskId}` (8-byte BE offset + <=256 KiB payloads).
//! `operationId` falls back to a local uuid on Host API 1.0 (ssh-sftp
//! main.rs:694 pattern).

// The inline MCP connection schema (mcp::stdio::inline_connection_properties)
// is one large `json!` literal — the default 128-depth macro recursion limit
// no longer fits it.
#![recursion_limit = "256"]

mod archive;
mod local_downloads;
mod mount;
mod mcp;
mod model;
mod policy;
mod remote_edit;
mod rclone;
mod store;
mod transfers;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
// Underscore import: only the trait's methods are needed, and the name
// `Engine` belongs to the rclone engine type.
use base64::Engine as _;
use dbx_plugin_sdk::{
    PluginEmitter, PluginError, PluginHandler, PluginMetadata, PluginServer, PluginTransport,
    RequestContext,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::runtime::Runtime;

use model::StoredConnection;
use store::Store;

struct Plugin {
    runtime: Runtime,
    rclone: Arc<rclone::RcloneEngine>,
    store: Arc<Store>,
    mcp: Arc<mcp::Mcp>,
    /// F-RCLONE Phase C sync jobs (`files/syncDir`|`files/copyDir`): jobId →
    /// the frozen `sync.rs` handle plus the DirJob projection metadata. The
    /// map is shared with each job's `on_event` closure (std Mutex; every
    /// hold is a short sync section, nothing awaits under the lock), and the
    /// handle is backfilled once `start_job` answers.
    sync_jobs: Arc<std::sync::Mutex<HashMap<String, RcloneSyncRecord>>>,
    /// Local mounts (`files/mount`): mountId → live record. Same std-Mutex
    /// discipline as `sync_jobs` — short sync sections, no awaits held.
    mounts: mount::MountTable,
    /// `files/about` usage cache keyed by connectionId (60s TTL) — the
    /// sidebar renders it on every pane load and rc about is not free.
    about_cache: std::sync::Mutex<HashMap<String, (std::time::Instant, Value)>>,
    /// 本机共享（files/serve/*）：serveId → (connectionId, serveType) 登记表。
    /// serve 实例本身活在 rcd 进程里（rcd 死掉即随之消失），此表只做归属
    /// 记账；serve/list 以 rc 的活跃 id 集合清理陈旧条目。同一 std-Mutex
    /// 短临界区纪律（不持锁 await）。
    serves: std::sync::Mutex<HashMap<String, (String, String)>>,
    /// Remote-edit sessions (`files/remote-edit/*`, 打开方式): session key →
    /// live edit session. Same std-Mutex discipline — the watcher loops and
    /// the RPC arms only flip short bookkeeping fields under the lock.
    remote_edits: remote_edit::EditEngine,
}

impl Plugin {
    fn new() -> Result<Self, String> {
        let data_dir = Store::default_dir();
        init_data_dir(&data_dir);
        let runtime =
            Runtime::new().map_err(|error| format!("Failed to create async runtime: {error}"))?;
        let store = Arc::new(Store::new(data_dir.clone()));
        let rclone = Arc::new(rclone::RcloneEngine::new());
        // Keepalive watchdog: proactive crash respawn + re-registration and
        // idle proxy-group reaping (DBX_FILES_RCLONE_KEEPALIVE_SECS, 0=off).
        rclone.start_keepalive();
        {
            // Phase D transfers-history persistence: the engine appends
            // terminal single-file jobs to the same transfers.json, and the
            // mirror is hydrated here (JobTable::load_history parity) so the
            // panel and the local reveal/open whitelist survive restarts.
            // Only terminal records are ever persisted.
            *rclone_lock(&rclone.history) = Some(Arc::clone(&store));
            let mut jobs = rclone_lock(&rclone.jobs);
            for record in store.load_transfers() {
                if jobs.contains_key(&record.task_id) {
                    continue;
                }
                jobs.insert(record.task_id.clone(), rclone_job_from_record(record));
            }
        }
        let sync_jobs: Arc<std::sync::Mutex<HashMap<String, RcloneSyncRecord>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let mounts: mount::MountTable = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let about_cache: std::sync::Mutex<HashMap<String, (std::time::Instant, Value)>> =
            std::sync::Mutex::new(HashMap::new());
        let serves: std::sync::Mutex<HashMap<String, (String, String)>> =
            std::sync::Mutex::new(HashMap::new());
        let mut mcp = mcp::Mcp::new(data_dir);
        // The rclone engine is the only engine: the MCP storage tools route
        // through it too (same registry, same gates). The sync starter
        // closure shares the workbench syncDir job mirror, so
        // `files/transfer/status` stays the single poll surface.
        mcp.attach_rclone(mcp::RcloneRoute {
            engine: Arc::clone(&rclone),
            store: Arc::clone(&store),
            start_sync: Some(mcp_sync_starter(
                Arc::clone(&rclone),
                Arc::clone(&sync_jobs),
            )),
        });
        let mcp = Arc::new(mcp);
        Ok(Self {
            runtime,
            rclone,
            store,
            mcp,
            sync_jobs,
            mounts,
            about_cache,
            serves,
            remote_edits: remote_edit::EditEngine::new(),
        })
    }

    /// The request route (docs/IMPL_PLAN_RCLONE.zh-CN.md §2/§5/§7, Phase D
    /// end state): every storage method is answered by the rclone engine;
    /// the engine-free support methods (`files/local/*`, `files/audit/list`,
    /// the MCP surface) are answered inline. Storage methods the rclone
    /// engine does not implement fall to the terminal `_` arm with an
    /// explicit unsupported error — there is no second engine to fall back
    /// to anymore.
    async fn handle_request_via_rclone(
        &self,
        method: &str,
        params: Value,
        emitter: &PluginEmitter,
    ) -> Result<Value, String> {
        match method {
            "connection/test" => {
                let connection = StoredConnection::from_lifecycle_params(&params)?;
                // The ::test key keeps a probe from disrupting the live
                // forwarder of an already-connected same-id connection.
                let connection = self
                    .rclone
                    .prepare(&format!("{}::test", connection.id), &connection)
                    .await?;
                let client = self.rclone.client_for(&connection).await?;
                rclone::registry::test_connection(&client, &connection).await?;
                Ok(json!({
                    "success": true,
                    "message": "Storage backend reachable"
                }))
            }
            "connection/connect" => {
                let connection = StoredConnection::from_lifecycle_params(&params)?;
                // Reserved id: the built-in local filesystem must never be
                // shadowed by a host-registered connection of the same id.
                if connection.id == rclone::LOCAL_CONNECTION_ID {
                    return Err(format!(
                        "connectionId '{}' is reserved for the built-in local filesystem",
                        rclone::LOCAL_CONNECTION_ID
                    ));
                }
                // A reconnect that changed its proxy config now lives in a
                // different rcd group; the stale remote in the old group's
                // config would leak until that rcd dies — pre-delete it
                // through the old group first.
                if let Some(old) = self.rclone.registry.get(&connection.id) {
                    let same_group = old.proxy.as_ref().map(|proxy| proxy.group_key())
                        == connection.proxy.as_ref().map(|proxy| proxy.group_key());
                    if !same_group {
                        if let Ok(old_client) = self.rclone.client_for_binding(&old).await {
                            let _ = old_client
                                .config_delete(&rclone::registry::remote_name(&connection.id))
                                .await;
                        }
                    }
                }
                let connection = self.rclone.prepare(&connection.id, &connection).await?;
                let client = self.rclone.client_for(&connection).await?;
                if let Err(error) =
                    rclone::registry::connect(&self.rclone.registry, &client, &connection).await
                {
                    // A failed connect must not leave a half-established
                    // forwarder behind for this id.
                    self.rclone.release_tunnel(&connection.id).await;
                    return Err(error);
                }
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
                // Route by the registered binding's proxy group so
                // `config/delete` hits the rcd that actually holds the
                // remote; an unknown id is already disconnected.
                let Some(binding) = self.rclone.registry.get(&connection_id) else {
                    return Ok(json!({ "success": true }));
                };
                let client = self.rclone.client_for_binding(&binding).await?;
                rclone::registry::disconnect(&self.rclone.registry, &client, &connection_id)
                    .await?;
                // Tear down the tunnel forwarder(s) alongside the remote.
                self.rclone.release_tunnel(&connection_id).await;
                // Local mounts follow the connection: unmount what is still
                // reachable, drop the table entries (best-effort).
                let _unmounted =
                    mount::unmount_connection(&self.rclone, &self.mounts, &connection_id, None)
                        .await;
                // Idle-group teardown: when this was the group's last
                // connection and no async work is in flight, stop its rcd.
                // Groups still draining work are reaped by the keepalive
                // sweep once the work settles.
                let group = rclone::registry::group_key_of(binding.proxy.as_ref());
                self.rclone.shutdown_group_if_idle(&group).await;
                Ok(json!({ "success": true }))
            }
            "files/list" | "files/listPaged" | "files/stat" | "files/size" => {
                let connection_id = params
                    .get("connectionId")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or("Missing connectionId")?
                    .to_string();
                let client = self.rclone.client_for_id(&connection_id).await?;
                match method {
                    "files/list" => {
                        let request: model::ListRequest = parse(params)?;
                        let binding = self.rclone.binding(&request.connection_id)?;
                        let fs = rclone::call_fs(&binding);
                        let entries = rclone::ops::list(
                            &client,
                            &fs,
                            &request.path,
                            request.recurse,
                            &binding.root,
                            binding.lock_to_root,
                        )
                        .await?;
                        Ok(json!({ "entries": entries }))
                    }
                    "files/listPaged" => {
                        let request: model::ListPagedRequest = parse(params)?;
                        let binding = self.rclone.binding(&request.connection_id)?;
                        let fs = rclone::call_fs(&binding);
                        let (entries, total) = rclone::ops::list_paged(
                            &client,
                            &fs,
                            &request.path,
                            request.page,
                            request.page_size,
                            &binding.root,
                            binding.lock_to_root,
                        )
                        .await?;
                        Ok(json!({ "entries": entries, "total": total }))
                    }
                    "files/stat" => {
                        let request: model::PathRequest = parse(params)?;
                        let binding = self.rclone.binding(&request.connection_id)?;
                        let fs = rclone::call_fs(&binding);
                        let entry = rclone::ops::stat(
                            &client,
                            &fs,
                            &request.path,
                            &binding.root,
                            binding.lock_to_root,
                        )
                        .await?;
                        Ok(json!({ "entry": entry }))
                    }
                    _ => {
                        let request: model::PathRequest = parse(params)?;
                        let binding = self.rclone.binding(&request.connection_id)?;
                        let fs = rclone::call_fs(&binding);
                        let (count, bytes) = rclone::ops::size(
                            &client,
                            &fs,
                            &request.path,
                            &binding.root,
                            binding.lock_to_root,
                        )
                        .await?;
                        Ok(json!({ "count": count, "bytes": bytes }))
                    }
                }
            }
            "files/capabilities" => {
                let connection_id = connection_id_param(&params)?.to_string();
                let binding = self.rclone.binding(&connection_id)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                let capabilities =
                    rclone::ops::capabilities(&client, &rclone::call_fs(&binding), &binding.backend_type)
                        .await?;
                let mut payload =
                    serde_json::to_value(capabilities).map_err(|error| error.to_string())?;
                if let Some(object) = payload.as_object_mut() {
                    object.insert("readOnly".to_string(), serde_json::Value::Bool(binding.read_only));
                }
                Ok(payload)
            }
            "files/quickPaths" => {
                let connection_id = connection_id_param(&params)?.to_string();
                let binding = self.rclone.binding(&connection_id)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                let payload = rclone::ops::quick_paths(
                    &client,
                    &rclone::call_fs(&binding),
                    &binding.backend_type,
                    &binding.root,
                )
                .await?;
                Ok(payload)
            }

            // ------------------------------------------------------------------
            // Phase B file surface (§5 files/read|write|mkdir|rmdir|delete|
            // purge|copy|move|rename|publicLink). Gate direction and response
            // shapes follow the method contract; the connection-level
            // read_only/allow_delete gates bind to the registry binding (the
            // engine keeps no StoredConnection records), while the
            // path whitelist / lock_to_root / purge-root rules are enforced
            // inside ops.rs through the shared policy layer.
            // ------------------------------------------------------------------
            "files/read" | "files/write" => {
                let request_connection_id = params
                    .get("connectionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let binding = self.rclone.binding(&request_connection_id)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                if method == "files/read" {
                    let request: model::ReadRequest = parse(params)?;
                    // Clamp: default 256 KiB, hard cap MAX_PREVIEW_BYTES
                    // (2 MiB).
                    let max_bytes = request
                        .max_bytes
                        .unwrap_or(256 * 1024)
                        .clamp(1, model::MAX_PREVIEW_BYTES as u64);
                    let remote = rclone_gate(
                        &binding.root,
                        binding.lock_to_root,
                        &request.path,
                        crate::policy::PathPolicy::check_read,
                    )?;
                    let (data, truncated) = rclone::ops::read_prefix(
                        &client,
                        &rclone::call_fs(&binding),
                        &remote,
                        max_bytes,
                    )
                    .await?;
                    Ok(json!({
                        "dataBase64": BASE64_STANDARD.encode(data),
                        "truncated": truncated
                    }))
                } else {
                    let request: model::WriteRequest = parse(params)?;
                    ensure_binding_writable(&binding)?;
                    let remote = rclone_gate(
                        &binding.root,
                        binding.lock_to_root,
                        &request.path,
                        crate::policy::PathPolicy::check_write,
                    )?;
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
                    rclone::ops::write_bytes(
                        &client,
                        &rclone::call_fs(&binding),
                        &remote,
                        &data,
                    )
                    .await?;
                    Ok(json!({ "success": true }))
                }
            }
            // `files/readRange`：大文件预览的分段读取。读侧门（白名单 +
            // lock_to_root）与 files/read 完全一致；read_only 不拦读。
            "files/readRange" => {
                let request: model::ReadRangeRequest = parse(params)?;
                let binding = self.rclone.binding(&request.connection_id)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                let remote = rclone_gate(
                    &binding.root,
                    binding.lock_to_root,
                    &request.path,
                    crate::policy::PathPolicy::check_read,
                )?;
                let (data, total_size) = rclone::ops::read_range(
                    &client,
                    &rclone::call_fs(&binding),
                    &remote,
                    request.offset,
                    rclone::ops::clamp_range_length(request.length),
                )
                .await?;
                let eof = rclone::ops::range_is_eof(request.offset, data.len(), total_size);
                Ok(json!({
                    "dataBase64": BASE64_STANDARD.encode(data),
                    "totalSize": total_size,
                    "offset": request.offset,
                    "eof": eof,
                }))
            }
            "files/mkdir" | "files/rmdir" | "files/delete" | "files/purge" => {
                let request: model::PathRequest = parse(params)?;
                let binding = self.rclone.binding(&request.connection_id)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                let fs = rclone::call_fs(&binding);
                match method {
                    "files/mkdir" => {
                        ensure_binding_writable(&binding)?;
                        // 空目录占位回退：CanHaveEmptyDirectories=false 的
                        // 后端补一个空 `.keep` 让新目录可见（决策见
                        // ops::needs_empty_dir_placeholder —— 能力未知或
                        // 权限类失败一律不触发）。
                        rclone::ops::mkdir_with_placeholder(
                            &client,
                            &fs,
                            &binding.backend_type,
                            &request.path,
                            &binding.root,
                            binding.lock_to_root,
                        )
                        .await?;
                    }
                    "files/rmdir" => {
                        // Double gate (write + delete: read_only rejects,
                        // allow_delete rejects).
                        ensure_binding_writable(&binding)?;
                        ensure_binding_deletable(&binding)?;
                        rclone::ops::rmdir(
                            &client,
                            &fs,
                            &request.path,
                            &binding.root,
                            binding.lock_to_root,
                        )
                        .await?;
                    }
                    "files/delete" => {
                        ensure_binding_deletable(&binding)?;
                        rclone::ops::delete_file(
                            &client,
                            &fs,
                            &request.path,
                            &binding.root,
                            binding.lock_to_root,
                        )
                        .await?;
                        self.audit_id(&request.connection_id, method, &request.path, "ok")?;
                        // Best-effort mount-view refresh: the parent listing
                        // changed, active rclone mounts re-read it (batch 5).
                        mount::best_effort_refresh_mount_caches(
                            &self.rclone,
                            &self.mounts,
                            &request.connection_id,
                            Some(&parent_dir_of(&request.path)),
                        )
                        .await;
                    }
                    _ => {
                        ensure_binding_deletable(&binding)?;
                        refuse_purge_of_root(&binding.root, &request.path)?;
                        rclone::ops::purge(
                            &client,
                            &fs,
                            &request.path,
                            &binding.root,
                            binding.lock_to_root,
                        )
                        .await?;
                        self.audit_id(&request.connection_id, method, &request.path, "ok")?;
                        // Purge emptied the directory itself — the listing
                        // that changed is its parent's (batch 5 hook).
                        mount::best_effort_refresh_mount_caches(
                            &self.rclone,
                            &self.mounts,
                            &request.connection_id,
                            Some(&parent_dir_of(&request.path)),
                        )
                        .await;
                    }
                }
                Ok(json!({ "success": true }))
            }
            "files/copy" | "files/move" => {
                let request: model::CopyMoveRequest = parse(params)?;
                let source_connection_id = request
                    .source_connection_id
                    .clone()
                    .unwrap_or_else(|| request.connection_id.clone());
                let target_connection_id = request
                    .target_connection_id
                    .clone()
                    .unwrap_or_else(|| request.connection_id.clone());
                let source_binding = self.rclone.binding(&source_connection_id)?;
                let target_binding = self.rclone.binding(&target_connection_id)?;
                ensure_binding_writable(&target_binding)?;
                // `move` deletes the source (copy+delete degrade semantics) —
                // the source connection passes the delete gate too.
                if method == "files/move" {
                    ensure_binding_deletable(&source_binding)?;
                }
                // Server-side copy/move runs both fs strings inside one rcd,
                // so the two connections must share a proxy group.
                rclone::ensure_same_proxy_group(&source_binding, &target_binding)?;
                let client = self.rclone.client_for_binding(&source_binding).await?;
                let src_fs = rclone::call_fs(&source_binding);
                let dst_fs = rclone::call_fs(&target_binding);
                if method == "files/copy" {
                    rclone::ops::copy_file(
                        &client,
                        &src_fs,
                        &request.source_path,
                        &dst_fs,
                        &request.target_path,
                        &target_binding.root,
                        target_binding.lock_to_root,
                    )
                    .await?;
                } else {
                    rclone::ops::move_file(
                        &client,
                        &src_fs,
                        &request.source_path,
                        &dst_fs,
                        &request.target_path,
                        &target_binding.root,
                        target_binding.lock_to_root,
                    )
                    .await?;
                }
                self.audit_id(&request.connection_id, method, &request.source_path, "ok")?;
                // rc operations/copyfile|movefile answer synchronously — no
                // degraded job, so `transport` is always "native".
                Ok(json!({
                    "success": true,
                    "transport": "native",
                    "jobId": Option::<String>::None,
                }))
            }
            "files/rename" => {
                let request: model::RenameRequest = parse(params)?;
                let binding = self.rclone.binding(&request.connection_id)?;
                // Rename removes the source path — write + delete gates
                // (ops::rename applies the policy check_rename whitelist on
                // both endpoints itself).
                ensure_binding_writable(&binding)?;
                ensure_binding_deletable(&binding)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                // Directories move server-side (`sync/move` job): the file
                // rename rc 404s on dirs. Same connection both sides — the
                // dir-job machinery tracks progress/stop like copyDir.
                let source = rclone::ops::stat(
                    &client,
                    &rclone::call_fs(&binding),
                    &request.path,
                    &binding.root,
                    binding.lock_to_root,
                )
                .await?;
                if source.kind == "dir" {
                    let request = model::DirJobRequest {
                        source_connection_id: request.connection_id.clone(),
                        source_path: request.path.clone(),
                        target_connection_id: request.connection_id.clone(),
                        target_path: request.new_path.clone(),
                        dry_run: Some(false),
                        max_delete: None,
                        include: None,
                        exclude: None,
                        backup_dir: None,
                        suffix: None,
                        // Directory rename carries no filters (rename
                        // cannot be a filtered operation anyway).
                        metadata: Some(false),
                        min_size: None,
                        max_size: None,
                        min_age: None,
                        max_age: None,
                        transfers: None,
                        checkers: None,
                        retries: None,
                    };
                    let job_id = rclone_start_dir_job(
                        Arc::clone(&self.rclone),
                        Arc::clone(&self.sync_jobs),
                        &request,
                        false,
                        true,
                        Some(emitter),
                    )
                    .await?;
                    return Ok(json!({ "success": true, "transport": "dirJob", "jobId": job_id }));
                }
                rclone::ops::rename(
                    &client,
                    &rclone::call_fs(&binding),
                    &request.path,
                    &request.new_path,
                    &binding.root,
                    binding.lock_to_root,
                )
                .await?;
                self.audit_id(&request.connection_id, method, &request.path, "ok")?;
                Ok(json!({ "success": true, "transport": "native", "jobId": Option::<String>::None }))
            }
            "files/publicLink" => {
                let request: model::PublicLinkRequest = parse(params)?;
                let binding = self.rclone.binding(&request.connection_id)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                // `expire_secs` is ignored: rc `operations/publiclink` takes
                // no expiry parameter (backends without public links surface
                // rclone's own error text through ops::public_link).
                let url = rclone::ops::public_link(
                    &client,
                    &rclone::call_fs(&binding),
                    &request.path,
                    &binding.root,
                    binding.lock_to_root,
                )
                .await?;
                Ok(json!({ "url": url }))
            }

            // ------------------------------------------------------------------
            // Phase D archive surface (method-face closeout): archiveList /
            // extract / compress over the rc byte channel. Request/return
            // shapes are field-for-field stable;
            // zip is read via rc-serve Range reads (central directory at the
            // file tail), tar/tar.gz reuse the pure `crate::archive` parsers
            // (see rclone/archive.rs). Both methods run synchronously under
            // the same bomb guards and always answer `transport:"native"`.
            // ------------------------------------------------------------------
            "files/archiveList" => {
                let request: model::ArchiveListRequest = parse(params)?;
                let binding = self.rclone.binding(&request.connection_id)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                let entries = rclone::archive::archive_list(
                    &client,
                    &rclone::call_fs(&binding),
                    &request.path,
                    &binding.root,
                    binding.lock_to_root,
                )
                .await?;
                // Pagination clamp: default page 1 / page_size 200, slice
                // via the shared pure paginator.
                let total = entries.len() as u64;
                let page = request.page.unwrap_or(1).max(1);
                let page_size = request.page_size.unwrap_or(200).clamp(1, 1000);
                let (start, end) = archive::paginate(total, page, page_size);
                Ok(json!({ "entries": &entries[start..end], "total": total }))
            }
            "files/extract" => {
                let request: model::ExtractRequest = parse(params)?;
                let binding = self.rclone.binding(&request.connection_id)?;
                // Extract writes the target tree but never deletes the source
                // archive → read_only gate applies, allow_delete does not.
                ensure_binding_writable(&binding)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                rclone::archive::extract(
                    &client,
                    &rclone::call_fs(&binding),
                    &request.path,
                    &request.target_path,
                    &binding.root,
                    binding.lock_to_root,
                )
                .await?;
                self.audit_id(&request.connection_id, method, &request.path, "ok")?;
                Ok(json!({
                    "success": true,
                    "transport": "native",
                    "jobId": Option::<String>::None,
                }))
            }
            "files/compress" => {
                let request: model::CompressRequest = parse(params)?;
                let binding = self.rclone.binding(&request.connection_id)?;
                ensure_binding_writable(&binding)?;
                if request.paths.is_empty() {
                    return Err("paths must not be empty".to_string());
                }
                // Archive file (not a directory): no trailing-slash rewrite.
                // rclone mode additionally accepts .zip (stored entries).
                let lower = request.target_path.to_lowercase();
                if !(lower.ends_with(".tar")
                    || lower.ends_with(".tar.gz")
                    || lower.ends_with(".tgz")
                    || lower.ends_with(".zip"))
                {
                    return Err(format!(
                        "Archive target '{}' must end with .tar, .tar.gz, .tgz or .zip",
                        request.target_path
                    ));
                }
                let client = self.rclone.client_for_binding(&binding).await?;
                let fs = rclone::call_fs(&binding);
                // Refuse to overwrite: an existing target is never clobbered
                // by a compression run (stat-first; the gate runs here
                // because ops::stat's not-found is an error, not a signal).
                let target_rel = rclone_gate(
                    &binding.root,
                    binding.lock_to_root,
                    &request.target_path,
                    crate::policy::PathPolicy::check_write,
                )?;
                if !target_rel.trim_matches('/').is_empty() {
                    let stat = client
                        .operations_stat(&fs, target_rel.trim_matches('/'))
                        .await
                        .map_err(|error| {
                            format!("Failed to stat '{target_rel}': {error}")
                        })?;
                    if stat.get("item").filter(|item| !item.is_null()).is_some() {
                        return Err(format!(
                            "Archive target '{}' already exists",
                            request.target_path
                        ));
                    }
                }
                rclone::archive::compress(
                    &client,
                    &fs,
                    &request.paths,
                    &request.target_path,
                    &binding.root,
                    binding.lock_to_root,
                )
                .await?;
                self.audit_id(&request.connection_id, method, &request.target_path, "ok")?;
                Ok(json!({
                    "success": true,
                    "transport": "native",
                    "jobId": Option::<String>::None,
                }))
            }

            // ------------------------------------------------------------------
            // Phase B binary channels (§5/§7): upload frames land in a local
            // UploadStaging sink and `finish` streams the staged file to the
            // exact remote path; downloads pump the rc-serve GET body into
            // `files/download/{taskId}` frames (8-byte BE offset + ≤256 KiB).
            // ------------------------------------------------------------------
            "files/upload/start" => {
                let request: model::UploadStartRequest = parse(params)?;
                let binding = self.rclone.binding(&request.connection_id)?;
                ensure_binding_writable(&binding)?;
                let remote = rclone_gate(
                    &binding.root,
                    binding.lock_to_root,
                    &request.remote_path,
                    crate::policy::PathPolicy::check_write,
                )?;
                // taskId is a uuid v4; the staging sink carries the same id.
                let task_id = uuid::Uuid::new_v4().to_string();
                let staging =
                    rclone::bytes_channel::UploadStaging::start(&task_id, request.size)?;
                let job = transfers::TransferJob {
                    task_id: task_id.clone(),
                    connection_id: request.connection_id.clone(),
                    kind: transfers::TransferKind::Upload,
                    remote_path: request.remote_path.clone(),
                    total_bytes: Some(request.size),
                    transferred_bytes: 0,
                    status: transfers::JobStatus::Queued,
                    error: None,
                    started_at: Some(store::unix_millis_now()),
                    finished_at: None,
                    local_path: None,
                };
                {
                    rclone_lock(&self.rclone.jobs).insert(task_id.clone(), job.clone());
                    rclone_lock(&self.rclone.uploads).insert(
                        task_id.clone(),
                        rclone::UploadTask {
                            staging,
                            fs: rclone::call_fs(&binding),
                            remote,
                            declared_size: request.size,
                            throttle: transfers::Throttle::default(),
                            _work: self
                                .rclone
                                .start_work(&rclone::registry::group_key_of(
                                    binding.proxy.as_ref(),
                                )),
                        },
                    );
                }
                // Initial queued event, identical payload shape to the
                // JobTable's start_upload emission.
                let _ = emitter.event("files/transfer/progress", rclone_job_progress_event(&job));
                Ok(json!({ "taskId": task_id }))
            }
            "files/upload/finish" => {
                let request: model::TaskRequest = parse(params)?;
                // A task the engine never started is an error here — there is
                // no second engine's job table to fall through to.
                self.finish_rclone_upload(&request.task_id, emitter).await?;
                Ok(json!({ "success": true }))
            }
            "files/download/start" => {
                let request: model::DownloadStartRequest = parse(params)?;
                let binding = self.rclone.binding(&request.connection_id)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                let remote = rclone_gate(
                    &binding.root,
                    binding.lock_to_root,
                    &request.remote_path,
                    crate::policy::PathPolicy::check_read,
                )?;
                let fs = rclone::call_fs(&binding);
                // stat-first (missing object / directory), then the size
                // prefetch via bytes_channel::remote_size.
                let entry = rclone::ops::stat(
                    &client,
                    &fs,
                    &request.remote_path,
                    &binding.root,
                    binding.lock_to_root,
                )
                .await?;
                if entry.kind == "dir" {
                    return Err(format!(
                        "Cannot download '{}': it is a directory",
                        remote.trim_matches('/')
                    ));
                }
                let size = rclone::bytes_channel::remote_size(&client, &fs, &remote)
                    .await?
                    .unwrap_or(0);
                let staging = if request.save_to_local {
                    // Geometry: validate the preference dir, .part staging
                    // under the downloads base, pre-created so an unwritable
                    // dir fails at start.
                    if let Some(download_dir) = request
                        .download_dir
                        .as_deref()
                        .map(str::trim)
                        .filter(|download_dir| !download_dir.is_empty())
                    {
                        local_downloads::validate_download_dir(download_dir)?;
                    }
                    let base = local_downloads::downloads_base_dir(
                        request.download_dir.as_deref(),
                        |key| std::env::var_os(key),
                        &Store::default_dir(),
                    );
                    let staging = base.join(format!(
                        "{}.part",
                        local_downloads::sanitize_file_name(remote_file_name(
                            &request.remote_path
                        ))
                    ));
                    if let Some(parent) = staging.parent() {
                        std::fs::create_dir_all(parent)
                            .map_err(|error| format!("Failed to create download dir: {error}"))?;
                    }
                    std::fs::File::create(&staging)
                        .map_err(|error| format!("Failed to create staging file: {error}"))?;
                    Some(staging)
                } else {
                    None
                };
                let task_id = uuid::Uuid::new_v4().to_string();
                let cancel = Arc::new(AtomicBool::new(false));
                let pump_done = Arc::new(AtomicBool::new(false));
                let job = transfers::TransferJob {
                    task_id: task_id.clone(),
                    connection_id: request.connection_id.clone(),
                    kind: transfers::TransferKind::Download,
                    remote_path: request.remote_path.clone(),
                    total_bytes: Some(size),
                    transferred_bytes: 0,
                    status: transfers::JobStatus::Queued,
                    error: None,
                    started_at: Some(store::unix_millis_now()),
                    finished_at: None,
                    local_path: None,
                };
                {
                    rclone_lock(&self.rclone.jobs).insert(task_id.clone(), job.clone());
                    rclone_lock(&self.rclone.downloads).insert(
                        task_id.clone(),
                        rclone::DownloadTask {
                            size,
                            cancel: cancel.clone(),
                            pump_done: pump_done.clone(),
                            staging: staging.clone(),
                            _work: self
                                .rclone
                                .start_work(&rclone::registry::group_key_of(
                                    binding.proxy.as_ref(),
                                )),
                        },
                    );
                }
                let _ = emitter.event("files/transfer/progress", rclone_job_progress_event(&job));
                // Independent pump task; the event sequence and frame format
                // stay identical (running → throttled running events; kind-1
                // frames 8B BE offset + ≤256 KiB).
                tokio::spawn(rclone_download_pump(
                    Arc::clone(&self.rclone),
                    task_id.clone(),
                    request.connection_id.clone(),
                    fs,
                    remote,
                    size,
                    staging,
                    cancel,
                    pump_done,
                    emitter.clone(),
                ));
                Ok(json!({ "taskId": task_id, "size": size }))
            }
            // ------------------------------------------------------------------
            // files/archiveDownload：远端目录 → 单个 `<dirname>.zip`（与
            // files/compress 的 .zip 同构，stored 条目）→ 与 files/download/
            // start 完全相同的下载管道（同一任务表、同一
            // `files/download/{taskId}` 帧通道、同一 finish）。压缩字节只落
            // sidecar 临时目录、不写远端 —— read_only 连接同样可下载，目录
            // 炸弹守卫（条目/字节上限）与 compress 共用。
            // ------------------------------------------------------------------
            "files/archiveDownload" => {
                let request: model::ArchiveDownloadRequest = parse(params)?;
                let binding = self.rclone.binding(&request.connection_id)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                let fs = rclone::call_fs(&binding);
                let relative = rclone_gate(
                    &binding.root,
                    binding.lock_to_root,
                    &request.path,
                    crate::policy::PathPolicy::check_read,
                )?;
                // 源校验：只接受目录（文件走普通下载，根目录无名字可打包）。
                let entry = rclone::ops::stat(
                    &client,
                    &fs,
                    &request.path,
                    &binding.root,
                    binding.lock_to_root,
                )
                .await?;
                rclone::archive::validate_archive_source(entry.kind, &relative)?;
                let name = rclone::archive::archive_download_artifact_name(&relative)?;
                let data = rclone::archive::build_dir_zip_bytes(
                    &client,
                    &fs,
                    &request.path,
                    &binding.root,
                    binding.lock_to_root,
                )
                .await?;
                // sidecar 临时目录 staging；泵内的 TempArchiveGuard 负责任何
                // 退出路径（成功/失败/取消/panic）的整目录清扫。
                let stage_dir = std::env::temp_dir().join(format!(
                    "dbx-files-archivedl-{}-{}",
                    std::process::id(),
                    uuid::Uuid::new_v4().simple()
                ));
                std::fs::create_dir_all(&stage_dir)
                    .map_err(|error| format!("Failed to stage archive: {error}"))?;
                let archive_path = stage_dir.join(&name);
                if let Err(error) = std::fs::write(&archive_path, &data) {
                    let _ = std::fs::remove_dir_all(&stage_dir);
                    return Err(format!("Failed to stage archive: {error}"));
                }
                let size = data.len() as u64;
                drop(data);
                let task_id = uuid::Uuid::new_v4().to_string();
                let cancel = Arc::new(AtomicBool::new(false));
                let pump_done = Arc::new(AtomicBool::new(false));
                let job = transfers::TransferJob {
                    task_id: task_id.clone(),
                    connection_id: request.connection_id.clone(),
                    kind: transfers::TransferKind::ArchiveDownload,
                    remote_path: request.path.clone(),
                    total_bytes: Some(size),
                    transferred_bytes: 0,
                    status: transfers::JobStatus::Queued,
                    error: None,
                    started_at: Some(store::unix_millis_now()),
                    finished_at: None,
                    local_path: None,
                };
                {
                    rclone_lock(&self.rclone.jobs).insert(task_id.clone(), job.clone());
                    rclone_lock(&self.rclone.downloads).insert(
                        task_id.clone(),
                        rclone::DownloadTask {
                            size,
                            cancel: cancel.clone(),
                            pump_done: pump_done.clone(),
                            staging: None,
                            _work: self
                                .rclone
                                .start_work(&rclone::registry::group_key_of(
                                    binding.proxy.as_ref(),
                                )),
                        },
                    );
                }
                let _ = emitter.event("files/transfer/progress", rclone_job_progress_event(&job));
                tokio::spawn(rclone_archive_download_pump(
                    Arc::clone(&self.rclone),
                    task_id.clone(),
                    archive_path,
                    size,
                    cancel,
                    pump_done,
                    emitter.clone(),
                ));
                Ok(json!({ "taskId": task_id, "size": size }))
            }
            "files/download/finish" => {
                let request: model::TaskRequest = parse(params)?;
                let local_path = self
                    .finish_rclone_download(&request.task_id, emitter)
                    .await?;
                // Response shape identical to the retired JobTable finish arm:
                // `{success, taskId}` plus `localPath` for saveToLocal runs.
                let mut response = json!({ "success": true, "taskId": request.task_id });
                if let Some(local_path) = local_path {
                    response["localPath"] = json!(local_path);
                }
                Ok(response)
            }
            // ------------------------------------------------------------------
            // Phase C dir sync (§5/§6): syncDir/copyDir run as rclone sync
            // jobs — `sync::start_job` on the no-timeout transfer client,
            // gates mirroring `validate_dir_job_gates`, a pollable `jobId`
            // response, progress events in the exact `emit_dir_progress`
            // DirJob shape. `files/transfer/status` and the transfers/* panel
            // methods answer from the rclone job mirrors.
            // ------------------------------------------------------------------
            "files/syncDir" | "files/copyDir" => {
                let request: model::DirJobRequest = parse(params)?;
                // Phase D: the start logic is shared verbatim with the MCP
                // `files_sync` tool (see [`rclone_start_dir_job`]).
                let job_id = rclone_start_dir_job(
                    Arc::clone(&self.rclone),
                    Arc::clone(&self.sync_jobs),
                    &request,
                    method == "files/syncDir",
                    false,
                    Some(emitter),
                )
                .await?;
                // The jobId doubles as the cancel/status taskId (shared
                // namespace with the single-file taskIds).
                Ok(json!({ "jobId": job_id }))
            }
            // ------------------------------------------------------------------
            // Bandwidth limit (`files/bwlimit`): absent `rate` reads the
            // persisted pref, `"off"` clears it, anything else is applied to
            // every live group rcd and persisted for respawn replay.
            // ------------------------------------------------------------------
            "files/bwlimit" => {
                let request: model::BwlimitRequest = parse(params)?;
                let rate = request
                    .rate
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                match rate.as_deref() {
                    Some("off") => {
                        self.rclone.set_bwlimit(None).await?;
                        Ok(json!({ "rate": Value::Null }))
                    }
                    Some(rate) => {
                        self.rclone.set_bwlimit(Some(rate)).await?;
                        Ok(json!({ "rate": rate }))
                    }
                    None => Ok(json!({ "rate": self.rclone.bwlimit_pref() })),
                }
            }
            // ------------------------------------------------------------------
            // Space & integrity tools: about (usage quota), check (async
            // comparison job), hashsum (SUM file next to the directory),
            // cleanup (remote trash), rmdirs (empty dirs under a path).
            // ------------------------------------------------------------------
            "files/about" => {
                let request: model::AboutRequest = parse(params)?;
                {
                    let cache = self
                        .about_cache
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if let Some((at, value)) = cache.get(&request.connection_id) {
                        if at.elapsed() < ABOUT_CACHE_TTL {
                            return Ok(value.clone());
                        }
                    }
                }
                let binding = self.rclone.binding(&request.connection_id)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                let value = client
                    .operations_about(&rclone::call_fs(&binding))
                    .await
                    .map_err(|error| error.to_string())?;
                if let Ok(mut cache) = self.about_cache.lock() {
                    cache.insert(request.connection_id.clone(), (std::time::Instant::now(), value.clone()));
                }
                Ok(value)
            }
            "files/check" => {
                let request: model::CheckRequest = parse(params)?;
                let job_id = rclone_start_check_job(
                    Arc::clone(&self.rclone),
                    Arc::clone(&self.sync_jobs),
                    &request,
                    Some(emitter),
                )
                .await?;
                Ok(json!({ "jobId": job_id }))
            }
            // 右键 SUM 校验文件（`.md5`/`.sha1`/…）→ 核验其同名兄弟目录
            // （`data.md5` → `data`，批次7）。
            // 单连接：同一 CheckRequest 走 SUM 分支，终态报告与 files/check
            // 同形态，复用 check 作业面板与 checkSummary 展示。
            "files/checksum/verify" => {
                let request: model::SumVerifyRequest = parse(params)?;
                let job_id = rclone_start_check_job(
                    Arc::clone(&self.rclone),
                    Arc::clone(&self.sync_jobs),
                    &model::CheckRequest {
                        source_connection_id: request.connection_id.clone(),
                        source_path: request.sum_path.clone(),
                        target_connection_id: request.connection_id.clone(),
                        target_path: request.sum_path.clone(),
                        one_way: None,
                        download: None,
                        sum_path: Some(request.sum_path),
                        hash_type: request.hash_type,
                    },
                    Some(emitter),
                )
                .await?;
                Ok(json!({ "jobId": job_id }))
            }
            "files/hashsum" => {
                let request: model::HashsumRequest = parse(params)?;
                let binding = self.rclone.binding(&request.connection_id)?;
                ensure_binding_writable(&binding)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                let fs = rclone::call_fs(&binding);
                let remote = rclone_gate(
                    &binding.root,
                    binding.lock_to_root,
                    &request.path,
                    crate::policy::PathPolicy::check_read,
                )?;
                if remote.trim_matches('/').is_empty() {
                    return Err("hashsum needs a subdirectory (the SUM file is written next to it)".to_string());
                }
                // Size precheck: refuse absurd trees before hashing — with
                // the SAME scope the generation call uses. operations/size
                // only parses the fs (live-pinned v1.75.1: `remote` is
                // silently ignored, the operations/hashsum twin below), so
                // the verified directory rides inside the fs string. The old
                // spelling (`fs`=connection root + `remote`=directory) counted
                // the whole root: a small folder under a big root was
                // mis-rejected by the cap — conservative, but wrong.
                let dir_fs = rclone::sync::compose_fs(&fs, &remote);
                let size = client
                    .operations_size(&dir_fs, "")
                    .await
                    .map_err(|error| error.to_string())?;
                let count = size.get("count").and_then(Value::as_u64).unwrap_or(0);
                if count > HASHSUM_MAX_FILES {
                    return Err(format!(
                        "directory holds {count} files; hashsum is capped at {HASHSUM_MAX_FILES} — pick a smaller folder"
                    ));
                }
                let hash_type = request
                    .hash_type
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or("md5")
                    .to_lowercase();
                // operations/hashsum walks `dir_fs` (already directory-scoped
                // for the precheck above — v1.75.1 never narrows by `remote`),
                // and the SUM lines come out relative to that directory
                // ("a.txt", "sub/b.txt") — the batch-7 shape.
                let result = client
                    .operations_hashsum(&dir_fs, "", &hash_type, false)
                    .await
                    .map_err(|error| error.to_string())?;
                let lines: Vec<String> = result
                    .get("hashsum")
                    .and_then(Value::as_array)
                    .map(|array| {
                        array
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                if lines.is_empty() {
                    return Err(format!("no hashable files under {}", request.path));
                }
                // SUM file lives NEXT TO the directory (`/photos` →
                // `/photos.md5`), so a rerun never hashes its own report.
                let trimmed = request.path.trim_matches('/');
                let (parent, base) = match trimmed.rsplit_once('/') {
                    Some((parent, base)) => (parent.to_string(), base.to_string()),
                    None => (String::new(), trimmed.to_string()),
                };
                let sum_path = if parent.is_empty() {
                    format!("/{base}.{hash_type}")
                } else {
                    format!("/{parent}/{base}.{hash_type}")
                };
                let sum_remote = rclone_gate(
                    &binding.root,
                    binding.lock_to_root,
                    &sum_path,
                    crate::policy::PathPolicy::check_write,
                )?;
                let mut content = lines.join("\n");
                content.push('\n');
                rclone::ops::write_bytes(&client, &fs, &sum_remote, content.as_bytes()).await?;
                self.audit_id(&request.connection_id, "files/hashsum", &sum_path, "ok")?;
                Ok(json!({ "path": sum_path, "hashType": hash_type, "files": lines.len() }))
            }
            "files/cleanup" => {
                let request: model::CleanupRequest = parse(params)?;
                let binding = self.rclone.binding(&request.connection_id)?;
                ensure_binding_deletable(&binding)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                client
                    .operations_cleanup(&rclone::call_fs(&binding))
                    .await
                    .map_err(|error| error.to_string())?;
                self.audit_id(&request.connection_id, "files/cleanup", "/", "ok")?;
                Ok(json!({ "success": true }))
            }
            "files/rmdirs" => {
                let request: model::PathRequest = parse(params)?;
                let binding = self.rclone.binding(&request.connection_id)?;
                ensure_binding_deletable(&binding)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                let remote = rclone_gate(
                    &binding.root,
                    binding.lock_to_root,
                    &request.path,
                    crate::policy::PathPolicy::check_write,
                )?;
                client
                    .operations_rmdirs(&rclone::call_fs(&binding), &remote)
                    .await
                    .map_err(|error| error.to_string())?;
                self.audit_id(&request.connection_id, "files/rmdirs", &request.path, "ok")?;
                Ok(json!({ "success": true }))
            }
            "files/bisync/start" => {
                let request: model::BisyncStartRequest = parse(params)?;
                let job_id = rclone_start_bisync_job(
                    Arc::clone(&self.rclone),
                    Arc::clone(&self.sync_jobs),
                    Arc::clone(&self.store),
                    &request,
                    Some(emitter),
                )
                .await?;
                Ok(json!({ "jobId": job_id }))
            }
            "files/bisync/state" => {
                let request: model::BisyncStateRequest = parse(params)?;
                let source_binding = self.rclone.binding(&request.source_connection_id)?;
                let target_binding = self.rclone.binding(&request.target_connection_id)?;
                let src_rel = rclone_gate(
                    &source_binding.root,
                    source_binding.lock_to_root,
                    &request.source_path,
                    crate::policy::PathPolicy::check_read,
                )?;
                let dst_rel = rclone_gate(
                    &target_binding.root,
                    target_binding.lock_to_root,
                    &request.target_path,
                    crate::policy::PathPolicy::check_read,
                )?;
                let session = bisync_session_name(
                    &rclone::sync::compose_fs(&rclone::call_fs(&source_binding), &src_rel),
                    &rclone::sync::compose_fs(&rclone::call_fs(&target_binding), &dst_rel),
                );
                let marker = self
                    .store
                    .data_dir()
                    .join("bisync-workdir")
                    .join(format!("{session}.path1.lst"));
                Ok(json!({
                    "session": session,
                    "state": if marker.exists() { "synced" } else { "new" },
                }))
            }
            "files/search" => {
                let request: model::SearchRequest = parse(params)?;
                let binding = self.rclone.binding(&request.connection_id)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                let fs = rclone::call_fs(&binding);
                let remote = rclone_gate(
                    &binding.root,
                    binding.lock_to_root,
                    request.root.as_deref().unwrap_or(""),
                    crate::policy::PathPolicy::check_read,
                )?;
                // Substring term → rclone include glob (`**term**` matches
                // the substring anywhere in the relative path). Glob
                // metacharacters are stripped so the term stays literal.
                let term: String = request
                    .pattern
                    .trim()
                    .chars()
                    .filter(|c| !matches!(c, '*' | '?' | '[' | ']' | '{' | '}'))
                    .collect();
                if term.is_empty() {
                    return Err("search pattern is empty".to_string());
                }
                // Scan budget: a full-tree listing is not free — refuse
                // absurd trees instead of crawling for minutes. 口径与
                // files/size 相同（ops::subtree_size）：operations/size 只认
                // 并入 fs 的路径，`fs`+`remote` 拆传会被 rclone 忽略 remote
                // 而数成整棵连接根（scanned 会比真实子树多出根下额外文件）。
                let (scanned, _) = rclone::ops::subtree_size(&client, &fs, &remote).await?;
                if scanned > SEARCH_MAX_SCAN {
                    return Err(format!(
                        "this tree holds {scanned} files; search is capped at {SEARCH_MAX_SCAN} — pick a smaller folder"
                    ));
                }
                let result = client
                    .operations_list_filtered(&fs, &remote, &format!("**{term}**"), true)
                    .await
                    .map_err(|error| error.to_string())?;
                let limit = request.limit.unwrap_or(200).clamp(1, SEARCH_RESULT_LIMIT) as usize;
                let empty = Vec::new();
                let list = result.get("list").and_then(Value::as_array).unwrap_or(&empty);
                let truncated = list.len() > limit;
                let entries: Vec<Value> = list
                    .iter()
                    .take(limit)
                    .map(|entry| {
                        json!({
                            "path": entry.get("Path").and_then(Value::as_str).unwrap_or_default(),
                            "size": entry.get("Size").and_then(Value::as_u64).unwrap_or(0),
                            "modifiedAt": entry.get("ModTime").and_then(Value::as_str).unwrap_or_default(),
                        })
                    })
                    .collect();
                Ok(json!({
                    "entries": entries,
                    "truncated": truncated,
                    "scanned": scanned,
                }))
            }
            "files/copyurl" => {
                let request: model::CopyUrlRequest = parse(params)?;
                // Web URLs only: rclone supports more schemes, but the
                // workbench entry targets downloadable resources.
                let lower = request.url.to_lowercase();
                if !lower.starts_with("http://") && !lower.starts_with("https://") {
                    return Err("only http(s) URLs are supported".to_string());
                }
                let binding = self.rclone.binding(&request.connection_id)?;
                ensure_binding_writable(&binding)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                let fs = rclone::call_fs(&binding);
                let dir_remote = rclone_gate(
                    &binding.root,
                    binding.lock_to_root,
                    &request.dir_path,
                    crate::policy::PathPolicy::check_write,
                )?;
                let filename = match request.filename.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
                    Some(name) => name.to_string(),
                    None => {
                        // URL last path segment (query/fragment stripped),
                        // mirroring rclone's autoFilename pick.
                        let without_query = request.url.split(['?', '#']).next().unwrap_or("");
                        let segment = without_query.rsplit('/').find(|part| !part.is_empty()).unwrap_or("download");
                        segment.to_string()
                    }
                };
                let target_wire = format!("{}/{}", request.dir_path.trim_end_matches('/'), filename);
                let target_remote = rclone_gate(
                    &binding.root,
                    binding.lock_to_root,
                    &target_wire,
                    crate::policy::PathPolicy::check_write,
                )?;
                client
                    .operations_copyurl(&fs, &target_remote, &request.url, false)
                    .await
                    .map_err(|error| error.to_string())?;
                self.audit_id(&request.connection_id, "files/copyurl", &target_wire, "ok")?;
                Ok(json!({ "path": target_wire, "filename": filename }))
            }
            // ------------------------------------------------------------------
            // 本机共享（对标 rclone serve 家族）：把远端目录经 rcd 的 serve/start
            // 以 HTTP/WebDAV 暴露给本机应用。serve 端点无鉴权，因此两条硬边界：
            // 只绑 127.0.0.1 回环、只开放 http/webdav 两类（ftp/sftp/nfs 等
            // 暴露面更大，一律拒绝）。
            // ------------------------------------------------------------------
            "files/serve/start" => {
                let request: model::ServeStartRequest = parse(params)?;
                // serve_type 白名单在进入 rc 调用前收口；空值/缺省 = http。
                let serve_type = match request.serve_type.as_deref().map(str::trim) {
                    None | Some("") | Some("http") => "http",
                    Some("webdav") => "webdav",
                    Some(other) => {
                        return Err(format!(
                            "unsupported serve type '{other}'; only http and webdav are allowed"
                        ))
                    }
                };
                let binding = self.rclone.binding(&request.connection_id)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                let remote = rclone_gate(
                    &binding.root,
                    binding.lock_to_root,
                    &request.path,
                    crate::policy::PathPolicy::check_read,
                )?;
                let fs = rclone::sync::compose_fs(&rclone::call_fs(&binding), &remote);
                // 安全：serve 无鉴权，绝不绑 0.0.0.0——回环绑定 + 端口 0 让
                // rcd 自动挑选空闲端口并回传实际地址（实测 v1.75.1）。
                const SERVE_ADDR: &str = "127.0.0.1:0";
                let answer = client
                    .serve_start(&fs, serve_type, SERVE_ADDR)
                    .await
                    .map_err(|error| error.to_string())?;
                let serve_id = answer
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "rc serve/start returned no id".to_string())?
                    .to_string();
                let addr = answer
                    .get("addr")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "rc serve/start returned no addr".to_string())?
                    .to_string();
                {
                    let mut serves = rclone_lock(&self.serves);
                    serves.insert(
                        serve_id.clone(),
                        (request.connection_id.clone(), serve_type.to_string()),
                    );
                }
                self.audit_id(&request.connection_id, "files/serve/start", &request.path, "ok")?;
                Ok(json!({
                    "serveId": serve_id,
                    "url": format!("http://{addr}"),
                    "serveType": serve_type,
                }))
            }
            "files/serve/stop" => {
                let request: model::ServeStopRequest = parse(params)?;
                // 归属校验：只能停本连接名下的实例（登记表无此 id = 实例已随
                // rcd 消失，走幂等成功，不再泄露归属信息）。
                if let Some((owner, _)) = rclone_lock(&self.serves).get(&request.serve_id) {
                    if *owner != request.connection_id {
                        return Err(format!(
                            "serve '{}' does not belong to this connection",
                            request.serve_id
                        ));
                    }
                }
                let binding = self.rclone.binding(&request.connection_id)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                // 幂等：未知 id 在 rc 侧报错，但目标状态就是“不存在”，按成功处理。
                if let Err(error) = client.serve_stop(&request.serve_id).await {
                    eprintln!("[io.dbx.files] files/serve/stop idempotent ignore: {error}");
                }
                rclone_lock(&self.serves).remove(&request.serve_id);
                self.audit_id(&request.connection_id, "files/serve/stop", &request.serve_id, "ok")?;
                Ok(json!({ "success": true }))
            }
            "files/serve/list" => {
                let request: model::ServeListRequest = parse(params)?;
                let binding = self.rclone.binding(&request.connection_id)?;
                let client = self.rclone.client_for_binding(&binding).await?;
                let answer = client
                    .serve_list()
                    .await
                    .map_err(|error| error.to_string())?;
                let list = answer
                    .get("list")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                // rc 返回的活跃 id 集合是事实源：登记表里不在集合内的条目是
                // rcd 重启/serve 已停留下的陈旧记录，先行清理。
                let active: std::collections::HashSet<&str> = list
                    .iter()
                    .filter_map(|entry| entry.get("id").and_then(Value::as_str))
                    .collect();
                let addr_of = |serve_id: &str| -> Option<String> {
                    list.iter()
                        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(serve_id))
                        .and_then(|entry| {
                            entry
                                .get("addr")
                                .and_then(Value::as_str)
                                .or_else(|| {
                                    entry
                                        .get("params")
                                        .and_then(|params| params.get("addr"))
                                        .and_then(Value::as_str)
                                })
                                .map(str::to_string)
                        })
                };
                let entries = {
                    let mut serves = rclone_lock(&self.serves);
                    serves.retain(|serve_id, _| active.contains(serve_id.as_str()));
                    serves
                        .iter()
                        .filter(|(_, (owner, _))| owner == &request.connection_id)
                        .filter_map(|(serve_id, (_, serve_type))| {
                            addr_of(serve_id).map(|addr| {
                                json!({
                                    "serveId": serve_id,
                                    "url": format!("http://{addr}"),
                                    "serveType": serve_type,
                                })
                            })
                        })
                        .collect::<Vec<_>>()
                };
                Ok(json!({ "serves": entries }))
            }
            // ------------------------------------------------------------------
            // Local mounts (docs/MOUNT.zh-CN.md, M1): rclone mount first,
            // read-only WebDAV gateway fallback. `mount/mod.rs` owns the
            // strategy; these arms only parse and route.
            // ------------------------------------------------------------------
            "files/mount" => {
                let connection_id = connection_id_param(&params)?.to_string();
                let request: model::MountRequest = parse(params)?;
                mount::start_mount(&self.rclone, &self.mounts, &connection_id, &request).await
            }
            "files/unmount" => {
                let connection_id = connection_id_param(&params)?.to_string();
                let request: model::MountUnmountRequest = parse(params)?;
                Ok(mount::unmount_connection(
                    &self.rclone,
                    &self.mounts,
                    &connection_id,
                    request.mount_id.as_deref(),
                )
                .await)
            }
            "files/mountStatus" => {
                let connection_id = connection_id_param(&params)?.to_string();
                let request: model::MountStatusRequest = parse(params)?;
                mount::mount_status(
                    &self.rclone,
                    &self.mounts,
                    &connection_id,
                    request.mount_id.as_deref(),
                )
                .await
            }
            // VFS cache management (batch 5): rclone-strategy mounts refresh
            // their directory cache / report stats through the rcd's vfs/*
            // endpoints; WebDAV gateway mounts answer `skipped` (no VFS).
            "files/mount/refresh" => {
                let connection_id = connection_id_param(&params)?.to_string();
                let request: model::MountRefreshRequest = parse(params)?;
                mount::refresh_mount_caches(
                    &self.rclone,
                    &self.mounts,
                    &connection_id,
                    request.mount_id.as_deref(),
                    request.path.as_deref(),
                    request.recursive.unwrap_or(false),
                )
                .await
            }
            "files/mount/stats" => {
                let connection_id = connection_id_param(&params)?.to_string();
                let request: model::MountStatsRequest = parse(params)?;
                mount::mount_vfs_stats(
                    &self.rclone,
                    &self.mounts,
                    &connection_id,
                    request.mount_id.as_deref(),
                )
                .await
            }
            "files/transfer/status" => {
                let request: model::JobRequest = parse(params)?;
                // rclone mirrors first (single-file + sync jobs). Lock order
                // jobs → sync_jobs everywhere.
                let answer = {
                    let jobs = rclone_lock(&self.rclone.jobs);
                    let sync_jobs = rclone_lock(&self.sync_jobs);
                    match (jobs.get(&request.job_id), sync_jobs.get(&request.job_id)) {
                        (Some(job), Some(record)) => Some(json!({
                            "job": rclone_sync_dir_job_value(job, record),
                            "kind": "dirJob",
                        })),
                        (Some(job), None) => Some(json!({
                            "job": serde_json::to_value(job)
                                .map_err(|error| error.to_string())?,
                            "kind": "transfer",
                        })),
                        _ => None,
                    }
                };
                if let Some(answer) = answer {
                    return Ok(answer);
                }
                // Mirror miss but a sync handle survives: query_status as the
                // fallback — a live rclone job still reports as running.
                let handle = {
                    let sync_jobs = rclone_lock(&self.sync_jobs);
                    sync_jobs
                        .get(&request.job_id)
                        .and_then(|record| record.handle.as_ref())
                        .map(|handle| rclone::sync::SyncJobHandle {
                            jobid: handle.jobid,
                            group: handle.group.clone(),
                            kind: handle.kind,
                        })
                };
                if let (Some(handle), Some(record)) = (handle, {
                    let sync_jobs = rclone_lock(&self.sync_jobs);
                    sync_jobs.get(&request.job_id).cloned()
                }) {
                    // Status polling must hit the rcd group that owns the
                    // jobid — the source connection's group.
                    let client = self.rclone.client_for_id(&record.src_conn).await?;
                    if let Ok(_stats) = rclone::sync::query_status(&client, &handle).await {
                        let probe = transfers::TransferJob {
                            task_id: request.job_id.clone(),
                            connection_id: record.src_conn.clone(),
                            kind: transfers::TransferKind::Upload,
                            remote_path: record.src_rel.clone(),
                            total_bytes: None,
                            transferred_bytes: 0,
                            status: transfers::JobStatus::Running,
                            error: None,
                            started_at: None,
                            finished_at: None,
                            local_path: None,
                        };
                        return Ok(json!({
                            "job": rclone_sync_dir_job_value(&probe, &record),
                            "kind": "dirJob",
                        }));
                    }
                }
                // Same not-found message the retired JobTable status answered
                // with on a mirror miss.
                Err(format!("Unknown jobId '{}'", request.job_id))
            }
            "files/transfers/list" => {
                let request: model::TransfersListRequest = parse(params)?;
                // Phase D: the rclone mirror is hydrated from the shared
                // transfers.json at startup and terminal records persist back
                // into it, so this list is restart-safe exactly like the
                // JobTable's list_merged (single-file records only — sync
                // jobs stay memory-only).
                let mut entries: Vec<(u64, Value)> = {
                    let jobs = rclone_lock(&self.rclone.jobs);
                    let sync_jobs = rclone_lock(&self.sync_jobs);
                    let mut entries: Vec<(u64, Value)> = Vec::new();
                    for (id, job) in jobs.iter() {
                        let record = sync_jobs.get(id);
                        if let Some(filter) = request.connection_id.as_deref() {
                            let hit = job.connection_id == filter
                                || record
                                    .map(|record| {
                                        record.src_conn == filter || record.dst_conn == filter
                                    })
                                    .unwrap_or(false);
                            if !hit {
                                continue;
                            }
                        }
                        let value = match record {
                            Some(record) => rclone_sync_dir_job_value(job, record),
                            None => serde_json::to_value(job)
                                .map_err(|error| error.to_string())?,
                        };
                        entries.push((job.started_at.unwrap_or(0), value));
                    }
                    entries
                };
                // list_merged parity: oldest start first.
                entries.sort_by_key(|(started_at, _)| *started_at);
                let jobs_out: Vec<Value> = entries.into_iter().map(|(_, value)| value).collect();
                Ok(json!({ "jobs": jobs_out }))
            }
            "files/transfers/clear" => {
                let request: model::TransfersListRequest = parse(params)?;
                // Mirror of the JobTable clear: terminal records leave the
                // table (with their sync projection records), queued/running
                // jobs are never touched. Response shape identical.
                let drop_ids: Vec<String> = {
                    let jobs = rclone_lock(&self.rclone.jobs);
                    let sync_jobs = rclone_lock(&self.sync_jobs);
                    jobs.iter()
                        .filter(|(_, job)| job.status.is_terminal())
                        .filter(|(id, job)| match request.connection_id.as_deref() {
                            Some(filter) => {
                                job.connection_id == filter
                                    || sync_jobs
                                        .get(*id)
                                        .map(|record| {
                                            record.src_conn == filter
                                                || record.dst_conn == filter
                                        })
                                        .unwrap_or(false)
                            }
                            None => true,
                        })
                        .map(|(id, _)| id.clone())
                        .collect()
                };
                let mut cleared = 0u64;
                if !drop_ids.is_empty() {
                    let mut jobs = rclone_lock(&self.rclone.jobs);
                    let mut sync_jobs = rclone_lock(&self.sync_jobs);
                    for id in drop_ids {
                        if jobs.remove(&id).is_some() {
                            cleared += 1;
                            // The store row for a dropped mirror record is
                            // removed too (history parity with JobTable::clear);
                            // best-effort — the panel count comes from the
                            // mirror.
                            let _ = self.store.delete_transfer(&id);
                        }
                        sync_jobs.remove(&id);
                    }
                }
                Ok(json!({ "cleared": cleared }))
            }
            "files/transfers/delete" => {
                let request: model::TaskRequest = parse(params)?;
                // Mirror of the JobTable delete: active jobs refuse with the
                // same message, terminal records leave the table together
                // with their sync projection record.
                let mut removed = 0u64;
                {
                    let mut jobs = rclone_lock(&self.rclone.jobs);
                    if let Some(job) = jobs.get(&request.task_id) {
                        if !job.status.is_terminal() {
                            return Err(
                                "Transfer is still in progress; cancel it first".to_string()
                            );
                        }
                        jobs.remove(&request.task_id);
                        removed += 1;
                        // Persisted row goes with the mirror record (history
                        // parity with `JobTable::delete_record`); best-effort.
                        let _ = self.store.delete_transfer(&request.task_id);
                    }
                }
                let record_gone =
                    rclone_lock(&self.sync_jobs).remove(&request.task_id).is_some();
                if record_gone && removed == 0 {
                    removed = 1;
                }
                Ok(json!({ "removed": removed }))
            }
            "files/transfer/cancel" => {
                let request: model::TaskRequest = parse(params)?;
                let task_id = request.task_id.as_str();
                let ours = {
                    let jobs = rclone_lock(&self.rclone.jobs);
                    let uploads = rclone_lock(&self.rclone.uploads);
                    let downloads = rclone_lock(&self.rclone.downloads);
                    let sync_jobs = rclone_lock(&self.sync_jobs);
                    jobs.contains_key(task_id)
                        || uploads.contains_key(task_id)
                        || downloads.contains_key(task_id)
                        || sync_jobs.contains_key(task_id)
                };
                if !ours {
                    // Same not-found message the retired JobTable cancel
                    // answered for ids absent from every table.
                    return Err("Transfer task was not found".to_string());
                }
                // Upload: abort the staging file and drop the slot.
                if let Some(task) = rclone_lock(&self.rclone.uploads).remove(task_id) {
                    task.staging.abort();
                    rclone_complete_job(
                        &self.rclone,
                        task_id,
                        transfers::JobStatus::Canceled,
                        None,
                        emitter,
                    );
                    return Ok(json!({ "success": true }));
                }
                // Download: raise the flag but keep the slot — the pump owns
                // the cleanup while it lives (issue#4 mirror); a pump that
                // already exited is cleaned up here.
                if let Some(slot) = rclone_lock(&self.rclone.downloads).get(task_id).cloned() {
                    slot.cancel.store(true, Ordering::Release);
                    if slot.pump_done.load(Ordering::Acquire) {
                        rclone_lock(&self.rclone.downloads).remove(task_id);
                        if let Some(staging) = &slot.staging {
                            let _ = std::fs::remove_file(staging);
                        }
                    }
                    rclone_complete_job(
                        &self.rclone,
                        task_id,
                        transfers::JobStatus::Canceled,
                        None,
                        emitter,
                    );
                    return Ok(json!({ "success": true }));
                }
                // Phase C sync job (branch order preserved: uploads →
                // downloads → sync). A settled mirror answers not-found like
                // the fallthrough below; otherwise stop the rclone job and
                // settle Canceled locally — terminal-once against the
                // on_event side, DirJob-shaped final event.
                let sync_stop = {
                    let jobs = rclone_lock(&self.rclone.jobs);
                    let sync_jobs = rclone_lock(&self.sync_jobs);
                    match sync_jobs.get(task_id) {
                        None => None,
                        Some(record) => {
                            if jobs
                                .get(task_id)
                                .map(|job| job.status.is_terminal())
                                .unwrap_or(false)
                            {
                                Some(Err("Transfer task was not found".to_string()))
                            } else {
                                match record.handle.as_ref() {
                                    Some(handle) => {
                                        Some(Ok((
                                            rclone::sync::SyncJobHandle {
                                                jobid: handle.jobid,
                                                group: handle.group.clone(),
                                                kind: handle.kind,
                                            },
                                            record.src_conn.clone(),
                                        )))
                                    }
                                    // start_job has not returned its handle
                                    // yet (a millisecond-scale window).
                                    None => Some(Err(
                                        "Transfer task was not started yet".to_string()
                                    )),
                                }
                            }
                        }
                    }
                };
                if let Some(stop) = sync_stop {
                    let (handle, src_conn) = stop?;
                    let client = self.rclone.client_for_id(&src_conn).await?;
                    rclone::sync::stop_job(&client, &handle).await?;
                    rclone_sync_terminal(
                        &self.rclone.jobs,
                        &self.sync_jobs,
                        Some(emitter),
                        task_id,
                        transfers::JobStatus::Canceled,
                        None,
                        None,
                    );
                    return Ok(json!({ "success": true }));
                }
                // Only a terminal record remains: the JobTable's cancel
                // answers not-found once the slots are gone — mirror it.
                Err("Transfer task was not found".to_string())
            }
            // ------------------------------------------------------------------
            // Engine-free support surface (none of these methods touch a
            // storage engine): local download capabilities/whitelists, the
            // audit trail and the MCP tool bridge.
            // ------------------------------------------------------------------
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
            // Validate a user-selected local download directory without
            // creating it. The frontend uses this before persisting the
            // preference; start_download repeats the check for stale prefs.
            "files/local/validate-directory" => {
                let path = params
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or("Missing path")?;
                let path = local_downloads::validate_download_dir(path)?;
                Ok(json!({ "valid": true, "path": path.to_string_lossy() }))
            }
            // 在文件管理器中定位已完成的下载。只允许 reveal 传输历史里记录过
            // 的 localPath，不能成为任意路径打开原语。
            "files/local/reveal" => {
                let path = params
                    .get("path")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or("Missing path")?;
                // 挂载成功后的「在文件管理器中打开」：只放行当前活跃挂载点
                //（mount 表内存比对），其余路径仍走下载历史白名单。
                if mount::is_active_mount_point(&self.mounts, std::path::Path::new(path))? {
                    local_downloads::reveal_in_file_manager(std::path::Path::new(path))?;
                    return Ok(json!({ "success": true }));
                }
                let history = self.store.load_transfers();
                local_downloads::reveal_validated(&history, std::path::Path::new(path))?;
                Ok(json!({ "success": true }))
            }
            // Validate a user-configured external open-with app path without
            // launching it (issue #11). The settings panel calls this when
            // persisting the preference so a typo surfaces immediately;
            // files/local/open repeats the check for stale prefs.
            "files/local/validate-open-app" => {
                let path = params
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or("Missing path")?;
                let path = local_downloads::validate_open_app(path)?;
                Ok(json!({ "valid": true, "path": path.to_string_lossy() }))
            }
            // 平台感知的「打开方式」预设（WPS/Excel/LibreOffice/...，按平台默认
            // 安装路径探测）：只回报告本机真实存在的候选，前端据此渲染一键预设。
            // 纯探测，不做任何启动，也不读文件内容。
            "files/local/detect-apps" => {
                let platform = local_downloads::platform_name();
                let apps = local_downloads::detect_apps(platform)
                    .into_iter()
                    .map(|preset| {
                        json!({
                            "id": preset.id,
                            "name": preset.name,
                            "path": preset.path,
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(json!({ "platform": platform, "apps": apps }))
            }
            // 在默认应用或用户配置的外部应用中打开已完成的本机下载；同样只允许
            // 打开传输历史中记录过的路径，避免把这个按钮变成任意本机路径执行
            // 入口。`app` 是可选的用户外部应用可执行文件绝对路径（issue #11，
            // 单一路径、绝不接受 shell 命令行）；缺省走系统默认应用。
            "files/local/open" => {
                let path = params
                    .get("path")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or("Missing path")?;
                let app = params.get("app").and_then(Value::as_str);
                let history = self.store.load_transfers();
                local_downloads::open_validated(&history, std::path::Path::new(path), app)?;
                Ok(json!({ "success": true }))
            }
            // ------------------------------------------------------------------
            // Remote-edit sessions (打开方式, FinalShell-style local editing):
            // `open` pulls one remote file into the per-connection temp
            // workspace (`remote_edit::workspace_base`), launches the OS
            // default or a user-pinned app and keeps a watcher loop that
            // streams settled editor saves back to the exact remote path.
            // `status` lists live sessions, `close` stops one and removes
            // its local copy. Every rclone touch is gated like the sibling
            // storage methods (read on open, write so the sync-back cannot
            // be doomed from the start on a read-only connection).
            // ------------------------------------------------------------------
            "files/remote-edit/open" => {
                let request: model::RemoteEditOpenRequest = parse(params)?;
                if !local_downloads::can_save_local(|key| std::env::var_os(key)) {
                    return Err(
                        "Remote edit needs a desktop sidecar with local filesystem access"
                            .to_string(),
                    );
                }
                let app = request
                    .app
                    .as_deref()
                    .map(str::trim)
                    .filter(|app| !app.is_empty())
                    .map(local_downloads::validate_open_app)
                    .transpose()?;
                let binding = self.rclone.binding(&request.connection_id)?;
                // A watcher that can never sync back is a trap: gate the
                // write up front, same message as the storage arms.
                ensure_binding_writable(&binding)?;
                let fs = rclone::call_fs(&binding);
                let remote = rclone_gate(
                    &binding.root,
                    binding.lock_to_root,
                    &request.remote_path,
                    crate::policy::PathPolicy::check_read,
                )?;
                let client = self.rclone.client_for_binding(&binding).await?;
                // stat-first: missing object / directory rejection mirrors
                // files/download/start.
                let entry = rclone::ops::stat(
                    &client,
                    &fs,
                    &request.remote_path,
                    &binding.root,
                    binding.lock_to_root,
                )
                .await?;
                if entry.kind == "dir" {
                    return Err(format!(
                        "Cannot open '{}' for remote edit: it is a directory",
                        remote.trim_matches('/')
                    ));
                }
                let base = remote_edit::workspace_base(|key| std::env::var_os(key));
                let local = remote_edit::local_copy_path(&base, &request.connection_id, &remote)?;
                if let Some(parent) = local.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        format!("Failed to create remote-edit workspace: {error}")
                    })?;
                }
                // Re-open of a live session: refresh the pinned app and let
                // the refresh task decide whether the local copy needs a
                // re-pull (remote moved ahead, local copy clean) before the
                // app is launched again. A second watch loop is never
                // spawned — the first open owns the session's watcher.
                if let Some(session) =
                    self.remote_edits
                        .find_for_path(&request.connection_id, &remote)
                {
                    let key = session.key.clone();
                    self.remote_edits.update(&key, |session| session.app = app.clone());
                    if session.status == remote_edit::STATUS_DOWNLOADING
                        || session.status == remote_edit::STATUS_SYNCING
                    {
                        return Ok(json!({
                            "key": key,
                            "localPath": session.local_path,
                            "busy": true,
                        }));
                    }
                    let emitter = emitter.clone();
                    tokio::spawn(remote_edit_refresh_task(
                        Arc::clone(&self.rclone),
                        self.remote_edits.clone(),
                        key.clone(),
                        request.connection_id.clone(),
                        fs,
                        remote,
                        PathBuf::from(&session.local_path),
                        app,
                        session.baseline,
                        emitter,
                    ));
                    return Ok(json!({
                        "key": key,
                        "localPath": session.local_path,
                        "reused": true,
                    }));
                }
                let key = uuid::Uuid::new_v4().to_string();
                self.remote_edits.insert(remote_edit::EditSession {
                    key: key.clone(),
                    connection_id: request.connection_id.clone(),
                    remote_path: remote.clone(),
                    local_path: local.display().to_string(),
                    app: app.clone(),
                    status: remote_edit::STATUS_DOWNLOADING,
                    last_error: None,
                    last_sync_at: None,
                    created_at: store::unix_millis_now(),
                    baseline: None,
                    pending_stable: 0,
                    error_ticks: 0,
                    sync_seq: 0,
                    closing: false,
                });
                let emitter = emitter.clone();
                tokio::spawn(remote_edit_open_task(
                    Arc::clone(&self.rclone),
                    self.remote_edits.clone(),
                    key.clone(),
                    request.connection_id.clone(),
                    fs,
                    remote,
                    local.clone(),
                    app,
                    emitter,
                ));
                Ok(json!({ "key": key, "localPath": local.display().to_string() }))
            }
            "files/remote-edit/status" => {
                let filter = params
                    .get("connectionId")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty());
                Ok(json!({ "sessions": self.remote_edits.snapshot(filter) }))
            }
            "files/remote-edit/close" => {
                let key = params
                    .get("key")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or("Missing key")?;
                if !self.remote_edits.request_close(key) {
                    return Err(format!("Remote-edit session '{key}' was not found"));
                }
                Ok(json!({ "success": true }))
            }
            "files/audit/list" => audit_list_response(&self.store, &params),

            // ------------------------------------------------------------------
            // MCP tool surface (shared/IMPL_PLAN_PLUGIN_MCP §2; ssh mcp.rs
            // skeleton parity): discovery / execution / settings, plus the
            // frontend-side intent report channel (design §1).
            // ------------------------------------------------------------------
            "mcp/tools" => Ok(self.mcp.tool_definitions(&params)),
            "mcp/call" => Ok(self.mcp.call(&self.store, emitter, &params).await?),
            "mcp/settings/get" => Ok(self.mcp.settings_get()),
            "mcp/settings/set" => self.mcp.settings_set(&params),
            "files/ui/state/report" => self.mcp.report(&params),
            // Unknown/unrouted methods keep the historical "Method not
            // found" phrasing — the smoke suite's SKIP semantics and MCP
            // clients match on it.
            _ => Err(format!("Method not found: {method}")),
        }
    }

    /// Phase B `files/upload/finish` byte path: waits out in-flight frames
    /// (same 250ms × 40 stall window as the JobTable's finish, issue#6-6),
    /// then streams the staging file to the exact remote path and lands the
    /// job in a terminal state. Failure removes the staging entry (the
    /// `UploadStaging` cleans its temp file either way) and stores the error
    /// for the terminal-replay path.
    async fn finish_rclone_upload(
        &self,
        task_id: &str,
        emitter: &PluginEmitter,
    ) -> Result<(), String> {
        // issue#6-3 twin: a settled job replays its stored outcome instead of
        // silently re-completing.
        {
            let jobs = rclone_lock(&self.rclone.jobs);
            if let Some(job) = jobs.get(task_id) {
                if job.status.is_terminal() {
                    return match job.status {
                        transfers::JobStatus::Completed => Ok(()),
                        transfers::JobStatus::Canceled => {
                            Err("Upload was canceled".to_string())
                        }
                        _ => Err(job
                            .error
                            .clone()
                            .unwrap_or_else(|| "Upload failed".to_string())),
                    };
                }
            }
        }
        // Wait for in-flight frames while the staging entry is still live in
        // the table, so concurrently arriving frames keep appending.
        if let Some(declared) = {
            let uploads = rclone_lock(&self.rclone.uploads);
            uploads
                .get(task_id)
                .map(|task| task.declared_size)
                .filter(|declared| *declared > 0)
        } {
            let mut last = rclone_lock(&self.rclone.uploads)
                .get(task_id)
                .map(|task| task.staging.received())
                .unwrap_or(0);
            let mut stall_ticks: u32 = 0;
            while last < declared {
                if stall_ticks >= UPLOAD_FINISH_STALL_TICKS {
                    break;
                }
                tokio::time::sleep(UPLOAD_FINISH_TICK).await;
                let Some(current) = rclone_lock(&self.rclone.uploads)
                    .get(task_id)
                    .map(|task| task.staging.received())
                else {
                    break; // slot consumed underneath (cancel / append failure)
                };
                if current == last {
                    stall_ticks += 1;
                } else {
                    stall_ticks = 0;
                    last = current;
                }
            }
        }
        // Single owner for the finish: take the task out (late frames after
        // this point are past the declared size anyway).
        let Some(task) = rclone_lock(&self.rclone.uploads).remove(task_id) else {
            return Err("Upload task was not found".to_string());
        };
        // Transfer-path client (plan finding #11): no wall-clock timeout —
        // the staged upload streams the whole file through one HTTP body and
        // would die inside the default client's 30s. Routed to the owning
        // connection's proxy group (the staging entry carries fs/remote
        // only; the group lives on the connection).
        let connection_id = rclone_lock(&self.rclone.jobs)
            .get(task_id)
            .map(|job| job.connection_id.clone())
            .ok_or("Upload task was not found")?;
        let client = self.rclone.client_for_id(&connection_id).await?.transfer_client();
        let outcome = task.staging.finish(&client, &task.fs, &task.remote, None).await;
        match outcome {
            Ok(_uploaded) => {
                rclone_complete_job(
                    &self.rclone,
                    task_id,
                    transfers::JobStatus::Completed,
                    None,
                    emitter,
                );
                // Best-effort mount-view refresh (batch 5): the upload changed
                // the target file's parent listing, so active rclone-strategy
                // mounts of the connection re-read it. Terminal job records
                // survive completion, so the wire path is still readable here.
                let changed_dir = rclone_lock(&self.rclone.jobs)
                    .get(task_id)
                    .map(|job| parent_dir_of(&job.remote_path));
                mount::best_effort_refresh_mount_caches(
                    &self.rclone,
                    &self.mounts,
                    &connection_id,
                    changed_dir.as_deref(),
                )
                .await;
                Ok(())
            }
            Err(error) => {
                rclone_complete_job(
                    &self.rclone,
                    task_id,
                    transfers::JobStatus::Failed,
                    Some(error.clone()),
                    emitter,
                );
                Err(error)
            }
        }
    }

    /// Phase B `files/download/finish`: mirrors the JobTable's
    /// `finish_download` (in-flight wait, cancel replay, pump-exit grace,
    /// saveToLocal promotion) against the rclone slot tables. Returns the
    /// `localPath` for saveToLocal runs (`None` otherwise).
    async fn finish_rclone_download(
        &self,
        task_id: &str,
        emitter: &PluginEmitter,
    ) -> Result<Option<String>, String> {
        let slot = rclone_lock(&self.rclone.downloads).get(task_id).cloned();
        let Some(slot) = slot else {
            // Slot already settled: replay the stored terminal outcome
            // (Completed idempotently carries localPath).
            if let Some(job) = rclone_lock(&self.rclone.jobs).get(task_id) {
                if job.status.is_terminal() {
                    return rclone_download_finish_result(job);
                }
            }
            return Err(format!("Download task '{task_id}' was not found"));
        };
        let staged_bytes = || -> u64 {
            match &slot.staging {
                Some(staging) => std::fs::metadata(staging)
                    .map(|meta| meta.len())
                    .unwrap_or(0),
                None => rclone_lock(&self.rclone.jobs)
                    .get(task_id)
                    .map(|job| job.transferred_bytes)
                    .unwrap_or(0),
            }
        };
        // In-flight wait (issue#6-6/issue#3 mirror): bytes still advancing
        // keep the wait alive; stall/cancel/pump-exit break out.
        let mut last = staged_bytes();
        let mut stall_ticks: u32 = 0;
        while last < slot.size {
            if stall_ticks >= UPLOAD_FINISH_STALL_TICKS
                || slot.cancel.load(Ordering::Acquire)
                || slot.pump_done.load(Ordering::Acquire)
            {
                break;
            }
            tokio::time::sleep(UPLOAD_FINISH_TICK).await;
            if rclone_lock(&self.rclone.downloads).get(task_id).is_none() {
                break; // pump consumed the slot → terminal replay below
            }
            let current = staged_bytes();
            if current == last {
                stall_ticks += 1;
            } else {
                stall_ticks = 0;
                last = current;
            }
        }
        if slot.cancel.load(Ordering::Acquire) {
            // Canceled: the pump cleans up when it observes the flag; the
            // idempotent residue sweep happens here too, and the stored
            // Canceled outcome replays on any later finish.
            rclone_lock(&self.rclone.downloads).remove(task_id);
            if let Some(staging) = &slot.staging {
                let _ = std::fs::remove_file(staging);
            }
            return Err("Download was canceled".to_string());
        }
        // Bytes settled: wait for the pump to exit (sub-millisecond normally)
        // so the staging rename never races an open handle (Windows).
        let mut grace_ticks: u32 = 0;
        while !slot.pump_done.load(Ordering::Acquire) && grace_ticks < UPLOAD_FINISH_STALL_TICKS {
            tokio::time::sleep(UPLOAD_FINISH_TICK).await;
            if rclone_lock(&self.rclone.downloads).get(task_id).is_none() {
                break;
            }
            grace_ticks += 1;
        }
        let Some(slot) = rclone_lock(&self.rclone.downloads).remove(task_id) else {
            if let Some(job) = rclone_lock(&self.rclone.jobs).get(task_id) {
                if job.status.is_terminal() {
                    return rclone_download_finish_result(job);
                }
            }
            return Err("Download was canceled".to_string());
        };
        // The pump may have landed the job in Failed/Canceled while we
        // waited (short read, sink error): replay the stored outcome.
        if let Some(job) = rclone_lock(&self.rclone.jobs).get(task_id) {
            if job.status.is_terminal() {
                if let Some(staging) = &slot.staging {
                    let _ = std::fs::remove_file(staging);
                }
                return rclone_download_finish_result(job);
            }
        }
        let mut local_path = None;
        if let Some(staging) = &slot.staging {
            match promote_rclone_staging(staging, slot.size) {
                Ok(path) => {
                    if let Some(job) = rclone_lock(&self.rclone.jobs).get_mut(task_id) {
                        job.local_path = Some(path.clone());
                    }
                    local_path = Some(path);
                }
                Err(error) => {
                    let _ = std::fs::remove_file(staging);
                    rclone_complete_job(
                        &self.rclone,
                        task_id,
                        transfers::JobStatus::Failed,
                        Some(error.clone()),
                        emitter,
                    );
                    return Err(error);
                }
            }
        }
        rclone_complete_job(
            &self.rclone,
            task_id,
            transfers::JobStatus::Completed,
            None,
            emitter,
        );
        Ok(local_path)
    }

    fn handle_request(
        &self,
        method: &str,
        params: Value,
        emitter: &PluginEmitter,
    ) -> Result<Value, String> {
        // The rclone engine is the only engine: every request is routed
        // through it (storage methods) or answered inline (support methods).
        self.runtime
            .block_on(self.handle_request_via_rclone(method, params, emitter))
    }

    /// Connection-id-keyed audit used by the rclone arms (the rclone
    /// registry keeps bindings, not StoredConnection records).
    fn audit_id(
        &self,
        connection_id: &str,
        action: &str,
        target: &str,
        result: &str,
    ) -> Result<(), String> {
        if let Err(error) = self.store.append_audit(store::AuditRecord {
            time: store::format_rfc3339(store::unix_millis_now() as i64),
            connection_id: connection_id.to_string(),
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

    /// rclone upload-frame ingest for `handle_binary`, `JobTable::append_upload`
    /// semantics twin: strict 8-byte BE offset continuity (misaligned frame =
    /// hard error), declared-size guard, throttled `files/transfer/progress`.
    /// A staging write failure consumes the slot and lands the job `Failed`
    /// with the stored detail, exactly like the retired writer-failure path.
    fn append_rclone_upload(
        &self,
        task_id: &str,
        data: &[u8],
        emitter: &PluginEmitter,
    ) -> Result<(), String> {
        if !rclone_lock(&self.rclone.uploads).contains_key(task_id) {
            return Err("Upload task was not found".to_string());
        }
        self.append_rclone_upload_inner(task_id, data, emitter)
    }

    /// Staging-table mutation half of [`Plugin::append_rclone_upload`] (the
    /// membership check runs before anything else).
    fn append_rclone_upload_inner(
        &self,
        task_id: &str,
        data: &[u8],
        emitter: &PluginEmitter,
    ) -> Result<(), String> {
        let (offset, payload) = transfers::parse_upload_frame(data)?;
        let outcome = {
            let mut uploads = rclone_lock(&self.rclone.uploads);
            let Some(task) = uploads.get_mut(task_id) else {
                return Err("Upload task was not found".to_string());
            };
            match task.staging.append(offset, payload) {
                Ok(received) => {
                    let emit = task
                        .throttle
                        .should_emit(received, Some(task.declared_size));
                    (received, Some(task.declared_size), emit, None)
                }
                Err(error) => {
                    uploads.remove(task_id);
                    (0, None, false, Some(error))
                }
            }
        };
        match outcome {
            (_, _, _, Some(error)) => {
                rclone_complete_job(
                    &self.rclone,
                    task_id,
                    transfers::JobStatus::Failed,
                    Some(error.clone()),
                    emitter,
                );
                Err(error)
            }
            (received, total, emit, None) => {
                if let Some(job) = rclone_lock(&self.rclone.jobs).get_mut(task_id) {
                    job.transferred_bytes = received;
                    job.status = transfers::JobStatus::Running;
                }
                if emit {
                    // Same throttled payload shape as append_upload:
                    // `{taskId, transferred, total, size, state}`.
                    let mut event = transfers::progress_event(task_id, received, total);
                    if let Some(object) = event.as_object_mut() {
                        object.insert("size".into(), serde_json::json!(total));
                        object.insert(
                            "state".into(),
                            serde_json::json!(transfers::JobStatus::Running.as_str()),
                        );
                    }
                    let _ = emitter.event("files/transfer/progress", event);
                }
                Ok(())
            }
        }
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

/// Connection-level write gate on a StoredConnection — the message-identical
/// sibling of [`ensure_binding_writable`]. Production arms gate on the rclone
/// registry binding, so this copy stays test-only: the gate-parity tests pin
/// its semantics against the shared policy layer.
#[cfg(test)]
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
/// Test-only twin of [`ensure_binding_deletable`] (see [`ensure_writable`]).
#[cfg(test)]
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
/// connection root and `/`). Test-only StoredConnection wrapper of
/// [`refuse_purge_of_root`] (the production arms pass the binding root).
#[cfg(test)]
fn refuse_root_purge(connection: &StoredConnection, path: &str) -> Result<(), String> {
    refuse_purge_of_root(&connection.root, path)
}

/// Parent directory (plugin-space absolute) of a mutated path — the
/// directory whose listing a file/dir mutation actually changed and which
/// the mount VFS auto-refresh re-reads: `/a/b.txt` → `/a`, `/a` → `/`,
/// `/a/b/` → `/a`. Never returns `""`.
fn parent_dir_of(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => parent.to_string(),
        _ => "/".to_string(),
    }
}

/// Root-string twin of [`refuse_root_purge`] for the rclone arms (bindings
/// carry the root, not a StoredConnection); the message text is identical.
fn refuse_purge_of_root(root: &str, path: &str) -> Result<(), String> {
    fn core(path: &str) -> String {
        path.trim().trim_matches('/').to_string()
    }
    if core(path).is_empty() {
        return Err(
            "Purge of the connection root '/' is refused; purge a subdirectory instead"
                .to_string(),
        );
    }
    if !root.is_empty() && core(path) == core(root) {
        return Err(format!(
            "Purge of the connection root '{root}' is refused; purge a subdirectory instead"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// F-RCLONE Phase B helpers (docs/IMPL_PLAN_RCLONE.zh-CN.md §5/§7)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// F-RCLONE Phase C helpers (docs/IMPL_PLAN_RCLONE.zh-CN.md §5/§6)
// ---------------------------------------------------------------------------

/// One rclone sync job (`files/syncDir`|`files/copyDir`): the frozen
/// `sync.rs` handle (backfilled once `start_job` answers) plus the request
/// metadata the DirJob-shaped status/list/terminal projections need after
/// start. The `RcloneEngine.jobs` mirror stays in the single-file TransferJob
/// shape — every wire projection of a sync job goes through
/// [`rclone_sync_dir_job_value`], so the placeholder fields there never leak.
#[derive(Clone)]
struct RcloneSyncRecord {
    handle: Option<rclone::sync::SyncJobHandle>,
    kind: rclone::sync::SyncKind,
    src_conn: String,
    src_rel: String,
    dst_conn: String,
    dst_rel: String,
    dry_run: bool,
    max_delete: Option<u64>,
    files_done: u64,
    files_total: Option<u64>,
    /// Check jobs: one-line difference summary from the terminal report
    /// (`None` while running and for sync/copy/move jobs).
    check_summary: Option<String>,
    /// Bisync jobs: rclone session name from the terminal report (`p1..p2`),
    /// used to stamp the last-synced pref on success.
    bisync_session: Option<String>,
    /// In-flight marker for the source connection's proxy group: dropped
    /// with the record on terminal removal, releasing the idle-group
    /// teardown hold. Clones share the guard's done flag.
    work: rclone::WorkGuard,
}

/// Starts one rclone dir job (`files/syncDir`|`files/copyDir` semantics).
/// Shared verbatim by the workbench arm and the MCP `files_sync` tool so
/// both enqueue into the same job mirror and `files/transfer/status` stays
/// the single poll surface.
///
/// Phase C 终态语义记录（决策固化）：rc 异步作业由 rclone rcd 自身排队，
/// 侧车不做第二层限流（全局并发 3 + 按连接 FIFO 的旧模型不再强制）。这是
/// 终态语义而非遗留缺口；并发行为由 rclone 的作业调度兜底。
#[allow(clippy::too_many_arguments)]
async fn rclone_start_dir_job(
    rclone: Arc<rclone::RcloneEngine>,
    sync_jobs: Arc<std::sync::Mutex<HashMap<String, RcloneSyncRecord>>>,
    request: &model::DirJobRequest,
    sync: bool,
    move_dir: bool,
    emitter: Option<&PluginEmitter>,
) -> Result<String, String> {
    let source_binding = rclone.binding(&request.source_connection_id)?;
    let target_binding = rclone.binding(&request.target_connection_id)?;
    // Server-side mirror runs both fs strings inside one rcd, so the two
    // connections must share a proxy group (same rule as files/copy).
    rclone::ensure_same_proxy_group(&source_binding, &target_binding)?;
    // Gate order/message parity with `validate_dir_job_gates(target,
    // sync)`: the target must be writable, and sync's delete phase
    // additionally requires allow_delete (read_only rejects both). A
    // server-side move deletes the source tree — the source needs the
    // delete gate too (for rename, source and target are one connection).
    ensure_binding_writable(&target_binding)?;
    if sync {
        ensure_binding_deletable(&target_binding)?;
    }
    if move_dir {
        ensure_binding_deletable(&source_binding)?;
    }
    // Path whitelist (Phase B convention): policy-relative remotes for the
    // sync job — source read / target write, connection
    // read_only/allow_delete already gated above.
    let src_rel = rclone_gate(
        &source_binding.root,
        source_binding.lock_to_root,
        &request.source_path,
        crate::policy::PathPolicy::check_read,
    )?;
    let dst_rel = rclone_gate(
        &target_binding.root,
        target_binding.lock_to_root,
        &request.target_path,
        crate::policy::PathPolicy::check_write,
    )?;
    let job_id = uuid::Uuid::new_v4().to_string();
    let job = transfers::TransferJob {
        task_id: job_id.clone(),
        connection_id: request.source_connection_id.clone(),
        // The jobs mirror only carries the single-file TransferJob
        // shape; sync jobs are always projected through
        // `rclone_sync_dir_job_value` (kind: syncDir/copyDir), so
        // this placeholder kind never reaches the wire.
        kind: transfers::TransferKind::Upload,
        remote_path: request.source_path.clone(),
        total_bytes: None,
        transferred_bytes: 0,
        status: transfers::JobStatus::Queued,
        error: None,
        started_at: Some(store::unix_millis_now()),
        finished_at: None,
        local_path: None,
    };
    let record = RcloneSyncRecord {
        handle: None,
        kind: if move_dir {
            rclone::sync::SyncKind::Move
        } else if sync {
            rclone::sync::SyncKind::Sync
        } else {
            rclone::sync::SyncKind::Copy
        },
        src_conn: request.source_connection_id.clone(),
        src_rel: src_rel.clone(),
        dst_conn: request.target_connection_id.clone(),
        dst_rel: dst_rel.clone(),
        dry_run: request.dry_run.unwrap_or(false),
        max_delete: request.max_delete,
        files_done: 0,
        files_total: None,
        check_summary: None,
        bisync_session: None,
        // Source and target share one proxy group (enforced above), so the
        // source group's key tracks the rcd the job runs on.
        work: rclone.start_work(&rclone::registry::group_key_of(
            source_binding.proxy.as_ref(),
        )),
    };
    let is_move = matches!(record.kind, rclone::sync::SyncKind::Move);
    // rclone's sync family does not auto-create the destination root on
    // every backend (FTP answers 501 "No such directory" and the job
    // fails); pre-create it best-effort — mkdir is idempotent elsewhere.
    {
        let client = rclone.client_for_binding(&target_binding).await?;
        let _ = client
            .call(
                "operations/mkdir",
                &serde_json::json!({
                    "fs": rclone::call_fs(&target_binding),
                    "remote": dst_rel,
                }),
            )
            .await;
    }
    rclone_lock(&rclone.jobs).insert(job_id.clone(), job.clone());
    // Initial queued event before the spawn — enqueue_dir_job
    // parity (queued is a wire-visible state there too).
    if let Some(emitter) = emitter {
        let _ = emitter.event(
            "files/transfer/progress",
            rclone_sync_event_from(&job, &record),
        );
    }
    rclone_lock(&sync_jobs).insert(job_id.clone(), record);

    // The on_event closure mirrors progress into the jobs table
    // and re-emits `files/transfer/progress` under the shared
    // PROGRESS_INTERVAL_MS/1% throttle, byte-shape aligned with
    // `emit_dir_progress`; Failed/Canceled settle the same
    // terminal-once semantics as complete_dir_job.
    // Owned clones feed the 'static on_event closure (start_job spawns it);
    // the caller's handles stay live for the unwind path below.
    let engine = Arc::clone(&rclone);
    let shared_jobs = Arc::clone(&sync_jobs);
    let event_emitter = emitter.cloned();
    let event_job_id = job_id.clone();
    let mut throttle = transfers::Throttle::default();
    // A server-side move (sync/move) relocates files but leaves the empty
    // source directory tree behind — the Completed handler purges it
    // (best-effort; the delete gate was checked at enqueue time).
    let move_cleanup = if is_move {
        Some((
            rclone.client_for_binding(&source_binding).await?.transfer_client(),
            rclone::call_fs(&source_binding),
            src_rel.clone(),
        ))
    } else {
        None
    };
    let on_event: Box<dyn FnMut(rclone::sync::SyncEvent) + Send> =
        Box::new(move |event| match event {
            rclone::sync::SyncEvent::Progress {
                transferred,
                total,
                rate: _,
            } => {
                let emit = {
                    let mut jobs = rclone_lock(&engine.jobs);
                    let Some(job) = jobs.get_mut(&event_job_id) else {
                        return;
                    };
                    job.transferred_bytes = transferred;
                    if job.status == transfers::JobStatus::Queued {
                        job.status = transfers::JobStatus::Running;
                    }
                    if total > 0 {
                        job.total_bytes = Some(total);
                    }
                    throttle.should_emit(transferred, job.total_bytes)
                };
                if emit {
                    let job = rclone_lock(&engine.jobs).get(&event_job_id).cloned();
                    let record = rclone_lock(&shared_jobs).get(&event_job_id).cloned();
                    if let (Some(job), Some(record)) = (job, record) {
                        if let Some(emitter) = &event_emitter {
                            let _ = emitter.event(
                                "files/transfer/progress",
                                rclone_sync_event_from(&job, &record),
                            );
                        }
                    }
                }
            }
            rclone::sync::SyncEvent::Completed { bytes, files } => {
                {
                    let mut jobs = rclone_lock(&engine.jobs);
                    if let Some(job) = jobs.get_mut(&event_job_id) {
                        job.transferred_bytes = bytes;
                        if bytes > 0 {
                            job.total_bytes = Some(bytes);
                        }
                    }
                }
                rclone_sync_terminal(
                    &engine.jobs,
                    &shared_jobs,
                    event_emitter.as_ref(),
                    &event_job_id,
                    transfers::JobStatus::Completed,
                    None,
                    Some(files),
                );
                // sync/move leaves the emptied source tree; purge it so a
                // directory rename does not leave the old name behind.
                if let Some((ref client, ref src_fs, ref src_rel)) = move_cleanup {
                    let client = client.clone();
                    let src_fs = src_fs.clone();
                    let src_rel = src_rel.clone();
                    tokio::spawn(async move {
                        if let Err(error) = client
                            .call(
                                "operations/purge",
                                &serde_json::json!({ "fs": src_fs, "remote": src_rel }),
                            )
                            .await
                        {
                            eprintln!("rclone move-dir source cleanup failed: {error}");
                        }
                    });
                }
            }
            rclone::sync::SyncEvent::Failed { message } => {
                rclone_sync_terminal(
                    &engine.jobs,
                    &shared_jobs,
                    event_emitter.as_ref(),
                    &event_job_id,
                    transfers::JobStatus::Failed,
                    Some(message),
                    None,
                );
            }
            rclone::sync::SyncEvent::Canceled => {
                rclone_sync_terminal(
                    &engine.jobs,
                    &shared_jobs,
                    event_emitter.as_ref(),
                    &event_job_id,
                    transfers::JobStatus::Canceled,
                    None,
                    None,
                );
            }
            // Dir jobs never emit the check report; exhaustive match only.
            rclone::sync::SyncEvent::CheckFinished { .. } => {}
            rclone::sync::SyncEvent::BisyncFinished { .. } => {}
        });
    // Plan finding #11: the transfer client drops the wall-clock
    // timeout — long mirror runs would die inside the 30s default.
    let started = rclone::sync::start_job(
        rclone.client_for_binding(&source_binding).await?.transfer_client(),
        rclone::sync::SyncJobParams {
            task_id: job_id.clone(),
            kind: if sync {
                rclone::sync::SyncKind::Sync
            } else {
                rclone::sync::SyncKind::Copy
            },
            src_fs: rclone::call_fs(&source_binding),
            dst_fs: rclone::call_fs(&target_binding),
            src_rel,
            dst_rel,
            dry_run: request.dry_run.unwrap_or(false),
            max_delete: request.max_delete,
            include: request.include.clone(),
            exclude: request.exclude.clone(),
            backup_dir_rel: request.backup_dir.clone(),
            suffix: request.suffix.clone(),
            metadata: request.metadata.unwrap_or(false),
            min_size: request.min_size.clone(),
            max_size: request.max_size.clone(),
            min_age: request.min_age.clone(),
            max_age: request.max_age.clone(),
            transfers: request.transfers,
            checkers: request.checkers,
            retries: request.retries,
            check_one_way: false,
            check_download: false,
            // 目录作业不携带 SUM 校验参数（仅 check 作业的 SUM 分支使用）。
            sum_remote: None,
            sum_hash: None,
            bisync_workdir: None,
            bisync_resync: false,
            bisync_resync_mode: None,
        },
        on_event,
    )
    .await;
    match started {
        Ok(handle) => {
            if let Some(record) = rclone_lock(&sync_jobs).get_mut(&job_id) {
                record.handle = Some(handle);
            }
        }
        Err(error) => {
            // The job never started: unwind the mirrors so the
            // panel never sees a ghost entry.
            rclone_lock(&rclone.jobs).remove(&job_id);
            rclone_lock(&sync_jobs).remove(&job_id);
            return Err(error);
        }
    }
    Ok(job_id)
}

/// SUM 文件扩展名 → rclone 哈希类型（批次7）。猜错哈希类型会让 rclone 把
/// 整个目录报成差异，因此无法识别的扩展名直接拒绝作业，让用户显式指定。
fn sum_hash_from_extension(sum_path: &str) -> Result<String, String> {
    let extension = std::path::Path::new(sum_path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "md5" => Ok("md5".to_string()),
        "sha1" => Ok("sha1".to_string()),
        "sha256" => Ok("sha256".to_string()),
        "sha512" => Ok("sha512".to_string()),
        "crc32" => Ok("crc32".to_string()),
        other => Err(format!(
            "cannot infer the hash type from '.{other}' — pass hashType (md5/sha1/sha256/sha512/crc32)"
        )),
    }
}

/// Starts one `operations/check` comparison job (`files/check`): the same
/// job mirror + transfer-tracker surface as dir jobs. The terminal report
/// lands on the record as `checkSummary`; differences are data, so the job
/// completes unless rclone itself errors.
///
/// SUM 校验模式（批次7）：`sum_path` 存在时改为单连接核验——files/hashsum
/// 把校验文件写在目录旁 `<目录>.<hash>`，行相对该目录（hashsum 走
/// fs-scoped 生成），因此被核验目录 = SUM 文件名去扩展名后的同名目录；
/// SUM 文件与目录各过一次 read 门 + 存在性预检（rclone 的 operations/check
/// 对缺失 checkFile 回空报告，会把坏 SUM 静默判成 identical）；哈希类型
/// 缺省按扩展名推断；src 侧不参与比较（`src_rel` 置空）。
async fn rclone_start_check_job(
    rclone: Arc<rclone::RcloneEngine>,
    sync_jobs: Arc<std::sync::Mutex<HashMap<String, RcloneSyncRecord>>>,
    request: &model::CheckRequest,
    emitter: Option<&PluginEmitter>,
) -> Result<String, String> {
    let source_binding = rclone.binding(&request.source_connection_id)?;
    let target_binding = rclone.binding(&request.target_connection_id)?;
    // One rc call drives both fs strings inside a single rcd.
    rclone::ensure_same_proxy_group(&source_binding, &target_binding)?;
    // SUM 分支预取：SUM 文件 read 门 + 被核验目录 read 门 + 存在性预检 +
    // 哈希类型解析。wire 路径的 base/扩展名拆分与 files/hashsum 臂保持一致。
    let sum_target = match request
        .sum_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(sum_path) => {
            let sum_remote = rclone_gate(
                &target_binding.root,
                target_binding.lock_to_root,
                sum_path,
                crate::policy::PathPolicy::check_read,
            )?;
            let trimmed = sum_path.trim_matches('/');
            let (parent, base) = match trimmed.rsplit_once('/') {
                Some((parent, base)) => (format!("/{parent}"), base),
                None => (String::new(), trimmed),
            };
            let stem = base
                .rsplit_once('.')
                .map(|(stem, _extension)| stem)
                .filter(|stem| !stem.is_empty());
            let Some(stem) = stem else {
                return Err(format!(
                    "cannot infer the verified directory from SUM '{sum_path}' — expected '<dir>.<hash>' (e.g. /data.md5)"
                ));
            };
            let dir_path = format!("{parent}/{stem}");
            let dir_rel = rclone_gate(
                &target_binding.root,
                target_binding.lock_to_root,
                &dir_path,
                crate::policy::PathPolicy::check_read,
            )?;
            let hash_type = match request
                .hash_type
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                Some(explicit) => explicit.to_lowercase(),
                None => sum_hash_from_extension(sum_path)?,
            };
            // 存在性预检：SUM 与被核验目录缺一即拒——否则 rclone 对缺失
            // checkFile 回空差异报告，作业以 identical 假阴性收场。
            let target_client = rclone.client_for_binding(&target_binding).await?;
            let target_fs = rclone::call_fs(&target_binding);
            for probe in [sum_remote.as_str(), dir_rel.as_str()] {
                let exists = target_client
                    .operations_stat(&target_fs, probe)
                    .await
                    .ok()
                    .is_some_and(|value| {
                        value.get("item").is_some_and(|item| !item.is_null())
                    });
                if !exists {
                    return Err(format!(
                        "Failed to stat '{}': path does not exist",
                        probe.trim_matches('/')
                    ));
                }
            }
            Some((dir_rel, sum_remote, hash_type))
        }
        None => None,
    };
    // Check reads both sides and writes nothing — read gates on both paths.
    let src_rel = rclone_gate(
        &source_binding.root,
        source_binding.lock_to_root,
        &request.source_path,
        crate::policy::PathPolicy::check_read,
    )?;
    let mut dst_rel = rclone_gate(
        &target_binding.root,
        target_binding.lock_to_root,
        &request.target_path,
        crate::policy::PathPolicy::check_read,
    )?;
    if let Some((dir_rel, _, _)) = &sum_target {
        dst_rel = dir_rel.clone();
    }
    let job_id = uuid::Uuid::new_v4().to_string();
    let job = transfers::TransferJob {
        task_id: job_id.clone(),
        connection_id: request.source_connection_id.clone(),
        // Projected through `rclone_sync_dir_job_value` (kind: check) like
        // every dir job; the placeholder kind never reaches the wire.
        kind: transfers::TransferKind::Upload,
        remote_path: request.source_path.clone(),
        total_bytes: None,
        transferred_bytes: 0,
        status: transfers::JobStatus::Queued,
        error: None,
        started_at: Some(store::unix_millis_now()),
        finished_at: None,
        local_path: None,
    };
    rclone_lock(&rclone.jobs).insert(job_id.clone(), job);
    let record = RcloneSyncRecord {
        handle: None,
        kind: rclone::sync::SyncKind::Check,
        src_conn: request.source_connection_id.clone(),
        // SUM 模式无源侧：占位空串，面板只看 dst_rel（被核验目录）。
        src_rel: if sum_target.is_some() { String::new() } else { src_rel.clone() },
        dst_conn: request.target_connection_id.clone(),
        dst_rel: dst_rel.clone(),
        dry_run: false,
        max_delete: None,
        files_done: 0,
        files_total: None,
        check_summary: None,
        bisync_session: None,
        work: rclone.start_work(&rclone::registry::group_key_of(
            source_binding.proxy.as_ref(),
        )),
    };
    rclone_lock(&sync_jobs).insert(job_id.clone(), record);

    let engine = Arc::clone(&rclone);
    let shared_jobs = Arc::clone(&sync_jobs);
    let event_emitter = emitter.cloned();
    let event_job_id = job_id.clone();
    let on_event: Box<dyn FnMut(rclone::sync::SyncEvent) + Send> =
        Box::new(move |event| match event {
            rclone::sync::SyncEvent::CheckFinished { report } => {
                let summary = check_summary_from(&report);
                if let Some(record) = rclone_lock(&shared_jobs).get_mut(&event_job_id) {
                    record.check_summary = Some(summary);
                }
                rclone_sync_terminal(
                    &engine.jobs,
                    &shared_jobs,
                    event_emitter.as_ref(),
                    &event_job_id,
                    transfers::JobStatus::Completed,
                    None,
                    None,
                );
            }
            // Unreachable for check jobs (the report beats the counters),
            // kept for mirror consistency.
            rclone::sync::SyncEvent::Completed { .. } => {
                rclone_sync_terminal(
                    &engine.jobs,
                    &shared_jobs,
                    event_emitter.as_ref(),
                    &event_job_id,
                    transfers::JobStatus::Completed,
                    None,
                    None,
                );
            }
            rclone::sync::SyncEvent::Progress { .. } => {}
            rclone::sync::SyncEvent::Failed { message } => {
                rclone_sync_terminal(
                    &engine.jobs,
                    &shared_jobs,
                    event_emitter.as_ref(),
                    &event_job_id,
                    transfers::JobStatus::Failed,
                    Some(message),
                    None,
                );
            }
            rclone::sync::SyncEvent::Canceled => {
                rclone_sync_terminal(
                    &engine.jobs,
                    &shared_jobs,
                    event_emitter.as_ref(),
                    &event_job_id,
                    transfers::JobStatus::Canceled,
                    None,
                    None,
                );
            }
            // Check jobs never emit bisync reports; exhaustive match only.
            rclone::sync::SyncEvent::BisyncFinished { .. } => {}
        });
    let started = rclone::sync::start_job(
        rclone.client_for_binding(&source_binding).await?.transfer_client(),
        rclone::sync::SyncJobParams {
            task_id: job_id.clone(),
            kind: rclone::sync::SyncKind::Check,
            src_fs: rclone::call_fs(&source_binding),
            dst_fs: rclone::call_fs(&target_binding),
            // SUM 模式无源侧：src_rel 置空（rc 体不含 srcFs，见 sync.rs）。
            src_rel: if sum_target.is_some() { String::new() } else { src_rel },
            dst_rel,
            dry_run: false,
            max_delete: None,
            include: None,
            exclude: None,
            backup_dir_rel: None,
            suffix: None,
            // Check/bisync never carry the size/age/metadata filters: rc
            // would accept them (live-verified v1.75.1) but a filtered
            // comparison quietly narrows the difference report.
            metadata: false,
            min_size: None,
            max_size: None,
            min_age: None,
            max_age: None,
            transfers: None,
            checkers: None,
            retries: None,
            check_one_way: sum_target.is_none() && request.one_way.unwrap_or(false),
            check_download: sum_target.is_none() && request.download.unwrap_or(false),
            sum_remote: sum_target
                .as_ref()
                .map(|(_, sum_remote, _)| sum_remote.clone()),
            sum_hash: sum_target.as_ref().map(|(_, _, hash_type)| hash_type.clone()),
            bisync_workdir: None,
            bisync_resync: false,
            bisync_resync_mode: None,
        },
        on_event,
    )
    .await;
    match started {
        Ok(handle) => {
            if let Some(record) = rclone_lock(&sync_jobs).get_mut(&job_id) {
                record.handle = Some(handle);
            }
        }
        Err(error) => {
            rclone_lock(&rclone.jobs).remove(&job_id);
            rclone_lock(&sync_jobs).remove(&job_id);
            return Err(error);
        }
    }
    Ok(job_id)
}

/// rclone bisync session name for a path pair (sanitize rule pinned against
/// v1.75.1 probes: leading `/` dropped, everything outside
/// `[A-Za-z0-9._-]` → `_`, joined with `..`).
fn bisync_session_name(path1: &str, path2: &str) -> String {
    fn sanitize(path: &str) -> String {
        path.trim_start_matches('/')
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
            .collect()
    }
    format!("{}..{}", sanitize(path1), sanitize(path2))
}

/// Extracts a readable failure hint from a bisync abort log (the last
/// `ERROR :` line, ANSI stripped). Falls back to the raw error text.
fn bisync_failure_message(report: &Value, fallback: &str) -> String {
    let log = report
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let last_error = log
        .lines()
        .filter(|line| line.contains("ERROR :"))
        .next_back()
        .map(|line| {
            let cleaned: String = line
                .chars()
                .filter(|c| !c.is_control())
                .collect::<String>()
                .replace("[31m", "")
                .replace("[32m", "")
                .replace("[0m", "");
            cleaned.trim().to_string()
        });
    match last_error {
        Some(line) if !line.is_empty() => {
            if line.contains("Must run --resync") {
                format!("{line} (run the comparison/sync again in resync mode to recover)")
            } else {
                line
            }
        }
        _ => fallback.to_string(),
    }
}

/// Starts one `sync/bisync` job (`files/bisync/start`): bidirectional sync
/// over the shared job mirror. State files live under
/// `<data dir>/bisync-workdir/` so sessions survive rcd respawns and sidecar
/// restarts; a successful run stamps `bisyncLast.<session>` in prefs.
async fn rclone_start_bisync_job(
    rclone: Arc<rclone::RcloneEngine>,
    sync_jobs: Arc<std::sync::Mutex<HashMap<String, RcloneSyncRecord>>>,
    store: Arc<Store>,
    request: &model::BisyncStartRequest,
    emitter: Option<&PluginEmitter>,
) -> Result<String, String> {
    let source_binding = rclone.binding(&request.source_connection_id)?;
    let target_binding = rclone.binding(&request.target_connection_id)?;
    rclone::ensure_same_proxy_group(&source_binding, &target_binding)?;
    // Bisync writes AND deletes on both sides.
    ensure_binding_writable(&source_binding)?;
    ensure_binding_deletable(&source_binding)?;
    ensure_binding_writable(&target_binding)?;
    ensure_binding_deletable(&target_binding)?;
    let src_rel = rclone_gate(
        &source_binding.root,
        source_binding.lock_to_root,
        &request.source_path,
        crate::policy::PathPolicy::check_write,
    )?;
    let dst_rel = rclone_gate(
        &target_binding.root,
        target_binding.lock_to_root,
        &request.target_path,
        crate::policy::PathPolicy::check_write,
    )?;
    // Persistent state dir (best-effort create; rclone recreates as needed).
    let workdir = store.data_dir().join("bisync-workdir");
    if let Err(error) = std::fs::create_dir_all(&workdir) {
        eprintln!("[io.dbx.files] bisync workdir create failed: {error}");
    }
    let resync = request.mode.as_deref().map(str::trim) == Some("resync");
    let job_id = uuid::Uuid::new_v4().to_string();
    let job = transfers::TransferJob {
        task_id: job_id.clone(),
        connection_id: request.source_connection_id.clone(),
        kind: transfers::TransferKind::Upload, // placeholder; projected via dir_job_value
        remote_path: request.source_path.clone(),
        total_bytes: None,
        transferred_bytes: 0,
        status: transfers::JobStatus::Queued,
        error: None,
        started_at: Some(store::unix_millis_now()),
        finished_at: None,
        local_path: None,
    };
    rclone_lock(&rclone.jobs).insert(job_id.clone(), job);
    let record = RcloneSyncRecord {
        handle: None,
        kind: rclone::sync::SyncKind::Bisync,
        src_conn: request.source_connection_id.clone(),
        src_rel: src_rel.clone(),
        dst_conn: request.target_connection_id.clone(),
        dst_rel: dst_rel.clone(),
        dry_run: request.dry_run.unwrap_or(false),
        max_delete: None,
        files_done: 0,
        files_total: None,
        check_summary: None,
        bisync_session: None,
        work: rclone.start_work(&rclone::registry::group_key_of(
            source_binding.proxy.as_ref(),
        )),
    };
    rclone_lock(&sync_jobs).insert(job_id.clone(), record);

    let engine = Arc::clone(&rclone);
    let shared_jobs = Arc::clone(&sync_jobs);
    let event_emitter = emitter.cloned();
    let event_job_id = job_id.clone();
    let prefs_store = Arc::clone(&store);
    let on_event: Box<dyn FnMut(rclone::sync::SyncEvent) + Send> =
        Box::new(move |event| match event {
            rclone::sync::SyncEvent::BisyncFinished { report, success } => {
                let session = report
                    .get("session")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(session) = &session {
                    if let Some(record) = rclone_lock(&shared_jobs).get_mut(&event_job_id) {
                        record.bisync_session = Some(session.clone());
                    }
                    // Stamp last-synced time per session (success only).
                    if success {
                        let mut prefs = prefs_store.load_prefs();
                        if let Some(object) = prefs.as_object_mut() {
                            object.insert(
                                format!("bisyncLast.{session}"),
                                json!(store::unix_millis_now()),
                            );
                        }
                        let _ = prefs_store.save_prefs(&prefs);
                    }
                }
                let terminal = if success {
                    (transfers::JobStatus::Completed, None)
                } else {
                    (
                        transfers::JobStatus::Failed,
                        Some(bisync_failure_message(&report, "bisync aborted")),
                    )
                };
                rclone_sync_terminal(
                    &engine.jobs,
                    &shared_jobs,
                    event_emitter.as_ref(),
                    &event_job_id,
                    terminal.0,
                    terminal.1,
                    None,
                );
            }
            rclone::sync::SyncEvent::Completed { .. } => {
                rclone_sync_terminal(
                    &engine.jobs,
                    &shared_jobs,
                    event_emitter.as_ref(),
                    &event_job_id,
                    transfers::JobStatus::Completed,
                    None,
                    None,
                );
            }
            rclone::sync::SyncEvent::Progress { .. } => {}
            // Check jobs never emit bisync reports; exhaustive match only.
            rclone::sync::SyncEvent::CheckFinished { .. } => {}
            rclone::sync::SyncEvent::BisyncFinished { .. } => {}
            rclone::sync::SyncEvent::Failed { message } => {
                rclone_sync_terminal(
                    &engine.jobs,
                    &shared_jobs,
                    event_emitter.as_ref(),
                    &event_job_id,
                    transfers::JobStatus::Failed,
                    Some(message),
                    None,
                );
            }
            rclone::sync::SyncEvent::Canceled => {
                rclone_sync_terminal(
                    &engine.jobs,
                    &shared_jobs,
                    event_emitter.as_ref(),
                    &event_job_id,
                    transfers::JobStatus::Canceled,
                    None,
                    None,
                );
            }
        });
    let started = rclone::sync::start_job(
        rclone.client_for_binding(&source_binding).await?.transfer_client(),
        rclone::sync::SyncJobParams {
            task_id: job_id.clone(),
            kind: rclone::sync::SyncKind::Bisync,
            src_fs: rclone::call_fs(&source_binding),
            dst_fs: rclone::call_fs(&target_binding),
            src_rel,
            dst_rel,
            dry_run: request.dry_run.unwrap_or(false),
            max_delete: None,
            include: None,
            exclude: None,
            backup_dir_rel: None,
            suffix: None,
            // Bisync never carries the size/age/metadata filters (see the
            // check construction above); resync included.
            metadata: false,
            min_size: None,
            max_size: None,
            min_age: None,
            max_age: None,
            transfers: None,
            checkers: None,
            retries: None,
            check_one_way: false,
            check_download: false,
            sum_remote: None,
            sum_hash: None,
            bisync_workdir: Some(workdir.to_string_lossy().into_owned()),
            bisync_resync: resync,
            bisync_resync_mode: request.resync_mode.clone(),
        },
        on_event,
    )
    .await;
    match started {
        Ok(handle) => {
            if let Some(record) = rclone_lock(&sync_jobs).get_mut(&job_id) {
                record.handle = Some(handle);
            }
        }
        Err(error) => {
            rclone_lock(&rclone.jobs).remove(&job_id);
            rclone_lock(&sync_jobs).remove(&job_id);
            return Err(error);
        }
    }
    Ok(job_id)
}

/// MCP `RcloneRoute.start_sync` factory: an owned closure over the engine +
/// sync-job mirror so the MCP `files_sync` tool can enqueue jobs with the
/// exact workbench semantics (same gates, same mirror, same response).
fn mcp_sync_starter(
    rclone: Arc<rclone::RcloneEngine>,
    sync_jobs: Arc<std::sync::Mutex<HashMap<String, RcloneSyncRecord>>>,
) -> mcp::SyncJobStarter {
    Arc::new(move |request: model::DirJobRequest, sync: bool, emitter: Option<PluginEmitter>| {
        let rclone = Arc::clone(&rclone);
        let sync_jobs = Arc::clone(&sync_jobs);
        Box::pin(async move {
            rclone_start_dir_job(rclone, sync_jobs, &request, sync, false, emitter.as_ref()).await
        })
    })
}

/// Persisted `TransferRecord` → live mirror `TransferJob` (hydration twin of
/// the persistence below; both share the camelCase field set).
fn rclone_job_from_record(record: store::TransferRecord) -> transfers::TransferJob {
    let kind = match record.kind.as_str() {
        "download" => transfers::TransferKind::Download,
        _ => transfers::TransferKind::Upload,
    };
    let status = match record.status.as_str() {
        "failed" => transfers::JobStatus::Failed,
        "canceled" => transfers::JobStatus::Canceled,
        _ => transfers::JobStatus::Completed,
    };
    transfers::TransferJob {
        task_id: record.task_id,
        connection_id: record.connection_id,
        kind,
        remote_path: record.remote_path,
        total_bytes: record.total_bytes,
        transferred_bytes: record.transferred_bytes,
        status,
        error: record.error,
        started_at: record.started_at,
        finished_at: record.finished_at,
        local_path: record.local_path,
    }
}

/// Serialization for `transfers.json` writes: terminal records append
/// under this lock. Each write is an atomic tmp+rename, so even a racing
/// writer costs at most one dropped history line, never a corrupt file.
/// Every writer goes through here.
fn history_write_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(()));
    rclone_lock(&LOCK)
}

/// Appends a terminal single-file job to the persisted transfer history
/// (`store.record_transfer`, ring-capped) — the JobTable history parity the
/// transfers panel and the `files/local/reveal|open` whitelist rely on. Dir
/// jobs (syncDir/copyDir) stay memory-only: `TransferRecord.kind` is
/// upload|download only. Best-effort: a failed write never fails the job.
fn persist_rclone_history(rclone: &rclone::RcloneEngine, job: &transfers::TransferJob) {
    if !job.status.is_terminal() {
        return;
    }
    let Some(store) = rclone_lock(&rclone.history).clone() else {
        return;
    };
    let record = store::TransferRecord {
        task_id: job.task_id.clone(),
        connection_id: job.connection_id.clone(),
        kind: match job.kind {
            transfers::TransferKind::Upload => "upload".to_string(),
            // 归档下载按普通下载落历史：`TransferRecord.kind` 契约保持
            // upload|download（重启示见 is_recorded_download 的白名单语义）。
            transfers::TransferKind::Download | transfers::TransferKind::ArchiveDownload => {
                "download".to_string()
            }
        },
        remote_path: job.remote_path.clone(),
        total_bytes: job.total_bytes,
        transferred_bytes: job.transferred_bytes,
        status: job.status.as_str().to_string(),
        error: job.error.clone(),
        started_at: job.started_at,
        finished_at: job.finished_at,
        local_path: job.local_path.clone(),
    };
    let _guard = history_write_lock();
    if let Err(error) = store.record_transfer(record) {
        eprintln!("[io.dbx.files] rclone transfer history write failed: {error}");
    }
}

/// `files/transfer/progress` payload for an rclone sync job, byte-shape
/// aligned with `transfers.rs::emit_dir_progress` (the DirJob base keys plus
/// state/sync/kind/remotePath and the optional dryRun/error flags). Skipped
/// counters stay 0: rclone does the incremental compare server-side and the
/// frozen `SyncEvent` carries no skip statistics.
fn rclone_sync_event_from(job: &transfers::TransferJob, record: &RcloneSyncRecord) -> Value {
    let mut event = json!({
        "jobId": job.task_id,
        "filesDone": record.files_done,
        "filesTotal": record.files_total,
        "bytesDone": job.transferred_bytes,
        "bytesTotal": job.total_bytes,
        "filesSkipped": 0,
        "bytesSkipped": 0,
        "state": job.status.as_str(),
        "sync": matches!(record.kind, rclone::sync::SyncKind::Sync),
        "kind": match record.kind {
            rclone::sync::SyncKind::Sync => "syncDir",
            rclone::sync::SyncKind::Copy => "copyDir",
            rclone::sync::SyncKind::Move => "moveDir",
            rclone::sync::SyncKind::Check => "check",
            rclone::sync::SyncKind::Bisync => "bisync",
        },
        "remotePath": job.remote_path,
    });
    if record.dry_run {
        event["dryRun"] = json!(true);
    }
    if let Some(error) = &job.error {
        event["error"] = json!(error);
    }
    event
}

/// DirJob-shaped projection of an rclone sync job for `files/transfer/status`
/// (kind: "dirJob") and `files/transfers/list` — the exact camelCase `DirJob`
/// wire shape `JobTable::dir_status`/`list_merged` carry (`maxDelete` omitted
/// when unset, `error` always present, mirroring the serde derives).
fn rclone_sync_dir_job_value(job: &transfers::TransferJob, record: &RcloneSyncRecord) -> Value {
    let mut value = json!({
        "jobId": job.task_id,
        "sourceConnectionId": job.connection_id,
        "sourcePath": job.remote_path,
        "targetConnectionId": record.dst_conn,
        "targetPath": record.dst_rel,
        "sync": matches!(record.kind, rclone::sync::SyncKind::Sync),
        "deleteSource": matches!(record.kind, rclone::sync::SyncKind::Move),
        "kind": match record.kind {
            rclone::sync::SyncKind::Sync => "syncDir",
            rclone::sync::SyncKind::Copy => "copyDir",
            rclone::sync::SyncKind::Move => "moveDir",
            rclone::sync::SyncKind::Check => "check",
            rclone::sync::SyncKind::Bisync => "bisync",
        },
        "filesDone": record.files_done,
        "filesTotal": record.files_total,
        "bytesDone": job.transferred_bytes,
        "bytesTotal": job.total_bytes,
        "filesSkipped": 0,
        "bytesSkipped": 0,
        "dryRun": record.dry_run,
        "status": job.status.as_str(),
        "error": job.error,
        "startedAt": job.started_at,
        "finishedAt": job.finished_at,
    });
    if let Some(max_delete) = record.max_delete {
        value["maxDelete"] = json!(max_delete);
    }
    if let Some(summary) = &record.check_summary {
        value["checkSummary"] = json!(summary);
    }
    value
}

/// One-line human summary of an `operations/check` report (live-verified
/// shape: missingOnSrc/missingOnDst/differ/error arrays + a `status` line).
fn check_summary_from(report: &Value) -> String {
    let count = |key: &str| {
        report
            .get(key)
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0)
    };
    let (src, dst, differ, errors) =
        (count("missingOnSrc"), count("missingOnDst"), count("differ"), count("error"));
    let total = src + dst + differ + errors;
    if total == 0 {
        "identical".to_string()
    } else {
        format!(
            "{total} differences (missing on source: {src}, missing on target: {dst}, differ: {differ}, errors: {errors})"
        )
    }
}

/// Terminal transition for an rclone sync job — `complete_dir_job` twin:
/// settle the jobs mirror terminal-once, record the Completed file counters
/// (the only SyncEvent carrying them), then emit the DirJob-shaped final
/// progress event. `emitter` is `None` on emitter-less callers (stdio MCP);
/// dir jobs are never persisted (single-file `TransferRecord` shape only).
/// Returns `true` when this call performed the transition.
fn rclone_sync_terminal(
    jobs: &std::sync::Mutex<HashMap<String, transfers::TransferJob>>,
    sync_jobs: &std::sync::Mutex<HashMap<String, RcloneSyncRecord>>,
    emitter: Option<&PluginEmitter>,
    job_id: &str,
    status: transfers::JobStatus,
    error: Option<String>,
    files: Option<u64>,
) -> bool {
    let job = {
        let mut jobs = rclone_lock(jobs);
        let Some(job) = jobs.get_mut(job_id) else {
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
    if files.is_some() {
        let mut sync_jobs = rclone_lock(sync_jobs);
        if let Some(record) = sync_jobs.get_mut(job_id) {
            record.files_done = files.unwrap_or(0);
            record.files_total = files;
        }
    }
    if let (Some(record), Some(emitter)) = (rclone_lock(sync_jobs).get(job_id), emitter) {
        let _ = emitter.event(
            "files/transfer/progress",
            rclone_sync_event_from(&job, record),
        );
    }
    true
}

/// Poison-tolerant lock for the rclone Phase B tables (std Mutex; a panic in
/// some other worker must not wedge every later request).
/// `files/hashsum` guard: `operations/hashsum` walks every object inside one
/// rc call — refuse absurd trees instead of hashing for minutes.
const HASHSUM_MAX_FILES: u64 = 20_000;

/// `files/about` per-connection cache TTL (sidebar renders it on every load).
const ABOUT_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// `files/search` guard: refuse full-tree listings above this many files —
/// narrow the root instead.
const SEARCH_MAX_SCAN: u64 = 20_000;
/// `files/search` hard cap on returned rows (the wire limit, not the scan).
const SEARCH_RESULT_LIMIT: u32 = 500;

fn rclone_lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Upload/download finish in-flight wait, copied from transfers.rs
/// (issue#6-6): 250ms sampling, stalled after 40 ticks (10s).
const UPLOAD_FINISH_TICK: Duration = Duration::from_millis(250);
const UPLOAD_FINISH_STALL_TICKS: u32 = 40;

/// Root-relative remote resolution for the Phase B ops that take a plain
/// remote (`read_prefix`, `write_bytes`, the download pump): the same
/// `policy::PathPolicy` whitelist the ops layer applies internally, so the
/// wiring contributes identical gate semantics. The permissive read_only /
/// allow_delete flags here are deliberate — those gates bind at the
/// connection level ([`ensure_binding_writable`] / [`ensure_binding_deletable`]).
fn rclone_gate(
    root: &str,
    lock_to_root: bool,
    path: &str,
    check: fn(&crate::policy::PathPolicy, &str) -> Result<crate::policy::ResolvedPath, String>,
) -> Result<String, String> {
    let policy = crate::policy::PathPolicy::from_parts(root, lock_to_root, false, true);
    check(&policy, path).map(|resolved| resolved.relative)
}

/// Connection-level write gate for the rclone arms, message-identical to
/// [`ensure_writable`] (bindings carry the flags instead of a connection
/// record).
fn ensure_binding_writable(binding: &rclone::registry::RemoteBinding) -> Result<(), String> {
    if binding.read_only {
        Err("Connection is read-only; write operations are rejected".to_string())
    } else {
        Ok(())
    }
}

/// Connection-level delete gate for the rclone arms, message-identical to
/// [`ensure_deletable`] (§9 stricter semantics: read_only rejects deletes).
fn ensure_binding_deletable(binding: &rclone::registry::RemoteBinding) -> Result<(), String> {
    if binding.read_only {
        Err("Connection is read-only; delete operations are rejected".to_string())
    } else if !binding.allow_delete {
        Err("Connection disallows delete operations (allow_delete=false)".to_string())
    } else {
        Ok(())
    }
}

/// Remote path → display/download file name: last non-empty path segment
/// (both separators accepted) — `transfers.rs` twin for the rclone pump.
fn remote_file_name(remote_path: &str) -> &str {
    remote_path
        .rsplit(['/', '\\'])
        .find(|segment| !segment.is_empty())
        .unwrap_or(remote_path)
}

/// Renames the finished `.part` staging file to its final collision-free
/// name after verifying the staged byte count matches `size` — byte-for-byte
/// the `JobTable::promote_staging` semantics.
fn promote_rclone_staging(staging: &std::path::Path, size: u64) -> Result<String, String> {
    let staged = std::fs::metadata(staging)
        .map_err(|error| format!("Failed to stat staging file: {error}"))?
        .len();
    if staged != size {
        return Err(format!(
            "Download ended short: {staged} of {size} bytes were received"
        ));
    }
    let base = staging.parent().unwrap_or(std::path::Path::new("."));
    let name = staging
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".part"))
        .unwrap_or("download");
    let final_path = local_downloads::pick_download_path(base, name);
    std::fs::rename(staging, &final_path)
        .map_err(|error| format!("Failed to finalize download: {error}"))?;
    Ok(final_path.to_string_lossy().to_string())
}

/// Progress payload builder mirroring `transfers.rs::emit_job_progress`
/// byte-for-byte (same event name is applied by the caller).
fn rclone_job_progress_event(job: &transfers::TransferJob) -> Value {
    let mut event =
        transfers::progress_event(&job.task_id, job.transferred_bytes, job.total_bytes);
    if let Some(object) = event.as_object_mut() {
        object.insert("size".into(), serde_json::json!(job.total_bytes));
        object.insert("state".into(), serde_json::json!(job.status.as_str()));
        object.insert(
            "kind".into(),
            serde_json::json!(match job.kind {
                transfers::TransferKind::Upload => "upload",
                transfers::TransferKind::Download => "download",
                transfers::TransferKind::ArchiveDownload => "archiveDownload",
            }),
        );
        object.insert("connectionId".into(), serde_json::json!(job.connection_id));
        object.insert("remotePath".into(), serde_json::json!(job.remote_path));
        if let Some(error) = &job.error {
            object.insert("error".into(), serde_json::json!(error));
        }
    }
    event
}

/// `state:"running"` progress payload (pump start), `emit_running` twin.
fn rclone_running_event(task_id: &str) -> Value {
    let mut event = transfers::progress_event(task_id, 0, None);
    if let Some(object) = event.as_object_mut() {
        object.insert(
            "state".into(),
            serde_json::json!(transfers::JobStatus::Running.as_str()),
        );
    }
    event
}

/// Throttled mid-flight progress payload, `emit_running_at` twin.
fn rclone_running_at_event(task_id: &str, transferred: u64, total: Option<u64>) -> Value {
    let mut event = transfers::progress_event(task_id, transferred, total);
    if let Some(object) = event.as_object_mut() {
        object.insert("size".into(), serde_json::json!(total));
        object.insert(
            "state".into(),
            serde_json::json!(transfers::JobStatus::Running.as_str()),
        );
    }
    event
}

/// Marks a queued rclone job `running` (no-op otherwise) — `mark_running` twin.
fn rclone_mark_running(rclone: &rclone::RcloneEngine, task_id: &str) {
    if let Some(job) = rclone_lock(&rclone.jobs).get_mut(task_id) {
        if job.status == transfers::JobStatus::Queued {
            job.status = transfers::JobStatus::Running;
        }
    }
}

/// Moves a single-file rclone job to a terminal state exactly once and emits
/// the final progress event — `Inner::complete_job` twin WITH the Phase D
/// history persistence: the terminal record is appended to the shared
/// transfers.json (`persist_rclone_history`), so the panel history and the
/// local reveal/open whitelist behave identically under both engines.
fn rclone_complete_job(
    rclone: &rclone::RcloneEngine,
    task_id: &str,
    status: transfers::JobStatus,
    error: Option<String>,
    emitter: &PluginEmitter,
) -> bool {
    let job = {
        let mut jobs = rclone_lock(&rclone.jobs);
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
    persist_rclone_history(rclone, &job);
    let _ = emitter.event("files/transfer/progress", rclone_job_progress_event(&job));
    true
}

// ---------------------------------------------------------------------------
// Remote-edit tasks (打开方式): the async half of `files/remote-edit/*` —
// pull → launch → watch → sync-back. Session bookkeeping lives in
// remote_edit.rs; every rclone touch here is the same byte path as the
// transfer arms (`download_to_file` pull, `upload_staged_exact` sync-back),
// and every pull/sync registers a regular TransferJob so the transfers
// panel, the persisted history and the reveal/open whitelist see it like
// any other transfer.
// ---------------------------------------------------------------------------

/// Broadcasts one `files/remote-edit/state` event for a session. The
/// workbench turns `opened`/`synced` into notices and `error` into the
/// error banner; `downloading`/`syncing`/`closed` are informational.
fn remote_edit_emit_state(
    edits: &remote_edit::EditEngine,
    emitter: &PluginEmitter,
    key: &str,
    state: &str,
    error: Option<&str>,
) {
    let Some(session) = edits.get(key) else {
        return;
    };
    let mut event = json!({
        "key": session.key,
        "connectionId": session.connection_id,
        "remotePath": session.remote_path,
        "localPath": session.local_path,
        "state": state,
    });
    if let Some(error) = error {
        event["error"] = json!(error);
    }
    let _ = emitter.event("files/remote-edit/state", event);
}

/// Drops a session and removes its temp copy (best-effort; the emptied
/// per-connection directories go too — `remove_dir` only removes empty
/// dirs, so concurrent sessions in the same tree are never harmed).
fn remote_edit_close_session(
    edits: &remote_edit::EditEngine,
    emitter: &PluginEmitter,
    key: &str,
    local: &std::path::Path,
) {
    let Some(session) = edits.remove(key) else {
        return;
    };
    let _ = std::fs::remove_file(local);
    if let Some(dir) = local.parent() {
        let _ = std::fs::remove_dir(dir);
        if let Some(base) = dir.parent() {
            let _ = std::fs::remove_dir(base);
        }
    }
    let _ = emitter.event(
        "files/remote-edit/state",
        json!({
            "key": session.key,
            "connectionId": session.connection_id,
            "remotePath": session.remote_path,
            "localPath": session.local_path,
            "state": "closed",
        }),
    );
}

/// First open of a remote file: pull it into the workspace copy, launch the
/// chosen app and hand the session to the watch loop.
#[allow(clippy::too_many_arguments)]
async fn remote_edit_open_task(
    rclone: Arc<rclone::RcloneEngine>,
    edits: remote_edit::EditEngine,
    key: String,
    connection_id: String,
    fs: String,
    remote: String,
    local: PathBuf,
    app: Option<PathBuf>,
    emitter: PluginEmitter,
) {
    // The initial pull is a regular download job (panel + history parity).
    let task_id = format!("edit-{key}");
    {
        let job = transfers::TransferJob {
            task_id: task_id.clone(),
            connection_id: connection_id.clone(),
            kind: transfers::TransferKind::Download,
            remote_path: remote.clone(),
            total_bytes: None,
            transferred_bytes: 0,
            status: transfers::JobStatus::Running,
            error: None,
            started_at: Some(store::unix_millis_now()),
            finished_at: None,
            local_path: Some(local.display().to_string()),
        };
        rclone_lock(&rclone.jobs).insert(task_id.clone(), job);
    }
    let _ = emitter.event(
        "files/transfer/progress",
        rclone_running_at_event(&task_id, 0, None),
    );
    // Transfer-path client (no wall-clock timeout), pump parity.
    let client = match rclone
        .client_for_id(&connection_id)
        .await
        .map(|client| client.transfer_client())
    {
        Ok(client) => client,
        Err(error) => {
            edits.update(&key, |session| {
                session.status = remote_edit::STATUS_ERROR;
                session.last_error = Some(error.clone());
            });
            rclone_complete_job(
                &rclone,
                &task_id,
                transfers::JobStatus::Failed,
                Some(error.clone()),
                &emitter,
            );
            remote_edit_emit_state(&edits, &emitter, &key, "error", Some(&error));
            return;
        }
    };
    match rclone::bytes_channel::download_to_file(&client, &fs, &remote, &local).await {
        Ok(size) => {
            if let Some(job) = rclone_lock(&rclone.jobs).get_mut(&task_id) {
                job.transferred_bytes = size;
                job.total_bytes = Some(size);
            }
            rclone_complete_job(
                &rclone,
                &task_id,
                transfers::JobStatus::Completed,
                None,
                &emitter,
            );
            // Baseline before launch: the watcher only treats deviations
            // from this stamp as editor saves.
            let stamp = remote_edit::snapshot_stat(&local);
            if !edits.update(&key, |session| {
                session.status = remote_edit::STATUS_WATCHING;
                session.baseline = stamp;
                session.last_error = None;
            }) {
                return; // closed while downloading; close swept the copy
            }
            if edits.get(&key).map(|session| session.closing).unwrap_or(false) {
                remote_edit_close_session(&edits, &emitter, &key, &local);
                return;
            }
            if let Err(error) = local_downloads::open_in_app(&local, app.as_deref()) {
                edits.update(&key, |session| {
                    session.status = remote_edit::STATUS_ERROR;
                    session.last_error = Some(error.clone());
                });
                remote_edit_emit_state(&edits, &emitter, &key, "error", Some(&error));
            } else {
                remote_edit_emit_state(&edits, &emitter, &key, "opened", None);
            }
            remote_edit_watch_loop(rclone, edits, client, fs, remote, local, key, emitter).await;
        }
        Err(error) => {
            edits.update(&key, |session| {
                session.status = remote_edit::STATUS_ERROR;
                session.last_error = Some(error.clone());
            });
            rclone_complete_job(
                &rclone,
                &task_id,
                transfers::JobStatus::Failed,
                Some(error.clone()),
                &emitter,
            );
            remote_edit_emit_state(&edits, &emitter, &key, "error", Some(&error));
        }
    }
}

/// Re-open of a live session: re-launch the chosen app, optionally
/// re-pulling the remote copy first — only when the remote moved ahead of
/// the synced baseline AND the local copy is still exactly that baseline.
/// An unsynced editor save is never clobbered; the watcher syncs it.
#[allow(clippy::too_many_arguments)]
async fn remote_edit_refresh_task(
    rclone: Arc<rclone::RcloneEngine>,
    edits: remote_edit::EditEngine,
    key: String,
    connection_id: String,
    fs: String,
    remote: String,
    local: PathBuf,
    app: Option<PathBuf>,
    baseline: Option<remote_edit::FileStamp>,
    emitter: PluginEmitter,
) {
    let Ok(client) = rclone
        .client_for_id(&connection_id)
        .await
        .map(|client| client.transfer_client())
    else {
        return;
    };
    let remote_size = rclone::bytes_channel::remote_size(&client, &fs, &remote)
        .await
        .ok()
        .flatten();
    let needs_refresh = match (baseline, remote_edit::snapshot_stat(&local)) {
        // Never pulled (previous open failed) or copy gone: pull fresh.
        (None, None) => true,
        (Some(base), Some(stamp)) => {
            stamp == base && remote_size.is_some_and(|size| size != base.0)
        }
        // Local edits pending (or unknown local state): never clobber.
        _ => false,
    };
    if needs_refresh {
        edits.update(&key, |session| {
            session.status = remote_edit::STATUS_DOWNLOADING;
        });
        remote_edit_emit_state(&edits, &emitter, &key, "downloading", None);
        match rclone::bytes_channel::download_to_file(&client, &fs, &remote, &local).await {
            Ok(_) => {
                let stamp = remote_edit::snapshot_stat(&local);
                if !edits.update(&key, |session| {
                    session.status = remote_edit::STATUS_WATCHING;
                    session.baseline = stamp;
                    session.last_error = None;
                }) {
                    return;
                }
            }
            Err(error) => {
                edits.update(&key, |session| {
                    session.status = remote_edit::STATUS_ERROR;
                    session.last_error = Some(error.clone());
                });
                remote_edit_emit_state(&edits, &emitter, &key, "error", Some(&error));
                return;
            }
        }
    }
    if edits.get(&key).map(|session| session.closing).unwrap_or(true) {
        remote_edit_close_session(&edits, &emitter, &key, &local);
        return;
    }
    match local_downloads::open_in_app(&local, app.as_deref()) {
        Ok(()) => remote_edit_emit_state(&edits, &emitter, &key, "opened", None),
        Err(error) => {
            edits.update(&key, |session| {
                session.status = remote_edit::STATUS_ERROR;
                session.last_error = Some(error.clone());
            });
            remote_edit_emit_state(&edits, &emitter, &key, "error", Some(&error));
        }
    }
}

/// The per-session watcher: polls the local copy every [`WATCH_POLL`] and
/// hands each tick to [`remote_edit::tick_watch`]. Sync decisions run
/// through [`remote_edit_sync_back`]; a close decision removes the session
/// and its temp copy. The loop ends when the session is gone.
#[allow(clippy::too_many_arguments)]
async fn remote_edit_watch_loop(
    rclone: Arc<rclone::RcloneEngine>,
    edits: remote_edit::EditEngine,
    client: rclone::rc::RcClient,
    fs: String,
    remote: String,
    local: PathBuf,
    key: String,
    emitter: PluginEmitter,
) {
    loop {
        tokio::time::sleep(remote_edit::WATCH_POLL).await;
        let current = remote_edit::snapshot_stat(&local);
        let Some(decision) =
            edits.map_mut(&key, |session| remote_edit::tick_watch(session, current))
        else {
            return;
        };
        match decision {
            remote_edit::WatchDecision::Continue => {}
            remote_edit::WatchDecision::Close => {
                remote_edit_close_session(&edits, &emitter, &key, &local);
                return;
            }
            remote_edit::WatchDecision::Sync => {
                remote_edit_sync_back(
                    &rclone,
                    &edits,
                    &client,
                    &fs,
                    &remote,
                    &local,
                    &key,
                    &emitter,
                )
                .await;
            }
        }
    }
}

/// Streams a settled editor save back to the exact remote path. The
/// sync-back is a regular upload job (panel + history parity). On success
/// the baseline resets to the pre-upload stamp, so a save that landed while
/// the upload streamed re-syncs on the next ticks instead of being silently
/// considered synced; on failure the session drops to `error` and
/// `tick_watch` retries after [`remote_edit::ERROR_RETRY_TICKS`].
#[allow(clippy::too_many_arguments)]
async fn remote_edit_sync_back(
    rclone: &rclone::RcloneEngine,
    edits: &remote_edit::EditEngine,
    client: &rclone::rc::RcClient,
    fs: &str,
    remote: &str,
    local: &std::path::Path,
    key: &str,
    emitter: &PluginEmitter,
) {
    let Some(session) = edits.get(key) else {
        return;
    };
    if session.closing {
        return;
    }
    let seq = session.sync_seq + 1;
    let pre_stamp = remote_edit::snapshot_stat(local);
    edits.update(key, |session| {
        session.sync_seq = seq;
        session.status = remote_edit::STATUS_SYNCING;
        session.pending_stable = 0;
    });
    let task_id = format!("{key}-sync-{seq}");
    let size = std::fs::metadata(local).map(|meta| meta.len()).unwrap_or(0);
    {
        let job = transfers::TransferJob {
            task_id: task_id.clone(),
            connection_id: session.connection_id.clone(),
            kind: transfers::TransferKind::Upload,
            remote_path: remote.to_string(),
            total_bytes: Some(size),
            transferred_bytes: 0,
            status: transfers::JobStatus::Running,
            error: None,
            started_at: Some(store::unix_millis_now()),
            finished_at: None,
            local_path: Some(local.display().to_string()),
        };
        rclone_lock(&rclone.jobs).insert(task_id.clone(), job);
    }
    let _ = emitter.event(
        "files/transfer/progress",
        rclone_running_at_event(&task_id, 0, Some(size)),
    );
    remote_edit_emit_state(edits, emitter, key, "syncing", None);
    match rclone::bytes_channel::upload_staged_exact(client, fs, local, remote, None).await {
        Ok(()) => {
            if !edits.update(key, |session| {
                session.status = remote_edit::STATUS_WATCHING;
                session.baseline = pre_stamp;
                session.last_error = None;
                session.last_sync_at = Some(store::unix_millis_now());
            }) {
                return;
            }
            if let Some(job) = rclone_lock(&rclone.jobs).get_mut(&task_id) {
                job.transferred_bytes = size;
            }
            rclone_complete_job(rclone, &task_id, transfers::JobStatus::Completed, None, emitter);
            remote_edit_emit_state(edits, emitter, key, "synced", None);
        }
        Err(error) => {
            edits.update(key, |session| {
                session.status = remote_edit::STATUS_ERROR;
                session.last_error = Some(error.clone());
            });
            rclone_complete_job(
                rclone,
                &task_id,
                transfers::JobStatus::Failed,
                Some(error.clone()),
                emitter,
            );
            remote_edit_emit_state(edits, emitter, key, "error", Some(&error));
        }
    }
}

/// Terminal-replay twin of `download_finish_result`: Completed finishes
/// idempotently with the recorded `localPath`; Failed/Canceled surface the
/// stored outcome.
fn rclone_download_finish_result(job: &transfers::TransferJob) -> Result<Option<String>, String> {
    match job.status {
        transfers::JobStatus::Completed => Ok(job.local_path.clone()),
        transfers::JobStatus::Canceled => Err("Download was canceled".to_string()),
        _ => Err(job
            .error
            .clone()
            .unwrap_or_else(|| "Download failed".to_string())),
    }
}

/// Pump-side terminal cleanup (`pump_cleanup` twin): sweep the `.part`
/// residue, settle the job state and — for cancel exits — drop the slot.
fn rclone_pump_cleanup(
    rclone: &rclone::RcloneEngine,
    task_id: &str,
    staging: &Option<PathBuf>,
    status: transfers::JobStatus,
    error: Option<String>,
    emitter: &PluginEmitter,
    clear_slot: bool,
) {
    if let Some(staging) = staging {
        let _ = std::fs::remove_file(staging);
    }
    rclone_complete_job(rclone, task_id, status, error, emitter);
    if clear_slot {
        rclone_lock(&rclone.downloads).remove(task_id);
    }
}

/// Settles `pump_done` on every pump exit path (early bails and panics
/// included) — `transfers::PumpDoneGuard` twin.
struct RclonePumpDoneGuard(Arc<AtomicBool>);

impl Drop for RclonePumpDoneGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// rclone download pump (plan §5 independent-spawn allowance): streams
/// `fs:remote` through `bytes_channel::download_pump` and re-emits each
/// ≤256 KiB slice as a `files/download/{taskId}` kind-1 frame (8-byte BE
/// offset + payload) — byte-identical to the JobTable pump's frame format.
/// Honors the cooperative cancel flag between chunks; saveToLocal runs append
/// into the start-created `.part` file and hand the terminal state to
/// `finish_rclone_download` (like the JobTable pump does).
#[allow(clippy::too_many_arguments)]
async fn rclone_download_pump(
    rclone: Arc<rclone::RcloneEngine>,
    task_id: String,
    connection_id: String,
    fs: String,
    remote: String,
    size: u64,
    staging: Option<PathBuf>,
    cancel: Arc<AtomicBool>,
    pump_done: Arc<AtomicBool>,
    emitter: PluginEmitter,
) {
    let _pump_done_guard = RclonePumpDoneGuard(pump_done.clone());
    rclone_mark_running(&rclone, &task_id);
    let _ = emitter.event("files/transfer/progress", rclone_running_event(&task_id));
    let channel = format!("files/download/{task_id}");
    if cancel.load(Ordering::Acquire) {
        rclone_pump_cleanup(
            &rclone,
            &task_id,
            &staging,
            transfers::JobStatus::Canceled,
            None,
            &emitter,
            true,
        );
        return;
    }
    // saveToLocal: the staging file was pre-created at start; an unwritable
    // file fails the job here, mirroring the JobTable pump.
    let mut staging_file = match staging.as_deref() {
        Some(path) => match std::fs::OpenOptions::new().append(true).open(path) {
            Ok(file) => Some(file),
            Err(error) => {
                rclone_pump_cleanup(
                    &rclone,
                    &task_id,
                    &staging,
                    transfers::JobStatus::Failed,
                    Some(format!("Failed to open staging file: {error}")),
                    &emitter,
                    false,
                );
                return;
            }
        },
        None => None,
    };
    // Transfer-path client (plan finding #11): no wall-clock timeout, the
    // pump streams the whole object through one GET body. Routed to the
    // owning connection's proxy group.
    let client = match rclone
        .client_for_id(&connection_id)
        .await
        .map(|client| client.transfer_client())
    {
        Ok(client) => client,
        Err(error) => {
            rclone_pump_cleanup(
                &rclone,
                &task_id,
                &staging,
                transfers::JobStatus::Failed,
                Some(error),
                &emitter,
                false,
            );
            return;
        }
    };
    let mut offset = 0u64;
    let mut throttle = transfers::Throttle::default();
    let jobs = &rclone.jobs;
    let mut sink = |chunk: &[u8]| -> Result<(), String> {
        if cancel.load(Ordering::Acquire) {
            return Err("download canceled".to_string());
        }
        if let Some(file) = staging_file.as_mut() {
            use std::io::Write;
            file.write_all(chunk)
                .map_err(|error| format!("Failed to write staging file: {error}"))?;
        }
        // Kind-1 download frame: 8-byte BE offset + payload chunk
        // (≤256 KiB).
        let mut payload = Vec::with_capacity(8 + chunk.len());
        payload.extend_from_slice(&(offset as u64).to_be_bytes());
        payload.extend_from_slice(chunk);
        emitter
            .binary(&channel, &payload)
            .map_err(|error| format!("Failed to push download frame: {error:?}"))?;
        offset = offset.saturating_add(chunk.len() as u64);
        if let Some(job) = rclone_lock(jobs).get_mut(&task_id) {
            job.transferred_bytes = offset;
        }
        if throttle.should_emit(offset, Some(size)) {
            let _ = emitter.event(
                "files/transfer/progress",
                rclone_running_at_event(&task_id, offset, Some(size)),
            );
        }
        Ok(())
    };
    let outcome = rclone::bytes_channel::download_pump(&client, &fs, &remote, &mut sink).await;
    match outcome {
        Ok(_) => {
            if cancel.load(Ordering::Acquire) {
                rclone_pump_cleanup(
                    &rclone,
                    &task_id,
                    &staging,
                    transfers::JobStatus::Canceled,
                    None,
                    &emitter,
                    true,
                );
                return;
            }
            if staging.is_some() {
                // saveToLocal: the terminal state belongs to finish (the job
                // stays running until finish confirms). Close the staging
                // handle first — finish renames the `.part` once it observes
                // pump_done, and Windows can neither rename nor delete an
                // open file — then settle pump_done under the downloads lock
                // racing cancel's "pump already exited" check. This is the
                // JobTable pump's exact handover dance.
                drop(staging_file.take());
                let clean_up = {
                    let mut downloads = rclone_lock(&rclone.downloads);
                    let cancel_seen = cancel.load(Ordering::Acquire);
                    let job_terminal = rclone_lock(&rclone.jobs)
                        .get(&task_id)
                        .map(|job| job.status.is_terminal())
                        .unwrap_or(false);
                    if cancel_seen || job_terminal {
                        downloads.remove(&task_id);
                        true
                    } else {
                        pump_done.store(true, Ordering::Release);
                        false
                    }
                };
                if clean_up {
                    rclone_pump_cleanup(
                        &rclone,
                        &task_id,
                        &staging,
                        transfers::JobStatus::Canceled,
                        None,
                        &emitter,
                        false,
                    );
                }
                return;
            }
            // Channel download: complete immediately; the slot stays until
            // finish removes it (JobTable pump parity — a late cancel still
            // finds the slot and answers Ok).
            rclone_complete_job(
                &rclone,
                &task_id,
                transfers::JobStatus::Completed,
                None,
                &emitter,
            );
        }
        Err(error) => {
            let canceled = cancel.load(Ordering::Acquire);
            let (status, detail) = if canceled {
                (transfers::JobStatus::Canceled, None)
            } else {
                (transfers::JobStatus::Failed, Some(error))
            };
            rclone_pump_cleanup(
                &rclone,
                &task_id,
                &staging,
                status,
                detail,
                &emitter,
                false,
            );
        }
    }
}

/// 归档临时目录清扫护栏（files/archiveDownload 专属）：泵的任何退出路径
/// （成功/失败/取消/panic 展开）都删除整个临时目录 ——
/// `rclone_pump_cleanup` 对 `.part` 残留的清扫孪生，drop 即执行。
struct TempArchiveGuard(Option<std::path::PathBuf>);

impl TempArchiveGuard {
    /// 认领 `dir`（staging 文件的父目录）；drop 时整目录递归删除。
    fn claim(dir: std::path::PathBuf) -> Self {
        Self(Some(dir))
    }
}

impl Drop for TempArchiveGuard {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// 归档下载泵（files/archiveDownload 专属）：把 sidecar 临时目录里的
/// `<dirname>.zip` 按 ≤256 KiB 切片推成 `files/download/{taskId}` kind-1
/// 帧（8 字节 BE offset + 载荷）。帧格式、进度节流、协作取消与终态清理
/// 与 `rclone_download_pump` 完全一致，只是字节源从 rc-serve GET 换成
/// 本地 staging 文件（归档已在 start 内同步产出，泵不做远端 I/O）。
async fn rclone_archive_download_pump(
    rclone: Arc<rclone::RcloneEngine>,
    task_id: String,
    archive_path: std::path::PathBuf,
    size: u64,
    cancel: Arc<AtomicBool>,
    pump_done: Arc<AtomicBool>,
    emitter: PluginEmitter,
) {
    let _pump_done_guard = RclonePumpDoneGuard(pump_done.clone());
    let _archive_guard = TempArchiveGuard::claim(
        archive_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| archive_path.clone()),
    );
    rclone_mark_running(&rclone, &task_id);
    let _ = emitter.event("files/transfer/progress", rclone_running_event(&task_id));
    let channel = format!("files/download/{task_id}");
    let canceled = |cancel: &AtomicBool| cancel.load(Ordering::Acquire);
    if canceled(&cancel) {
        rclone_pump_cleanup(
            &rclone,
            &task_id,
            &None,
            transfers::JobStatus::Canceled,
            None,
            &emitter,
            true,
        );
        return;
    }
    let mut file = match std::fs::File::open(&archive_path) {
        Ok(file) => file,
        Err(error) => {
            rclone_pump_cleanup(
                &rclone,
                &task_id,
                &None,
                transfers::JobStatus::Failed,
                Some(format!("Failed to open staged archive: {error}")),
                &emitter,
                false,
            );
            return;
        }
    };
    let mut offset = 0u64;
    let mut throttle = transfers::Throttle::default();
    let jobs = &rclone.jobs;
    let mut buffer = vec![0u8; crate::model::TRANSFER_CHUNK_SIZE];
    use std::io::Read as _;
    loop {
        if canceled(&cancel) {
            rclone_pump_cleanup(
                &rclone,
                &task_id,
                &None,
                transfers::JobStatus::Canceled,
                None,
                &emitter,
                false,
            );
            return;
        }
        let read = match file.read(&mut buffer) {
            Ok(0) => break, // EOF：staging 字节已全部出泵
            Ok(read) => read,
            Err(error) => {
                rclone_pump_cleanup(
                    &rclone,
                    &task_id,
                    &None,
                    transfers::JobStatus::Failed,
                    Some(format!("Failed to read staged archive: {error}")),
                    &emitter,
                    false,
                );
                return;
            }
        };
        // Kind-1 download frame: 8-byte BE offset + payload chunk
        // (≤256 KiB) — byte-identical to rclone_download_pump.
        let mut payload = Vec::with_capacity(8 + read);
        payload.extend_from_slice(&offset.to_be_bytes());
        payload.extend_from_slice(&buffer[..read]);
        if let Err(error) = emitter.binary(&channel, &payload) {
            rclone_pump_cleanup(
                &rclone,
                &task_id,
                &None,
                transfers::JobStatus::Failed,
                Some(format!("Failed to push download frame: {error:?}")),
                &emitter,
                false,
            );
            return;
        }
        offset = offset.saturating_add(read as u64);
        if let Some(job) = rclone_lock(jobs).get_mut(&task_id) {
            job.transferred_bytes = offset;
        }
        if throttle.should_emit(offset, Some(size)) {
            let _ = emitter.event(
                "files/transfer/progress",
                rclone_running_at_event(&task_id, offset, Some(size)),
            );
        }
    }
    if canceled(&cancel) {
        rclone_pump_cleanup(
            &rclone,
            &task_id,
            &None,
            transfers::JobStatus::Canceled,
            None,
            &emitter,
            false,
        );
        return;
    }
    // Channel download: complete immediately; the slot stays until finish
    // removes it (rclone_download_pump parity — a late cancel still finds
    // the slot and answers Ok).
    rclone_complete_job(
        &rclone,
        &task_id,
        transfers::JobStatus::Completed,
        None,
        &emitter,
    );
}

impl PluginHandler for Plugin {
    fn handle(
        &self,
        _context: RequestContext,
        method: &str,
        params: Value,
        emitter: &PluginEmitter,
    ) -> Result<Value, PluginError> {
        // Panic safety net: any engine/transfer panic must never kill the
        // request task silently (the host would then hang waiting for a
        // response frame that is never sent). Convert the panic into a
        // business error labeled as an internal sidecar fault — the message
        // keeps the method name so host-side logs can attribute the crash
        // (audit #11: the old text misleadingly claimed "Method not found").
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.handle_request(method, params, emitter)
        }));
        match outcome {
            Ok(result) => result.map_err(to_plugin_error),
            Err(panic) => Err(to_plugin_error(format!(
                "internal error (sidecar panic) while handling {method}: {}",
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
            // The rclone staging table membership IS the engine-selection
            // check (there is no second engine); an unknown id is the same
            // "Upload task was not found" error the retired JobTable path
            // answered with.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.append_rclone_upload(task_id, &data, emitter)
            }));
            return match outcome {
                Ok(result) => result.map_err(to_plugin_error),
                Err(panic) => Err(to_plugin_error(format!(
                    "internal error (sidecar panic) while appending upload frame for task '{task_id}': {}",
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

fn to_plugin_error(error: String) -> PluginError {
    PluginError::new(-32000, error)
}

/// Best-effort creation of the plugin data dir. Non-fatal by design
/// (issue #16): a data dir that cannot be created — a docker mount shadowing
/// the path, a read-only home, a non-root container user — used to kill the
/// sidecar before `serve()`, which the host only saw as `Broken pipe` on the
/// first framed write. The sidecar now stays alive and every persisting call
/// (prefs/transfers/audit writes) fails per call with a structured error.
fn init_data_dir(data_dir: &std::path::Path) {
    if let Err(error) = std::fs::create_dir_all(data_dir) {
        eprintln!(
            "[io.dbx.files] warning: cannot create data dir {}: {error}; \
             prefs/transfer/audit persistence will fail until it is writable",
            data_dir.display()
        );
    }
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
    // Startup diagnostics (issue #16): everything below travels on stderr —
    // stdout is the framed protocol — so users can paste the plugin log when
    // reporting a failed initialization.
    eprintln!(
        "[io.dbx.files] plugin starting: version {} (framed stdio mode)",
        env!("CARGO_PKG_VERSION")
    );
    eprintln!("[io.dbx.files] data dir: {}", Store::default_dir().display());
    eprintln!("[io.dbx.files] {}", rclone::proc::startup_diagnostic());
    let plugin = match Plugin::new() {
        Ok(plugin) => plugin,
        Err(error) => {
            // The only surviving failure is the async runtime itself; make
            // the reason visible before the process goes away.
            eprintln!("[io.dbx.files] initialization failed: {error}");
            return Err(std::io::Error::other(error));
        }
    };
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

    /// Issue #16 regression: `init_data_dir` must never panic and must never
    /// make startup fatal. A path whose parent is a regular file cannot be
    /// created (the docker-mount scenario that used to kill the sidecar
    /// before `serve()`, surfacing to the host as `Broken pipe`); a healthy
    /// path is still created.
    #[test]
    fn init_data_dir_degrades_instead_of_dying() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        init_data_dir(&blocker.join("data")); // must not panic
        assert!(!blocker.join("data").exists(), "no dir can appear under a file");

        let healthy = dir.path().join("healthy").join("data");
        init_data_dir(&healthy);
        assert!(healthy.is_dir());
    }

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
        use crate::policy::PathPolicy;
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

    // -- Phase D rclone transfers-history persistence -------------------------

    #[test]
    fn rclone_history_persists_and_hydrates_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::new(dir.path().to_path_buf()));
        let rclone = rclone::RcloneEngine::new();
        *rclone_lock(&rclone.history) = Some(Arc::clone(&store));

        let job = transfers::TransferJob {
            task_id: "hist-1".into(),
            connection_id: "c1".into(),
            kind: transfers::TransferKind::Download,
            remote_path: "/big.bin".into(),
            total_bytes: Some(11),
            transferred_bytes: 11,
            status: transfers::JobStatus::Completed,
            error: None,
            started_at: Some(1_700_000_000_000),
            finished_at: Some(1_700_000_000_001),
            local_path: Some("/downloads/big.bin".into()),
        };
        persist_rclone_history(&rclone, &job);
        // Non-terminal records are never persisted.
        let mut queued = job.clone();
        queued.task_id = "hist-2".into();
        queued.status = transfers::JobStatus::Queued;
        persist_rclone_history(&rclone, &queued);

        let mut records = store.load_transfers();
        assert_eq!(records.len(), 1, "only terminal records persist");
        assert_eq!(records[0].task_id, "hist-1");
        assert_eq!(records[0].kind, "download");
        assert_eq!(records[0].status, "completed");
        assert_eq!(records[0].local_path.as_deref(), Some("/downloads/big.bin"));

        // Hydration twin: the record maps back onto a terminal mirror job.
        let hydrated = rclone_job_from_record(records.remove(0));
        assert_eq!(hydrated.status, transfers::JobStatus::Completed);
        assert_eq!(hydrated.kind, transfers::TransferKind::Download);
        assert_eq!(hydrated.total_bytes, Some(11));
    }

    #[test]
    fn bisync_session_name_matches_rclone_sanitization() {
        // Live-verified v1.75.1 session names for these shapes.
        assert_eq!(
            bisync_session_name("/tmp/rc-bisync/p1", "/tmp/rc-bisync/p2"),
            "tmp_rc-bisync_p1..tmp_rc-bisync_p2"
        );
        assert_eq!(bisync_session_name("bx:p1", "bx:p2"), "bx_p1..bx_p2");
    }

    #[test]
    fn check_summary_counts_every_difference_class() {
        let report = json!({
            "missingOnSrc": ["c.txt"],
            "missingOnDst": ["b.txt"],
            "differ": ["d.txt"],
            "error": []
        });
        let summary = check_summary_from(&report);
        assert!(summary.contains("3 differences"), "{summary}");
        assert!(summary.contains("missing on source: 1"), "{summary}");
        assert_eq!(check_summary_from(&json!({ "missingOnSrc": [], "missingOnDst": [], "differ": [], "error": [] })), "identical");
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

    // -- files/archiveDownload ------------------------------------------------

    #[test]
    fn temp_archive_guard_cleans_staging_on_any_drop() {
        // 泵失败/取消/panic 展开的共同兜底：整目录（含 .zip）必须消失。
        let dir = tempfile::tempdir().unwrap();
        let stage_dir = dir.path().join("dbx-files-archivedl-x");
        std::fs::create_dir_all(&stage_dir).unwrap();
        let archive = stage_dir.join("photos.zip");
        std::fs::write(&archive, b"PK").unwrap();

        let guard = TempArchiveGuard::claim(stage_dir.clone());
        drop(guard);
        assert!(!stage_dir.exists(), "drop 必须删除整个临时目录");
        assert!(!archive.exists());

        // 正常完成路径 take 之后手动清理同样幂等：目录不存在时不报错。
        let guard = TempArchiveGuard::claim(dir.path().join("missing"));
        drop(guard);
        assert!(!dir.path().join("missing").exists());
    }

    #[test]
    fn archive_download_job_persists_as_plain_download() {
        // TransferRecord.kind 契约（upload|download）不被归档下载打破。
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::new(dir.path().to_path_buf()));
        let rclone = rclone::RcloneEngine::new();
        *rclone_lock(&rclone.history) = Some(Arc::clone(&store));

        let job = transfers::TransferJob {
            task_id: "arch-1".into(),
            connection_id: "c1".into(),
            kind: transfers::TransferKind::ArchiveDownload,
            remote_path: "/photos".into(),
            total_bytes: Some(10),
            transferred_bytes: 10,
            status: transfers::JobStatus::Completed,
            error: None,
            started_at: Some(1_700_000_000_000),
            finished_at: Some(1_700_000_000_001),
            local_path: None,
        };
        persist_rclone_history(&rclone, &job);
        let record = store
            .load_transfers()
            .into_iter()
            .find(|record| record.task_id == "arch-1")
            .expect("terminal archive job persisted");
        assert_eq!(record.kind, "download", "历史仍按普通下载记录");
        // 活跃镜像仍是 archiveDownload（事件/列表的区分 kind）。
        assert_eq!(rclone_job_progress_event(&job)["kind"], "archiveDownload");
    }
}
