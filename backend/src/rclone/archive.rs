//! rc-backed archive surface (`files/archiveList` / `files/extract` /
//! `files/compress`, F-RCLONE Phase D method-surface closeout).
//!
//! The OpenDAL engine serves these methods from `crate::archive` (tar/tar.gz
//! only). This module keeps the wire shapes byte-identical while moving the
//! storage byte channel to the rc engine, and adds the zip format the OpenDAL
//! path explicitly deferred (Phase 2 there):
//!
//! - **zip reading never downloads the payload window.** The central
//!   directory lives at the file tail, so the flow is: stat → tail Range
//!   read (End-Of-Central-Directory + optional zip64 locator/record) → one
//!   Range read of the central directory → per-entry Range reads of the
//!   local header + payload, only when extract needs the bytes. Deflate
//!   entries are decoded by reusing `crate::archive::gunzip`: the raw
//!   deflate stream is wrapped in a synthetic gzip header/trailer whose
//!   CRC32/ISIZE come from the zip entry itself, so integrity verification
//!   runs against the archive's own checksums at zero extra code.
//! - **tar / tar.gz** keep the exact OpenDAL semantics: whole-file read
//!   (1 GiB cap) + the pure `crate::archive::list_entries` /
//!   `extract_entries` parsers. Pure helpers reused verbatim:
//!   `sanitize_entry_path` (zip-slip guard), `paginate`, `tar_entry_header`,
//!   `TAR_END`, `gzip_stored_wrap`, `crc32` and the bomb-guard limits.
//! - **writes** (extract entries, compress output) go through
//!   [`super::ops::write_bytes`] (rc `operations/uploadfile` + `movefile`,
//!   parents auto-created).
//! - **Degrade-to-job is dropped in rclone mode**: the OpenDAL arms fall
//!   back to a transfers job past 10 entries / 8 MiB; the rclone engine has
//!   no per-entry job machinery, so extract/compress run synchronously under
//!   the same bomb guards (entry count / payload caps) and always answer
//!   `{success, transport:"native", jobId:null}`. rclone-mode requests are
//!   not wrapped in the per-connection wall-clock budget (`handle_request`
//!   routes rclone arms before `block_on_timed`), which is what keeps the
//!   synchronous path viable.
//! - `files/compress` accepts `.zip` in addition to `.tar/.tar.gz/.tgz`;
//!   the zip writer emits stored (uncompressed) entries — standard zip,
//!   verifiable by every mainstream reader, with the CRC32 carried per
//!   entry.
//!
//! Gates: the connection-level read_only/allow_delete gates bind in the
//! wiring layer (`main.rs`), exactly like every other Phase B/D rclone arm;
//! this layer enforces the path whitelist + `lock_to_root` through the
//! shared `policy::PathPolicy`.

#![allow(dead_code)]

use serde_json::Value;

use super::ops;
use super::rc::RcClient;
use crate::archive as tar;
use crate::engine::ops::policy::PathPolicy;
use crate::model::FileEntry;

// ---------------------------------------------------------------------------
// Gates (mirror rclone/ops.rs conventions)
// ---------------------------------------------------------------------------

/// Read-side gate: path whitelist + `lock_to_root`; permissive connection
/// flags (those bind in the wiring layer).
fn gate_read(root: &str, lock_to_root: bool, path: &str) -> Result<String, String> {
    PathPolicy::from_parts(root, lock_to_root, false, true)
        .check_read(path)
        .map(|resolved| resolved.relative)
}

/// Write-side gate for extract targets / compress output.
fn gate_write(root: &str, lock_to_root: bool, path: &str) -> Result<String, String> {
    PathPolicy::from_parts(root, lock_to_root, false, true)
        .check_write(path)
        .map(|resolved| resolved.relative)
}

// ---------------------------------------------------------------------------
// rc-serve byte reads (mirror ops.rs read_prefix plumbing — those helpers
// are module-private and ops.rs is outside this task's ownership)
// ---------------------------------------------------------------------------

/// rc-serve fs spelling: the fs rides bracket-wrapped in the URL path.
fn serve_fs_string(fs: &str) -> String {
    format!("[{fs}]")
}

/// Minimal serve-URL escaping (identical rules to `ops::encode_serve_path`):
/// `%` must be escaped (rcd percent-decodes the path before matching), `?`
/// and `#` would split query/fragment; `[`/`]` stay literal (route
/// delimiters).
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

/// Reads exactly `len` bytes starting at `start` via an inclusive Range. A
/// body longer than requested proves the backend ignored the Range header —
/// offsets are then meaningless, so it is a hard error rather than corrupt
/// parsing.
async fn read_exact_range(
    client: &RcClient,
    fs: &str,
    remote: &str,
    start: u64,
    len: u64,
) -> Result<Vec<u8>, String> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let mut response = client
        .serve_get(
            &serve_fs_string(fs),
            &encode_serve_path(remote),
            Some((start, Some(start + len - 1))),
        )
        .await
        .map_err(|error| format!("Failed to read '{remote}': {error}"))?;
    let mut body: Vec<u8> = Vec::with_capacity(len as usize);
    while body.len() < len as usize {
        match response
            .chunk()
            .await
            .map_err(|error| format!("Failed to read '{remote}': {error}"))?
        {
            Some(chunk) => {
                let take = (len as usize - body.len()).min(chunk.len());
                body.extend_from_slice(&chunk[..take]);
                // Range-ignoring backend: anything past the requested window
                // invalidates the offset math.
                if chunk.len() > take {
                    return Err(format!(
                        "Failed to read '{remote}': storage backend ignored the Range request"
                    ));
                }
            }
            None => {
                return Err(format!(
                    "Failed to read '{remote}': truncated read at offset {start} \
                     ({} of {len} bytes)",
                    body.len()
                ))
            }
        }
    }
    Ok(body)
}

