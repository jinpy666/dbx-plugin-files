//! Host `filesystem/*` RPC facade (provider `io.dbx.files.fs`): the host-side
//! file-system contract exposed over the same rclone engine the workbench
//! route uses. Nothing here owns storage logic — every byte operation is the
//! existing `rclone::ops` / `rclone::bytes_channel` path, re-entered with a
//! different request grammar:
//!
//! - addressing is a whole URI (`rclone://<connectionId>/<encoded segments>`)
//!   instead of `connectionId` + plain `path`;
//! - list paging is an opaque offset cursor instead of `page`/`pageSize`;
//! - reads and writes are inline base64 (`filesystem/read|write`) and chunked
//!   downloads are pull sessions (`filesystem/download/open|read|close`) —
//!   the push-pump binary channel the workbench uses is not part of this face.
//!
//! Gate parity is the invariant: connection-level `read_only` / `allow_delete`
//! bind through the same [`crate::ensure_binding_writable`] /
//! [`crate::ensure_binding_deletable`] helpers the workbench arms use, and the
//! path gate is the same `policy::PathPolicy` whitelist (`lock_to_root` +
//! traversal rejection) applied at every method entry. `§9` semantics hold:
//! `read_only` rejects deletes too.

use std::collections::HashMap;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL_SAFE_NO_PAD;
use base64::Engine as _;
use dbx_plugin_sdk::PluginEmitter;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::rclone::{self, registry::RemoteBinding};
use crate::Plugin;

/// The only provider id this facade answers for. Requests carrying anything
/// else are rejected before any storage call.
pub const PROVIDER_ID: &str = "io.dbx.files.fs";

/// `filesystem/read` byte cap — the same inline payload budget the workbench
/// route reserves for `files/write` ([`crate::model::MAX_INLINE_WRITE_BYTES`]).
const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;

/// One `filesystem/download/read` chunk. The contract pins ≤1 MiB per call.
const DOWNLOAD_CHUNK_BYTES: u64 = 1024 * 1024;

/// `filesystem/list` page size: contract default and clamp.
const LIST_DEFAULT_LIMIT: u64 = 200;
const LIST_MAX_LIMIT: u64 = 1000;

