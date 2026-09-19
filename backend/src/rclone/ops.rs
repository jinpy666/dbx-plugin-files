//! rc-backed storage operations (F-RCLONE phase A, Agent B).
//!
//! Implements the OpenDAL storage-method surface of `engine/ops.rs` on top of
//! the rc HTTP API: `list` / `list_paged` / `stat` / `size` /
//! `capabilities` / `quick_paths`. Output shapes are field-for-field aligned
//! with the OpenDAL engine (`model::FileEntry`, `model::Capabilities`), so the
//! frontend contract is untouched by the engine swap.
//!
//! Contract: docs/IMPL_PLAN_RCLONE.zh-CN.md §5 (method mapping) and §12
//! (file ownership). Only this file may be modified by the ops task.
//!
//! Semantics pinned against a live rclone v1.75.1 process (plan doc end
//! notes + this module's live tests — do not re-derive):
//!
//! - `operations/list` answers `{"list":[...]}` with items keyed
//!   `Path` / `Name` / `Size` / `MimeType` / `ModTime` / `IsDir`
//!   (capitalized Go marshalling). A missing `list` key behaves as an empty
//!   directory (`{"list": []}` is what an empty dir actually returns).
//! - `ModTime` is an **RFC3339 string with nanoseconds and offset**
//!   (`"2026-09-18T14:53:50.448025022+08:00"`), not unix seconds/millis.
//! - Item `Path` is relative to the listed fs WITHOUT a leading slash and
//!   keeps the listed prefix (`"sub/nested.txt"` when recursing under
//!   `sub`); the listed prefix's own marker never appears for local fs but
//!   may for bucket-based backends — filtered here either way.
//! - The `remote` parameter must NOT carry a trailing slash
//!   (`"sub/"` lists as empty on local); policy `relative` paths never have
//!   one, and this module strips defensively.
//! - `operations/stat` on a missing object is `Ok({"item": null})`
//!   (surfaced as the existing NotFound-style error here), while a missing
//!   target for `operations/list` / `operations/size` is a real rc error.
//! - `operations/size` reads only the `fs` parameter (a `remote` key is
//!   ignored by rcd) — subdirectory sizing passes `fs` + `remote` combined
//!   (`"remote:path/sub"` / `"/local/root/sub"`).
//! - `backend/features` does not exist on stock rcd (404); the live feature
//!   source is `operations/fsinfo`, with a per-protocol static matrix as
//!   fallback. fsinfo overrides: `Copy` → `copy`, `Move` → `rename`,
//!   `PublicLink` → `presign`, `PutStream|PutUnchecked|OpenWriterAt` →
//!   `write`. `list`/`read`/`stat`/`delete`/`createDir` stay unconditionally
//!   true (rclone core operations exist for every backend and degrade
//!   internally).
//!
//! Phase B file ops (this module's second half) pin the same live process:
//!
//! - Mutating rc endpoints take `{fs, remote}` (`copyfile`/`movefile`:
//!   `{srcFs, srcRemote, dstFs, dstRemote}`) and answer `{}` on success.
//!   `mkdir` is mkdir -p (one call creates the whole parent chain) and
//!   idempotent; `rmdir` refuses missing/non-empty with rc's own stat errors;
//!   `deletefile` 404s on missing objects while the OpenDAL engine's delete
//!   is idempotent → stat-first tolerance here; `purge` recurses but 500s on
//!   missing paths → same tolerance. `copyfile`/`movefile` overwrite an
//!   existing destination (verified).
//! - `operations/uploadfile` writes the multipart part to
//!   `path.Join(remote, <part filename>)` (rc.go `Rcat(ctx, f, path.Join(...))`,
//!   v1.75.1 source) and auto-creates every missing parent. rc.rs pins the
//!   part filename to `payload`, so [`write_bytes`] uploads into the target's
//!   PARENT directory and `movefile`s `…/payload` onto the target — a plain
//!   `remote=<full filename>` upload would land at `<dir-of-name>/payload`.
//! - The rc-serve byte channel routes GET `/{base}/[{fs}]/{remote}` (fsMatch
//!   regex `^\[(.*?)\](.*)$`, identical in v1.68.0 and v1.75.1) — the plain
//!   `{fs}/{remote}` spelling answers 404 "Not Found" on every supported
//!   version, so [`read_prefix`] wraps the fs in literal brackets. Only
//!   `%`, `?` and `#` are escaped by hand (spaces and non-ASCII bytes are
//!   percent-encoded by the URL parser itself, and rcd decodes the path
//!   before matching). `Range: bytes=0-{max}` is inclusive; a 200-or-206
//!   body longer than `max_bytes` proves truncation without parsing
//!   Content-Range. A GET on a directory answers 500 JSON (or an HTML
//!   listing for the bare fs), so reads stat first and refuse directories
//!   like the OpenDAL engine.

#![allow(dead_code)]

use serde_json::{json, Value};

use crate::policy::PathPolicy;
use crate::model::{Capabilities, FileEntry};

use super::rc::RcClient;

/// Hard cap for the full enumeration behind `list_paged` (plan §5
/// files/listPaged: 100,000 entries, cursor evolution deferred to phase C).
pub const LIST_PAGED_MAX: usize = 100_000;

// ---------------------------------------------------------------------------
// fs-string helpers
// ---------------------------------------------------------------------------