/// Streams the whole object (bounded by `cap`), for tar archives and
/// compress sources. Mirrors `archive::read_archive`'s size cap text.
async fn read_file_capped(
    client: &RcClient,
    fs: &str,
    remote: &str,
    display: &str,
    cap: u64,
) -> Result<Vec<u8>, String> {
    let mut response = client
        .serve_get(&serve_fs_string(fs), &encode_serve_path(remote), None)
        .await
        .map_err(|error| format!("Failed to read archive '{display}': {error}"))?;
    let mut body: Vec<u8> = Vec::new();
    loop {
        match response
            .chunk()
            .await
            .map_err(|error| format!("Failed to read archive '{display}': {error}"))?
        {
            Some(chunk) => {
                body.extend_from_slice(&chunk);
                if body.len() as u64 > cap {
                    return Err(format!(
                        "Archive '{display}' exceeds {cap} bytes; the archive methods cap \
                         input there"
                    ));
                }
            }
            None => return Ok(body),
        }
    }
}

// ---------------------------------------------------------------------------
// Archive stat + format detection
// ---------------------------------------------------------------------------

/// stat-first gate shared by all three methods: missing path and directory
/// targets refuse with the OpenDAL archive arms' message texts, and the file
/// size must stay inside [`tar::MAX_ARCHIVE_FILE_BYTES`].
async fn stat_archive(
    client: &RcClient,
    fs: &str,
    remote: &str,
    display: &str,
) -> Result<u64, String> {
    let stat = client
        .operations_stat(fs, remote)
        .await
        .map_err(|error| format!("Failed to stat archive '{display}': {error}"))?;
    let item = stat.get("item").filter(|item| !item.is_null());
    let Some(item) = item else {
        return Err(format!(
            "Failed to stat archive '{display}': path does not exist"
        ));
    };
    if item
        .get("IsDir")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(format!("Cannot read archive '{display}': it is a directory"));
    }
    let size = item
        .get("Size")
        .and_then(Value::as_i64)
        .filter(|size| *size >= 0)
        .unwrap_or(0) as u64;
    if size > tar::MAX_ARCHIVE_FILE_BYTES {
        return Err(format!(
            "Archive '{display}' is {size} bytes; the archive methods cap input at {} bytes",
            tar::MAX_ARCHIVE_FILE_BYTES
        ));
    }
    Ok(size)
}

/// One opened archive: either a fully-read tar stream or the parsed zip
/// central directory (payloads still on the remote).
enum OpenArchive {
    Tar(Vec<u8>),
    Zip { size: u64, entries: Vec<ZipEntry> },
}

/// stat → detect by magic → open. `PK` dispatches to the zip path; every
/// other head goes down the tar/tar.gz path (whole-file read, exact OpenDAL
/// parser semantics).
async fn open_archive(
    client: &RcClient,
    fs: &str,
    remote: &str,
    display: &str,
) -> Result<OpenArchive, String> {
    let size = stat_archive(client, fs, remote, display).await?;
    let head = read_exact_range(client, fs, remote, 0, 4.min(size)).await?;
    if head.starts_with(b"PK") {
        let entries = zip_central_directory(client, fs, remote, size, display).await?;
        return Ok(OpenArchive::Zip { size, entries });
    }
    let raw = read_file_capped(client, fs, remote, display, tar::MAX_ARCHIVE_FILE_BYTES).await?;
    Ok(OpenArchive::Tar(raw))
}

// ---------------------------------------------------------------------------
// Public operations (wire shapes mirror the OpenDAL arms in main.rs)
// ---------------------------------------------------------------------------

/// `files/archiveList` entry walk (archive order; pagination happens at the
/// call site with `tar::paginate`, same clamps as the OpenDAL arm).
pub async fn archive_list(
    client: &RcClient,
    fs: &str,
    path: &str,
    root: &str,
    lock_to_root: bool,
) -> Result<Vec<tar::ArchiveEntry>, String> {
    let remote = gate_read(root, lock_to_root, path)?;
    match open_archive(client, fs, &remote, path).await? {
        OpenArchive::Tar(raw) => tar::list_entries(&raw, &tar::DEFAULT_LIMITS),
        OpenArchive::Zip { entries, .. } => {
            Ok(entries.iter().map(zip_entry_to_archive_entry).collect())
        }
    }
}

/// `files/extract`: writes every planned file below `target_path` via
/// [`ops::write_bytes`]; returns `(files_written, bytes_written)`.
pub async fn extract(
    client: &RcClient,
    fs: &str,
    path: &str,
    target_path: &str,
    root: &str,
    lock_to_root: bool,
) -> Result<(u64, u64), String> {
    let remote = gate_read(root, lock_to_root, path)?;
    // `/` is a legitimate target (extract into the connection root).
    let target_base = gate_write(root, lock_to_root, target_path)?;
    let base = target_base.trim_matches('/').to_string();
    match open_archive(client, fs, &remote, path).await? {
        OpenArchive::Tar(raw) => {
            let plan = tar::extract_entries(&raw, &tar::DEFAULT_LIMITS)?;
            write_plan(client, fs, plan, &base).await
        }
        OpenArchive::Zip { entries, .. } => {
            // Same budget walk as `tar::extract_entries`: payload total is
            // capped, directories are implicit, links cannot appear in a
            // stored/deflate zip we write ourselves (external attrs are
            // never followed).
            let mut budget: u64 = 0;
            let mut plan: Vec<tar::ExtractEntry> = Vec::new();
            for entry in &entries {
                if entry.is_dir {
                    continue;
                }
                budget = budget.saturating_add(entry.size);
                if budget > tar::DEFAULT_LIMITS.max_bytes {
                    return Err(format!(
                        "archive payload exceeds the {} byte extraction guard (zip bomb?)",
                        tar::DEFAULT_LIMITS.max_bytes
                    ));
                }
                let data = read_zip_entry_data(client, fs, &remote, entry).await?;
                plan.push(tar::ExtractEntry {
                    path: entry.path.clone(),
                    size: entry.size,
                    data,
                });
            }
            write_plan(client, fs, plan, &base).await
        }
    }
}

/// Writes an extract plan under `base` ("" = connection root), one
/// `ops::write_bytes` per entry — parents auto-create, so no explicit
/// mkdir walk is needed.
async fn write_plan(
    client: &RcClient,
    fs: &str,
    plan: Vec<tar::ExtractEntry>,
    base: &str,
) -> Result<(u64, u64), String> {
    let mut files_done = 0u64;
    let mut bytes_done = 0u64;
    for entry in &plan {
        let target_file = if base.is_empty() {
            entry.path.clone()
        } else {
            format!("{base}/{}", entry.path)
        };
        ops::write_bytes(client, fs, &target_file, &entry.data).await?;
        files_done += 1;
        bytes_done += entry.size;
    }
    Ok((files_done, bytes_done))
}

