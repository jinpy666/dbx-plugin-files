//! Storage operations (browse/metadata/structure) — F-B implementation.
//!
//! Semantics come from the implementation doc §5.2/§8.1/§8.2/§9:
//! - paths arrive from `main.rs` as OpenDAL-relative paths (the Operator root
//!   already confines physical access); [`normalize_path`] / [`Gate`] provide
//!   the shared whitelist normalization + gate helpers for the consolidated
//!   call-site wiring (and are unit-tested here);
//! - the caller (main.rs) enforces `read_only` / `allow_delete` before every
//!   mutating op with the unified stricter §9 semantics (`read_only` rejects
//!   deletes too; `Gate::ensure_deletable_path` mirrors it); the frozen op
//!   signatures carry no `Gate`, so the ops layer re-checks only what it can
//!   derive from the Operator itself (purge root guard, capability probes);
//! - OpenDAL 0.59 notes: recursive delete is `delete_with(path).recursive(true)`
//!   (`remove_all` is deprecated), `create_dir` needs a trailing `/`,
//!   capability probing uses `op.info().capability()`, and listing uses
//!   `list_with(path).recursive(true)` (the `Lister` stream needs a
//!   `futures`/`Stream` dependency that is out of scope — `list_with`
//!   internally pages through the same backend cursors).
//!
//! Errors are returned as `String` and mapped to business error -32000 by
//! `main.rs::to_plugin_error`; messages always embed the backend's original
//! error text and never echo credentials.

#![allow(dead_code)]

use std::time::Duration;

use opendal::Operator;
use serde_json::{json, Value};

use crate::model::{Capabilities, FileEntry, StoredConnection, MAX_INLINE_WRITE_BYTES, MAX_PREVIEW_BYTES};

// The §9 policy layer ships as its own file but is wired into the crate from
// here (main.rs is frozen), so path whitelist / gate logic and the ops layer
// share one implementation. Path is relative to this file's directory.
#[path = "../policy.rs"]
pub mod policy;

/// Inline streaming chunk used by the degraded read→write copy transport
/// (4 MiB, matching the upload writer chunking in `engine::transfer`).
const INLINE_COPY_CHUNK: u64 = 4 * 1024 * 1024;

/// The gating context every mutating call needs. Produced by the policy layer
/// from `StoredConnection` + request path; ops functions must never re-derive
/// credentials.
#[derive(Debug, Clone)]
pub struct Gate {
    /// Reject all writes when set (`read_only`).
    pub read_only: bool,
    /// Reject delete/purge when unset (`allow_delete`).
    pub allow_delete: bool,
    /// Connection root; request paths are normalized against it.
    pub root: String,
    /// Hard-fail paths escaping `root` (`lock_to_root`).
    pub lock_to_root: bool,
}

impl Gate {
    /// Builds the gate from a stored connection record (single source of the
    /// field mapping for the future consolidated call-site wiring).
    pub fn from_connection(connection: &crate::model::StoredConnection) -> Self {
        Self {
            read_only: connection.read_only,
            allow_delete: connection.allow_delete,
            root: connection.root.clone(),
            lock_to_root: connection.lock_to_root,
        }
    }

    /// The §9 policy view of this gate (path whitelist + write/delete checks).
    fn policy(&self) -> policy::PathPolicy {
        policy::PathPolicy::from_parts(
            &self.root,
            self.lock_to_root,
            self.read_only,
            self.allow_delete,
        )
    }

    /// Path whitelist + `read_only` gate (write/mkdir/copy-target calls).
    pub fn ensure_writable_path(&self, path: &str, is_dir: bool) -> Result<String, String> {
        let resolved = self.policy().check_write(path)?;
        Ok(render_relative(&resolved.relative, is_dir))
    }

    /// Path whitelist + delete gate (`delete`/`rmdir`/`purge` calls).
    ///
    /// Unified §9 semantics (stricter, X-A consolidation — F-B handover item
    /// ①): `read_only` rejects every mutating operation — deletes included —
    /// and `allow_delete` independently rejects delete-class ops. Same rule
    /// as `policy::PathPolicy::check_delete` and the consolidated `main.rs`
    /// gates.
    pub fn ensure_deletable_path(&self, path: &str, is_dir: bool) -> Result<String, String> {
        let resolved = self.policy().check_delete(path)?;
        Ok(render_relative(&resolved.relative, is_dir))
    }

    /// Whitelist-only check for read paths.
    pub fn readable_path(&self, path: &str, is_dir: bool) -> Result<String, String> {
        let resolved = self.policy().check_read(path)?;
        Ok(render_relative(&resolved.relative, is_dir))
    }
}

/// Renders a `ResolvedPath.relative` into an OpenDAL path, guaranteeing the
/// trailing `/` for directory semantics (`""` renders as `/` for dirs).
fn render_relative(relative: &str, is_dir: bool) -> String {
    if is_dir {
        if relative.is_empty() {
            "/".to_string()
        } else {
            format!("{relative}/")
        }
    } else {
        relative.to_string()
    }
}

/// `files/list` (§8.1): `path`, `recurse?` → `{entries:[FileEntry]}`.
///
/// Listing goes through `list_with(path).recursive(true)` for the recursive
/// case; the directory-marker entry OpenDAL returns for the listed prefix
/// itself is filtered out. Entries are sorted by path for stable pagination.
pub async fn list(
    operator: &Operator,
    path: &str,
    recurse: bool,
) -> Result<Vec<FileEntry>, String> {
    let entries = list_raw(operator, path, recurse).await?;
    Ok(entries.iter().map(entry_from_opendal).collect())
}

/// `files/listPaged` (§8.1): `path`, `page`, `pageSize` → `{entries, total}`.
///
/// Pagination is an in-memory slice over the same listing as [`list`]
/// (`start_after` is backend-dependent and not used for paging). `page` is
/// 1-based; an out-of-range page returns an empty `entries` with the real
/// `total`.
pub async fn list_paged(
    operator: &Operator,
    path: &str,
    page: u64,
    page_size: u64,
) -> Result<(Vec<FileEntry>, u64), String> {
    let entries = list_raw(operator, path, false).await?;
    let total = entries.len() as u64;
    let start = page
        .saturating_sub(1)
        .saturating_mul(page_size.max(1))
        as usize;
    let slice: Vec<FileEntry> = if start >= entries.len() {
        Vec::new()
    } else {
        let end = (start + page_size.max(1) as usize).min(entries.len());
        entries[start..end].iter().map(entry_from_opendal).collect()
    };
    Ok((slice, total))
}

/// `files/stat` (§8.1): `path` → `{entry}`.
///
/// Uses `op.stat(path)`; directories are detected via
/// `Metadata::mode().is_dir()`. Returns `FileEntry` with `size`/`modifiedAt`
/// when the backend reports them.
pub async fn stat(operator: &Operator, path: &str) -> Result<FileEntry, String> {
    let metadata = stat_flex(operator, path).await?;
    Ok(file_entry_from_path(path, &metadata))
}

/// `files/quickPaths` (§8.1): no params → `{paths: [{key, path}]}`.
///
/// Workbench quick-jump chips (tiny-rdm parity). The well-known user
/// directories (home/Desktop/Downloads/Documents/Pictures) are only exposed
/// for the `fs` protocol, and only when nothing confines the Operator root
/// (`lock_to_root` off, root `/` — the fs service requires a root, so
/// "unconfined" means the whole disk) — otherwise absolute user paths
/// would resolve inside the configured root and mislead navigation. Each
/// candidate is verified with a directory stat so a chip never navigates to a
/// missing path. Every other protocol returns the root chip only.
pub async fn quick_paths(operator: &Operator, connection: &StoredConnection) -> Result<Value, String> {
    quick_paths_with_home(operator, connection, fs_home_dir().as_deref()).await
}