/// `fs` 协议（本地远端）：`root` 非空用 root，否则 `/`（plan §4 映射表）。
pub fn local_fs_string(root: &str) -> String {
    let trimmed = root.trim();
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Joins a connection fs string with a root-relative remote path for rc
/// methods that take the whole path in `fs` (e.g. `operations/size`, which
/// ignores a separate `remote` key). `"remote:" + "a/b"` → `"remote:a/b"`;
/// `"/local/root" + "sub"` → `"/local/root/sub"`; the bare root stays as-is.
fn fs_with_path(fs: &str, remote: &str) -> String {
    if remote.is_empty() {
        fs.to_string()
    } else if fs.ends_with(':') {
        format!("{fs}{remote}")
    } else {
        format!("{}/{remote}", fs.trim_end_matches('/'))
    }
}

// ---------------------------------------------------------------------------
// Path gating (crate::engine::policy — same whitelist as the OpenDAL engine)
// ---------------------------------------------------------------------------

/// Read-side gate shared by every op: sanitizes the request path and, when
/// `lock_to_root` is set, requires it to stay inside `root`. Returns the
/// root-relative remote path (`""` = the root itself). Error text is the
/// policy's own, byte-identical to the existing engine call sites.
fn gate_read(root: &str, lock_to_root: bool, path: &str) -> Result<String, String> {
    let policy = PathPolicy::from_parts(root, lock_to_root, false, true);
    let resolved = policy.check_read(path)?;
    Ok(resolved.relative)
}

// ---------------------------------------------------------------------------
// files/list (§5: operations/list)
// ---------------------------------------------------------------------------

/// `files/list`: `path`, `recurse?` → `{entries:[FileEntry]}`.
///
/// Non-recursive by default; recursion is `opt: {"recurse": true}` (no
/// maxDepth — matches the unbounded OpenDAL `list_with(...).recursive(true)`).
/// The listed prefix's own marker entry is filtered and entries are sorted by
/// path, mirroring `engine::ops::list_raw`.
pub async fn list(
    client: &RcClient,
    fs: &str,
    path: &str,
    recurse: bool,
    root: &str,
    lock_to_root: bool,
) -> Result<Vec<FileEntry>, String> {
    let remote = gate_read(root, lock_to_root, path)?;
    let prefix = remote.trim_matches('/').to_string();
    let opt = if recurse {
        json!({ "recurse": true })
    } else {
        Value::Null
    };
    let response = client
        .operations_list(fs, &remote, opt)
        .await
        .map_err(|error| format!("Failed to list '{prefix}': {error}"))?;
    // `list` 缺失视为空目录（rcd 总是回 `{"list": []}`，防御旧版本）。
    let items = response
        .get("list")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let entries: Vec<FileEntry> = items.iter().map(entry_from_item).collect();
    Ok(filter_and_sort(entries, &prefix))
}

// ---------------------------------------------------------------------------
// files/listPaged (§5: operations/list 全量 + 切片)
// ---------------------------------------------------------------------------

/// `files/listPaged`: `path`, `page`, `pageSize` → `{entries, total}`.
///
/// Same in-memory slice as the OpenDAL engine (1-based page, empty slice past
/// the end, `total` always the real count) with one new guard: the full
/// enumeration must stay within [`LIST_PAGED_MAX`] or the call errors.
pub async fn list_paged(
    client: &RcClient,
    fs: &str,
    path: &str,
    page: u64,
    page_size: u64,
    root: &str,
    lock_to_root: bool,
) -> Result<(Vec<FileEntry>, u64), String> {
    let entries = list(client, fs, path, false, root, lock_to_root).await?;
    let total = entries.len() as u64;
    if entries.len() > LIST_PAGED_MAX {
        return Err(format!(
            "listing '{}' returned {} entries, exceeding the {LIST_PAGED_MAX} limit for \
             paged listing",
            path.trim().trim_matches('/'),
            entries.len()
        ));
    }
    let (start, end) = page_window(entries.len(), page, page_size);
    Ok((entries[start..end].to_vec(), total))
}

/// Pure pagination window over a full listing, byte-identical to
/// `engine::ops::list_paged`'s slicing (including `page_size: 0` acting as 1).
/// Returns the `[start, end)` slice bounds for a listing of `total` entries.
fn page_window(total: usize, page: u64, page_size: u64) -> (usize, usize) {
    let size = page_size.max(1);
    let start: usize = page
        .saturating_sub(1)
        .saturating_mul(size)
        .try_into()
        .unwrap_or(total);
    let start = start.min(total);
    let end = start.saturating_add(size as usize).min(total);
    (start, end)
}

// ---------------------------------------------------------------------------
// files/stat (§5: operations/stat, item:null = 不存在)
// ---------------------------------------------------------------------------

/// `files/stat`: `path` → `{entry}`.
///
/// `{"item": null}` (and a missing `item` key) maps to the existing
/// NotFound error semantics/style — `Failed to stat '<path>': <reason>` —
/// matching what OpenDAL backends surfaced through `main.rs`.
pub async fn stat(
    client: &RcClient,
    fs: &str,
    path: &str,
    root: &str,
    lock_to_root: bool,
) -> Result<FileEntry, String> {
    let remote = gate_read(root, lock_to_root, path)?;
    let response = client
        .operations_stat(fs, &remote)
        .await
        .map_err(|error| format!("Failed to stat '{}': {error}", remote.trim_matches('/')))?;
    let item = response.get("item").filter(|item| !item.is_null());
    let Some(item) = item else {
        return Err(format!(
            "Failed to stat '{}': path does not exist",
            remote.trim_matches('/')
        ));
    };
    Ok(entry_from_item(item))
}

// ---------------------------------------------------------------------------
// files/size (§5: operations/size → {count, bytes})
// ---------------------------------------------------------------------------

/// `files/size`: `path` → `(count, bytes)` (files only, recursive — rclone's
/// own semantics; dirs carry no size in the answer).
pub async fn size(
    client: &RcClient,
    fs: &str,
    path: &str,
    root: &str,
    lock_to_root: bool,
) -> Result<(u64, u64), String> {
    let remote = gate_read(root, lock_to_root, path)?;
    // operations/size 只读 fs 参数：子路径并入 fs 字符串（remote:path/sub）。
    let target = fs_with_path(fs, &remote);
    let response = client
        .call("operations/size", &json!({ "fs": target }))
        .await
        .map_err(|error| format!("Failed to size '{}': {error}", remote.trim_matches('/')))?;
    let count = response.get("count").and_then(Value::as_u64).unwrap_or(0);
    let bytes = response.get("bytes").and_then(Value::as_u64).unwrap_or(0);
    Ok((count, bytes))
}

// ---------------------------------------------------------------------------
// files/capabilities (§5: 每协议静态矩阵 + operations/fsinfo 覆盖)
// ---------------------------------------------------------------------------

/// `files/capabilities` → `Capabilities{scheme, list, write, read, stat,
/// delete, createDir, copy, rename, presign}`.
///
/// `scheme` is the backend type itself (the rclone type replaces the OpenDAL
/// scheme string). The static matrix is conservative: basic browse/write ops
/// for every backend, server-side copy/rename advertised for fs + object
/// stores, presign only for object stores. `operations/fsinfo` (the live
/// feature source — `backend/features` 404s on stock rcd) overrides the
/// boolean positions it actually reports; failures keep the static matrix.
pub async fn capabilities(
    client: &RcClient,
    fs: &str,
    backend_type: &str,
) -> Result<Capabilities, String> {
    let mut caps = static_capabilities(backend_type);
    if let Ok(response) = client.call("operations/fsinfo", &json!({ "fs": fs })).await {
        apply_features(&mut caps, response.get("Features"));
    }
    Ok(caps)
}

/// Conservative per-protocol baseline (plan §5: fs/s3 系全 true、presign 仅
/// 对象存储). Accepted spellings cover the manifest values and their rclone
/// aliases (`local`, `azureblob`).
fn static_capabilities(backend_type: &str) -> Capabilities {
    let is_fs = matches!(backend_type, "fs" | "local");
    let is_object = matches!(
        backend_type,
        "s3" | "oss" | "cos" | "obs" | "gcs" | "azblob" | "azureblob"
    );
    Capabilities {
        scheme: backend_type.to_string(),
        list: true,
        write: true,
        read: true,
        stat: true,
        delete: true,
        create_dir: true,
        copy: is_fs || is_object,
        rename: is_fs || is_object,
        presign: is_object,
    }
}

/// Overrides the boolean positions `operations/fsinfo` actually reports.
/// Feature keys are the live rcd `Features` object (capitalized Go fields).
fn apply_features(caps: &mut Capabilities, features: Option<&Value>) {
    let Some(features) = features else { return };
    let flag = |key: &str| features.get(key).and_then(Value::as_bool);
    if let Some(presign) = flag("PublicLink") {
        caps.presign = presign;
    }
    // `copy`/`rename` stay on the static baseline: fsinfo's Copy/Move flags
    // advertise server-side copy *optimization*, not capability — the local
    // backend reports Copy:false on Linux while operations/copyfile and
    // operations/rename work fine (container smoke, all protocols). The
    // streaming flags (PutStream/PutUnchecked/OpenWriterAt) report
    // unknown-size upload support, not writability — webdav reports
    // PutStream:false yet PUTs (and our known-size multipart uploadfile)
    // work fine, verified e2e.
}

// ---------------------------------------------------------------------------
// files/quickPaths (§5: 形状不变 — engine::ops::quick_paths 对齐)
// ---------------------------------------------------------------------------

/// `files/quickPaths` → `{"paths":[{"key","path"}]}`, shape-aligned with
/// `engine::ops::quick_paths`:
///
/// - every protocol gets the `root` chip;
/// - `fs` (local) additionally exposes the well-known user directories
///   (home/Desktop/Downloads/Documents/Pictures), but only when the root
///   doesn't confine them (`root` empty or `/`) and each candidate is
///   stat-verified against the live process;
/// - every other protocol (including bucket-based ones — buckets are browsed
///   through the normal root listing) stays root-only, as in the engine.
pub async fn quick_paths(
    client: &RcClient,
    fs: &str,
    backend_type: &str,
    root: &str,
) -> Result<Value, String> {
    let mut paths = vec![json!({ "key": "root", "path": "/" })];
    if !quick_paths_eligible(backend_type, root) {
        return Ok(json!({ "paths": paths }));
    }
    let Some(home) = home_dir() else {
        return Ok(json!({ "paths": paths }));
    };
    for (key, path) in home_candidates(&home) {
        if is_dir_remote(client, fs, &path).await.unwrap_or(false) {
            paths.push(json!({ "key": key, "path": path }));
        }
    }
    Ok(json!({ "paths": paths }))
}

/// User-directory chip eligibility (engine parity): fs-only and only when
/// nothing confines navigation via the root. (`lock_to_root` is enforced
/// per-request by [`gate_read`]; callers wiring locked connections should
/// keep passing the configured root so the chips fall back to root-only.)
fn quick_paths_eligible(backend_type: &str, root: &str) -> bool {
    matches!(backend_type, "fs" | "local") && (root.is_empty() || root == "/")
}

/// `$HOME` (Unix) with a `USERPROFILE` fallback; `None` keeps the chips at
/// the root-only fallback (same rule as the engine).
fn home_dir() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .filter(|value| value.starts_with('/'))
        .or_else(|| {
            std::env::var("USERPROFILE")
                .ok()
                .filter(|value| !value.is_empty())
        })
}

/// Well-known user directories under `$HOME` (Unix convention, engine parity).
fn home_candidates(home: &str) -> Vec<(&'static str, String)> {
    vec![
        ("home", home.to_string()),
        ("desktop", format!("{home}/Desktop")),
        ("downloads", format!("{home}/Downloads")),
        ("documents", format!("{home}/Documents")),
        ("pictures", format!("{home}/Pictures")),
    ]
}

/// Stat-verification for a chip candidate: directory present on `fs`.
async fn is_dir_remote(client: &RcClient, fs: &str, path: &str) -> Result<bool, String> {
    let remote = path.trim_start_matches('/');
    let response = client
        .operations_stat(fs, remote)
        .await
        .map_err(|error| error.to_string())?;
    Ok(response
        .get("item")
        .and_then(|item| item.get("IsDir"))
        .and_then(Value::as_bool)
        .unwrap_or(false))
}