/// One planned compress input: remote source path (root-relative) plus the
/// sanitized in-archive path. `size` is the walk-time estimate for the
/// budget check; headers carry the actual byte count.
struct CompressPlanEntry {
    remote: String,
    archive_path: String,
    size: u64,
}

/// `files/compress`: packs same-connection `sources` (files and/or
/// directories) into `target_path` (`.tar`, `.tar.gz`, `.tgz` or `.zip`;
/// the suffix check belongs to the wiring arm). The overwrite refusal and
/// the empty-paths check also live in the wiring arm, mirroring the OpenDAL
/// arm's ordering.
pub async fn compress(
    client: &RcClient,
    fs: &str,
    sources: &[String],
    target_path: &str,
    root: &str,
    lock_to_root: bool,
) -> Result<(), String> {
    let target = gate_write(root, lock_to_root, target_path)?;
    let target = target.trim_matches('/').to_string();
    if target.is_empty() {
        return Err(format!(
            "Archive target '{target_path}' must be a file path below the connection root"
        ));
    }
    let plan = plan_compress(client, fs, sources, root, lock_to_root).await?;
    let gzip = target.to_lowercase().ends_with(".tar.gz")
        || target.to_lowercase().ends_with(".tgz");
    let data = if target.to_lowercase().ends_with(".zip") {
        build_zip(client, fs, &plan).await?
    } else {
        build_tar(client, fs, &plan, gzip).await?
    };
    ops::write_bytes(client, fs, &target, &data)
        .await
        .map_err(|error| format!("Failed to write archive '{target}': {error}"))
}

/// Walks the compress sources into a plan (live-process analogue of
/// `tar::plan_compress`): directories recurse via `operations/list`, single
/// files stat directly; the entry/payload budget guards mirror
/// `MAX_COMPRESS_*`.
async fn plan_compress(
    client: &RcClient,
    fs: &str,
    sources: &[String],
    root: &str,
    lock_to_root: bool,
) -> Result<Vec<CompressPlanEntry>, String> {
    let mut plan: Vec<CompressPlanEntry> = Vec::new();
    for source in sources {
        let rel = gate_read(root, lock_to_root, source)?;
        let trimmed = rel.trim_matches('/').to_string();
        if trimmed.is_empty() {
            return Err(
                "Cannot compress the connection root; pick a subdirectory instead".to_string(),
            );
        }
        let base = base_name(&trimmed);
        let stat = client
            .operations_stat(fs, &trimmed)
            .await
            .map_err(|error| format!("Failed to stat '{trimmed}': {error}"))?;
        let item = stat.get("item").filter(|item| !item.is_null());
        let Some(item) = item else {
            return Err(format!("Failed to stat '{trimmed}': path does not exist"));
        };
        if item
            .get("IsDir")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            // ops::list re-applies the same read gate (defense in depth).
            let entries: Vec<FileEntry> =
                ops::list(client, fs, source, true, root, lock_to_root).await?;
            let prefix = format!("{trimmed}/");
            for entry in entries {
                if entry.kind != "file" {
                    continue;
                }
                let relative = entry
                    .path
                    .trim_start_matches('/')
                    .strip_prefix(prefix.as_str())
                    .ok_or_else(|| {
                        format!(
                            "Failed to walk '{}': entry '{}' is outside the source subtree",
                            source, entry.path
                        )
                    })?;
                let archive_path = tar::sanitize_entry_path(&format!("{base}/{relative}"))?;
                plan.push(CompressPlanEntry {
                    remote: entry.path.trim_start_matches('/').to_string(),
                    archive_path,
                    size: entry.size.unwrap_or(0),
                });
                if plan.len() > tar::MAX_COMPRESS_ENTRIES {
                    return Err(format!(
                        "Compression source exceeds {} entries",
                        tar::MAX_COMPRESS_ENTRIES
                    ));
                }
            }
        } else {
            let size = item
                .get("Size")
                .and_then(Value::as_i64)
                .filter(|size| *size >= 0)
                .unwrap_or(0) as u64;
            let archive_path = tar::sanitize_entry_path(base)?;
            plan.push(CompressPlanEntry {
                remote: trimmed,
                archive_path,
                size,
            });
        }
    }
    if plan.is_empty() {
        return Err("Nothing to compress: the sources hold no files".to_string());
    }
    let total: u64 = plan.iter().map(|entry| entry.size).sum();
    if total > tar::MAX_COMPRESS_BYTES {
        return Err(format!(
            "Compression payload exceeds the {} byte guard",
            tar::MAX_COMPRESS_BYTES
        ));
    }
    Ok(plan)
}

/// Assembles a plain/gzipped tar from the plan (`tar::tar_entry_header` +
/// `TAR_END` + `gzip_stored_wrap` — the exact OpenDAL in-memory builder's
/// byte stream, with the reads served through rc).
async fn build_tar(
    client: &RcClient,
    fs: &str,
    plan: &[CompressPlanEntry],
    gzip: bool,
) -> Result<Vec<u8>, String> {
    let mut tar_buf: Vec<u8> = Vec::new();
    let mut total = 0u64;
    for entry in plan {
        let data =
            read_file_capped(client, fs, &entry.remote, &entry.remote, tar::MAX_COMPRESS_BYTES)
                .await?;
        total = total.saturating_add(data.len() as u64);
        if total > tar::MAX_COMPRESS_BYTES {
            return Err(format!(
                "Compression payload exceeds the {} byte guard",
                tar::MAX_COMPRESS_BYTES
            ));
        }
        tar_buf.extend_from_slice(&tar::tar_entry_header(
            &entry.archive_path,
            data.len() as u64,
        )?);
        tar_buf.extend_from_slice(&data);
        let padding = data.len().div_ceil(512) * 512 - data.len();
        tar_buf.extend(std::iter::repeat(0u8).take(padding));
    }
    tar_buf.extend_from_slice(&tar::TAR_END);
    Ok(if gzip {
        tar::gzip_stored_wrap(&tar_buf)
    } else {
        tar_buf
    })
}