/// Testable core of [`quick_paths`]: `home` is injected so the stat-verified
/// chip filtering can be exercised against a synthetic tree.
async fn quick_paths_with_home(
    operator: &Operator,
    connection: &StoredConnection,
    home: Option<&str>,
) -> Result<Value, String> {
    let mut paths = vec![json!({ "key": "root", "path": "/" })];
    if !quick_paths_eligible(connection) {
        return Ok(json!({ "paths": paths }));
    }
    let Some(home) = home else {
        return Ok(json!({ "paths": paths }));
    };
    for (key, path) in fs_quick_path_candidates(home) {
        if is_dir_path(operator, &path).await.unwrap_or(false) {
            paths.push(json!({ "key": key, "path": path }));
        }
    }
    Ok(json!({ "paths": paths }))
}

/// Eligibility gate for the user-directory chips (root-confined or non-fs
/// connections always fall back to the root chip).
fn quick_paths_eligible(connection: &StoredConnection) -> bool {
    connection.protocol == "fs"
        && !connection.lock_to_root
        && (connection.root.is_empty() || connection.root == "/")
}

/// `$HOME` (Unix) with a `USERPROFILE` fallback; `None` keeps the chips at the
/// root-only fallback.
fn fs_home_dir() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .filter(|value| value.starts_with('/'))
        .or_else(|| std::env::var("USERPROFILE").ok().filter(|value| !value.is_empty()))
}

/// Well-known user directories under `$HOME` (Unix convention; Windows
/// localized names fall back to the root chip until dedicated semantics land).
fn fs_quick_path_candidates(home: &str) -> Vec<(&'static str, String)> {
    vec![
        ("home", home.to_string()),
        ("desktop", format!("{home}/Desktop")),
        ("downloads", format!("{home}/Downloads")),
        ("documents", format!("{home}/Documents")),
        ("pictures", format!("{home}/Pictures")),
    ]
}

/// `files/capabilities` (§8.1): no params → `{scheme, list, write, read, stat,
/// delete, createDir, copy, rename, presign}`.
///
/// Projects `op.info().capability()` (0.59: `Capability` has `bool`
/// fields per op). The scheme string comes from `op.info().scheme()`.
///
/// Note: `main.rs` currently carries an inline copy of this projection (the
/// file is frozen for F-B); this function is the single-logic version meant to
/// replace the inline body during the call-site consolidation.
pub fn capabilities(operator: &Operator) -> Result<Capabilities, String> {
    let info = operator.info();
    let capability = info.capability();
    Ok(Capabilities {
        scheme: info.scheme().to_string(),
        list: capability.list,
        write: capability.write,
        read: capability.read,
        stat: capability.stat,
        delete: capability.delete,
        create_dir: capability.create_dir,
        copy: capability.copy,
        rename: capability.rename,
        presign: capability.presign_read,
    })
}

/// `files/size` (§8.1): `path` → `{count, bytes}`.
///
/// Aggregates over a recursive listing: `count` = files only (dirs excluded),
/// `bytes` = sum of file `content_length`.
pub async fn size(operator: &Operator, path: &str) -> Result<(u64, u64), String> {
    let entries = list_raw(operator, path, true).await?;
    let mut count = 0u64;
    let mut bytes = 0u64;
    for entry in &entries {
        if entry.metadata().mode().is_file() {
            count += 1;
            bytes += entry.metadata().content_length();
        }
    }
    Ok((count, bytes))
}

/// `files/publicLink` (§8.1): `path`, `expireSecs?` → `{url}`.
///
/// Uses `op.presign_read(path, expire)`; only signature-capable backends
/// (s3/gcs/azblob/oss) support it — any other backend returns a business
/// error ("backend does not support presigned links"), NOT a panic.
pub async fn public_link(
    operator: &Operator,
    path: &str,
    expire_secs: u64,
) -> Result<String, String> {
    let info = operator.info();
    if !info.capability().presign_read {
        return Err(format!(
            "Backend '{}' does not support presigned public links (presign is only \
             available on signature-capable backends such as s3/gcs/azblob/oss)",
            info.scheme()
        ));
    }
    let request = operator
        .presign_read(path, Duration::from_secs(expire_secs.max(1)))
        .await
        .map_err(|error| format!("Failed to presign '{path}': {error}"))?;
    Ok(request.uri().to_string())
}

/// `files/read` (§8.2): `path`, `maxBytes?` → `{dataBase64, truncated}`.
///
/// Preview use only: reads at most `max_bytes` (hard cap
/// `model::MAX_PREVIEW_BYTES` = 2 MiB). Small files go through `op.read`; for
/// larger files a ranged reader fetches exactly the preview window.
/// `truncated` is true when the file is larger than the returned payload.
pub async fn read(
    operator: &Operator,
    path: &str,
    max_bytes: usize,
) -> Result<(Vec<u8>, bool), String> {
    let max_bytes = max_bytes.clamp(1, MAX_PREVIEW_BYTES);
    let metadata = stat_flex(operator, path).await?;
    if metadata.mode().is_dir() {
        return Err(format!("Cannot read '{path}': it is a directory"));
    }
    let size = metadata.content_length();
    if size == 0 {
        return Ok((Vec::new(), false));
    }
    if size <= max_bytes as u64 {
        let buffer = operator
            .read(path)
            .await
            .map_err(|error| format!("Failed to read '{path}': {error}"))?;
        Ok((buffer.to_vec(), false))
    } else {
        let reader = operator
            .reader(path)
            .await
            .map_err(|error| format!("Failed to open reader for '{path}': {error}"))?;
        let buffer = reader
            .read(0..max_bytes as u64)
            .await
            .map_err(|error| format!("Failed to read preview of '{path}': {error}"))?;
        Ok((buffer.to_vec(), true))
    }
}

/// `files/write` (§8.2): `path`, `dataBase64` → `{success}`.
///
/// Overwrites small files: decoded payload must be <= `MAX_INLINE_WRITE_BYTES`
/// (4 MiB) or the call fails with a message directing the caller to the
/// upload channel. Caller has already checked the `read_only` gate.
pub async fn write(operator: &Operator, path: &str, data: Vec<u8>) -> Result<(), String> {
    if data.len() > MAX_INLINE_WRITE_BYTES {
        return Err(format!(
            "Inline write payload of {} bytes exceeds {MAX_INLINE_WRITE_BYTES}; \
             use the upload channel",
            data.len()
        ));
    }
    operator
        .write(path, data)
        .await
        .map(|_| ())
        .map_err(|error| format!("Failed to write '{path}': {error}"))
}

/// `files/mkdir` (§8.2): `path` → `{success}`.
///
/// `op.create_dir` with mkdir -p semantics; the path must end with `/`
/// (OpenDAL 0.59 NotADirectory trap) — normalized here, not at call sites.
pub async fn mkdir(operator: &Operator, path: &str) -> Result<(), String> {
    let trimmed = path.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Err("Cannot create the connection root directory".to_string());
    }
    operator
        .create_dir(&format!("{trimmed}/"))
        .await
        .map_err(|error| format!("Failed to create directory '{path}': {error}"))
}