// ---------------------------------------------------------------------------
// Phase B: file operations (§5 files/write|read|mkdir|rmdir|delete|purge|
// copy|move|rename|publicLink)
//
// Gate split: the connection-level `read_only` / `allow_delete` gates are
// enforced by the wiring layer (`main.rs::ensure_writable` /
// `ensure_deletable`, same rule as the OpenDAL call sites) BEFORE these ops
// run. The ops layer therefore builds its PathPolicy with the connection
// flags at their permissive defaults and only enforces what is structural
// here: the path whitelist, `lock_to_root`, and `purge`'s hard refusal of the
// connection root (defense in depth — the wiring re-checks both).
// ---------------------------------------------------------------------------

/// Policy view shared by the mutating Phase B ops (see the section comment).
fn write_policy(root: &str, lock_to_root: bool) -> PathPolicy {
    PathPolicy::from_parts(root, lock_to_root, false, true)
}

/// Write-side gate: path whitelist + `lock_to_root` (`mkdir`, copy/move
/// targets). Error text is the policy's own.
fn gate_write(root: &str, lock_to_root: bool, path: &str) -> Result<String, String> {
    write_policy(root, lock_to_root)
        .check_write(path)
        .map(|resolved| resolved.relative)
}

/// Delete-side gate: path whitelist + `lock_to_root` (`rmdir`,
/// `delete_file`).
fn gate_delete(root: &str, lock_to_root: bool, path: &str) -> Result<String, String> {
    write_policy(root, lock_to_root)
        .check_delete(path)
        .map(|resolved| resolved.relative)
}

/// Purge gate: [`gate_delete`] plus the hard refusal of the connection root
/// (`""`, `"/"`, or the configured root) — `policy::PathPolicy::check_purge`.
fn gate_purge(root: &str, lock_to_root: bool, path: &str) -> Result<String, String> {
    write_policy(root, lock_to_root)
        .check_purge(path)
        .map(|resolved| resolved.relative)
}

/// `files/read` byte layer: reads at most `max_bytes` bytes, returns
/// `(data, truncated)`.
///
/// Flow mirrors the OpenDAL `engine::ops::read`: stat first (missing → the
/// read-style error, directory → `it is a directory`), then fetch bytes via
/// the rc-serve channel with `Range: bytes=0-{max_bytes}` (INCLUSIVE end, so
/// `max_bytes + 1` bytes are requested). A body of exactly `max_bytes + 1`
/// proves the file is larger → `truncated`; a body of `≤ max_bytes` hit EOF
/// → complete. The same body-length verdict covers 200 (backend ignored the
/// Range) and 206 (honored) responses, and the read loop stops as soon as the
/// verdict is known so a Range-ignoring backend cannot stream a whole huge
/// file into memory.
///
/// The clamp to `MAX_PREVIEW_BYTES` and the default (256 KiB) belong to the
/// wiring layer (main.rs `files/read`), byte layer only here.
pub async fn read_prefix(
    client: &RcClient,
    fs: &str,
    remote: &str,
    max_bytes: u64,
) -> Result<(Vec<u8>, bool), String> {
    let remote = remote.trim_matches('/');
    let stat = client
        .operations_stat(fs, remote)
        .await
        .map_err(|error| format!("Failed to read '{remote}': {error}"))?;
    let item = stat.get("item").filter(|item| !item.is_null());
    let Some(item) = item else {
        return Err(format!("Failed to read '{remote}': path does not exist"));
    };
    if item.get("IsDir").and_then(Value::as_bool).unwrap_or(false) {
        return Err(format!("Cannot read '{remote}': it is a directory"));
    }
    // rc-serve 只认 `[{fs}]/{remote}`（模块头文档，实测 v1.68.0 = v1.75.1）。
    let mut response = client
        .serve_get(
            &serve_fs_string(fs),
            &encode_serve_path(remote),
            Some((0, Some(max_bytes))),
        )
        .await
        .map_err(|error| format!("Failed to read '{remote}': {error}"))?;
    let cap = max_bytes.saturating_add(1) as usize;
    let mut body: Vec<u8> = Vec::new();
    while body.len() < cap {
        match response
            .chunk()
            .await
            .map_err(|error| format!("Failed to read '{remote}': {error}"))?
        {
            Some(chunk) => {
                let take = (cap - body.len()).min(chunk.len());
                body.extend_from_slice(&chunk[..take]);
            }
            None => break,
        }
    }
    let truncated = body.len() > max_bytes as usize;
    body.truncate(max_bytes as usize);
    Ok((body, truncated))
}

/// rc-serve fs spelling: the fs rides bracket-wrapped in the URL path
/// (`[{fs}]/{remote}`); `read_prefix` is the only consumer.
fn serve_fs_string(fs: &str) -> String {
    format!("[{fs}]")
}

/// Minimal serve-URL escaping for the fs/remote path components: `%` must be
/// escaped (rcd percent-decodes `r.URL.Path` before matching, so a raw `%41`
/// would resurrect as `A`), `?` and `#` would split query/fragment in the
/// URL parser, `[`/`]` are the route delimiters themselves and stay literal.
/// Spaces and non-ASCII bytes are left to the URL parser's own path
/// percent-encoding. Byte-wise so UTF-8 names pass through untouched.
fn encode_serve_path(value: &str) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(value.len());
    for &byte in value.as_bytes() {
        match byte {
            b'%' => out.extend_from_slice(b"%25"),
            b'?' => out.extend_from_slice(b"%3F"),
            b'#' => out.extend_from_slice(b"%23"),
            other => out.push(other),
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| value.to_string())
}

/// `files/write` byte layer: writes `data` to `remote` (overwrite semantics).
///
/// rc `operations/uploadfile` destinations are `path.Join(remote, <part
/// filename>)` (module doc), and rc.rs pins the part filename to `payload` —
/// so the payload is uploaded into the target's PARENT directory and then
/// `movefile`d onto the target (move overwrites, verified live; parents
/// auto-create). The payload staging object is deleted if the move fails, and
/// the local staging file is always removed. Gates (read_only + path
/// whitelist) belong to the wiring layer.
pub async fn write_bytes(
    client: &RcClient,
    fs: &str,
    remote: &str,
    data: &[u8],
) -> Result<(), String> {
    let target = remote.trim().trim_matches('/').to_string();
    if target.is_empty() {
        return Err("Cannot write the connection root directory".to_string());
    }
    let (parent, payload_remote) = split_upload_target(&target);
    let staging = stage_bytes(data)?;
    let uploaded = client
        .operations_uploadfile(fs, &parent, &staging, None)
        .await;
    let _ = std::fs::remove_file(&staging);
    uploaded.map_err(|error| format!("Failed to write '{target}': {error}"))?;
    // Root-level targets whose basename is "payload" are already in place.
    if payload_remote != target {
        if let Err(error) = client
            .call(
                "operations/movefile",
                &json!({
                    "srcFs": fs,
                    "srcRemote": payload_remote,
                    "dstFs": fs,
                    "dstRemote": target,
                }),
            )
            .await
        {
            // 归位失败时清掉落盘的 staging 对象，避免目录里残留 payload。
            let _ = client
                .call(
                    "operations/deletefile",
                    &json!({ "fs": fs, "remote": payload_remote }),
                )
                .await;
            return Err(format!(
                "Failed to write '{target}': uploaded payload could not be moved \
                 into place: {error}"
            ));
        }
    }
    Ok(())
}

/// Upload staging geometry for [`write_bytes`]: where the payload lands and
/// what it must be moved to. `(parent_dir, payload_remote)`.
fn split_upload_target(target: &str) -> (String, String) {
    match target.rsplit_once('/') {
        Some((parent, _)) => (parent.to_string(), format!("{parent}/payload")),
        None => (String::new(), "payload".to_string()),
    }
}