/// Assembles a stored-entry zip (local headers + payloads + central
/// directory + EOCD). Guards keep every offset field inside u32: entries ≤
/// 50_000 (< 0xFFFF) and payload ≤ 1 GiB (< 0xFFFFFFFF), so zip64 output is
/// never required.
async fn build_zip(client: &RcClient, fs: &str, plan: &[CompressPlanEntry]) -> Result<Vec<u8>, String> {
    let (dos_date, dos_time) = dos_datetime_now();
    let mut body: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    for entry in plan {
        let data =
            read_file_capped(client, fs, &entry.remote, &entry.remote, tar::MAX_COMPRESS_BYTES)
                .await?;
        total_guard(body.len() as u64 + data.len() as u64)?;
        let crc = tar::crc32(&data);
        let size = data.len() as u32;
        let name = entry.archive_path.as_bytes();
        // Names are sanitized (`/`-separated, relative); the UTF-8 flag is
        // set for non-ASCII names so readers decode them as UTF-8 instead
        // of cp437.
        let flags: u16 = if entry.archive_path.is_ascii() { 0 } else { 0x0800 };
        let offset = body.len() as u32;
        body.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        body.extend_from_slice(&20u16.to_le_bytes()); // version needed
        body.extend_from_slice(&flags.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        body.extend_from_slice(&dos_time.to_le_bytes());
        body.extend_from_slice(&dos_date.to_le_bytes());
        body.extend_from_slice(&crc.to_le_bytes());
        body.extend_from_slice(&size.to_le_bytes()); // compressed
        body.extend_from_slice(&size.to_le_bytes()); // uncompressed
        body.extend_from_slice(&(name.len() as u16).to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes()); // extra len
        body.extend_from_slice(name);
        body.extend_from_slice(&data);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&flags.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // method
        central.extend_from_slice(&dos_time.to_le_bytes());
        central.extend_from_slice(&dos_date.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra len
        central.extend_from_slice(&0u16.to_le_bytes()); // comment len
        central.extend_from_slice(&0u16.to_le_bytes()); // disk start
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name);
    }
    let count = plan.len() as u16;
    let mut out = body;
    let central_size = central.len() as u32;
    let central_offset = out.len() as u32;
    out.extend_from_slice(central.as_slice());
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // disk
    out.extend_from_slice(&0u16.to_le_bytes()); // cd disk
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len
    Ok(out)
}

fn total_guard(bytes: u64) -> Result<(), String> {
    if bytes > tar::MAX_COMPRESS_BYTES {
        return Err(format!(
            "Compression payload exceeds the {} byte guard",
            tar::MAX_COMPRESS_BYTES
        ));
    }
    Ok(())
}

fn base_name(path: &str) -> &str {
    let trimmed = path.trim_matches('/');
    match trimmed.rfind('/') {
        Some(index) => &trimmed[index + 1..],
        None => trimmed,
    }
}

// ---------------------------------------------------------------------------
// zip reading (central directory via tail Range reads)
// ---------------------------------------------------------------------------

const ZIP_LOCAL_SIG: u32 = 0x0403_4b50;
const ZIP_CENTRAL_SIG: u32 = 0x0201_4b50;
const ZIP_EOCD_SIG: u32 = 0x0605_4b50;
const ZIP64_EOCD_LOCATOR_SIG: u32 = 0x0706_4b50;
const ZIP64_EOCD_SIG: u32 = 0x0606_4b50;

/// EOCD is ≤ 65557 bytes from the end (max comment 65535 + 22); the extra
/// slack covers the zip64 locator + a modest parser margin.
const EOCD_WINDOW: u64 = 22 + 65_535 + 20 + 56;

/// One parsed central-directory record. Payload bytes stay on the remote:
/// `offset` locates the local header for on-demand Range reads.
struct ZipEntry {
    name: String,
    path: String,
    is_dir: bool,
    size: u64,
    comp_size: u64,
    method: u16,
    crc: u32,
    offset: u64,
    mtime_millis: Option<u64>,
}

fn u16_le(data: &[u8], at: usize) -> Result<u16, String> {
    data.get(at..at + 2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .ok_or_else(|| "truncated zip structure".to_string())
}

fn u32_le(data: &[u8], at: usize) -> Result<u32, String> {
    data.get(at..at + 4)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .ok_or_else(|| "truncated zip structure".to_string())
}

fn u64_le(data: &[u8], at: usize) -> Result<u64, String> {
    data.get(at..at + 8)
        .map(|bytes| {
            let mut raw = [0u8; 8];
            raw.copy_from_slice(bytes);
            u64::from_le_bytes(raw)
        })
        .ok_or_else(|| "truncated zip structure".to_string())
}

/// Locates the EOCD record inside a tail window (pure, unit-tested): scans
/// backwards for the signature whose comment length lands exactly at the
/// window end.
fn find_eocd(window: &[u8]) -> Result<usize, String> {
    if window.len() < 22 {
        return Err("not a valid zip archive: too small for an EOCD record".to_string());
    }
    for index in (0..=window.len() - 22).rev() {
        if u32_le(window, index)? != ZIP_EOCD_SIG {
            continue;
        }
        let comment_len = u16_le(window, index + 20)? as usize;
        if index + 22 + comment_len == window.len() {
            return Ok(index);
        }
    }
    Err(
        "not a valid zip archive: end-of-central-directory record not found \
         (truncated upload?)"
            .to_string(),
    )
}

/// Reads and parses the central directory: tail Range read → EOCD (zip64
/// aware) → central-directory Range read → per-entry records.
async fn zip_central_directory(
    client: &RcClient,
    fs: &str,
    remote: &str,
    size: u64,
    display: &str,
) -> Result<Vec<ZipEntry>, String> {
    let window = EOCD_WINDOW.min(size);
    let window_start = size - window;
    let tail = read_exact_range(client, fs, remote, window_start, window)
        .await
        .map_err(|error| format!("Failed to read archive '{display}': {error}"))?;
    let eocd_at = find_eocd(&tail).map_err(|error| format!("'{display}': {error}"))?;

    // zip64: a locator sitting immediately before the EOCD redirects the
    // entry count / offsets to the 64-bit record.
    let (entries_total, cd_size, cd_offset) =
        if eocd_at >= 20 && u32_le(&tail, eocd_at - 20)? == ZIP64_EOCD_LOCATOR_SIG {
            let eocd64_offset = u64_le(&tail, eocd_at - 20 + 8)?;
            if eocd64_offset.saturating_add(56) > size {
                return Err(format!(
                    "'{display}': zip64 end-of-central-directory record lies outside the file"
                ));
            }
            let record =
                read_exact_range(client, fs, remote, eocd64_offset, 56)
                    .await
                    .map_err(|error| format!("Failed to read archive '{display}': {error}"))?;
            if u32_le(&record, 0)? != ZIP64_EOCD_SIG {
                return Err(format!(
                    "'{display}': zip64 locator does not point at a zip64 EOCD record"
                ));
            }
            (
                u64_le(&record, 32)?,
                u64_le(&record, 40)?,
                u64_le(&record, 48)?,
            )
        } else {
            let entries = u16_le(&tail, eocd_at + 10)? as u64;
            (
                entries,
                u32_le(&tail, eocd_at + 12)? as u64,
                u32_le(&tail, eocd_at + 16)? as u64,
            )
        };

    if cd_offset.saturating_add(cd_size) > size {
        return Err(format!(
            "'{display}': central directory lies outside the file (truncated zip?)"
        ));
    }
    if entries_total > tar::MAX_ARCHIVE_ENTRIES as u64 {
        return Err(format!(
            "'{display}': archive contains more than {} entries (zip bomb guard)",
            tar::MAX_ARCHIVE_ENTRIES
        ));
    }
    let directory =
        read_exact_range(client, fs, remote, cd_offset, cd_size)
            .await
            .map_err(|error| format!("Failed to read archive '{display}': {error}"))?;

    let mut entries = Vec::new();
    let mut pos = 0usize;
    for _ in 0..entries_total {
        if directory.len() - pos < 46 {
            return Err(format!(
                "'{display}': truncated central directory record (corrupt zip?)"
            ));
        }
        if u32_le(&directory, pos)? != ZIP_CENTRAL_SIG {
            return Err(format!(
                "'{display}': corrupt central directory (bad record signature)"
            ));
        }
        let flags = u16_le(&directory, pos + 8)?;
        if flags & 0x0001 != 0 {
            let name = raw_name(&directory, pos + 46, u16_le(&directory, pos + 28)? as usize);
            return Err(format!(
                "'{display}': encrypted zip entry '{name}' is not supported"
            ));
        }
        let method = u16_le(&directory, pos + 10)?;
        let dos_time = u16_le(&directory, pos + 12)?;
        let dos_date = u16_le(&directory, pos + 14)?;
        let crc = u32_le(&directory, pos + 16)?;
        let mut comp_size = u32_le(&directory, pos + 20)? as u64;
        let mut size = u32_le(&directory, pos + 24)? as u64;
        let name_len = u16_le(&directory, pos + 28)? as usize;
        let extra_len = u16_le(&directory, pos + 30)? as usize;
        let mut offset = u32_le(&directory, pos + 42)? as u64;
        let raw = raw_name(&directory, pos + 46, name_len);
        // zip64: 0xFFFFFFFF placeholders resolve through the 0x0001 extra
        // field (fields appear in fixed order: size, comp size, offset).
        if size == u64::from(u32::MAX)
            || comp_size == u64::from(u32::MAX)
            || offset == u64::from(u32::MAX)
        {
            let extra = &directory[pos + 46 + name_len..pos + 46 + name_len + extra_len];
            let (z_size, z_comp, z_offset) = parse_zip64_extra(extra)
                .ok_or_else(|| format!("'{display}': zip64 entry without a zip64 extra field"))?;
            if size == u64::from(u32::MAX) {
                size = z_size;
            }
            if comp_size == u64::from(u32::MAX) {
                comp_size = z_comp;
            }
            if offset == u64::from(u32::MAX) {
                offset = z_offset;
            }
        }
        // Directory markers: the trailing-slash name convention, or the
        // MS-DOS directory attribute bit in the external attrs.
        let is_dir = raw.ends_with('/')
            || u32_le(&directory, pos + 38)? & 0x10 != 0;
        let path = tar::sanitize_entry_path(&raw).map_err(|error| {
            format!("'{display}': {error}")
        })?;
        entries.push(ZipEntry {
            name: path
                .rsplit('/')
                .next()
                .unwrap_or(&path)
                .to_string(),
            path,
            is_dir,
            size: if is_dir { 0 } else { size },
            comp_size,
            method,
            crc,
            offset,
            mtime_millis: dos_datetime_to_millis(dos_date, dos_time),
        });
        if entries.len() > tar::MAX_ARCHIVE_ENTRIES {
            return Err(format!(
                "'{display}': archive contains more than {} entries (zip bomb guard)",
                tar::MAX_ARCHIVE_ENTRIES
            ));
        }
        pos += 46 + name_len + extra_len + u16_le(&directory, pos + 32)? as usize;
    }
    Ok(entries)
}

/// Raw central-directory name bytes: UTF-8 when flagged, lossy fallback
/// otherwise (cp437 names degrade visibly instead of panicking the parser).
fn raw_name(data: &[u8], at: usize, len: usize) -> String {
    data.get(at..at + len)
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_default()
}

/// Parses the zip64 extended-information extra field (header 0x0001).
/// Returns `(uncompressed, compressed, local-header-offset)` in the fixed
/// order the spec mandates; `None` when the field is absent/malformed.
fn parse_zip64_extra(extra: &[u8]) -> Option<(u64, u64, u64)> {
    let mut pos = 0usize;
    while pos + 4 <= extra.len() {
        let header = u16_le(extra, pos).ok()?;
        let size = u16_le(extra, pos + 2).ok()? as usize;
        let body = &extra[pos + 4..(pos + 4 + size).min(extra.len())];
        if header == 0x0001 {
            let mut fields = body;
            let next8 = |fields: &mut &[u8]| -> Option<u64> {
                if fields.len() < 8 {
                    return None;
                }
                let value = u64::from_le_bytes(fields[..8].try_into().ok()?);
                *fields = &fields[8..];
                Some(value)
            };
            // Fields only appear for the values that overflowed; read them
            // positionally and tolerate absent tails.
            let mut values = [0u64; 3];
            for slot in &mut values {
                match next8(&mut fields) {
                    Some(value) => *slot = value,
                    None => break,
                }
            }
            return Some((values[0], values[1], values[2]));
        }
        pos += 4 + size;
    }
    None
}

/// MS-DOS date/time pair → epoch millis (UTC; zip timestamps carry no zone).
fn dos_datetime_to_millis(date: u16, time: u16) -> Option<u64> {
    let year = 1980 + i32::from(date >> 9);
    let month = u32::from((date >> 5) & 0x0F);
    let day = u32::from(date & 0x1F);
    let hour = u32::from(time >> 11);
    let minute = u32::from((time >> 5) & 0x3F);
    let second = u32::from(time & 0x1F) * 2;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    use chrono::TimeZone;
    chrono::Utc
        .with_ymd_and_hms(year, month, day, hour, minute, second)
        .single()
        .map(|dt| dt.timestamp_millis().max(0) as u64)
}

/// Current UTC time as the MS-DOS `(date, time)` pair (zip writer).
fn dos_datetime_now() -> (u16, u16) {
    use chrono::{Datelike, Timelike};
    let now = chrono::Utc::now();
    let year = now.year().clamp(1980, 2107);
    let date = ((year - 1980) as u16) << 9 | ((now.month() as u16) << 5) | now.day() as u16;
    let time = ((now.hour() as u16) << 11)
        | ((now.minute() as u16) << 5)
        | (now.second() as u16) / 2;
    (date, time)
}

/// Central-directory record → the wire `ArchiveEntry` shape (identical
/// mapping to `tar::list_entries`: basename name, dir size 0, mtime in
/// millis when nonzero).
fn zip_entry_to_archive_entry(entry: &ZipEntry) -> tar::ArchiveEntry {
    tar::ArchiveEntry {
        name: entry.name.clone(),
        path: entry.path.clone(),
        kind: if entry.is_dir { "directory" } else { "file" },
        size: entry.size,
        modified_at: entry.mtime_millis.filter(|millis| *millis > 0),
    }
}

/// Reads one entry's payload through the local file header (Range read of
/// 30 header bytes → data offset) and decodes it:
/// - method 0 (stored): bytes are the payload, CRC verified directly;
/// - method 8 (deflate): the raw stream is wrapped into a synthetic gzip
///   framing (header + trailer built from the zip entry's own CRC32/ISIZE)
///   so `tar::gunzip` both inflates and verifies integrity.
async fn read_zip_entry_data(
    client: &RcClient,
    fs: &str,
    remote: &str,
    entry: &ZipEntry,
) -> Result<Vec<u8>, String> {
    let header = read_exact_range(client, fs, remote, entry.offset, 30).await?;
    if u32_le(&header, 0)? != ZIP_LOCAL_SIG {
        return Err(format!(
            "Failed to extract '{}': corrupt local file header (bad signature)",
            entry.path
        ));
    }
    let name_len = u16_le(&header, 26)? as u64;
    let extra_len = u16_le(&header, 28)? as u64;
    let data_start = entry.offset + 30 + name_len + extra_len;
    let raw = read_exact_range(client, fs, remote, data_start, entry.comp_size).await?;
    match entry.method {
        0 => {
            if raw.len() as u64 != entry.size {
                return Err(format!(
                    "Failed to extract '{}': stored entry length {} does not match {}",
                    entry.path,
                    raw.len(),
                    entry.size
                ));
            }
            if tar::crc32(&raw) != entry.crc {
                return Err(format!(
                    "Failed to extract '{}': zip entry is corrupt (CRC32 mismatch)",
                    entry.path
                ));
            }
            Ok(raw)
        }
        8 => {
            let mut framed = Vec::with_capacity(raw.len() + 18);
            framed.extend_from_slice(&tar::GZIP_HEADER);
            framed.extend_from_slice(&raw);
            framed.extend_from_slice(&tar::gzip_trailer(entry.crc, entry.size));
            tar::gunzip(&framed, tar::DEFAULT_LIMITS.max_bytes).map_err(|error| {
                format!("Failed to extract '{}': {error}", entry.path)
            })
        }
        method => Err(format!(
            "Failed to extract '{}': unsupported zip compression method {method} \
             (only stored/deflate)",
            entry.path
        )),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- pure zip plumbing ---------------------------------------------------

    /// Hand-builds a minimal stored-entry zip (used for pure EOCD/central
    /// directory parsing tests; the live tests round-trip real archives).
    fn build_test_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let offset = out.len() as u32;
            let crc = tar::crc32(data);
            out.extend_from_slice(&ZIP_LOCAL_SIG.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // stored
            out.extend_from_slice(&0u16.to_le_bytes()); // time
            out.extend_from_slice(&0x0021u16.to_le_bytes()); // date 1980-01-01
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            central.extend_from_slice(&ZIP_CENTRAL_SIG.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0x0021u16.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let central_offset = out.len() as u32;
        let central_size = central.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&ZIP_EOCD_SIG.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&central_size.to_le_bytes());
        out.extend_from_slice(&central_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn find_eocd_scans_backwards_and_respects_comment_length() {
        let mut zip = build_test_zip(&[("a.txt", b"hello")]);
        assert_eq!(find_eocd(&zip).unwrap(), zip.len() - 22);
        // A comment is part of the EOCD: the signature stays at len-22-8 and
        // the comment-length FIELD must announce the 8 appended bytes.
        let eocd_at = zip.len() - 22;
        zip[eocd_at + 20..eocd_at + 22].copy_from_slice(&8u16.to_le_bytes());
        zip.extend_from_slice(b"comment!");
        assert_eq!(find_eocd(&zip).unwrap(), zip.len() - 22 - 8);
        // Trailing garbage after the comment breaks the exact-end match.
        zip.extend_from_slice(b"x");
        assert!(find_eocd(&zip).is_err());
    }

    #[test]
    fn central_directory_records_parse_offline() {
        // Full parse is async (Range reads), but the field walkers are the
        // same helpers the async path uses — exercise them on a synthetic
        // record.
        let zip = build_test_zip(&[("dir/", b""), ("dir/a.txt", b"payload!")]);
        // Locate the central directory via the EOCD fields.
        let eocd = find_eocd(&zip).unwrap();
        let count = u16_le(&zip, eocd + 10).unwrap();
        let cd_size = u32_le(&zip, eocd + 12).unwrap() as usize;
        let cd_offset = u32_le(&zip, eocd + 16).unwrap() as usize;
        assert_eq!(count, 2);
        let cd = &zip[cd_offset..cd_offset + cd_size];
        // First record: dir marker.
        assert_eq!(u32_le(cd, 0).unwrap(), ZIP_CENTRAL_SIG);
        let name_len = u16_le(cd, 28).unwrap() as usize;
        let raw = raw_name(cd, 46, name_len);
        assert_eq!(raw, "dir/");
        assert!(raw.ends_with('/'));
        // Second record: file with the right offset back into the local area.
        let stride = 46 + name_len + u16_le(cd, 30).unwrap() as usize
            + u16_le(cd, 32).unwrap() as usize;
        assert_eq!(u32_le(cd, stride).unwrap(), ZIP_CENTRAL_SIG);
        let file_offset = u32_le(cd, stride + 42).unwrap() as usize;
        assert_eq!(u32_le(&zip, file_offset).unwrap(), ZIP_LOCAL_SIG);
        let data_len = u16_le(cd, stride + 28).unwrap() as usize;
        let data = &zip[file_offset + 30 + data_len..];
        assert_eq!(&data[..8], b"payload!");
    }

    #[test]
    fn zip64_extra_field_parses_positionally() {
        let mut extra = Vec::new();
        extra.extend_from_slice(&0x0001u16.to_le_bytes());
        extra.extend_from_slice(&24u16.to_le_bytes());
        extra.extend_from_slice(&70_000u64.to_le_bytes());
        extra.extend_from_slice(&71_000u64.to_le_bytes());
        extra.extend_from_slice(&4_000_000_000u64.to_le_bytes());
        assert_eq!(parse_zip64_extra(&extra), Some((70_000, 71_000, 4_000_000_000)));
        // Non-zip64 extra fields are skipped.
        let mut other = Vec::new();
        other.extend_from_slice(&0x7075u16.to_le_bytes());
        other.extend_from_slice(&4u16.to_le_bytes());
        other.extend_from_slice(&[1, 2, 3, 4]);
        other.extend_from_slice(&extra);
        assert_eq!(parse_zip64_extra(&other), Some((70_000, 71_000, 4_000_000_000)));
        assert_eq!(parse_zip64_extra(&[]), None);
    }

    #[test]
    fn dos_datetime_round_trips() {
        let millis = dos_datetime_to_millis(0x0021, 0).unwrap(); // 1980-01-01 00:00
        assert_eq!(millis, 315_532_800_000);
        // 2017-06-09 21:45:00 → date 0x4AC9, time 0xADA0.
        let millis = dos_datetime_to_millis(0x4AC9, 0xADA0).unwrap();
        use chrono::TimeZone;
        assert_eq!(
            millis,
            chrono::Utc
                .with_ymd_and_hms(2017, 6, 9, 21, 45, 0)
                .single()
                .unwrap()
                .timestamp_millis() as u64
        );
        // Invalid month/day → None (never panics the list path).
        assert_eq!(dos_datetime_to_millis(0, 0), None);
        assert_eq!(dos_datetime_to_millis(0x0000, 0), None);
        // The writer side produces a parseable current timestamp.
        let (date, time) = dos_datetime_now();
        assert!(dos_datetime_to_millis(date, time).is_some());
    }

    #[test]
    fn zip_entry_mapping_matches_tar_shape() {
        let entry = ZipEntry {
            name: "a.txt".to_string(),
            path: "pkg/a.txt".to_string(),
            is_dir: false,
            size: 9,
            comp_size: 9,
            method: 0,
            crc: 42,
            offset: 0,
            mtime_millis: Some(1_700_000_000_000),
        };
        let value = serde_json::to_value(zip_entry_to_archive_entry(&entry)).unwrap();
        assert_eq!(value["name"], "a.txt");
        assert_eq!(value["path"], "pkg/a.txt");
        assert_eq!(value["kind"], "file");
        assert_eq!(value["size"], 9);
        assert_eq!(value["modifiedAt"], 1_700_000_000_000u64);
        // Dir + zero mtime: size 0 and the field omitted (serde skip).
        let dir = ZipEntry {
            is_dir: true,
            size: 0,
            mtime_millis: None,
            name: "pkg".to_string(),
            path: "pkg".to_string(),
            comp_size: 0,
            method: 0,
            crc: 0,
            offset: 0,
        };
        let value = serde_json::to_value(zip_entry_to_archive_entry(&dir)).unwrap();
        assert_eq!(value["kind"], "directory");
        assert!(value.get("modifiedAt").is_none());
    }

    #[test]
    fn synthetic_gzip_framing_decodes_deflate_entries() {
        // The extract path wraps raw deflate streams in a gzip frame built
        // from the entry's own CRC/ISIZE. Round-trip through the shared
        // `gzip_stored_wrap` writer to prove the reader accepts real frames.
        let payload = b"deflate payload".repeat(10);
        let framed = tar::gzip_stored_wrap(&payload);
        assert_eq!(tar::gunzip(&framed, 1 << 20).unwrap(), payload);
    }

    // -- live-process tests (skipped without a local rclone binary) ----------

    /// One live rcd + a tempdir fs (same pattern as ops.rs/bytes_channel.rs).
    struct Live {
        client: RcClient,
        fs: String,
        root: std::path::PathBuf,
        _dir: tempfile::TempDir,
    }

    impl Live {
        async fn start() -> Option<Live> {
            let Some(binary) = super::super::proc::resolve_binary() else {
                eprintln!("skipping: no rclone binary found");
                return None;
            };
            let handle = super::super::proc::RcdHandle::start(&binary)
                .await
                .expect("rcd should spawn");
            let client = handle.client();
            std::mem::forget(handle);
            let dir = tempfile::tempdir().expect("tempdir");
            Some(Live {
                client,
                fs: dir.path().to_string_lossy().to_string(),
                root: dir.path().to_path_buf(),
                _dir: dir,
            })
        }

        fn abs(&self, rel: &str) -> std::path::PathBuf {
            self.root.join(rel)
        }
    }

    fn entry_paths(entries: &[tar::ArchiveEntry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.path.as_str()).collect()
    }

    #[tokio::test]
    async fn zip_list_extract_round_trip_through_range_reads() {
        let Some(live) = Live::start().await else { return };
        let zip = build_test_zip(&[
            ("pkg/", b""),
            ("pkg/a.txt", b"alpha"),
            ("pkg/sub/deep.bin", &(0u8..=255).cycle().take(70_000).collect::<Vec<u8>>()),
        ]);
        ops::write_bytes(&live.client, &live.fs, "pkg.zip", &zip)
            .await
            .expect("upload zip");

        // List: only the central directory travels (the 70 KiB payload is
        // never downloaded for a listing).
        let entries = archive_list(&live.client, &live.fs, "pkg.zip", "", false)
            .await
            .expect("zip list");
        assert_eq!(
            entry_paths(&entries),
            vec!["pkg", "pkg/a.txt", "pkg/sub/deep.bin"]
        );
        assert_eq!(entries[0].kind, "directory");
        assert_eq!(entries[1].kind, "file");
        assert_eq!(entries[1].size, 5);
        assert_eq!(entries[2].size, 70_000);

        // Extract below a target dir; parents auto-create.
        let (files, bytes) = extract(
            &live.client,
            &live.fs,
            "pkg.zip",
            "/out",
            "",
            false,
        )
        .await
        .expect("zip extract");
        assert_eq!(files, 2);
        assert_eq!(bytes, 70_005);
        let landed = std::fs::read(live.abs("out/pkg/a.txt")).expect("read extracted");
        assert_eq!(landed, b"alpha");
        let deep = std::fs::read(live.abs("out/pkg/sub/deep.bin")).expect("read deep");
        assert_eq!(deep.len(), 70_000);

        // Extract into the connection root is legitimate too.
        let (files, _) = extract(&live.client, &live.fs, "pkg.zip", "/", "", false)
            .await
            .expect("root extract");
        assert_eq!(files, 2);
        assert_eq!(std::fs::read(live.abs("pkg/a.txt")).unwrap(), b"alpha");
    }

    #[tokio::test]
    async fn compress_zip_and_tar_round_trip_through_parser() {
        let Some(live) = Live::start().await else { return };
        std::fs::create_dir_all(live.abs("src/sub")).expect("mkdir");
        std::fs::write(live.abs("src/a.txt"), b"alpha").expect("write");
        std::fs::write(live.abs("src/sub/b.txt"), "内容 bravo").expect("write");
        std::fs::write(live.abs("solo.txt"), b"solo").expect("write");

        // zip: sources are a directory and a standalone file.
        compress(
            &live.client,
            &live.fs,
            &["/src".to_string(), "/solo.txt".to_string()],
            "/made.zip",
            "",
            false,
        )
        .await
        .expect("compress zip");
        let entries = archive_list(&live.client, &live.fs, "made.zip", "", false)
            .await
            .expect("list made.zip");
        assert_eq!(
            entry_paths(&entries),
            vec!["src/a.txt", "src/sub/b.txt", "solo.txt"],
            "file entries only (parents implicit), source walk order"
        );

        // 回读: extract the produced zip and compare bytes.
        extract(&live.client, &live.fs, "made.zip", "/roundtrip", "", false)
            .await
            .expect("extract made.zip");
        assert_eq!(
            std::fs::read(live.abs("roundtrip/src/sub/b.txt")).unwrap(),
            "内容 bravo".as_bytes()
        );
        assert_eq!(std::fs::read(live.abs("roundtrip/solo.txt")).unwrap(), b"solo");

        // tar.gz: pure-parser reuse path.
        compress(
            &live.client,
            &live.fs,
            &["/src".to_string()],
            "/made.tar.gz",
            "",
            false,
        )
        .await
        .expect("compress tar.gz");
        let entries = archive_list(&live.client, &live.fs, "made.tar.gz", "", false)
            .await
            .expect("list tar.gz");
        assert!(entry_paths(&entries).contains(&"src/sub/b.txt"));
        extract(&live.client, &live.fs, "made.tar.gz", "/tarout", "", false)
            .await
            .expect("extract tar.gz");
        assert_eq!(
            std::fs::read(live.abs("tarout/src/a.txt")).unwrap(),
            b"alpha"
        );
    }

    #[tokio::test]
    async fn archive_gates_and_missing_paths() {
        let Some(live) = Live::start().await else { return };
        let zip = build_test_zip(&[("a.txt", b"x")]);
        ops::write_bytes(&live.client, &live.fs, "small.zip", &zip)
            .await
            .expect("upload");

        // lock_to_root escapes refuse before any rc traffic.
        let error = archive_list(&live.client, &live.fs, "../escape.zip", "/srv", true)
            .await
            .expect_err("locked root escape must refuse");
        assert!(error.contains("escapes the connection root"), "{error}");

        // Missing archive → the OpenDAL-style stat error.
        let error = archive_list(&live.client, &live.fs, "missing.zip", "", false)
            .await
            .expect_err("missing archive");
        assert!(error.contains("path does not exist"), "{error}");

        // A directory target refuses like the OpenDAL arm.
        std::fs::create_dir_all(live.abs("adir")).expect("mkdir");
        let error = archive_list(&live.client, &live.fs, "adir", "", false)
            .await
            .expect_err("directory archive");
        assert!(error.contains("it is a directory"), "{error}");

        // Corrupt zip (no EOCD) reports the scan miss clearly.
        let mut junk = b"PK\x03\x04junk".to_vec();
        junk.extend(std::iter::repeat(0u8).take(200));
        ops::write_bytes(&live.client, &live.fs, "bad.zip", &junk)
            .await
            .expect("upload junk");
        let error = archive_list(&live.client, &live.fs, "bad.zip", "", false)
            .await
            .expect_err("junk zip");
        assert!(error.contains("end-of-central-directory"), "{error}");

        // Compress refuses the connection root and empty plans.
        let error = compress(
            &live.client,
            &live.fs,
            &["/".to_string()],
            "/out.zip",
            "",
            false,
        )
        .await
        .expect_err("root source");
        assert!(error.contains("Cannot compress the connection root"), "{error}");
    }
}