/// `files/rmdir` (§8.2): `path` → `{success}`.
///
/// Removes an EMPTY directory only: `stat` first and refuse when the dir has
/// children (OpenDAL's `delete` on a dir is silent even when non-empty).
pub async fn rmdir(operator: &Operator, path: &str) -> Result<(), String> {
    let trimmed = path.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Err("Cannot remove the connection root directory; purge a \
                    subdirectory instead"
            .to_string());
    }
    let metadata = stat_flex(operator, path).await?;
    if !metadata.mode().is_dir() {
        return Err(format!("Cannot rmdir '{path}': it is not a directory"));
    }
    let children = list_raw(operator, &trimmed, false).await?;
    if !children.is_empty() {
        return Err(format!(
            "Directory '{path}' is not empty; use purge to delete it recursively"
        ));
    }
    operator
        .delete(&format!("{trimmed}/"))
        .await
        .map_err(|error| format!("Failed to remove directory '{path}': {error}"))
}

/// `files/delete` (§8.2): `path` → `{success}`.
///
/// Idempotent single-object delete via `op.delete(path)` (OpenDAL delete is
/// idempotent by semantics). `allow_delete` gate checked by the caller.
pub async fn delete(operator: &Operator, path: &str) -> Result<(), String> {
    operator
        .delete(path)
        .await
        .map_err(|error| format!("Failed to delete '{path}': {error}"))
}

/// `files/purge` (§8.2): `path` → `{success}`.
///
/// Recursive delete via `op.delete_with(path).recursive(true)`. MUST refuse
/// when the normalized path equals the connection root or `/` (root purge
/// guard) — the caller also enforces this in policy; defense in depth.
pub async fn purge(operator: &Operator, path: &str) -> Result<(), String> {
    let trimmed = path.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Err(
            "Purge of the connection root '/' is refused; purge a subdirectory instead"
                .to_string(),
        );
    }
    // Directories are purged through their trailing-slash spelling so prefix-
    // based backends (memory, s3-like) also drop the directory marker itself.
    let target = if is_dir_path(operator, path).await.unwrap_or(false) {
        format!("{trimmed}/")
    } else {
        trimmed.to_string()
    };
    operator
        .delete_with(&target)
        .recursive(true)
        .await
        .map_err(|error| format!("Failed to purge '{path}': {error}"))
}

/// Cross-path copy/move result: reports which transport was used so the
/// frontend can render a job entry when the op degraded to a read→write job.
#[derive(Debug, Clone)]
pub struct CopyOutcome {
    pub transport: CopyTransport,
    /// Set only when `transport == Job` (the transfer job id to poll).
    pub job_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyTransport {
    /// Native server-side `op.copy` / `op.rename` (same Operator instance or
    /// a config-equivalent backend, capability supported).
    Native,
    /// Degraded read→write loop executed as a transfer job.
    Job,
}

/// `files/copy` (§8.2): `sourceConnectionId?`, `sourcePath`,
/// `targetConnectionId?`, `targetPath` → `{success, transport, jobId?}`.
///
/// Decision tree: the engine's [`BackendIdentity`] verdict yields a shared
/// namespace spelling ([`native_route`]) + `full_capability().copy` on the
/// executing operator → native `op.copy`; otherwise degrade to a streaming
/// read→write copy executed inline and reported as `transport: "job"`.
///
/// Note on the degraded transport: the frozen op signatures carry no
/// `JobTable`/`PluginEmitter`, so the transfer layer's registry is unreachable
/// from here and `job_id` stays `None`. Since the X-A consolidation the
/// degraded path is routed by main.rs to `JobTable::enqueue_copy_job` (real
/// async jobId); the inline fallback below remains for `files/rename` and as
/// the single-logic reference implementation.
pub async fn copy(
    source: &Operator,
    target: &Operator,
    source_path: &str,
    target_path: &str,
    identity: BackendIdentity,
) -> Result<CopyOutcome, String> {
    if let Some(route) = native_route(source, target, source_path, target_path, identity) {
        if route_executor(&route, source, target).info().capability().copy {
            execute_native_copy(source, target, source_path, target_path, route).await?;
            return Ok(CopyOutcome {
                transport: CopyTransport::Native,
                job_id: None,
            });
        }
    }
    inline_copy(source, target, source_path, target_path).await?;
    Ok(CopyOutcome {
        transport: CopyTransport::Job,
        job_id: None,
    })
}

/// Runs one server-side copy/rename along a [`NativeRoute`]. Legacy
/// ([`NativeRoute::SameNamespace`]) behavior is untouched; a translated route
/// additionally best-effort creates the parent of the translated endpoint on
/// the executor (the same physical directory the stream transport would have
/// created via the other operator's namespace) so filesystem-class backends
/// don't fail on a missing parent — the copy itself still surfaces any real
/// error.
async fn execute_native_copy(
    source: &Operator,
    target: &Operator,
    source_path: &str,
    target_path: &str,
    route: NativeRoute,
) -> Result<(), String> {
    match route {
        NativeRoute::SameNamespace => source
            .copy(source_path, target_path)
            .await
            .map(|_| ())
            .map_err(|error| {
                format!("Failed to copy '{source_path}' to '{target_path}': {error}")
            }),
        NativeRoute::TranslatedSource { to } => {
            ensure_parent_best_effort(source, &to).await;
            source
                .copy(source_path, &to)
                .await
                .map(|_| ())
                .map_err(|error| {
                    format!("Failed to copy '{source_path}' to '{target_path}': {error}")
                })
        }
        NativeRoute::TranslatedTarget { from } => {
            ensure_parent_best_effort(target, target_path).await;
            target
                .copy(&from, target_path)
                .await
                .map(|_| ())
                .map_err(|error| {
                    format!("Failed to copy '{source_path}' to '{target_path}': {error}")
                })
        }
    }
}

/// mkdir -p on the translated endpoint's parent; best-effort because the
/// native copy that follows is the authoritative error surface.
async fn ensure_parent_best_effort(executor: &Operator, path: &str) {
    let parent = crate::engine::transfer::parent_dir(path.trim_matches('/'));
    if !parent.is_empty() {
        let _ = executor.create_dir(&format!("{parent}/")).await;
    }
}

/// `files/move` (§8.2): params identical to [`copy`] → `{success, transport,
/// jobId?}`.
///
/// Shared namespace spelling + `full_capability().rename` → native
/// `op.rename`; otherwise copy (native copy when available, else read→write)
/// + source delete (delete only after the copy succeeded).
pub async fn move_path(
    source: &Operator,
    target: &Operator,
    source_path: &str,
    target_path: &str,
    identity: BackendIdentity,
) -> Result<CopyOutcome, String> {
    if let Some(route) = native_route(source, target, source_path, target_path, identity) {
        if route_executor(&route, source, target)
            .info()
            .capability()
            .rename
        {
            execute_native_move(source, target, source_path, target_path, route).await?;
            return Ok(CopyOutcome {
                transport: CopyTransport::Native,
                job_id: None,
            });
        }
    }
    inline_copy(source, target, source_path, target_path).await?;
    // Delete only after the copy succeeded (doc §8.2 note).
    let source_is_dir = is_dir_path(source, source_path).await.unwrap_or(false);
    let delete = if source_is_dir {
        source.delete_with(source_path).recursive(true)
    } else {
        source.delete_with(source_path)
    };
    delete
        .await
        .map_err(|error| format!("Copied '{source_path}' but failed to delete the source: {error}"))?;
    Ok(CopyOutcome {
        transport: CopyTransport::Job,
        job_id: None,
    })
}

/// [`execute_native_copy`] for moves: `rename` follows the same namespace
/// mapping, and a translated rename never needs extra parent setup (rename
/// keeps the source inode/marker placement on filesystem backends).
async fn execute_native_move(
    source: &Operator,
    target: &Operator,
    source_path: &str,
    target_path: &str,
    route: NativeRoute,
) -> Result<(), String> {
    match route {
        NativeRoute::SameNamespace => {
            source
                .rename(source_path, target_path)
                .await
                .map_err(|error| {
                    format!("Failed to move '{source_path}' to '{target_path}': {error}")
                })
        }
        NativeRoute::TranslatedSource { to } => {
            source.rename(source_path, &to).await.map_err(|error| {
                format!("Failed to move '{source_path}' to '{target_path}': {error}")
            })
        }
        NativeRoute::TranslatedTarget { from } => {
            target.rename(&from, target_path).await.map_err(|error| {
                format!("Failed to move '{source_path}' to '{target_path}': {error}")
            })
        }
    }
}

/// `files/rename` (§8.2): `path`, `newPath` → `{success}`.
///
/// Same-connection rename; when `full_capability().rename` is false, degrade
/// to copy + delete through the job layer (doc §8.2 note).
///
/// Known limitation: OpenDAL's `Operator::rename` validates FILE paths only,
/// so directory renames return a clear business error (directories should be
/// moved with copy + purge).
pub async fn rename(operator: &Operator, path: &str, new_path: &str) -> Result<(), String> {
    let source_is_dir = is_dir_path(operator, path).await?;
    if source_is_dir {
        return Err(format!(
            "Renaming directory '{path}' is not supported (OpenDAL rename accepts \
             files only); copy and purge instead"
        ));
    }
    if operator.info().capability().rename {
        operator
            .rename(path, new_path)
            .await
            .map_err(|error| {
                format!("Failed to rename '{path}' to '{new_path}': {error}")
            })?;
        return Ok(());
    }
    // Degrade: copy (native when the backend supports it) + source delete.
    if operator.info().capability().copy {
        operator
            .copy(path, new_path)
            .await
            .map_err(|error| {
                format!("Failed to copy '{path}' to '{new_path}' during rename: {error}")
            })?;
    } else {
        inline_copy(operator, operator, path, new_path).await?;
    }
    operator
        .delete(path)
        .await
        .map_err(|error| format!("Copied '{path}' but failed to delete the source: {error}"))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Cross-connection backend identity verdict supplied by the engine
/// ([`crate::engine::Engine::backend_identity`], fingerprint registry). The
/// ops layer never re-derives credentials or config; it only consumes the
/// verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendIdentity {
    /// Both sides share one Operator instance — paths live in a single
    /// namespace; the legacy decision path, byte-identical behavior.
    SameInstance,
    /// Different instances whose registered config fingerprints match (same
    /// scheme + same credential-free Builder kv; `root` may differ) — rclone
    /// `--server-side-across-configs`. Native copies additionally require the
    /// root relation to be translatable (see [`native_route`]).
    Equivalent,
    /// Genuinely different backends — everything degrades to the stream
    /// transport.
    Distinct,
}

/// How a server-side copy/rename maps onto a single operator namespace.
/// OpenDAL's `copy`/`rename` take both endpoints in ONE operator's
/// coordinates, so a cross-root native copy needs the destination translated
/// into the executor's namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeRoute {
    /// Same instance or same root: pass the paths through verbatim (legacy
    /// behavior, no translation side effects).
    SameNamespace,
    /// Target path translated into the source operator's namespace; copy
    /// executes on the source operator as `source.copy(source_path, to)`.
    TranslatedSource { to: String },
    /// Source path translated into the target operator's namespace; copy
    /// executes on the target operator as `target.copy(from, target_path)`.
    TranslatedTarget { from: String },
}