/// Buffers `data` into a unique 0600 staging file (the rc multipart upload
/// streams from a disk path; rc.rs owns the client side).
fn stage_bytes(data: &[u8]) -> Result<std::path::PathBuf, String> {
    use std::io::Write as _;
    let path = std::env::temp_dir().join(format!(
        "dbx-files-write-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| format!("Failed to buffer write payload: {error}"))?;
    file.write_all(data)
        .map_err(|error| format!("Failed to buffer write payload: {error}"))?;
    Ok(path)
}

/// `files/mkdir` (§5: `operations/mkdir`): whitelist gate + engine parity —
/// creating the connection root itself is refused, and rc's mkdir is mkdir -p
/// (parents created in one call) and idempotent on existing directories.
pub async fn mkdir(
    client: &RcClient,
    fs: &str,
    remote: &str,
    root: &str,
    lock_to_root: bool,
) -> Result<(), String> {
    let relative = gate_write(root, lock_to_root, remote)?;
    if relative.is_empty() {
        return Err("Cannot create the connection root directory".to_string());
    }
    client
        .call("operations/mkdir", &json!({ "fs": fs, "remote": relative }))
        .await
        .map(|_| ())
        .map_err(|error| format!("Failed to create directory '{relative}': {error}"))
}

/// `files/rmdir` (§5: `operations/rmdir`): delete gate + the engine's
/// root refusal and empty-only semantics (stat → must be a directory, list →
/// must have no children, then rc's own rmdir).
pub async fn rmdir(
    client: &RcClient,
    fs: &str,
    remote: &str,
    root: &str,
    lock_to_root: bool,
) -> Result<(), String> {
    let relative = gate_delete(root, lock_to_root, remote)?;
    if relative.is_empty() {
        return Err(
            "Cannot remove the connection root directory; purge a subdirectory instead"
                .to_string(),
        );
    }
    let stat = client
        .operations_stat(fs, &relative)
        .await
        .map_err(|error| format!("Failed to stat '{relative}': {error}"))?;
    let item = stat.get("item").filter(|item| !item.is_null());
    let Some(item) = item else {
        return Err(format!(
            "Failed to remove directory '{relative}': path does not exist"
        ));
    };
    if !item
        .get("IsDir")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(format!("Cannot rmdir '{relative}': it is not a directory"));
    }
    let listing = client
        .operations_list(fs, &relative, Value::Null)
        .await
        .map_err(|error| format!("Failed to list '{relative}': {error}"))?;
    let children = listing
        .get("list")
        .and_then(Value::as_array)
        .map(|list| list.len())
        .unwrap_or(0);
    if children > 0 {
        return Err(format!(
            "Directory '{relative}' is not empty; use purge to delete it recursively"
        ));
    }
    client
        .call("operations/rmdir", &json!({ "fs": fs, "remote": relative }))
        .await
        .map(|_| ())
        .map_err(|error| format!("Failed to remove directory '{relative}': {error}"))
}

/// `files/delete` (§5: `operations/deletefile`): delete gate + OpenDAL
/// idempotency parity — rc 404s a missing object while the engine's delete is
/// a silent no-op, so a missing path (stat `item: null`) succeeds.
pub async fn delete_file(
    client: &RcClient,
    fs: &str,
    remote: &str,
    root: &str,
    lock_to_root: bool,
) -> Result<(), String> {
    let relative = gate_delete(root, lock_to_root, remote)?;
    if relative.is_empty() {
        return Err("Cannot delete the connection root directory".to_string());
    }
    let stat = client
        .operations_stat(fs, &relative)
        .await
        .map_err(|error| format!("Failed to delete '{relative}': {error}"))?;
    if stat.get("item").filter(|item| !item.is_null()).is_none() {
        return Ok(());
    }
    client
        .call(
            "operations/deletefile",
            &json!({ "fs": fs, "remote": relative }),
        )
        .await
        .map(|_| ())
        .map_err(|error| format!("Failed to delete '{relative}': {error}"))
}

/// `files/purge` (§5: `operations/purge`): `check_purge` gate (root red line
/// included, defense in depth against the wiring's own refusal) + OpenDAL
/// idempotency parity for missing paths.
pub async fn purge(
    client: &RcClient,
    fs: &str,
    remote: &str,
    root: &str,
    lock_to_root: bool,
) -> Result<(), String> {
    let relative = gate_purge(root, lock_to_root, remote)?;
    let stat = client
        .operations_stat(fs, &relative)
        .await
        .map_err(|error| format!("Failed to purge '{relative}': {error}"))?;
    if stat.get("item").filter(|item| !item.is_null()).is_none() {
        return Ok(());
    }
    client
        .call("operations/purge", &json!({ "fs": fs, "remote": relative }))
        .await
        .map(|_| ())
        .map_err(|error| format!("Failed to purge '{relative}': {error}"))
}

/// `files/copy` single-file byte path (§5: `operations/copyfile`, keys
/// `srcFs`/`srcRemote`/`dstFs`/`dstRemote` — live-pinned). The source is
/// read-semantics and passes through ungated (the wiring's connection-level
/// `ensure_writable` covers the request; same direction as the OpenDAL
/// `files/copy` arm), the destination goes through [`gate_write`].
/// Overwrite-on-existing destination is rclone's own behavior.
pub async fn copy_file(
    client: &RcClient,
    src_fs: &str,
    src_remote: &str,
    dst_fs: &str,
    dst_remote: &str,
    dst_root: &str,
    dst_lock_to_root: bool,
) -> Result<(), String> {
    let dst = gate_write(dst_root, dst_lock_to_root, dst_remote)?;
    let src = src_remote.trim().trim_matches('/');
    client
        .call(
            "operations/copyfile",
            &json!({
                "srcFs": src_fs,
                "srcRemote": src,
                "dstFs": dst_fs,
                "dstRemote": dst,
            }),
        )
        .await
        .map(|_| ())
        .map_err(|error| format!("Failed to copy '{src}' to '{dst}': {error}"))
}

/// `files/move` single-file byte path (§5: `operations/movefile`): identical
/// gating and parameter shape to [`copy_file`]. The wiring layer additionally
/// owes this op the connection-level delete gate (the source disappears —
/// same rule as the OpenDAL `files/move` arm's `ensure_deletable`); this
/// layer's signature carries no gate flags by design.
pub async fn move_file(
    client: &RcClient,
    src_fs: &str,
    src_remote: &str,
    dst_fs: &str,
    dst_remote: &str,
    dst_root: &str,
    dst_lock_to_root: bool,
) -> Result<(), String> {
    let dst = gate_write(dst_root, dst_lock_to_root, dst_remote)?;
    let src = src_remote.trim().trim_matches('/');
    client
        .call(
            "operations/movefile",
            &json!({
                "srcFs": src_fs,
                "srcRemote": src,
                "dstFs": dst_fs,
                "dstRemote": dst,
            }),
        )
        .await
        .map(|_| ())
        .map_err(|error| format!("Failed to move '{src}' to '{dst}': {error}"))
}

/// `files/rename` (§5: `operations/movefile` within one fs): both endpoints
/// gated through `policy::PathPolicy::check_rename` — path whitelist on the
/// source, write gate on the target, delete gate for the implicit source
/// removal (the connection flags ride at their wiring-enforced defaults, so
/// what binds here is the whitelist + root lock on both ends).
pub async fn rename(
    client: &RcClient,
    fs: &str,
    path: &str,
    new_path: &str,
    root: &str,
    lock_to_root: bool,
) -> Result<(), String> {
    let (src, dst) = write_policy(root, lock_to_root).check_rename(path, new_path)?;
    client
        .call(
            "operations/movefile",
            &json!({
                "srcFs": fs,
                "srcRemote": src.relative,
                "dstFs": fs,
                "dstRemote": dst.relative,
            }),
        )
        .await
        .map(|_| ())
        .map_err(|error| {
            format!("Failed to rename '{}' to '{}': {error}", src.relative, dst.relative)
        })
}

/// `files/publicLink` (§5: `operations/publiclink` → `{"url": …}`): read-side
/// gate, rc error passthrough for backends without public links (verified
/// live on local: `Local file system at … doesn't support public links`).
pub async fn public_link(
    client: &RcClient,
    fs: &str,
    remote: &str,
    root: &str,
    lock_to_root: bool,
) -> Result<String, String> {
    let relative = gate_read(root, lock_to_root, remote)?;
    let response = client
        .call(
            "operations/publiclink",
            &json!({ "fs": fs, "remote": relative }),
        )
        .await
        .map_err(|error| format!("Failed to presign '{}': {error}", relative.trim_matches('/')))?;
    response
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "Failed to presign '{}': rc response carries no url",
                relative.trim_matches('/')
            )
        })
}

// ---------------------------------------------------------------------------
// Entry mapping (operations/list / operations/stat item → FileEntry)
// ---------------------------------------------------------------------------

/// Maps one rc item into a `FileEntry`. Live-pinned keys: `Path`, `Name`,
/// `Size`, `ModTime` (RFC3339 string), `IsDir`; anything missing stays
/// `None` (serde skip) per the frontend contract. `path` carries the leading
/// `/` exactly like `engine::ops::entry_from_opendal`; dirs never carry
/// `size` (OpenDAL parity — rc reports inode sizes on local).
fn entry_from_item(item: &Value) -> FileEntry {
    let raw_path = item
        .get("Path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_matches('/')
        .to_string();
    let name = item
        .get("Name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            raw_path
                .rsplit('/')
                .next()
                .filter(|segment| !segment.is_empty())
                .unwrap_or("/")
                .to_string()
        });
    let is_dir = item.get("IsDir").and_then(Value::as_bool).unwrap_or(false);
    let size = if is_dir {
        None
    } else {
        item.get("Size")
            .and_then(Value::as_i64)
            .filter(|size| *size >= 0)
            .map(|size| size as u64)
    };
    let modified_at = modtime_to_millis(item.get("ModTime"));
    FileEntry {
        name,
        path: if raw_path.is_empty() {
            "/".to_string()
        } else {
            format!("/{raw_path}")
        },
        kind: if is_dir { "dir" } else { "file" },
        size,
        modified_at,
    }
}