// ---------------------------------------------------------------------------
// Request payloads (host sidecar protocol, camelCase)
// ---------------------------------------------------------------------------
//
// `providerId` (mandatory) is validated on the raw params by
// [`ensure_provider`] before any struct is built, so it is deliberately NOT
// a struct field (no dead deserialized fields). `connectionId` is optional
// and must agree with the URI authority (checked in [`resolve_target`]).

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListParams {
    #[serde(default)]
    pub connection_id: Option<String>,
    pub uri: String,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadParams {
    #[serde(default)]
    pub connection_id: Option<String>,
    pub uri: String,
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteParams {
    #[serde(default)]
    pub connection_id: Option<String>,
    pub uri: String,
    #[serde(default)]
    pub data_base64: String,
    #[serde(default)]
    pub create: bool,
    #[serde(default)]
    pub overwrite: bool,
    #[serde(default)]
    pub etag: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UriParams {
    #[serde(default)]
    pub connection_id: Option<String>,
    pub uri: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteParams {
    #[serde(default)]
    pub connection_id: Option<String>,
    pub uri: String,
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameParams {
    #[serde(default)]
    pub connection_id: Option<String>,
    pub source_uri: String,
    pub target_uri: String,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadOpenParams {
    #[serde(default)]
    pub download_id: String,
    #[serde(default)]
    pub connection_id: Option<String>,
    pub uri: String,
    // Selection/filter fields beyond the URI (e.g. remote hints) are host
    // policy, not engine inputs — serde drops unknown keys.
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadChunkParams {
    #[serde(default)]
    pub download_id: String,
    #[serde(default)]
    pub connection_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Download sessions (`filesystem/download/*`)
// ---------------------------------------------------------------------------

/// One open pull-download session: the gated object to stream, its size, and
/// the next read offset. `fs`/`remote` are frozen at open time; a reconnect
/// between chunks re-resolves only the rc client.
#[derive(Debug, Clone, PartialEq)]
pub struct DownloadSession {
    pub connection_id: String,
    /// The connection's composed rc fs string (`rclone::call_fs`).
    pub fs: String,
    /// Root-relative gated remote path (as handed to the ops layer).
    pub remote: String,
    pub size: u64,
    pub offset: u64,
}

/// `downloadId → session` registry. Same std-Mutex short-section discipline
/// as the `serves`/`about_cache` tables: the lock is never held across an
/// await (chunks clone the session out, read, then advance the offset).
pub type DownloadSessions = std::sync::Mutex<HashMap<String, DownloadSession>>;

/// Pure chunk planner: `None` = the cursor is at/after EOF (`done:true`,
/// empty payload); `Some(want)` = bytes to request (≤[`DOWNLOAD_CHUNK_BYTES`],
/// never 0 while short of EOF — the contract forbids empty payloads before
/// `done`).
fn next_chunk_want(offset: u64, size: u64) -> Option<u64> {
    if offset >= size {
        return None;
    }
    Some(DOWNLOAD_CHUNK_BYTES.min(size - offset))
}

// ---------------------------------------------------------------------------
// URI grammar: rclone://<connectionId>/<percent-encoded path segments>
// ---------------------------------------------------------------------------

/// A parsed facade URI. `connection_id` is the URI authority; `segments` are
/// the percent-decoded path segments (empty vector = the root form).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsUri {
    pub connection_id: String,
    pub segments: Vec<String>,
}

impl FsUri {
    /// Strict parse. Rejected: wrong scheme, empty authority, any segment
    /// that decodes to empty / `..` / contains a backslash or NUL, and
    /// malformed percent escapes. `rclone://<connId>` and
    /// `rclone://<connId>/` are both the legal root form.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let rest = raw
            .strip_prefix("rclone://")
            .ok_or_else(|| format!("URI '{raw}' must use the rclone scheme (rclone://...)"))?;
        let (authority, path) = match rest.split_once('/') {
            Some((authority, path)) => (authority, Some(path)),
            None => (rest, None),
        };
        let connection_id = percent_decode(authority)
            .map_err(|error| format!("URI authority '{authority}': {error}"))?;
        if connection_id.is_empty() {
            return Err(format!("URI '{raw}' carries an empty connectionId authority"));
        }
        let mut segments = Vec::new();
        match path {
            None => {}
            Some("") => {}
            Some(path) => {
                for raw_segment in path.split('/') {
                    let segment = percent_decode(raw_segment)
                        .map_err(|error| format!("URI segment '{raw_segment}': {error}"))?;
                    if segment.is_empty() {
                        return Err(format!(
                            "URI '{raw}' carries an empty path segment"
                        ));
                    }
                    if segment == ".." {
                        return Err(format!(
                            "URI '{raw}' path segment '..' is rejected (traversal)"
                        ));
                    }
                    if segment.contains('\\') {
                        return Err(format!(
                            "URI '{raw}' path segment contains a backslash"
                        ));
                    }
                    if segment.contains('\0') {
                        return Err(format!(
                            "URI '{raw}' path segment contains a NUL byte"
                        ));
                    }
                    segments.push(segment);
                }
            }
        }
        Ok(Self {
            connection_id,
            segments,
        })
    }

    /// Builds the canonical URI for a connection-scoped root-relative remote
    /// ("" = the root). Every segment byte outside the RFC3986 unreserved set
    /// is percent-encoded, so the output round-trips through [`Self::parse`].
    pub fn build(connection_id: &str, relative_remote: &str) -> String {
        let mut uri = format!("rclone://{}", percent_encode(connection_id));
        let trimmed = relative_remote.trim_matches('/');
        if !trimmed.is_empty() {
            for segment in trimmed.split('/') {
                uri.push('/');
                uri.push_str(&percent_encode(segment));
            }
        }
        uri
    }

    /// The policy-vocabulary path handed to the gate / ops layer: `""` for
    /// the root, `"/a/b"` (decoded, leading slash) otherwise — byte-identical
    /// to the path shapes `files/list` callers submit.
    pub fn path(&self) -> String {
        if self.segments.is_empty() {
            String::new()
        } else {
            format!("/{}", self.segments.join("/"))
        }
    }
}

/// Percent-encodes one URI segment: RFC3986 unreserved bytes pass through,
/// everything else becomes `%XX` uppercase hex (the `/` delimiter is never
/// produced because encoding is per-segment).
fn percent_encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for &byte in segment.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Strict percent-decode: `%` must start a two-hex-digit escape (case
/// insensitive) and the decoded bytes must form valid UTF-8. `+` is NOT a
/// space (path vocabulary, not query vocabulary).
fn percent_decode(raw: &str) -> Result<String, String> {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                if index + 3 > bytes.len() {
                    return Err("truncated percent escape".to_string());
                }
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                    .map_err(|_| "invalid percent escape".to_string())?;
                let byte = u8::from_str_radix(hex, 16)
                    .map_err(|_| format!("invalid percent escape '%{hex}'"))?;
                out.push(byte);
                index += 3;
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| "decoded path is not valid UTF-8".to_string())
}

// ---------------------------------------------------------------------------
// List cursor: opaque URL-safe base64 of `{"o":<offset>}`
// ---------------------------------------------------------------------------

fn encode_cursor(offset: u64) -> String {
    BASE64_URL_SAFE_NO_PAD.encode(json!({ "o": offset }).to_string().as_bytes())
}

fn decode_cursor(raw: &str) -> Result<u64, String> {
    let bytes = BASE64_URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| format!("Invalid cursor: not URL-safe base64 ({raw:.64})"))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "Invalid cursor: payload is not a JSON object".to_string())?;
    value
        .get("o")
        .and_then(Value::as_u64)
        .ok_or_else(|| "Invalid cursor: missing offset field 'o'".to_string())
}

// ---------------------------------------------------------------------------
// Shared entry helpers
// ---------------------------------------------------------------------------

/// Every `filesystem/*` request must address this facade. Checked first so a
/// misrouted host request fails before any parsing or storage work.
fn ensure_provider(params: &Value) -> Result<(), String> {
    let provider = params
        .get("providerId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if provider != PROVIDER_ID {
        return Err(format!(
            "Unknown providerId '{provider}': this facade serves '{PROVIDER_ID}'"
        ));
    }
    Ok(())
}

/// Optional `connectionId` parameter must agree with the URI authority.
fn ensure_connection_match(param: Option<&str>, authority: &str) -> Result<(), String> {
    if let Some(id) = param.filter(|id| !id.is_empty()) {
        if id != authority {
            return Err(format!(
                "connectionId '{id}' does not match the URI authority '{authority}'"
            ));
        }
    }
    Ok(())
}

/// One resolved method target: the registered binding, its composed fs
/// string, and the gated root-relative remote for the request URI.
struct Target {
    binding: RemoteBinding,
    fs: String,
    remote: String,
}

/// The common entry: provider check → URI parse → connectionId agreement →
/// binding lookup → `lock_to_root` path gate. `write_gate` selects the
/// policy check (read vs write); the connection-level write/delete gates and
/// the ops-layer delete/purge/rename gates layer on top per method.
async fn resolve_target(
    plugin: &Plugin,
    params_connection_id: Option<&str>,
    uri: &FsUri,
    write_gate: bool,
) -> Result<Target, String> {
    ensure_connection_match(params_connection_id, &uri.connection_id)?;
    let binding = plugin.rclone.binding(&uri.connection_id)?;
    let remote = if write_gate {
        crate::rclone_gate(
            &binding.root,
            binding.lock_to_root,
            &uri.path(),
            crate::policy::PathPolicy::check_write,
        )?
    } else {
        rclone::list_stream::gate_remote(&binding.root, binding.lock_to_root, &uri.path())?
    };
    Ok(Target {
        fs: rclone::call_fs(&binding),
        binding,
        remote,
    })
}

// ---------------------------------------------------------------------------
// Entry projection (FileEntry → facade entry JSON)
// ---------------------------------------------------------------------------

/// Builds one facade entry: `name` is the bare file name, `uri` the canonical
/// whole-space address, `kind` the contract vocabulary (`directory` for the
/// engine's `dir`), RFC3339 `modifiedAt`, and `contentType` when the name
/// carries a known extension.
fn entry_json(connection_id: &str, entry: &crate::model::FileEntry) -> Value {
    let relative = entry.path.trim_matches('/');
    let mut value = json!({
        "name": entry.name,
        "uri": FsUri::build(connection_id, relative),
        "kind": if entry.kind == "dir" { "directory" } else { entry.kind },
    });
    if let Some(size) = entry.size {
        value["size"] = json!(size);
    }
    if let Some(millis) = entry.modified_at {
        value["modifiedAt"] = json!(crate::store::format_rfc3339(millis as i64));
    }
    if entry.kind != "dir" {
        if let Some(content_type) = guess_content_type(&entry.name) {
            value["contentType"] = json!(content_type);
        }
    }
    value
}

/// Conservative extension → MIME mapping (the lsjson `MimeType` field is not
/// projected into `FileEntry`, so the facade derives it from the name).
/// Unknown extensions leave the field off rather than guessing.
fn guess_content_type(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "7z" => "application/x-7z-compressed",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// filesystem/list
// ---------------------------------------------------------------------------

async fn list(plugin: &Plugin, params: ListParams) -> Result<Value, String> {
    let uri = FsUri::parse(&params.uri)?;
    let target = resolve_target(plugin, params.connection_id.as_deref(), &uri, false).await?;
    // Same rclone lsjson path as `files/list` (non-recursive, sorted,
    // self-marker filtered); the full enumeration guard mirrors
    // `ops::list_paged` since offset paging slices the same in-memory list.
    let client = plugin.rclone.client_for_binding(&target.binding).await?;
    let mut entries = rclone::ops::list(
        &client,
        &target.fs,
        &uri.path(),
        false,
        &target.binding.root,
        target.binding.lock_to_root,
    )
    .await?;
    // namespace 连接的非递归列根：目录条目是「桶」（与 files/list 同口径）。
    if uri.path().trim().trim_matches('/').is_empty()
        && rclone::is_bucket_namespace(&target.binding.backend_type, &target.binding.root)
    {
        rclone::mark_bucket_entries(&mut entries);
    }
    if entries.len() > rclone::ops::LIST_PAGED_MAX {
        return Err(format!(
            "listing '{}' returned {} entries, exceeding the {} limit for paged listing",
            uri.path().trim().trim_matches('/'),
            entries.len(),
            rclone::ops::LIST_PAGED_MAX
        ));
    }
    let total = entries.len() as u64;
    let offset = match params.cursor.as_deref() {
        Some(cursor) => decode_cursor(cursor)?,
        None => 0,
    };
    let limit = params
        .limit
        .unwrap_or(LIST_DEFAULT_LIMIT)
        .clamp(1, LIST_MAX_LIMIT);
    let start = (offset as usize).min(entries.len());
    let end = (start.saturating_add(limit as usize)).min(entries.len());
    let page: Vec<Value> = entries[start..end]
        .iter()
        .map(|entry| entry_json(&uri.connection_id, entry))
        .collect();
    let mut response = json!({ "entries": page });
    // The last page never carries a cursor: a cursor exists only while
    // entries remain beyond it.
    if (end as u64) < total {
        response["nextCursor"] = Value::String(encode_cursor(end as u64));
    }
    Ok(response)
}

// ---------------------------------------------------------------------------
// filesystem/read
// ---------------------------------------------------------------------------

async fn read(plugin: &Plugin, params: ReadParams) -> Result<Value, String> {
    let uri = FsUri::parse(&params.uri)?;
    let target = resolve_target(plugin, params.connection_id.as_deref(), &uri, false).await?;
    let max_bytes = params
        .max_bytes
        .unwrap_or(MAX_READ_BYTES)
        .clamp(1, MAX_READ_BYTES);
    // `read_prefix` is the files/read byte path: stat-first (missing →
    // read-style error, directory → "it is a directory"), then one rc-serve
    // prefix GET capped at max_bytes with the truncated flag.
    let (data, truncated) = rclone::ops::read_prefix(
        &plugin.rclone.client_for_binding(&target.binding).await?.transfer_client(),
        &target.fs,
        &target.remote,
        max_bytes,
    )
    .await?;
    let mut response = json!({
        "dataBase64": BASE64_STANDARD.encode(data),
        "truncated": truncated,
    });
    let name = uri.segments.last().cloned().unwrap_or_default();
    if let Some(content_type) = guess_content_type(&name) {
        response["contentType"] = json!(content_type);
    }
    Ok(response)
}

// ---------------------------------------------------------------------------
// filesystem/write
// ---------------------------------------------------------------------------

async fn write(plugin: &Plugin, params: WriteParams) -> Result<Value, String> {
    let uri = FsUri::parse(&params.uri)?;
    let target = resolve_target(plugin, params.connection_id.as_deref(), &uri, true).await?;
    crate::ensure_binding_writable(&target.binding)?;
    if target.remote.trim_matches('/').is_empty() {
        return Err("Cannot write the connection root directory".to_string());
    }
    // Root-level single-segment targets are dead ends on bucket-rooted
    // connections (empty object key) — same early, actionable refusal as
    // files/write.
    rclone::ensure_nested_file_target(&target.binding, &target.remote)?;
    let data = BASE64_STANDARD
        .decode(params.data_base64.as_bytes())
        .map_err(|error| format!("Invalid base64 file data: {error}"))?;
    if data.len() > crate::model::MAX_INLINE_WRITE_BYTES {
        return Err(format!(
            "Inline write payload of {} bytes exceeds {}; use the upload channel",
            data.len(),
            crate::model::MAX_INLINE_WRITE_BYTES
        ));
    }
    let client = plugin.rclone.client_for_binding(&target.binding).await?;
    let exists = stat_exists(&client, &target.fs, &target.remote).await?;
    if exists && !params.overwrite {
        return Err(format!(
            "Cannot write '{}': it already exists and overwrite is false",
            target.remote
        ));
    }
    if !exists && !params.create {
        return Err(format!(
            "Cannot write '{}': it does not exist and create is false",
            target.remote
        ));
    }
    // Optimistic concurrency: only enforced when the provided etag can be
    // checked against the current object hash; an unobtainable hash (backend
    // without MD5, transport hiccup) skips the check rather than blocking
    // the write.
    if let Some(etag) = params.etag.as_deref().map(str::trim).filter(|etag| !etag.is_empty()) {
        if exists {
            if let Some(current) = single_object_hash(&client, &target.fs, &target.remote).await {
                if !current.eq_ignore_ascii_case(etag) {
                    return Err(format!(
                        "Write conflict on '{}': the object hash does not match the provided etag",
                        target.remote
                    ));
                }
            }
        }
    }
    // Same write byte path as files/write: stage → uploadfile into the
    // parent → movefile into place (parents auto-create, move overwrites).
    // The 30s-timeout caveat applies identically → transfer client.
    rclone::ops::write_bytes(
        &client.transfer_client(),
        &target.fs,
        &target.remote,
        &data,
    )
    .await?;
    plugin
        .list_cache
        .invalidate_around(&uri.connection_id, &target.remote);
    plugin.audit_id(&uri.connection_id, "filesystem/write", &uri.path(), "ok")?;
    let entry = rclone::ops::stat(
        &client,
        &target.fs,
        &uri.path(),
        &target.binding.root,
        target.binding.lock_to_root,
    )
    .await
    .ok()
    .map(|entry| entry_json(&uri.connection_id, &entry));
    let mut response = json!({ "success": true });
    if let Some(entry) = entry {
        response["entry"] = entry;
    }
    Ok(response)
}

/// Existence probe over `operations/stat` (the compress-arm pattern):
/// `item: null` / missing key = absent; an rc error is a hard failure (a
/// transport glitch must never read as "absent" and let a write clobber).
async fn stat_exists(client: &rclone::RcClient, fs: &str, remote: &str) -> Result<bool, String> {
    let response = client
        .operations_stat(fs, remote)
        .await
        .map_err(|error| format!("Failed to stat '{remote}': {error}"))?;
    Ok(response
        .get("item")
        .filter(|item| !item.is_null())
        .is_some())
}

/// Current object MD5 via the repo's existing SUM helper (`rc.rs ::
/// operations_hashsum`) scoped to the single file: the file rides inside the
/// fs string (`list_stream::lsjson_target` composition, the repo's shared
/// fs+remote join) so exactly that object is listed. Returns `None` when the
/// backend errors or the SUM line carries no hash — callers then skip etag
/// enforcement.
async fn single_object_hash(
    client: &rclone::RcClient,
    fs: &str,
    remote: &str,
) -> Option<String> {
    let target = rclone::list_stream::lsjson_target(fs, remote);
    let result = client
        .operations_hashsum(&target, "", "md5", false)
        .await
        .ok()?;
    result
        .get("hashsum")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .find_map(sum_line_hash)
        .map(str::to_string)
}

/// Parses one `operations/hashsum` line (`"<hash>  <relative path>"`). The
/// hash column must look like a hash: ≥8 hex digits and a path column after
/// it — backends without the hash type emit blank columns where the first
/// whitespace token would be the path itself, and that must never pass.
fn sum_line_hash(line: &str) -> Option<&str> {
    let mut tokens = line.split_whitespace();
    let hash = tokens.next()?;
    if tokens.next().is_none() {
        return None;
    }
    if hash.len() < 8 || !hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    Some(hash)
}

// ---------------------------------------------------------------------------
// filesystem/createDirectory
// ---------------------------------------------------------------------------

async fn create_directory(plugin: &Plugin, params: UriParams) -> Result<Value, String> {
    let uri = FsUri::parse(&params.uri)?;
    let target = resolve_target(plugin, params.connection_id.as_deref(), &uri, true).await?;
    crate::ensure_binding_writable(&target.binding)?;
    // The root always exists: idempotent mkdir answers success.
    if target.remote.trim_matches('/').is_empty() {
        return Ok(json!({ "success": true }));
    }
    // Same entry as files/mkdir: rc mkdir -p semantics (idempotent) plus the
    // `.keep` placeholder fallback for backends that cannot show empty dirs.
    rclone::ops::mkdir_with_placeholder(
        &plugin.rclone.client_for_binding(&target.binding).await?,
        &target.fs,
        &target.binding.backend_type,
        &target.remote,
        &target.binding.root,
        target.binding.lock_to_root,
    )
    .await?;
    plugin
        .list_cache
        .invalidate_around(&uri.connection_id, &target.remote);
    plugin.audit_id(
        &uri.connection_id,
        "filesystem/createDirectory",
        &uri.path(),
        "ok",
    )?;
    Ok(json!({ "success": true }))
}

// ---------------------------------------------------------------------------
// filesystem/delete
// ---------------------------------------------------------------------------

async fn delete(plugin: &Plugin, params: DeleteParams) -> Result<Value, String> {
    let uri = FsUri::parse(&params.uri)?;
    let target = resolve_target(plugin, params.connection_id.as_deref(), &uri, false).await?;
    // §9: read_only rejects deletes too; allow_delete is the second gate.
    crate::ensure_binding_deletable(&target.binding)?;
    if target.remote.trim_matches('/').is_empty() {
        return Err("Cannot delete the connection root directory".to_string());
    }
    let client = plugin.rclone.client_for_binding(&target.binding).await?;
    let stat = client
        .operations_stat(&target.fs, &target.remote)
        .await
        .map_err(|error| format!("Failed to stat '{}': {error}", target.remote))?;
    let item = stat.get("item").filter(|item| !item.is_null());
    let Some(item) = item else {
        // Missing path is already gone: the method face is idempotent
        // (same semantics as files/delete's delete_file).
        return Ok(json!({ "success": true }));
    };
    if item
        .get("IsDir")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        if params.recursive {
            // gate_purge inside (root red line + missing-path tolerance).
            rclone::ops::purge(
                &client,
                &target.fs,
                &uri.path(),
                &target.binding.root,
                target.binding.lock_to_root,
            )
            .await?;
        } else {
            // Empty-only semantics: a non-empty directory errors.
            rclone::ops::rmdir(
                &client,
                &target.fs,
                &uri.path(),
                &target.binding.root,
                target.binding.lock_to_root,
            )
            .await?;
        }
    } else {
        // deletefile (idempotent) — the ops layer re-applies check_delete.
        rclone::ops::delete_file(
            &client,
            &target.fs,
            &uri.path(),
            &target.binding.root,
            target.binding.lock_to_root,
        )
        .await?;
    }
    plugin
        .list_cache
        .invalidate_around(&uri.connection_id, &target.remote);
    plugin.audit_id(&uri.connection_id, "filesystem/delete", &uri.path(), "ok")?;
    // Best-effort mount-view refresh, files/delete parity.
    crate::mount::best_effort_refresh_mount_caches(
        &plugin.rclone,
        &plugin.mounts,
        &uri.connection_id,
        Some(&crate::parent_dir_of(&uri.path())),
    )
    .await;
    Ok(json!({ "success": true }))
}

// ---------------------------------------------------------------------------
// filesystem/rename
// ---------------------------------------------------------------------------

async fn rename(
    plugin: &Plugin,
    emitter: &PluginEmitter,
    params: RenameParams,
) -> Result<Value, String> {
    let source_uri = FsUri::parse(&params.source_uri)?;
    let target_uri = FsUri::parse(&params.target_uri)?;
    if source_uri.connection_id != target_uri.connection_id {
        return Err(format!(
            "rename requires both URIs on one connection: '{}' vs '{}'",
            source_uri.connection_id, target_uri.connection_id
        ));
    }
    let target = resolve_target(
        plugin,
        params.connection_id.as_deref(),
        &source_uri,
        true,
    )
    .await?;
    // Rename removes the source path — the same double gate as files/rename
    // (ops::rename applies check_rename's whitelist on both endpoints).
    crate::ensure_binding_writable(&target.binding)?;
    crate::ensure_binding_deletable(&target.binding)?;
    let client = plugin.rclone.client_for_binding(&target.binding).await?;
    // Missing source is an error (stat's own NotFound semantics).
    let source_entry = rclone::ops::stat(
        &client,
        &target.fs,
        &source_uri.path(),
        &target.binding.root,
        target.binding.lock_to_root,
    )
    .await?;
    if !params.overwrite {
        let target_remote = crate::rclone_gate(
            &target.binding.root,
            target.binding.lock_to_root,
            &target_uri.path(),
            crate::policy::PathPolicy::check_write,
        )?;
        if target_remote.trim_matches('/').is_empty() {
            return Err("Cannot rename onto the connection root directory".to_string());
        }
        if stat_exists(&client, &target.fs, &target_remote).await? {
            return Err(format!(
                "Cannot rename to '{}': the target already exists and overwrite is false",
                target_remote
            ));
        }
    }
    if source_entry.kind == "dir" {
        // Directories move server-side (`sync/move` job): the file-rename rc
        // 404s on dirs. Same connection both sides — files/rename parity.
        let job_id = crate::rclone_start_dir_job(
            plugin.rclone.clone(),
            plugin.sync_jobs.clone(),
            &crate::model::DirJobRequest {
                source_connection_id: source_uri.connection_id.clone(),
                source_path: source_uri.path(),
                target_connection_id: target_uri.connection_id.clone(),
                target_path: target_uri.path(),
                dry_run: Some(false),
                max_delete: None,
                include: None,
                exclude: None,
                backup_dir: None,
                suffix: None,
                metadata: Some(false),
                update: None,
                existing: None,
                immutable: None,
                checksum: None,
                ignore_existing: None,
                ignore_times: None,
                min_size: None,
                max_size: None,
                min_age: None,
                max_age: None,
                transfers: None,
                checkers: None,
                retries: None,
            },
            false,
            true,
            Some(emitter),
        )
        .await?;
        invalidate_around(plugin, &source_uri, &target);
        invalidate_around(plugin, &target_uri, &target);
        return Ok(json!({ "success": true, "transport": "job", "jobId": job_id }));
    }
    // operations/movefile within one fs (rclone moveto; overwrites).
    rclone::ops::rename(
        &client,
        &target.fs,
        &source_uri.path(),
        &target_uri.path(),
        &target.binding.root,
        target.binding.lock_to_root,
    )
    .await?;
    plugin.audit_id(
        &source_uri.connection_id,
        "filesystem/rename",
        &source_uri.path(),
        "ok",
    )?;
    invalidate_around(plugin, &source_uri, &target);
    invalidate_around(plugin, &target_uri, &target);
    let entry = rclone::ops::stat(
        &client,
        &target.fs,
        &target_uri.path(),
        &target.binding.root,
        target.binding.lock_to_root,
    )
    .await
    .ok()
    .map(|entry| entry_json(&source_uri.connection_id, &entry));
    let mut response = json!({ "success": true, "transport": "native" });
    if let Some(entry) = entry {
        response["entry"] = entry;
    }
    Ok(response)
}

/// P2 decision-cache invalidation for one facade endpoint (root-relative
/// gated path form, same key discipline as the workbench arms).
fn invalidate_around(plugin: &Plugin, uri: &FsUri, target: &Target) {
    if let Ok(relative) = crate::rclone_gate(
        &target.binding.root,
        target.binding.lock_to_root,
        &uri.path(),
        crate::policy::PathPolicy::check_read,
    ) {
        plugin.list_cache.invalidate_around(&uri.connection_id, &relative);
    }
}

// ---------------------------------------------------------------------------
// filesystem/download/{open,read,close}
// ---------------------------------------------------------------------------

async fn download_open(plugin: &Plugin, params: DownloadOpenParams) -> Result<Value, String> {
    let download_id = params.download_id.trim().to_string();
    if download_id.is_empty() {
        return Err("Missing downloadId".to_string());
    }
    let uri = FsUri::parse(&params.uri)?;
    let target = resolve_target(plugin, params.connection_id.as_deref(), &uri, false).await?;
    let client = plugin.rclone.client_for_binding(&target.binding).await?;
    // stat-first (missing object / directory), exactly like files/download/start.
    let entry = rclone::ops::stat(
        &client,
        &target.fs,
        &uri.path(),
        &target.binding.root,
        target.binding.lock_to_root,
    )
    .await?;
    if entry.kind == "dir" {
        return Err(format!(
            "Cannot download '{}': it is a directory",
            target.remote.trim_matches('/')
        ));
    }
    let size = entry.size.unwrap_or(0);
    crate::rclone_lock(&plugin.download_sessions).insert(
        download_id,
        DownloadSession {
            connection_id: uri.connection_id.clone(),
            fs: target.fs,
            remote: target.remote,
            size,
            offset: 0,
        },
    );
    Ok(json!({ "size": size }))
}

async fn download_read(plugin: &Plugin, params: DownloadChunkParams) -> Result<Value, String> {
    let download_id = params.download_id.trim().to_string();
    if download_id.is_empty() {
        return Err("Missing downloadId".to_string());
    }
    let session = {
        let sessions = crate::rclone_lock(&plugin.download_sessions);
        sessions
            .get(&download_id)
            .cloned()
            .ok_or_else(|| format!("Download session '{download_id}' was not found"))?
    };
    if let Some(connection_id) = params.connection_id.as_deref().filter(|id| !id.is_empty()) {
        if connection_id != session.connection_id {
            return Err(format!(
                "connectionId '{connection_id}' does not match the download session's connection '{}'",
                session.connection_id
            ));
        }
    }
    let (data_base64, done, advanced_to) = match next_chunk_want(session.offset, session.size) {
        // Cursor at/after EOF: the terminating empty chunk.
        None => (String::new(), true, session.offset),
        Some(want) => {
            let client = plugin
                .rclone
                .client_for_id(&session.connection_id)
                .await?
                .transfer_client();
            let (data, _total_size) =
                rclone::ops::read_range(&client, &session.fs, &session.remote, session.offset, want as u32)
                    .await?;
            if data.is_empty() {
                // Contract invariant: no empty payload before `done`. A zero
                // byte read short of EOF is a backend anomaly, not a chunk.
                return Err(format!(
                    "Download of '{}' returned no bytes at offset {}",
                    session.remote, session.offset
                ));
            }
            let done = rclone::ops::range_is_eof(session.offset, data.len(), session.size);
            let advanced_to = session.offset + data.len() as u64;
            (BASE64_STANDARD.encode(&data), done, advanced_to)
        }
    };
    // Advance the cursor; a concurrently-closed session just drops the write.
    if let Some(session) = crate::rclone_lock(&plugin.download_sessions)
        .get_mut(&download_id)
    {
        session.offset = advanced_to;
    }
    Ok(json!({ "dataBase64": data_base64, "done": done }))
}

async fn download_close(plugin: &Plugin, params: DownloadChunkParams) -> Result<Value, String> {
    // Idempotent by contract: unknown / already-closed ids answer success —
    // the host calls close on every terminal path, error or not.
    crate::rclone_lock(&plugin.download_sessions).remove(params.download_id.trim());
    Ok(json!({}))
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// The `filesystem/*` route arm. Exactly the nine contract methods are
/// answered here; any other `filesystem/*` name falls through to the route's
/// terminal unsupported arm (the dispatch only routes the enumerated set).
pub async fn handle(
    plugin: &Plugin,
    method: &str,
    params: Value,
    emitter: &PluginEmitter,
) -> Result<Value, String> {
    ensure_provider(&params)?;
    match method {
        "filesystem/list" => list(plugin, crate::parse(params)?).await,
        "filesystem/read" => read(plugin, crate::parse(params)?).await,
        "filesystem/write" => write(plugin, crate::parse(params)?).await,
        "filesystem/createDirectory" => create_directory(plugin, crate::parse(params)?).await,
        "filesystem/delete" => delete(plugin, crate::parse(params)?).await,
        "filesystem/rename" => rename(plugin, emitter, crate::parse(params)?).await,
        "filesystem/download/open" => download_open(plugin, crate::parse(params)?).await,
        "filesystem/download/read" => download_read(plugin, crate::parse(params)?).await,
        "filesystem/download/close" => download_close(plugin, crate::parse(params)?).await,
        other => Err(format!("Method not found: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- URI grammar ---------------------------------------------------------

    #[test]
    fn uri_round_trips_names_and_special_characters() {
        for (connection_id, relative) in [
            ("conn-1", ""),
            ("conn-1", "file.txt"),
            ("conn-1", "sub/file.txt"),
            ("conn 1", "报告 v2.txt"),
            ("conn/1", "a b/c#d?e%f/g.txt"),
            ("__local__", "deep/nested/档案(2026)/最终.tar.gz"),
        ] {
            let uri = FsUri::build(connection_id, relative);
            let parsed = FsUri::parse(&uri)
                .unwrap_or_else(|error| panic!("parse('{uri}') failed: {error}"));
            assert_eq!(parsed.connection_id, connection_id, "{uri}");
            let expected_path = relative.trim_matches('/');
            if expected_path.is_empty() {
                assert!(parsed.segments.is_empty(), "{uri}");
            } else {
                assert_eq!(parsed.path(), format!("/{expected_path}"), "{uri}");
            }
        }
    }

    #[test]
    fn uri_root_forms_are_both_legal() {
        for raw in ["rclone://conn-1", "rclone://conn-1/"] {
            let parsed = FsUri::parse(raw).unwrap();
            assert_eq!(parsed.connection_id, "conn-1");
            assert!(parsed.segments.is_empty());
            assert_eq!(parsed.path(), "");
        }
    }

    #[test]
    fn uri_rejects_wrong_scheme_empty_authority_and_malformed_escapes() {
        for raw in [
            "file:///tmp/x",
            "RCLONE://conn/a",
            "rclone:/conn/a",
            "rclone://",
            "rclone:///a/b",
            "rclone://conn/a/%2g",
            "rclone://conn/a/%2",
        ] {
            assert!(FsUri::parse(raw).is_err(), "'{raw}' must be rejected");
        }
    }

    #[test]
    fn uri_rejects_traversal_backslash_empty_and_nul_segments() {
        for raw in [
            "rclone://conn/..",
            "rclone://conn/a/../../b",
            "rclone://conn/%2e%2e",
            "rclone://conn/a%5Cb",
            "rclone://conn/a//b",
            "rclone://conn/a/",
            "rclone://conn/%00",
        ] {
            let error = FsUri::parse(raw).unwrap_err();
            assert!(!error.is_empty(), "'{raw}' must carry a reason");
        }
    }

    #[test]
    fn uri_dot_segments_pass_through_and_gate_layer_decides() {
        // `.` is not listed as rejected by the contract; the policy layer
        // already folds dot segments, so the URI layer keeps it literal.
        let parsed = FsUri::parse("rclone://conn/a/./b").unwrap();
        assert_eq!(parsed.path(), "/a/./b");
    }

    #[test]
    fn connection_match_param_must_agree_with_authority() {
        assert!(ensure_connection_match(None, "conn-1").is_ok());
        assert!(ensure_connection_match(Some("conn-1"), "conn-1").is_ok());
        let error = ensure_connection_match(Some("other"), "conn-1").unwrap_err();
        assert!(error.contains("does not match"), "{error}");
    }

    // -- provider id -----------------------------------------------------------

    #[test]
    fn provider_id_mismatch_is_rejected_first() {
        assert!(ensure_provider(&json!({ "providerId": PROVIDER_ID })).is_ok());
        let error = ensure_provider(&json!({ "providerId": "io.dbx.files" })).unwrap_err();
        assert!(error.contains(PROVIDER_ID), "{error}");
        let error = ensure_provider(&json!({})).unwrap_err();
        assert!(error.contains(PROVIDER_ID), "absent providerId: {error}");
    }

    // -- list cursor -----------------------------------------------------------

    #[test]
    fn cursor_round_trips_offsets() {
        for offset in [0u64, 1, 199, 200, 12345, u64::MAX] {
            let encoded = encode_cursor(offset);
            assert!(!encoded.contains('+') && !encoded.contains('/'), "{encoded}");
            assert_eq!(decode_cursor(&encoded).unwrap(), offset);
        }
    }

    #[test]
    fn cursor_rejects_garbage() {
        // Decodes to bytes that are not a JSON object.
        let not_json = BASE64_URL_SAFE_NO_PAD.encode(b"notjson");
        // Plain text / junk that is not base64 at all.
        for raw in ["", "not base64!!", "AAAA", not_json.as_str()] {
            let result = decode_cursor(raw);
            assert!(result.is_err(), "'{raw}' must be rejected");
        }
        // A cursor without the offset field is rejected explicitly.
        let no_offset = BASE64_URL_SAFE_NO_PAD.encode(br#"{"x":1}"#);
        assert!(decode_cursor(&no_offset).is_err());
    }

    /// Pagination math over a synthetic listing: window slicing, the
    /// end-of-list rule (the final page never returns a cursor), and the
    /// clamp bounds.
    #[test]
    fn pagination_windows_and_terminal_page_have_no_next_cursor() {
        let total = 205usize;
        let mut offset = 0u64;
        let mut pages = 0;
        let mut last_had_cursor = true;
        while last_had_cursor {
            let limit = 100u64;
            let start = (offset as usize).min(total);
            let end = (start.saturating_add(limit as usize)).min(total);
            let page_len = end - start;
            assert!(page_len > 0 || pages == 0, "empty middle page");
            last_had_cursor = (end as u64) < total as u64;
            if last_had_cursor {
                assert_eq!(end as u64, decode_cursor(&encode_cursor(end as u64)).unwrap());
                offset = end as u64;
            } else {
                // Terminal page: entries remain non-empty and exactly cover
                // the tail.
                assert_eq!(end, total);
                assert!(page_len <= limit as usize);
            }
            pages += 1;
            assert!(pages < 10, "pagination must terminate");
        }
        assert_eq!(pages, 3, "205 entries at limit 100 → 3 pages");
    }

    #[test]
    fn limit_clamps_to_contract_bounds() {
        assert_eq!(Some(500u64).unwrap_or(LIST_DEFAULT_LIMIT).clamp(1, LIST_MAX_LIMIT), 500);
        assert_eq!(
            5_000u64.clamp(1, LIST_MAX_LIMIT),
            LIST_MAX_LIMIT,
            "oversized limit clamps to 1000"
        );
        assert_eq!(
            0u64.clamp(1, LIST_MAX_LIMIT),
            1,
            "zero limit clamps up to 1"
        );
        // Default applies only when the field is absent.
        assert_eq!(None.unwrap_or(LIST_DEFAULT_LIMIT), 200);
    }

    // -- download sessions ------------------------------------------------------

    #[test]
    fn chunk_planner_honors_size_and_eof() {
        assert_eq!(next_chunk_want(0, 0), None, "empty file: immediately done");
        assert_eq!(next_chunk_want(5, 5), None, "cursor at EOF");
        assert_eq!(next_chunk_want(7, 5), None, "cursor past EOF");
        assert_eq!(next_chunk_want(0, 3 * DOWNLOAD_CHUNK_BYTES), Some(DOWNLOAD_CHUNK_BYTES));
        assert_eq!(
            next_chunk_want(2 * DOWNLOAD_CHUNK_BYTES, 3 * DOWNLOAD_CHUNK_BYTES),
            Some(DOWNLOAD_CHUNK_BYTES)
        );
        // Tail chunk is short, never empty.
        assert_eq!(next_chunk_want(1_048_576, 1_048_579), Some(3));
    }

    #[test]
    fn chunk_done_uses_range_is_eof_contract() {
        assert!(rclone::ops::range_is_eof(0, 10, 10));
        assert!(!rclone::ops::range_is_eof(0, 9, 10));
        assert!(rclone::ops::range_is_eof(4, 6, 10));
        assert!(rclone::ops::range_is_eof(10, 0, 10), "offset == size is EOF");
    }

    #[test]
    fn session_registry_close_is_idempotent() {
        let sessions: DownloadSessions = std::sync::Mutex::new(HashMap::new());
        let missing = sessions.lock().unwrap().remove("nope");
        assert!(missing.is_none(), "closing an unknown id is a no-op");
        crate::rclone_lock(&sessions).insert(
            "d1".to_string(),
            DownloadSession {
                connection_id: "conn-1".to_string(),
                fs: "dbxRemote:".to_string(),
                remote: "dir/file.bin".to_string(),
                size: 10,
                offset: 0,
            },
        );
        let removed = crate::rclone_lock(&sessions).remove("d1");
        assert!(removed.is_some());
        assert!(crate::rclone_lock(&sessions).remove("d1").is_none());
    }

    #[test]
    fn session_offset_advances_by_delivered_bytes() {
        let mut session = DownloadSession {
            connection_id: "conn-1".to_string(),
            fs: "dbxRemote:".to_string(),
            remote: "file.bin".to_string(),
            size: 2_500_000,
            offset: 0,
        };
        // Simulated chunk loop without a live rcd: plan → deliver → advance.
        let mut reads = 0;
        loop {
            let Some(want) = next_chunk_want(session.offset, session.size) else {
                break;
            };
            let delivered = want.min(DOWNLOAD_CHUNK_BYTES) as usize;
            assert!(delivered > 0, "no empty payload before done");
            let done = rclone::ops::range_is_eof(session.offset, delivered, session.size);
            session.offset += delivered as u64;
            reads += 1;
            if done {
                assert_eq!(session.offset, session.size);
                break;
            }
            assert!(reads < 10, "stream must terminate within 3 chunks + margin");
        }
        assert_eq!(reads, 3, "2.5 MB at 1 MiB chunks → 3 reads");
        // The final planner call reports EOF.
        assert_eq!(next_chunk_want(session.offset, session.size), None);
    }

    // -- etag hash parsing --------------------------------------------------------

    #[test]
    fn sum_line_hash_parses_hash_column_only() {
        assert_eq!(
            sum_line_hash("d41d8cd98f00b204e9800998ecf8427e  file.txt"),
            Some("d41d8cd98f00b204e9800998ecf8427e")
        );
        assert_eq!(sum_line_hash("d41d8cd98f00b204e9800998ecf8427e  sub/dir f.txt"), Some("d41d8cd98f00b204e9800998ecf8427e"));
        // Blank hash column: the first whitespace token would be the path.
        assert_eq!(sum_line_hash("file.txt"), None);
        assert_eq!(sum_line_hash(""), None);
        // Junk that is not a hash (too short / non-hex).
        assert_eq!(sum_line_hash("abc  file.txt"), None);
        assert_eq!(sum_line_hash("zzzzzzzzzzzzzzzz  file.txt"), None);
    }

    // -- entry projection -----------------------------------------------------------

    fn entry(name: &str, path: &str, kind: &'static str, size: Option<u64>, millis: Option<u64>) -> crate::model::FileEntry {
        crate::model::FileEntry {
            name: name.to_string(),
            path: path.to_string(),
            kind,
            size,
            modified_at: millis,
            bucket: false,
        }
    }

    #[test]
    fn entry_json_maps_kind_uri_and_timestamp() {
        let value = entry_json(
            "conn-1",
            &entry("报告.txt", "/sub/报告.txt", "file", Some(7), Some(1_700_000_000_000)),
        );
        assert_eq!(value["name"], "报告.txt");
        assert_eq!(value["kind"], "file");
        assert_eq!(value["uri"], "rclone://conn-1/sub/%E6%8A%A5%E5%91%8A.txt");
        assert_eq!(value["size"], 7);
        assert_eq!(value["modifiedAt"], "2023-11-14T22:13:20.000Z");
        assert_eq!(value["contentType"], "text/plain");

        let dir = entry_json("conn-1", &entry("photos", "/photos", "dir", None, None));
        assert_eq!(dir["kind"], "directory");
        assert_eq!(dir["uri"], "rclone://conn-1/photos");
        assert!(dir.get("size").is_none());
        assert!(dir.get("modifiedAt").is_none());
        assert!(dir.get("contentType").is_none(), "dirs carry no contentType");
    }

    #[test]
    fn content_type_table_is_conservative() {
        assert_eq!(guess_content_type("a.PNG"), Some("image/png"));
        assert_eq!(guess_content_type("archive.tar.gz"), Some("application/gzip"));
        assert_eq!(guess_content_type("noext"), None);
        assert_eq!(guess_content_type("weird.xyzzy"), None);
    }
}