/// Physical namespace path of a backend-relative path. `info().root()` is
/// absolute but NOT uniformly slash-normalized across services (object stores
/// return `/dir/`, the fs service returns the bare configured root), so the
/// trailing separator is enforced here before concatenation; a trailing `/`
/// of a dir request is dropped (native dir-marker copies are not a flow —
/// directory trees go through the dir-job layer).
fn physical_path(root: &str, relative: &str) -> String {
    // Windows 的 fs root 携带 `\` 分隔符（canonicalize 风格）；几何判定与
    // OpenDAL 的路径面统一按 `/` 归一化。
    let normalized = root.replace('\\', "/");
    let base = if normalized.ends_with('/') {
        normalized
    } else {
        format!("{normalized}/")
    };
    format!("{base}{}", relative.trim_matches('/'))
}

/// Segment-wise remainder of `path` (absolute physical) under `base_root`
/// (normalized operator root). `None` when `path` escapes the base or is the
/// base itself — a native copy has no single-namespace spelling then.
fn path_remainder(path: &str, base_root: &str) -> Option<String> {
    // The base keeps its leading slash; the trailing one is the boundary we
    // require (so `/ab` does not strip under `/a`).
    let base = base_root.replace('\\', "/");
    let base = base.trim_end_matches('/');
    let rest = path.strip_prefix(base)?;
    let rest = rest.strip_prefix('/')?;
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

/// §8.2 decision geometry shared by the single-file ops and the transfer
/// layer's dir jobs: how (and whether) one server-side copy/rename between
/// the two operators can be expressed. `None` → no native spelling exists
/// (distinct backends, or roots neither nesting nor equal) → degrade to the
/// streaming transport.
pub fn native_route(
    source: &Operator,
    target: &Operator,
    source_path: &str,
    target_path: &str,
    identity: BackendIdentity,
) -> Option<NativeRoute> {
    match identity {
        BackendIdentity::Distinct => None,
        BackendIdentity::SameInstance => Some(NativeRoute::SameNamespace),
        BackendIdentity::Equivalent => {
            let source_root = source.info().root();
            let target_root = target.info().root();
            if source_root == target_root {
                return Some(NativeRoute::SameNamespace);
            }
            // Prefer the source operator (legacy executor). Nested roots
            // translate; divergent roots have no single-namespace spelling.
            if let Some(to) = path_remainder(&physical_path(&target_root, target_path), &source_root)
            {
                return Some(NativeRoute::TranslatedSource { to });
            }
            let from =
                path_remainder(&physical_path(&source_root, source_path), &target_root)?;
            Some(NativeRoute::TranslatedTarget { from })
        }
    }
}

/// The operator a [`NativeRoute`] executes on: the source, except when the
/// translation went the other way ([`NativeRoute::TranslatedTarget`]).
fn route_executor<'a>(
    route: &NativeRoute,
    source: &'a Operator,
    target: &'a Operator,
) -> &'a Operator {
    match route {
        NativeRoute::TranslatedTarget { .. } => target,
        NativeRoute::SameNamespace | NativeRoute::TranslatedSource { .. } => source,
    }
}

/// §8.2 decision predicate shared by `ops::copy` and the main.rs router:
/// `files/copy` can run natively (server-side `op.copy`) when the engine's
/// identity verdict yields a shared-namespace spelling ([`native_route`]) and
/// the executing backend advertises `copy`.
pub fn native_copy_available(
    source: &Operator,
    target: &Operator,
    source_path: &str,
    target_path: &str,
    identity: BackendIdentity,
) -> bool {
    match native_route(source, target, source_path, target_path, identity) {
        None => false,
        Some(route) => route_executor(&route, source, target)
            .info()
            .capability()
            .copy,
    }
}

/// §8.2 decision predicate shared by `ops::move_path` and the main.rs router:
/// `files/move` can run as a native server-side `op.rename` under the same
/// rules as [`native_copy_available`], against the `rename` capability.
pub fn native_move_available(
    source: &Operator,
    target: &Operator,
    source_path: &str,
    target_path: &str,
    identity: BackendIdentity,
) -> bool {
    match native_route(source, target, source_path, target_path, identity) {
        None => false,
        Some(route) => route_executor(&route, source, target)
            .info()
            .capability()
            .rename,
    }
}