/// `ModTime` → Unix epoch milliseconds. Live rclone sends an RFC3339 string;
/// numeric forms are accepted defensively (s/ms/µs/ns resolved by magnitude)
/// so intermediary layers that pre-marshal time as numbers still surface a
/// plausible timestamp. Unparseable/missing input stays `None` (serde skip).
fn modtime_to_millis(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::String(text) => {
            let parsed = chrono::DateTime::parse_from_rfc3339(text).ok()?;
            let millis = parsed.timestamp_millis();
            Some(millis.max(0) as u64)
        }
        Value::Number(number) => {
            let raw = number.as_f64()?;
            if raw < 0.0 {
                return None;
            }
            let millis = if raw >= 1e17 {
                raw / 1e6 // nanoseconds
            } else if raw >= 1e14 {
                raw / 1e3 // microseconds
            } else if raw >= 1e11 {
                raw // already milliseconds
            } else {
                raw * 1e3 // seconds
            };
            Some(millis as u64)
        }
        _ => None,
    }
}

/// Drops the listed prefix's own directory marker (empty `Path`, or `Path`
/// equal to the prefix with optional trailing slash) and sorts by path —
/// the rc analogue of `engine::ops::list_raw`'s marker filter.
fn filter_and_sort(mut entries: Vec<FileEntry>, prefix: &str) -> Vec<FileEntry> {
    entries.retain(|entry| {
        let trimmed = entry.path.trim_matches('/');
        !trimmed.is_empty() && trimmed != prefix
    });
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dead_client() -> RcClient {
        // Port 1 is never bound in tests; every rc call fails fast. Used by
        // gate tests that must fail BEFORE any HTTP traffic.
        RcClient::new(
            "http://127.0.0.1:1".to_string(),
            "u".to_string(),
            "p".to_string(),
        )
    }

    // -- local_fs_string ------------------------------------------------------

    #[test]
    fn local_fs_string_uses_root_or_slash() {
        assert_eq!(local_fs_string(""), "/");
        assert_eq!(local_fs_string("   "), "/");
        assert_eq!(local_fs_string("/tmp/data"), "/tmp/data");
        assert_eq!(local_fs_string(" /tmp/spaced "), "/tmp/spaced");
    }

    #[test]
    fn fs_with_path_joins_colon_and_local_forms() {
        assert_eq!(fs_with_path("dbxabc123:", ""), "dbxabc123:");
        assert_eq!(fs_with_path("dbxabc123:", "a/b"), "dbxabc123:a/b");
        assert_eq!(fs_with_path("/", "sub"), "/sub");
        assert_eq!(fs_with_path("/local/root", "sub/deep"), "/local/root/sub/deep");
        assert_eq!(fs_with_path("/local/root/", "x"), "/local/root/x");
    }

    // -- path gating（与 policy 文案一致） ------------------------------------

    #[tokio::test]
    async fn gate_locks_paths_before_any_rc_call() {
        let client = dead_client();
        let error = list(&client, "dbxdead:", "/etc/passwd", false, "/srv/data", true)
            .await
            .unwrap_err();
        assert!(
            error.contains("outside the locked connection root '/srv/data'"),
            "{error}"
        );
        let error = stat(&client, "dbxdead:", "../escape", "/srv/data", true)
            .await
            .unwrap_err();
        assert!(error.contains("escapes the connection root"), "{error}");
        let error = size(&client, "dbxdead:", "a\\b", "", false)
            .await
            .unwrap_err();
        assert!(error.contains("backslashes or control characters"), "{error}");
    }

    #[test]
    fn gate_read_resolves_relative_and_absolute_forms() {
        assert_eq!(gate_read("", false, "").unwrap(), "");
        assert_eq!(gate_read("", false, "/").unwrap(), "");
        assert_eq!(gate_read("", false, "sub/x").unwrap(), "sub/x");
        assert_eq!(gate_read("/srv/data", true, "sub/x").unwrap(), "sub/x");
        assert_eq!(gate_read("/srv/data", true, "/srv/data/sub").unwrap(), "sub");
        assert_eq!(gate_read("/srv/data", true, "").unwrap(), "");
        // Unlocked absolute outside paths still resolve inside the fs root —
        // the engine's Operator-root geometry (normalize_path 行为一致).
        assert_eq!(gate_read("/srv/data", false, "/etc/x").unwrap(), "etc/x");
    }

    // -- page_window（纯切片逻辑，含 listPaged 上限） -------------------------

    #[test]
    fn page_window_table() {
        assert_eq!(page_window(5, 1, 2), (0, 2));
        assert_eq!(page_window(5, 2, 2), (2, 4));
        assert_eq!(page_window(5, 3, 2), (4, 5));
        assert_eq!(page_window(5, 4, 2), (5, 5), "past-the-end page is empty");
        assert_eq!(page_window(5, 9, 2), (5, 5));
        assert_eq!(page_window(2, 1, 10), (0, 2));
        assert_eq!(page_window(0, 1, 10), (0, 0));
        assert_eq!(page_window(5, 1, 0), (0, 1), "page_size 0 acts as 1 (engine parity)");
        assert_eq!(page_window(5, 2, 0), (1, 2));
        assert_eq!(page_window(5, 1, u64::MAX), (0, 5), "oversize page clamps at total");
        assert_eq!(page_window(5, 2, u64::MAX), (5, 5), "start past the end is an empty page");
    }

    #[test]
    fn list_paged_cap_constant() {
        assert_eq!(LIST_PAGED_MAX, 100_000);
    }

    // -- ModTime 两种形态（RFC3339 字符串 / 防御性数字） ----------------------

    #[test]
    fn modtime_rfc3339_string_parses_to_millis() {
        let value = json!("2026-09-18T14:53:50.448025022+08:00");
        let millis = modtime_to_millis(Some(&value)).unwrap();
        // 14:53:50.448 +08:00 == 06:53:50.448Z
        assert_eq!(millis, 1_789_714_430_448);
        let utc = json!("2024-01-02T03:04:05Z");
        assert_eq!(modtime_to_millis(Some(&utc)).unwrap(), 1_704_164_645_000);
    }

    #[test]
    fn modtime_numeric_and_missing_forms() {
        assert_eq!(
            modtime_to_millis(Some(&json!(1_700_000_000i64))).unwrap(),
            1_700_000_000_000,
            "seconds by magnitude"
        );
        assert_eq!(
            modtime_to_millis(Some(&json!(1_700_000_000_000i64))).unwrap(),
            1_700_000_000_000,
            "milliseconds pass through"
        );
        assert_eq!(
            modtime_to_millis(Some(&json!(1_700_000_000_000_000_000i64))).unwrap(),
            1_700_000_000_000,
            "nanoseconds reduce"
        );
        assert_eq!(modtime_to_millis(Some(&json!(-5))), None);
        assert_eq!(modtime_to_millis(Some(&json!("not-a-time"))), None);
        assert_eq!(modtime_to_millis(Some(&Value::Null)), None);
        assert_eq!(modtime_to_millis(None), None);
    }

    // -- 条目 JSON → FileEntry 映射 -------------------------------------------

    #[test]
    fn entry_from_item_maps_live_response_shape() {
        let item = json!({
            "Path": "sub/nested.txt",
            "Name": "nested.txt",
            "Size": 2,
            "MimeType": "text/plain; charset=utf-8",
            "ModTime": "2026-09-18T14:53:50.448732016+08:00",
            "IsDir": false
        });
        let entry = entry_from_item(&item);
        assert_eq!(entry.name, "nested.txt");
        assert_eq!(entry.path, "/sub/nested.txt");
        assert_eq!(entry.kind, "file");
        assert_eq!(entry.size, Some(2));
        assert!(entry.modified_at.is_some());
        let text = serde_json::to_string(&entry).unwrap();
        assert!(text.contains("\"kind\":\"file\""), "{text}");
        assert!(text.contains("\"path\":\"/sub/nested.txt\""), "{text}");
        assert!(text.contains("\"modifiedAt\":"), "{text}");
    }

    #[test]
    fn entry_from_item_dir_and_sparse_fields() {
        let dir = json!({
            "Path": "sub",
            "Name": "sub",
            "Size": 96,
            "MimeType": "inode/directory",
            "ModTime": "2026-09-18T14:53:50.448704474+08:00",
            "IsDir": true
        });
        let entry = entry_from_item(&dir);
        assert_eq!(entry.kind, "dir");
        assert_eq!(entry.size, None, "dirs never carry size (OpenDAL parity)");
        assert!(entry.modified_at.is_some());

        // Missing ModTime / Name → derived or skipped fields.
        let sparse = json!({ "Path": "a/b.txt", "Size": 5, "IsDir": false });
        let entry = entry_from_item(&sparse);
        assert_eq!(entry.name, "b.txt");
        assert_eq!(entry.size, Some(5));
        assert_eq!(entry.modified_at, None);
        let text = serde_json::to_string(&entry).unwrap();
        assert!(!text.contains("modifiedAt"), "skip_serializing on None: {text}");

        // Root marker spelling → engine's "/" path.
        let root = json!({ "Path": "", "Name": "", "Size": -1, "IsDir": true });
        let entry = entry_from_item(&root);
        assert_eq!(entry.path, "/");
        assert_eq!(entry.name, "/");
        assert_eq!(entry.size, None, "negative size never leaks");
    }

    // -- 标记项过滤 + 排序 -----------------------------------------------------

    #[test]
    fn filter_and_sort_drops_marker_and_orders_by_path() {
        let entries = vec![
            FileEntry { name: "z.txt".into(), path: "/z.txt".into(), kind: "file", size: Some(1), modified_at: None },
            FileEntry { name: "sub".into(), path: "/sub".into(), kind: "dir", size: None, modified_at: None },
            FileEntry { name: "a.txt".into(), path: "/a.txt".into(), kind: "file", size: Some(1), modified_at: None },
        ];
        let filtered = filter_and_sort(entries, "sub");
        let paths: Vec<&str> = filtered.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(paths, vec!["/a.txt", "/z.txt"]);

        // Prefix marker with trailing slash + empty root marker both drop.
        let markers = vec![
            FileEntry { name: "sub".into(), path: "/sub/".into(), kind: "dir", size: None, modified_at: None },
            FileEntry { name: "/".into(), path: "/".into(), kind: "dir", size: None, modified_at: None },
            FileEntry { name: "keep".into(), path: "/keep".into(), kind: "dir", size: None, modified_at: None },
        ];
        let filtered = filter_and_sort(markers, "sub");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].path, "/keep");
    }

    // -- capabilities 矩阵 -----------------------------------------------------

    #[test]
    fn static_capabilities_matrix() {
        let fs_caps = static_capabilities("fs");
        assert_eq!(fs_caps.scheme, "fs");
        assert!(fs_caps.list && fs_caps.write && fs_caps.read && fs_caps.stat);
        assert!(fs_caps.delete && fs_caps.create_dir && fs_caps.copy && fs_caps.rename);
        assert!(!fs_caps.presign, "local fs has no presign");

        for scheme in ["s3", "oss", "cos", "obs", "gcs", "azblob"] {
            let caps = static_capabilities(scheme);
            assert!(caps.presign, "{scheme} is object storage");
            assert!(caps.copy && caps.rename, "{scheme}");
        }
        for scheme in ["webdav", "ftp", "sftp", "smb", "drive"] {
            let caps = static_capabilities(scheme);
            assert!(caps.list && caps.write && caps.read && caps.stat && caps.delete, "{scheme}");
            assert!(!caps.presign && !caps.copy && !caps.rename, "{scheme} conservative baseline");
        }
    }

    #[test]
    fn apply_features_overrides_reported_positions() {
        let mut caps = static_capabilities("s3");
        apply_features(
            &mut caps,
            Some(&json!({
                "Copy": false,
                "Move": false,
                "PublicLink": false,
                "PutStream": false,
                "OpenWriterAt": false
            })),
        );
        // Copy/Move/streaming flags describe server-side optimization and
        // unknown-size streaming — platform-reported false must not demote
        // the capability baseline (local backend: Copy:false on Linux,
        // operations/copyfile fine; webdav: PutStream:false, PUTs fine).
        assert!(caps.copy, "fsinfo Copy=false must not demote copy");
        assert!(caps.rename, "fsinfo Move=false must not demote rename");
        assert!(caps.write, "streaming flags must not demote write");
        assert!(!caps.presign, "PublicLink=false demotes presign");

        // Missing features leave the static baseline untouched.
        let mut caps = static_capabilities("smb");
        apply_features(&mut caps, Some(&json!({})));
        assert!(!caps.copy && !caps.rename && !caps.presign);
        assert!(caps.write);
        apply_features(&mut caps, None);
        assert!(caps.write);
    }

    #[test]
    fn capabilities_serializes_full_frontend_contract() {
        let caps = static_capabilities("fs");
        let text = serde_json::to_string(&caps).unwrap();
        for key in [
            "scheme", "list", "write", "read", "stat", "delete", "createDir", "copy", "rename",
            "presign",
        ] {
            assert!(text.contains(&format!("\"{key}\":")), "missing {key}: {text}");
        }
    }

    #[test]
    fn quick_paths_eligibility_and_candidates() {
        assert!(quick_paths_eligible("fs", ""));
        assert!(quick_paths_eligible("fs", "/"));
        assert!(!quick_paths_eligible("fs", "/srv/data"));
        assert!(!quick_paths_eligible("s3", ""));
        assert!(!quick_paths_eligible("local", "/srv/data"));

        let candidates = home_candidates("/Users/jin");
        let (keys, paths): (Vec<&str>, Vec<String>) =
            candidates.into_iter().map(|(key, path)| (key, path)).unzip();
        assert_eq!(
            keys,
            vec!["home", "desktop", "downloads", "documents", "pictures"]
        );
        assert_eq!(paths[0], "/Users/jin");
        assert_eq!(paths[4], "/Users/jin/Pictures");
    }

    #[tokio::test]
    async fn quick_paths_root_only_for_non_fs_without_rc() {
        // Non-fs eligibility short-circuits before any HTTP traffic.
        let payload = quick_paths(&dead_client(), "dbxdead:", "s3", "").await.unwrap();
        assert_eq!(
            payload["paths"],
            json!([{ "key": "root", "path": "/" }])
        );
    }

    // -- 活进程测试（无 rclone 二进制则跳过，CI 保绿） -------------------------

    /// rclone 活进程 + tempdir 目录树：
    /// `a.txt`(5B) / `big.bin`(4096B) / `sub/nested.txt`(2B) / `empty-dir/`。
    struct Live {
        client: RcClient,
        fs: String,
        _dir: tempfile::TempDir,
    }

    impl Live {
        async fn start() -> Option<Live> {
            let Some(binary) = super::super::proc::resolve_binary() else {
                eprintln!("skipping: no rclone binary found");
                return None;
            };
            let handle = super::super::proc::RcdHandle::start(&binary, None)
                .await
                .expect("rcd should spawn");
            let client = handle.client();
            std::mem::forget(handle); // tests are process-exit scoped; keep rcd alive
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(dir.path().join("sub")).unwrap();
            std::fs::create_dir_all(dir.path().join("empty-dir")).unwrap();
            std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
            std::fs::write(dir.path().join("big.bin"), vec![0u8; 4096]).unwrap();
            std::fs::write(dir.path().join("sub/nested.txt"), b"zz").unwrap();
            Some(Live {
                client,
                fs: local_fs_string(dir.path().to_string_lossy().as_ref()),
                _dir: dir,
            })
        }
    }

    #[tokio::test]
    async fn live_list_flat_and_recurse_sorted_and_filtered() {
        let Some(live) = Live::start().await else { return };
        let flat = list(&live.client, &live.fs, "", false, "", false)
            .await
            .expect("flat list");
        let names: Vec<&str> = flat.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "big.bin", "empty-dir", "sub"], "sorted by path");
        assert_eq!(flat[0].path, "/a.txt");
        assert_eq!(flat[0].kind, "file");
        assert_eq!(flat[0].size, Some(5));
        assert!(flat[0].modified_at.is_some(), "ModTime RFC3339 → millis");
        assert_eq!(flat[2].kind, "dir");
        assert_eq!(flat[2].size, None, "dirs carry no size");

        let recursive = list(&live.client, &live.fs, "", true, "", false)
            .await
            .expect("recursive list");
        let paths: Vec<&str> = recursive.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["/a.txt", "/big.bin", "/empty-dir", "/sub", "/sub/nested.txt"]
        );
        assert!(recursive.iter().any(|entry| entry.kind == "dir" && entry.name == "sub"));

        // Subdirectory listing keeps the rc-prefixed path shape, no marker.
        let sub = list(&live.client, &live.fs, "sub", false, "", false)
            .await
            .expect("sub list");
        assert_eq!(sub.len(), 1);
        assert_eq!(sub[0].path, "/sub/nested.txt");

        // Missing directory is a real rc error (engine error style).
        let error = list(&live.client, &live.fs, "nope", false, "", false)
            .await
            .unwrap_err();
        assert!(error.contains("Failed to list 'nope'"), "{error}");
    }

    #[tokio::test]
    async fn live_list_paged_slices_with_total() {
        let Some(live) = Live::start().await else { return };
        let (page, total) = list_paged(&live.client, &live.fs, "", 1, 2, "", false)
            .await
            .unwrap();
        assert_eq!(total, 4);
        assert_eq!(page.len(), 2);
        let (page, total) = list_paged(&live.client, &live.fs, "", 2, 2, "", false)
            .await
            .unwrap();
        assert_eq!(total, 4);
        assert_eq!(page.len(), 2);
        let (page, total) = list_paged(&live.client, &live.fs, "", 9, 2, "", false)
            .await
            .unwrap();
        assert_eq!(total, 4, "out-of-range page keeps the real total");
        assert!(page.is_empty());
    }

    #[tokio::test]
    async fn live_stat_file_dir_root_and_missing() {
        let Some(live) = Live::start().await else { return };
        let file = stat(&live.client, &live.fs, "a.txt", "", false).await.unwrap();
        assert_eq!(file.path, "/a.txt");
        assert_eq!(file.kind, "file");
        assert_eq!(file.size, Some(5));
        assert!(file.modified_at.is_some());

        let dir = stat(&live.client, &live.fs, "sub", "", false).await.unwrap();
        assert_eq!(dir.path, "/sub");
        assert_eq!(dir.kind, "dir");
        assert_eq!(dir.size, None);

        let root = stat(&live.client, &live.fs, "/", "", false).await.unwrap();
        assert_eq!(root.path, "/");
        assert_eq!(root.name, "/");
        assert_eq!(root.kind, "dir");

        // item:null → NotFound-style business error (plan §5).
        let error = stat(&live.client, &live.fs, "definitely-missing", "", false)
            .await
            .unwrap_err();
        assert!(error.contains("Failed to stat"), "{error}");
    }

    #[tokio::test]
    async fn live_size_recursive_counts_files_only() {
        let Some(live) = Live::start().await else { return };
        let (count, bytes) = size(&live.client, &live.fs, "", "", false).await.unwrap();
        assert_eq!(count, 3, "a.txt + big.bin + nested.txt, dirs excluded");
        assert_eq!(bytes, 5 + 4096 + 2);

        // Subdirectory sizing goes through the combined fs path.
        let (count, bytes) = size(&live.client, &live.fs, "sub", "", false).await.unwrap();
        assert_eq!(count, 1);
        assert_eq!(bytes, 2);

        let error = size(&live.client, &live.fs, "nope", "", false).await.unwrap_err();
        assert!(error.contains("Failed to size 'nope'"), "{error}");
    }

    #[tokio::test]
    async fn live_capabilities_shape_and_fsinfo_overrides() {
        let Some(live) = Live::start().await else { return };
        let caps = capabilities(&live.client, &live.fs, "fs").await.unwrap();
        assert_eq!(caps.scheme, "fs");
        assert!(caps.list && caps.write && caps.read && caps.stat);
        assert!(caps.delete && caps.create_dir && caps.copy && caps.rename);
        assert!(!caps.presign, "local fs PublicLink=false");
        let text = serde_json::to_string(&caps).unwrap();
        for key in [
            "scheme", "list", "write", "read", "stat", "delete", "createDir", "copy", "rename",
            "presign",
        ] {
            assert!(text.contains(&format!("\"{key}\":")), "missing {key}: {text}");
        }
    }

    #[tokio::test]
    async fn live_quick_paths_root_only_inside_confined_fs() {
        let Some(live) = Live::start().await else { return };
        // tempdir root → home candidates resolve outside it and are dropped;
        // configured root → eligibility off entirely. Both stay root-only.
        for root in ["", live.fs.as_str()] {
            let payload = quick_paths(&live.client, &live.fs, "fs", root).await.unwrap();
            assert_eq!(
                payload["paths"],
                json!([{ "key": "root", "path": "/" }]),
                "root={root:?}"
            );
        }
    }

    #[tokio::test]
    async fn live_quick_paths_fs_candidates_stat_verified() {
        let Some(live) = Live::start().await else { return };
        // fs rooted at "/" so the real $HOME candidates are stat-verifiable;
        // the shape contract is what matters (root first, key+path pairs).
        let payload = quick_paths(&live.client, "/", "fs", "").await.unwrap();
        let paths = payload["paths"].as_array().expect("paths array");
        assert!(!paths.is_empty(), "root chip always present");
        assert_eq!(paths[0], json!({ "key": "root", "path": "/" }));
        for chip in paths {
            assert!(chip.get("key").and_then(Value::as_str).is_some(), "{chip}");
            assert!(chip.get("path").and_then(Value::as_str).is_some(), "{chip}");
        }
    }

    // -- Phase B：门禁拒绝路径（dead client：任何 HTTP 都会变成 transport 文案）
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn phase_b_gates_refuse_before_any_rc_call() {
        let client = dead_client();
        let locked = "/srv/data";
        // lock_to_root 逃逸：七类操作的路径白名单都在 HTTP 之前拒绝。
        let escaped = vec![
            mkdir(&client, "dbx:", "/etc/x", locked, true).await.unwrap_err(),
            rmdir(&client, "dbx:", "/etc/x", locked, true).await.unwrap_err(),
            delete_file(&client, "dbx:", "/etc/x", locked, true)
                .await
                .unwrap_err(),
            purge(&client, "dbx:", "/etc/x", locked, true).await.unwrap_err(),
            rename(&client, "dbx:", "ok.txt", "/etc/x", locked, true)
                .await
                .unwrap_err(),
            copy_file(&client, "src:", "s.txt", "dst:", "/etc/x", locked, true)
                .await
                .unwrap_err(),
            move_file(&client, "src:", "s.txt", "dst:", "/etc/x", locked, true)
                .await
                .unwrap_err(),
            public_link(&client, "dbx:", "/etc/x", locked, true)
                .await
                .unwrap_err(),
        ];
        for error in escaped {
            assert!(
                error.contains("outside the locked connection root"),
                "{error}"
            );
        }
        // 遍历逃逸在 policy 的 sanitize 阶段就被拒（先于锁检查，Phase A 同语义）。
        let error = purge(&client, "dbx:", "../e", locked, true).await.unwrap_err();
        assert!(error.contains("escapes the connection root"), "{error}");
        // 反斜杠路径同样在 HTTP 之前拒绝。
        let error = delete_file(&client, "dbx:", "a\\b", "", false)
            .await
            .unwrap_err();
        assert!(error.contains("backslashes or control characters"), "{error}");
        // purge 根红线（policy::check_purge，与接线层 refuse_root_purge 同源）。
        for path in ["", "/"] {
            let error = purge(&client, "dbx:", path, "", false).await.unwrap_err();
            assert!(error.contains("refusing to purge"), "{path}: {error}");
        }
        // 结构性根拒绝（写/建/删根）。
        let error = write_bytes(&client, "dbx:", "", b"x").await.unwrap_err();
        assert!(error.contains("Cannot write the connection root"), "{error}");
        let error = mkdir(&client, "dbx:", "", "", false).await.unwrap_err();
        assert!(error.contains("Cannot create the connection root"), "{error}");
        let error = rmdir(&client, "dbx:", "", "", false).await.unwrap_err();
        assert!(error.contains("Cannot remove the connection root"), "{error}");
        let error = delete_file(&client, "dbx:", "", "", false).await.unwrap_err();
        assert!(error.contains("Cannot delete the connection root"), "{error}");
    }

    // -- Phase B：纯辅助函数表测 ------------------------------------------------

    #[test]
    fn serve_fs_string_wraps_brackets() {
        assert_eq!(serve_fs_string("/tmp/work"), "[/tmp/work]");
        assert_eq!(serve_fs_string("/"), "[/]");
        assert_eq!(serve_fs_string("dbxAb12:"), "[dbxAb12:]");
        assert_eq!(serve_fs_string("dbxAb12:srv/data"), "[dbxAb12:srv/data]");
    }

    #[test]
    fn encode_serve_path_escapes_url_delimiters_only() {
        assert_eq!(encode_serve_path("[/tmp/work]"), "[/tmp/work]", "定界符保留字面");
        assert_eq!(encode_serve_path("50%25.txt"), "50%2525.txt", "% 预转义");
        assert_eq!(encode_serve_path("a?b#c"), "a%3Fb%23c", "query/fragment 定界符");
        assert_eq!(
            encode_serve_path("sub dir/报 告.txt"),
            "sub dir/报 告.txt",
            "空格与非 ASCII 交给 URL 解析器编码，字节透传不破坏 UTF-8"
        );
    }

    #[test]
    fn split_upload_target_places_payload_beside_target() {
        assert_eq!(
            split_upload_target("out/hello.txt"),
            ("out".to_string(), "out/payload".to_string())
        );
        assert_eq!(
            split_upload_target("deep/a/b.txt"),
            ("deep/a".to_string(), "deep/a/payload".to_string())
        );
        assert_eq!(
            split_upload_target("hello.txt"),
            (String::new(), "payload".to_string())
        );
    }

    // -- Phase B：活进程测试（无 rclone 二进制则跳过，CI 保绿） ------------------

    #[tokio::test]
    async fn live_write_bytes_read_prefix_roundtrip() {
        let Some(live) = Live::start().await else { return };
        // 嵌套写：uploadfile 自动建父目录 + movefile 归位。
        write_bytes(&live.client, &live.fs, "out/hello.txt", b"hello")
            .await
            .unwrap();
        let entry = stat(&live.client, &live.fs, "out/hello.txt", "", false)
            .await
            .unwrap();
        assert_eq!(entry.kind, "file");
        assert_eq!(entry.size, Some(5));

        // 全量读：文件 ≤ max_bytes → 完整 + 未截断。
        let (data, truncated) =
            read_prefix(&live.client, &live.fs, "out/hello.txt", 256).await.unwrap();
        assert_eq!(data, b"hello");
        assert!(!truncated);

        // 截断档：Range bytes=0-4（含端，请求 5B），body=5 > 4 → 截到 4。
        let (data, truncated) =
            read_prefix(&live.client, &live.fs, "out/hello.txt", 4).await.unwrap();
        assert_eq!(data, b"hell");
        assert!(truncated);

        // 大文件截断档：4096B 的 big.bin 读前 256B。
        let (data, truncated) =
            read_prefix(&live.client, &live.fs, "big.bin", 256).await.unwrap();
        assert_eq!(data.len(), 256);
        assert!(truncated);

        // 覆盖写（第二版更长，验证 movefile 覆盖 + 尺寸更新）。
        write_bytes(&live.client, &live.fs, "out/hello.txt", b"longer payload!")
            .await
            .unwrap();
        let entry = stat(&live.client, &live.fs, "out/hello.txt", "", false)
            .await
            .unwrap();
        assert_eq!(entry.size, Some(15));
        let (data, _) = read_prefix(&live.client, &live.fs, "out/hello.txt", 256)
            .await
            .unwrap();
        assert_eq!(data, b"longer payload!");

        // 根级写 + 空文件。
        write_bytes(&live.client, &live.fs, "root.txt", b"").await.unwrap();
        let entry = stat(&live.client, &live.fs, "root.txt", "", false)
            .await
            .unwrap();
        assert_eq!(entry.size, Some(0));
        let (data, truncated) =
            read_prefix(&live.client, &live.fs, "root.txt", 256).await.unwrap();
        assert!(data.is_empty());
        assert!(!truncated);

        // staging 归位后无 payload 残留。
        let entries = list(&live.client, &live.fs, "out", false, "", false)
            .await
            .unwrap();
        assert!(
            !entries.iter().any(|entry| entry.name == "payload"),
            "staging payload must be moved away: {entries:?}"
        );

        // 读目录 / 读缺失：engine read 语义（stat 守卫）。
        let error = read_prefix(&live.client, &live.fs, "sub", 256).await.unwrap_err();
        assert!(error.contains("it is a directory"), "{error}");
        let error = read_prefix(&live.client, &live.fs, "ghost.bin", 256)
            .await
            .unwrap_err();
        assert!(error.contains("path does not exist"), "{error}");

        // 写根被拒。
        let error = write_bytes(&live.client, &live.fs, "", b"x").await.unwrap_err();
        assert!(error.contains("Cannot write the connection root"), "{error}");
    }

    #[tokio::test]
    async fn live_mkdir_rmdir_structure_ops() {
        let Some(live) = Live::start().await else { return };
        // mkdir 幂等（rc 对已存在目录返回 {}）+ 一次调用建整条父链（mkdir -p）。
        mkdir(&live.client, &live.fs, "made", "", false).await.unwrap();
        mkdir(&live.client, &live.fs, "made", "", false).await.unwrap();
        let entry = stat(&live.client, &live.fs, "made", "", false).await.unwrap();
        assert_eq!(entry.kind, "dir");
        mkdir(&live.client, &live.fs, "deep/a/b", "", false).await.unwrap();
        let entry = stat(&live.client, &live.fs, "deep", "", false).await.unwrap();
        assert_eq!(entry.kind, "dir");

        // 根拒绝。
        let error = mkdir(&live.client, &live.fs, "", "", false).await.unwrap_err();
        assert!(error.contains("Cannot create the connection root"), "{error}");
        let error = rmdir(&live.client, &live.fs, "", "", false).await.unwrap_err();
        assert!(error.contains("Cannot remove the connection root"), "{error}");

        // rmdir 非空拒绝（engine 同款文案）。
        let error = rmdir(&live.client, &live.fs, "sub", "", false).await.unwrap_err();
        assert!(error.contains("is not empty"), "{error}");

        // rmdir 缺失 → 错误（engine 对缺失路径也是错误语义）。
        let error = rmdir(&live.client, &live.fs, "ghost", "", false)
            .await
            .unwrap_err();
        assert!(error.contains("does not exist"), "{error}");

        // rmdir 空目录成功，逐级收敛。
        rmdir(&live.client, &live.fs, "deep/a/b", "", false)
            .await
            .unwrap();
        rmdir(&live.client, &live.fs, "deep/a", "", false).await.unwrap();
        rmdir(&live.client, &live.fs, "deep", "", false).await.unwrap();
        let error = stat(&live.client, &live.fs, "deep", "", false).await.unwrap_err();
        assert!(error.contains("does not exist"), "{error}");

        // rmdir 文件被拒。
        let error = rmdir(&live.client, &live.fs, "a.txt", "", false)
            .await
            .unwrap_err();
        assert!(error.contains("it is not a directory"), "{error}");
    }

    #[tokio::test]
    async fn live_copy_move_rename_delete_purge_chain() {
        let Some(live) = Live::start().await else { return };
        write_bytes(&live.client, &live.fs, "chain/a.txt", b"AAA")
            .await
            .unwrap();

        // copy：双方都在 + 内容一致。
        copy_file(
            &live.client,
            &live.fs,
            "chain/a.txt",
            &live.fs,
            "chain/copy.txt",
            "",
            false,
        )
        .await
        .unwrap();
        let copied = stat(&live.client, &live.fs, "chain/copy.txt", "", false)
            .await
            .unwrap();
        assert_eq!(copied.size, Some(3));
        let (data, _) = read_prefix(&live.client, &live.fs, "chain/copy.txt", 256)
            .await
            .unwrap();
        assert_eq!(data, b"AAA");
        assert!(stat(&live.client, &live.fs, "chain/a.txt", "", false)
            .await
            .is_ok());

        // move：源消失、目标在。
        move_file(
            &live.client,
            &live.fs,
            "chain/copy.txt",
            &live.fs,
            "chain/moved.txt",
            "",
            false,
        )
        .await
        .unwrap();
        assert!(stat(&live.client, &live.fs, "chain/copy.txt", "", false)
            .await
            .is_err());
        assert!(stat(&live.client, &live.fs, "chain/moved.txt", "", false)
            .await
            .is_ok());

        // rename（同 fs movefile，两端门禁在单测覆盖）。
        rename(
            &live.client,
            &live.fs,
            "chain/moved.txt",
            "chain/renamed.txt",
            "",
            false,
        )
        .await
        .unwrap();
        assert!(stat(&live.client, &live.fs, "chain/moved.txt", "", false)
            .await
            .is_err());
        let error = rename(
            &live.client,
            &live.fs,
            "chain/ghost.txt",
            "chain/next.txt",
            "",
            false,
        )
        .await
        .unwrap_err();
        assert!(error.contains("Failed to rename"), "{error}");

        // delete：幂等（第二次删除缺失路径仍 Ok，OpenDAL parity）。
        delete_file(&live.client, &live.fs, "chain/renamed.txt", "", false)
            .await
            .unwrap();
        delete_file(&live.client, &live.fs, "chain/renamed.txt", "", false)
            .await
            .unwrap();
        assert!(stat(&live.client, &live.fs, "chain/renamed.txt", "", false)
            .await
            .is_err());

        // purge 子目录：递归删除 + stat 终态。
        write_bytes(&live.client, &live.fs, "chain/nest/deep.txt", b"D")
            .await
            .unwrap();
        purge(&live.client, &live.fs, "chain/nest", "", false)
            .await
            .unwrap();
        assert!(stat(&live.client, &live.fs, "chain/nest", "", false)
            .await
            .is_err());

        // purge 根拒绝（不发请求，policy 红线）。
        for path in ["", "/"] {
            let error = purge(&live.client, &live.fs, path, "", false)
                .await
                .unwrap_err();
            assert!(error.contains("refusing to purge"), "{path}: {error}");
        }

        // purge 缺失路径：幂等 Ok（OpenDAL parity，stat-first 容忍）。
        purge(&live.client, &live.fs, "chain/nest", "", false)
            .await
            .unwrap();

        // 收尾。
        purge(&live.client, &live.fs, "chain", "", false).await.unwrap();
        assert!(stat(&live.client, &live.fs, "chain", "", false)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn live_public_link_local_error_passthrough() {
        let Some(live) = Live::start().await else { return };
        write_bytes(&live.client, &live.fs, "pl.txt", b"share").await.unwrap();
        let error = public_link(&live.client, &live.fs, "pl.txt", "", false)
            .await
            .unwrap_err();
        assert!(
            error.contains("doesn't support public links"),
            "rc 错误必须透传: {error}"
        );
    }
}