/// Lists a directory (non-recursive) or the whole subtree (recursive),
/// filtering the marker entry of the listed prefix itself. Paths inside the
/// returned entries are backend-namespace relative (no leading slash).
async fn list_raw(
    operator: &Operator,
    path: &str,
    recursive: bool,
) -> Result<Vec<opendal::Entry>, String> {
    let prefix = path.trim().trim_matches('/');
    let (list_path, is_root) = if prefix.is_empty() {
        ("/".to_string(), true)
    } else {
        (format!("/{prefix}/"), false)
    };
    let entries = if recursive {
        operator
            .list_with(&list_path)
            .recursive(true)
            .await
            .map_err(|error| format!("Failed to list '{prefix}': {error}"))?
    } else {
        operator
            .list(&list_path)
            .await
            .map_err(|error| format!("Failed to list '{prefix}': {error}"))?
    };
    Ok(entries
        .into_iter()
        .filter(|entry| {
            let entry_path = entry.path();
            if is_root {
                entry_path != "/"
            } else {
                entry_path != prefix && entry_path != format!("{prefix}/")
            }
        })
        .collect())
}

/// Stats a path, retrying once with a trailing `/` when the bare path is not
/// found. Some backends (e.g. the memory service) only resolve the directory
/// marker through its trailing-slash spelling.
async fn stat_flex(operator: &Operator, path: &str) -> Result<opendal::Metadata, String> {
    match operator.stat(path).await {
        Ok(metadata) => Ok(metadata),
        Err(error) => {
            let trimmed = path.trim_end_matches('/');
            if trimmed.is_empty() || path.ends_with('/') {
                return Err(format!("Failed to stat '{path}': {error}"));
            }
            operator
                .stat(&format!("{trimmed}/"))
                .await
                .map_err(|_| format!("Failed to stat '{path}': {error}"))
        }
    }
}

/// True when the path resolves to a directory (trailing-slash tolerant).
/// Public so the transfer layer's copy jobs can branch file vs. directory
/// plans on the same flexible stat.
pub async fn is_dir_path(operator: &Operator, path: &str) -> Result<bool, String> {
    let metadata = stat_flex(operator, path).await?;
    Ok(metadata.mode().is_dir())
}

/// Normalizes a request path to an OpenDAL path relative to the Operator root:
/// collapses `.`/`..`, strips leading `/`, guarantees a trailing `/` for dir
/// ops. Fills in the gate root prefix when `lock_to_root` is set.
///
/// Path whitelist only — the `read_only`/`allow_delete` gates have their own
/// [`Gate`] helpers because the frozen op signatures carry no gate argument.
pub fn normalize_path(gate: &Gate, path: &str, is_dir: bool) -> Result<String, String> {
    gate.readable_path(path, is_dir)
}

/// Normalizes a directory entry path coming from OpenDAL listing into a
/// `FileEntry` (kind from `EntryMode`, size/millis from `Metadata`).
pub fn entry_from_opendal(entry: &opendal::Entry) -> FileEntry {
    file_entry_from_path(entry.path(), entry.metadata())
}

/// Builds the camelCase `FileEntry` payload from an OpenDAL path + metadata.
fn file_entry_from_path(path: &str, metadata: &opendal::Metadata) -> FileEntry {
    let mode = metadata.mode();
    let is_dir = mode.is_dir();
    let trimmed = path.trim_matches('/');
    let name = trimmed
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or("/");
    let size = if !is_dir {
        Some(metadata.content_length())
    } else {
        None
    };
    let modified_at = metadata.last_modified().map(|timestamp| {
        // OpenDAL wraps jiff::Timestamp; convert to Unix epoch millis.
        timestamp.into_inner().as_millisecond().max(0) as u64
    });
    FileEntry {
        name: name.to_string(),
        path: if trimmed.is_empty() {
            "/".to_string()
        } else {
            format!("/{trimmed}")
        },
        kind: if is_dir { "dir" } else { "file" },
        size,
        modified_at,
    }
}

/// Degraded streaming read→write copy (doc §5.2: 跨服务降级). Directories are
/// walked file-by-file; files stream through 4 MiB chunks so arbitrary sizes
/// stay off the heap. Executed inline within the request (see [`copy`] for the
/// transport/jobId caveat).
async fn inline_copy(
    source: &Operator,
    target: &Operator,
    source_path: &str,
    target_path: &str,
) -> Result<(), String> {
    let metadata = stat_flex(source, source_path).await?;
    if metadata.mode().is_dir() {
        let files = crate::engine::transfer::walk_files(source, source_path).await?;
        for (relative, _size) in files {
            let from = join_relative(source_path, &relative);
            let to = join_relative(target_path, &relative);
            inline_copy_file(source, target, &from, &to).await?;
        }
        Ok(())
    } else {
        inline_copy_file(source, target, source_path, target_path).await
    }
}

/// Streams one file from `source` to `target` in chunks, creating the target
/// parent directory first.
async fn inline_copy_file(
    source: &Operator,
    target: &Operator,
    source_path: &str,
    target_path: &str,
) -> Result<(), String> {
    let parent = crate::engine::transfer::parent_dir(target_path.trim_matches('/'));
    if !parent.is_empty() {
        target
            .create_dir(&format!("{parent}/"))
            .await
            .map_err(|error| {
                format!("Failed to create target directory '{parent}' for copy: {error}")
            })?;
    }
    let (reader, size) =
        crate::engine::transfer::open_download_reader(source, source_path).await?;
    let mut writer =
        crate::engine::transfer::open_upload_writer(target, target_path).await?;
    let mut offset = 0u64;
    while offset < size {
        let end = (offset + INLINE_COPY_CHUNK).min(size);
        let chunk = crate::engine::transfer::read_chunk(&reader, offset, end).await?;
        if chunk.is_empty() {
            break;
        }
        writer
            .write(chunk)
            .await
            .map_err(|error| format!("Copy write to '{target_path}' failed at offset {offset}: {error}"))?;
        offset = end;
    }
    writer
        .close()
        .await
        .map_err(|error| format!("Failed to finish copy into '{target_path}': {error}"))?;
    Ok(())
}

/// Joins a base directory (may be the root `""`) with a walked relative path.
fn join_relative(base: &str, relative: &str) -> String {
    let trimmed = base.trim().trim_matches('/');
    if trimmed.is_empty() {
        relative.to_string()
    } else {
        format!("{trimmed}/{relative}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_operator() -> Operator {
        opendal::Operator::via_iter("memory", Vec::<(String, String)>::new()).unwrap()
    }

    fn gate(root: &str, lock: bool, read_only: bool, allow_delete: bool) -> Gate {
        Gate {
            root: root.to_string(),
            lock_to_root: lock,
            read_only,
            allow_delete,
        }
    }

    // -- Gate / normalize_path table tests ----------------------------------

    #[test]
    fn normalize_path_table() {
        let permissive = gate("", false, false, true);
        let cases: Vec<(&str, bool, Result<&str, ()>)> = vec![
            ("", false, Ok("")),
            ("/", false, Ok("")),
            ("a/b", false, Ok("a/b")),
            ("/a//b/./", false, Ok("a/b")),
            ("a/../b", false, Ok("b")),
            ("a", true, Ok("a/")),
            ("/a/b/", true, Ok("a/b/")),
            ("", true, Ok("/")),
            ("../x", false, Err(())),
            ("a\\b", false, Err(())),
        ];
        for (input, is_dir, expected) in cases {
            let outcome = normalize_path(&permissive, input, is_dir);
            match expected {
                Ok(expected_path) => assert_eq!(
                    outcome.as_deref(),
                    Ok(expected_path),
                    "normalize('{input}', is_dir={is_dir})"
                ),
                Err(()) => assert!(
                    outcome.is_err(),
                    "normalize('{input}', is_dir={is_dir}) should be rejected"
                ),
            }
        }
    }

    #[test]
    fn normalize_path_locks_to_root() {
        let locked = gate("/srv/data", true, false, true);
        assert_eq!(normalize_path(&locked, "sub/x", false).unwrap(), "sub/x");
        assert_eq!(
            normalize_path(&locked, "/srv/data/sub", false).unwrap(),
            "sub"
        );
        assert_eq!(normalize_path(&locked, "", true).unwrap(), "/");
        assert!(normalize_path(&locked, "/etc/passwd", false).is_err());
        assert!(normalize_path(&locked, "../escape", false).is_err());

        // Without the lock, absolute outside paths still resolve — the
        // Operator root is the physical confinement.
        let unlocked = gate("/srv/data", false, false, true);
        assert_eq!(normalize_path(&unlocked, "/etc/x", false).unwrap(), "etc/x");
    }

    #[test]
    fn gate_write_and_delete_table() {
        struct Row {
            read_only: bool,
            allow_delete: bool,
            writable: bool,
            deletable: bool,
        }
        let rows = [
            Row { read_only: false, allow_delete: true, writable: true, deletable: true },
            Row { read_only: true, allow_delete: true, writable: false, deletable: false },
            Row { read_only: false, allow_delete: false, writable: true, deletable: false },
            Row { read_only: true, allow_delete: false, writable: false, deletable: false },
        ];
        for row in rows {
            let gate = gate("", false, row.read_only, row.allow_delete);
            assert_eq!(
                gate.ensure_writable_path("a/b.txt", false).is_ok(),
                row.writable,
                "read_only={} writable",
                row.read_only
            );
            assert_eq!(
                gate.ensure_deletable_path("a/b.txt", false).is_ok(),
                row.deletable,
                "allow_delete={} deletable",
                row.allow_delete
            );
        }
    }

    #[test]
    fn gate_from_connection_maps_fields() {
        let connection = crate::model::StoredConnection::from_lifecycle_params(&serde_json::json!({
            "connection": {
                "id": "c",
                "external_config": {
                    "protocol": "fs",
                    "root": "/srv/data/",
                    "lock_to_root": true,
                    "read_only": true,
                    "allow_delete": false
                }
            }
        }))
        .unwrap();
        let gate = Gate::from_connection(&connection);
        assert_eq!(gate.root, "/srv/data/");
        assert!(gate.lock_to_root && gate.read_only && !gate.allow_delete);
    }

    #[test]
    fn gate_purge_refuses_root_via_policy() {
        let gate = gate("/mnt/nas", false, false, true);
        for path in ["", "/", "/mnt/nas"] {
            let error = gate
                .policy()
                .check_purge(path)
                .expect_err("root purge must be refused");
            assert!(error.contains("refusing to purge"), "{path}: {error}");
        }
        assert!(gate.policy().check_purge("sub").is_ok());
    }

    // -- memory:// full-operation matrix ------------------------------------

    #[tokio::test]
    async fn memory_full_operation_matrix() {
        let op = memory_operator();

        // mkdir (nested, mkdir -p) + write
        mkdir(&op, "alpha/bravo").await.unwrap();
        write(&op, "alpha/bravo/hello.txt", b"hello files".to_vec())
            .await
            .unwrap();
        write(&op, "alpha/bravo/big.bin", vec![0u8; 4096]).await.unwrap();

        // list (flat + recursive), sorted
        let entries = list(&op, "alpha/bravo", false).await.unwrap();
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["big.bin", "hello.txt"], "sorted by path");
        assert_eq!(entries[0].kind, "file");
        assert_eq!(entries[0].path, "/alpha/bravo/big.bin");

        let recursive = list(&op, "alpha", true).await.unwrap();
        assert!(
            recursive.iter().any(|entry| entry.kind == "dir" && entry.name == "bravo"),
            "recursive list keeps directory entries: {recursive:?}"
        );

        // list_paged (memory slice + total)
        let (page, total) = list_paged(&op, "alpha/bravo", 1, 1).await.unwrap();
        assert_eq!(total, 2);
        assert_eq!(page.len(), 1);
        let (page, _) = list_paged(&op, "alpha/bravo", 2, 1).await.unwrap();
        assert_eq!(page.len(), 1);
        let (page, total) = list_paged(&op, "alpha/bravo", 9, 1).await.unwrap();
        assert_eq!(total, 2, "out-of-range page keeps the real total");
        assert!(page.is_empty());

        // stat (camelCase payload shape)
        let entry = stat(&op, "alpha/bravo/hello.txt").await.unwrap();
        assert_eq!(entry.name, "hello.txt");
        assert_eq!(entry.path, "/alpha/bravo/hello.txt");
        assert_eq!(entry.kind, "file");
        assert_eq!(entry.size, Some(11));
        let dir_entry = stat(&op, "alpha/bravo").await.unwrap();
        assert_eq!(dir_entry.kind, "dir");
        assert_eq!(dir_entry.size, None);

        // size (files only)
        let (count, bytes) = size(&op, "alpha").await.unwrap();
        assert_eq!(count, 2);
        assert_eq!(bytes, 4096 + 11);

        // read: whole file and truncated preview
        let (data, truncated) = read(&op, "alpha/bravo/hello.txt", 1024).await.unwrap();
        assert_eq!(data, b"hello files");
        assert!(!truncated);
        let (data, truncated) = read(&op, "alpha/bravo/big.bin", 1024).await.unwrap();
        assert_eq!(data.len(), 1024);
        assert!(truncated);
        assert!(read(&op, "alpha/bravo", 1024).await.is_err(), "dirs unreadable");

        // copy: memory has no native copy capability → inline job degrade
        let outcome = copy(
            &op,
            &op,
            "alpha/bravo/hello.txt",
            "alpha/bravo/hello-copy.txt",
            BackendIdentity::SameInstance,
        )
        .await
        .unwrap();
        assert_eq!(outcome.transport, CopyTransport::Job);
        assert!(outcome.job_id.is_none(), "inline transport has no jobId yet");
        let copied = stat(&op, "alpha/bravo/hello-copy.txt").await.unwrap();
        assert_eq!(copied.size, Some(11));

        // move: copy + source delete (delete only after the copy succeeded)
        let outcome = move_path(
            &op,
            &op,
            "alpha/bravo/hello-copy.txt",
            "alpha/bravo/moved.txt",
            BackendIdentity::SameInstance,
        )
        .await
        .unwrap();
        assert_eq!(outcome.transport, CopyTransport::Job);
        assert!(stat(&op, "alpha/bravo/moved.txt").await.is_ok());
        assert!(stat(&op, "alpha/bravo/hello-copy.txt").await.is_err());

        // rename: memory lacks the rename capability → copy + delete degrade
        rename(&op, "alpha/bravo/moved.txt", "alpha/bravo/renamed.txt")
            .await
            .unwrap();
        assert!(stat(&op, "alpha/bravo/renamed.txt").await.is_ok());
        assert!(stat(&op, "alpha/bravo/moved.txt").await.is_err());

        // delete: idempotent
        delete(&op, "alpha/bravo/renamed.txt").await.unwrap();
        delete(&op, "alpha/bravo/renamed.txt").await.unwrap();

        // rmdir: empty only
        assert!(
            rmdir(&op, "alpha").await.is_err(),
            "non-empty rmdir must be refused"
        );
        mkdir(&op, "alpha/empty").await.unwrap();
        rmdir(&op, "alpha/empty").await.unwrap();

        // purge: refuses root, removes subtrees
        assert!(purge(&op, "/").await.is_err());
        assert!(purge(&op, "").await.is_err());
        purge(&op, "alpha/bravo").await.unwrap();
        assert!(stat(&op, "alpha/bravo").await.is_err());

        // capabilities + public_link (memory never supports presign)
        let caps = capabilities(&op).unwrap();
        assert_eq!(caps.scheme, "memory");
        assert!(caps.list && caps.read && caps.write && caps.stat);
        assert!(!caps.presign, "memory has no presign capability");
        let error = public_link(&op, "alpha/bravo/hello.txt", 60).await.unwrap_err();
        assert!(
            error.contains("does not support presigned"),
            "clear unsupported error, got: {error}"
        );
    }

    #[tokio::test]
    async fn fs_backend_uses_native_copy_and_rename() {
        let temp = tempfile::tempdir().unwrap();
        let op = Operator::via_iter(
            "fs",
            vec![("root".to_string(), temp.path().to_string_lossy().to_string())],
        )
        .unwrap();
        op.write("f.txt", "native").await.unwrap();

        let outcome = copy(
            &op,
            &op,
            "f.txt",
            "g.txt",
            BackendIdentity::SameInstance,
        )
        .await
        .unwrap();
        assert_eq!(outcome.transport, CopyTransport::Native);
        let outcome = move_path(
            &op,
            &op,
            "g.txt",
            "h.txt",
            BackendIdentity::SameInstance,
        )
        .await
        .unwrap();
        assert_eq!(outcome.transport, CopyTransport::Native);
        assert!(op.exists("h.txt").await.unwrap());
        assert!(!op.exists("g.txt").await.unwrap());

        // Native rename of a directory is rejected with a clear error.
        op.create_dir("d/").await.unwrap();
        let error = rename(&op, "d", "d2").await.unwrap_err();
        assert!(error.contains("not supported"), "{error}");
    }

    #[tokio::test]
    async fn copy_between_instances_degrades_to_job_transport() {
        let source = memory_operator();
        let target = memory_operator();
        source.write("f.txt", "12345").await.unwrap();

        // Two memory instances: equal configs but per-Operator state, so the
        // engine verdict is Distinct and the transport must stay streaming.
        let outcome = copy(
            &source,
            &target,
            "f.txt",
            "copy.txt",
            BackendIdentity::Distinct,
        )
        .await
        .unwrap();
        assert_eq!(outcome.transport, CopyTransport::Job);
        assert!(outcome.job_id.is_none(), "inline transport has no jobId yet");
        let data = target.read("copy.txt").await.unwrap();
        assert_eq!(data.to_vec(), b"12345");

        // move degrade: copy + source delete, nested target dir included.
        source.write("nested/f2.txt", "abcdef").await.unwrap();
        let outcome = move_path(
            &source,
            &target,
            "nested/f2.txt",
            "dst/moved.txt",
            BackendIdentity::Distinct,
        )
        .await
        .unwrap();
        assert_eq!(outcome.transport, CopyTransport::Job);
        assert_eq!(
            target.read("dst/moved.txt").await.unwrap().to_vec(),
            b"abcdef"
        );
        assert!(
            !source.exists("nested/f2.txt").await.unwrap(),
            "move deletes the source only after the copy succeeded"
        );
    }

    // -- config-equivalent cross-connection native copy (2026-09-17) --------

    /// Two fs Operators over one tempdir root: config-equivalent (the fs kv
    /// only carries `root`, which the fingerprint excludes) but distinct
    /// instances — the "same account added twice" shape.
    fn fs_operator(root: &std::path::Path) -> Operator {
        Operator::via_iter(
            "fs",
            vec![("root".to_string(), root.to_string_lossy().to_string())],
        )
        .unwrap()
    }

    #[tokio::test]
    async fn equivalent_configs_native_copy_across_connections() {
        let temp = tempfile::tempdir().unwrap();
        // Same root twice → same namespace → verbatim native copy.
        let source = fs_operator(temp.path());
        let target = fs_operator(temp.path());
        source.write("f.txt", "same").await.unwrap();

        assert!(
            native_copy_available(
                &source,
                &target,
                "f.txt",
                "g.txt",
                BackendIdentity::Equivalent
            ),
            "fingerprint-equivalent instances with copy capability go native"
        );
        let outcome = copy(
            &source,
            &target,
            "f.txt",
            "g.txt",
            BackendIdentity::Equivalent,
        )
        .await
        .unwrap();
        assert_eq!(outcome.transport, CopyTransport::Native);
        assert_eq!(target.read("g.txt").await.unwrap().to_vec(), b"same");

        // move joins the same rule (native rename).
        let outcome = move_path(
            &source,
            &target,
            "g.txt",
            "moved.txt",
            BackendIdentity::Equivalent,
        )
        .await
        .unwrap();
        assert_eq!(outcome.transport, CopyTransport::Native);
        assert!(target.exists("moved.txt").await.unwrap());
        assert!(!target.exists("g.txt").await.unwrap());
    }

    #[tokio::test]
    async fn equivalent_configs_native_copy_translates_divergent_roots() {
        let temp = tempfile::tempdir().unwrap();
        // Target root nests under the source root: the destination translates
        // into the source namespace (`target_root/target_path` is one and the
        // same physical file in both coordinates).
        let source = fs_operator(temp.path());
        let nested = temp.path().join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let target = fs_operator(&nested);
        source.write("f.txt", "cross").await.unwrap();

        let outcome = copy(
            &source,
            &target,
            "f.txt",
            "f2.txt",
            BackendIdentity::Equivalent,
        )
        .await
        .unwrap();
        assert_eq!(outcome.transport, CopyTransport::Native);
        // The file landed in the TARGET's physical namespace, not merely
        // beside the source under the source root.
        assert_eq!(
            std::fs::read(nested.join("f2.txt")).unwrap(),
            b"cross",
            "native copy must land in the target root"
        );
        assert!(!temp.path().join("f2.txt").exists(), "no stray copy at the source root");

        // Reverse nesting direction: the copy executes on the target
        // operator with the source translated instead.
        let outcome = move_path(
            &source,
            &target,
            "f.txt",
            "f3.txt",
            BackendIdentity::Equivalent,
        )
        .await
        .unwrap();
        assert_eq!(outcome.transport, CopyTransport::Native);
        assert_eq!(std::fs::read(nested.join("f3.txt")).unwrap(), b"cross");
        assert!(!temp.path().join("f.txt").exists(), "native move deleted the source");
    }

    #[tokio::test]
    async fn equivalent_configs_with_divergent_roots_degrade_to_stream() {
        let temp = tempfile::tempdir().unwrap();
        let source = fs_operator(&temp.path().join("a"));
        let target = fs_operator(&temp.path().join("b"));
        std::fs::create_dir_all(temp.path().join("a")).unwrap();
        std::fs::create_dir_all(temp.path().join("b")).unwrap();
        source.write("f.txt", "stream").await.unwrap();

        // Roots neither equal nor nested: fingerprints still match (root is
        // excluded), but no single-namespace copy spelling exists.
        assert!(!native_copy_available(
            &source,
            &target,
            "f.txt",
            "g.txt",
            BackendIdentity::Equivalent
        ));
        assert!(native_route(
            &source,
            &target,
            "f.txt",
            "g.txt",
            BackendIdentity::Equivalent
        )
        .is_none());
        let outcome = copy(
            &source,
            &target,
            "f.txt",
            "g.txt",
            BackendIdentity::Equivalent,
        )
        .await
        .unwrap();
        assert_eq!(outcome.transport, CopyTransport::Job);
        assert_eq!(target.read("g.txt").await.unwrap().to_vec(), b"stream");
    }

    #[test]
    fn native_route_geometry_table() {
        // Pure path math: physical root/relative joins + segment-safe strips.
        // Object-store roots arrive `/dir/`-normalized; the fs service returns
        // the bare root, so the join must enforce the trailing separator.
        assert_eq!(physical_path("/a/", "x/y"), "/a/x/y");
        assert_eq!(physical_path("/a", "x/y"), "/a/x/y");
        assert_eq!(physical_path("/", "x"), "/x");
        assert_eq!(path_remainder("/a/x/y", "/a/"), Some("x/y".to_string()));
        assert_eq!(path_remainder("/a/x/y", "/a"), Some("x/y".to_string()));
        assert_eq!(path_remainder("/a/x/y", "/"), Some("a/x/y".to_string()));
        assert_eq!(path_remainder("/ab/x", "/a/"), None, "segment boundary required");
        assert_eq!(path_remainder("/ab/x", "/a"), None, "segment boundary required");
        assert_eq!(path_remainder("/a/", "/a/"), None, "base itself is not expressible");
    }

    #[tokio::test]
    async fn write_rejects_oversize_inline_payload() {
        let op = memory_operator();
        let error = write(&op, "too-big.bin", vec![0u8; MAX_INLINE_WRITE_BYTES + 1])
            .await
            .unwrap_err();
        assert!(error.contains("upload channel"), "{error}");
    }

    #[tokio::test]
    async fn read_caps_preview_at_two_mebibytes() {
        let op = memory_operator();
        op.write("huge.bin", vec![7u8; MAX_PREVIEW_BYTES + 100])
            .await
            .unwrap();
        let (data, truncated) =
            read(&op, "huge.bin", usize::MAX).await.unwrap();
        assert_eq!(data.len(), MAX_PREVIEW_BYTES, "hard cap enforced");
        assert!(truncated);
    }

    #[tokio::test]
    async fn rename_directory_is_rejected_with_clear_error() {
        let op = memory_operator();
        mkdir(&op, "some-dir").await.unwrap();
        let error = rename(&op, "some-dir", "renamed-dir").await.unwrap_err();
        assert!(error.contains("not supported"), "{error}");
    }

    #[tokio::test]
    async fn entry_from_opendal_serializes_camel_case() {
        let op = memory_operator();
        op.write("e/f.txt", "xyz").await.unwrap();
        let entries = list_raw(&op, "e", false).await.unwrap();
        let entry = entry_from_opendal(&entries[0]);
        let text = serde_json::to_string(&entry).unwrap();
        assert!(text.contains("\"kind\":\"file\""), "{text}");
        assert!(text.contains("\"path\":\"/e/f.txt\""), "{text}");
    }

    #[tokio::test]
    async fn inline_copy_streams_files_across_instances() {
        let source = memory_operator();
        let target = memory_operator();
        let payload: Vec<u8> = (0..=255u8).cycle().take(5 * 1024 * 1024 + 17).collect();
        source.write("blob.bin", payload.clone()).await.unwrap();

        inline_copy(&source, &target, "blob.bin", "copy/blob.bin")
            .await
            .unwrap();
        assert_eq!(
            target.read("copy/blob.bin").await.unwrap().to_vec(),
            payload,
            "5 MiB payload crosses the 4 MiB chunk boundary intact"
        );
    }

    // -- files/quickPaths ----------------------------------------------------

    fn quick_connection(protocol: &str, root: &str, lock_to_root: bool) -> StoredConnection {
        // "memory" 在协议层以 opendal-custom 形式出现（engine scheme 白名单）。
        let external: serde_json::Value = if protocol == "memory" {
            json!({ "protocol": "opendal-custom", "service": "memory", "root": root, "lock_to_root": lock_to_root })
        } else {
            json!({ "protocol": protocol, "root": root, "lock_to_root": lock_to_root })
        };
        StoredConnection::from_lifecycle_params(&json!({
            "connection": { "id": "qp", "external_config": external }
        }))
        .unwrap()
    }

    #[test]
    fn quick_paths_eligibility_table() {
        assert!(quick_paths_eligible(&quick_connection("fs", "/", false)));
        assert!(
            !quick_paths_eligible(&quick_connection("fs", "/srv/data", false)),
            "configured root confines absolute user paths"
        );
        assert!(!quick_paths_eligible(&quick_connection("fs", "/", true)), "lock_to_root confines navigation");
        assert!(!quick_paths_eligible(&quick_connection("memory", "", false)), "non-fs falls back to root chip");
        assert!(!quick_paths_eligible(&quick_connection("s3", "", false)), "non-fs falls back to root chip");
    }

    #[test]
    fn fs_quick_path_candidates_follow_unix_home_layout() {
        let candidates = fs_quick_path_candidates("/Users/jin");
        let (keys, paths): (Vec<&str>, Vec<String>) =
            candidates.into_iter().map(|(key, path)| (key, path)).unzip();
        assert_eq!(keys, vec!["home", "desktop", "downloads", "documents", "pictures"]);
        assert_eq!(paths[0], "/Users/jin");
        assert_eq!(paths[1], "/Users/jin/Desktop");
        assert_eq!(paths[4], "/Users/jin/Pictures");
    }

    #[tokio::test]
    async fn quick_paths_root_only_fallback_on_memory() {
        let op = memory_operator();
        let connection = quick_connection("memory", "", false);
        let payload = quick_paths(&op, &connection).await.unwrap();
        assert_eq!(payload["paths"], json!([{ "key": "root", "path": "/" }]));
    }

    #[tokio::test]
    async fn quick_paths_root_only_fallback_on_confined_fs() {
        // fs with a configured root must not expose absolute user dirs, and the
        // "/" chip still resolves (OpenDAL maps it onto the configured root).
        let op = memory_operator();
        let connection = quick_connection("fs", "/srv/data", false);
        let payload = quick_paths(&op, &connection).await.unwrap();
        assert_eq!(payload["paths"], json!([{ "key": "root", "path": "/" }]));
    }

    #[tokio::test]
    async fn quick_paths_verify_candidates_with_stat_on_fs() {
        // Simulated fs layout: only Desktop exists under the injected home;
        // candidates are stat-verified so missing dirs never become chips.
        let op = memory_operator();
        mkdir(&op, "Users/qp/Desktop").await.unwrap();
        mkdir(&op, "Users/qp/missing-dir").await.unwrap_or_else(|_| ());
        let connection = quick_connection("fs", "/", false);
        let payload = quick_paths_with_home(&op, &connection, Some("/Users/qp")).await.unwrap();
        assert_eq!(
            payload["paths"],
            json!([
                { "key": "root", "path": "/" },
                { "key": "home", "path": "/Users/qp" },
                { "key": "desktop", "path": "/Users/qp/Desktop" },
            ]),
            "only stat-verified directories become chips"
        );
    }

    #[tokio::test]
    async fn quick_paths_without_home_stays_root_only() {
        let op = memory_operator();
        let connection = quick_connection("fs", "/", false);
        let payload = quick_paths_with_home(&op, &connection, None).await.unwrap();
        assert_eq!(payload["paths"], json!([{ "key": "root", "path": "/" }]));
    }
}
