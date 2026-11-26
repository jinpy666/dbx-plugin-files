//! S3 user-metadata editing without re-uploading the object (issue #100).
//!
//! rclone's rc surface has no set-metadata endpoint; the supported recipe is
//! a two-step server-side rewrite that keeps the object bytes untouched:
//!
//! 1. `operations/copyfile` source → temporary key, with the global `_config`
//!    block `{"Metadata": true, "MetadataSet": {...}}`. `Metadata` makes the
//!    S3 backend emit `x-amz-metadata-directive: REPLACE` after re-reading the
//!    source object's metadata, so unspecified keys are preserved;
//!    `MetadataSet` overrides the edited ones.
//! 2. `operations/movefile` temporary key → original key with
//!    `{"IgnoreTimes": true}`. Without `IgnoreTimes` rclone can consider the
//!    destination up-to-date (same size/mtime) and delete the temporary object
//!    instead of copying it, silently reverting the metadata.
//!
//! Keys inside `_config` must be **Go field names** (`Metadata`,
//! `MetadataSet`, `IgnoreTimes`); `encoding/json` matches case-insensitively
//! but *not* across underscores, so `metadata_set` is silently ignored
//! (verified against rclone v1.75.2).
//!
//! Scope and limits:
//! - only S3-family backends (`s3`/`oss`/`cos`/`obs`) where the user metadata
//!   vocabulary is `x-amz-meta-*` and the five editable HTTP headers;
//! - deleting a metadata key is **not** supported by this path (an empty
//!   value would write an empty string), so empty values are rejected;
//! - the S3 `Expires` header is not writable through this path and is
//!   rejected with an explicit message;
//! - the temporary key is unique per attempt and never overwrites an existing
//!   object (a pre-flight stat retries with a fresh token on collision). It is
//!   removed on every failure path, but a crashed sidecar can leave one orphan
//!   object behind — see `docs/PROGRESS-P-FILES.zh-CN.md` residual risks.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::ops::gate_write;
use super::RcClient;

/// HTTP headers this method can write back. `expires` is deliberately absent:
/// rclone's S3 backend does not round-trip it, and pretending to would be a
/// silent no-op.
pub const SUPPORTED_HEADERS: [&str; 5] = [
    "content-type",
    "cache-control",
    "content-disposition",
    "content-encoding",
    "content-language",
];

/// User metadata prefix in the S3 vocabulary.
pub const USER_METADATA_PREFIX: &str = "x-amz-meta-";

/// Marker infix of the temporary rewrite key.
pub const TEMP_KEY_MARKER: &str = ".dbx-meta-tmp";

/// S3 limits user-defined metadata (keys + values, prefix included) to 2 KiB.
pub const MAX_USER_METADATA_BYTES: usize = 2048;

/// How many fresh temporary-key tokens to try before giving up.
const MAX_TEMP_ATTEMPTS: u32 = 3;

/// Backends whose metadata vocabulary this method speaks. Deliberately an
/// allow-list: `local`/`azureblob` also advertise rc `WriteMetadata`, but
/// their metadata keys are not `x-amz-meta-*` (POSIX/xattr, `x-ms-meta-*`),
/// so advertising the capability there would be a lie.
pub fn metadata_write_supported(backend_type: &str) -> bool {
    matches!(backend_type, "s3" | "oss" | "cos" | "obs")
}

fn ensure_supported_backend(backend_type: &str) -> Result<(), String> {
    if metadata_write_supported(backend_type) {
        return Ok(());
    }
    Err(format!(
        "Editing object metadata is not supported for the '{}' backend: \
         only S3-compatible object storage (s3/oss/cos/obs) exposes \
         x-amz-meta-* user metadata",
        if backend_type.is_empty() {
            "unknown"
        } else {
            backend_type
        }
    ))
}

/// A validated metadata edit, split by wire vocabulary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetadataEdit {
    /// HTTP header name (canonical lowercase) → value.
    pub headers: BTreeMap<String, String>,
    /// Bare user-metadata key (lowercase, prefix stripped) → value.
    pub user: BTreeMap<String, String>,
}

impl MetadataEdit {
    pub fn is_empty(&self) -> bool {
        self.headers.is_empty() && self.user.is_empty()
    }

    /// rclone `fs.MetadataSet` map: headers verbatim, user keys prefixed.
    pub fn metadata_set(&self) -> BTreeMap<String, String> {
        let mut set = self.headers.clone();
        for (key, value) in &self.user {
            set.insert(format!("{USER_METADATA_PREFIX}{key}"), value.clone());
        }
        set
    }

    /// `_config` for the first `operations/copyfile` hop. The keys are Go
    /// field names on purpose (see module docs).
    fn copy_config(&self) -> Value {
        json!({
            "Metadata": true,
            "MetadataSet": self.metadata_set(),
        })
    }
}

/// Parses and validates the wire map accepted by `files/setMetadata`.
///
/// Accepted keys:
/// - the five [`SUPPORTED_HEADERS`] (case-insensitive, normalized lowercase);
/// - `x-amz-meta-<key>` (case-insensitive, normalized lowercase).
///
/// Everything else is rejected with an actionable message. Values must be
/// non-empty (deletion is unsupported) and free of control characters
/// (header-injection guard).
pub fn parse_metadata_map(map: &BTreeMap<String, String>) -> Result<MetadataEdit, String> {
    if map.is_empty() {
        return Err("No metadata changes supplied: pass at least one header or \
                    x-amz-meta-* entry"
            .to_string());
    }
    let mut edit = MetadataEdit::default();
    for (raw_key, raw_value) in map {
        let key = raw_key.trim().to_ascii_lowercase();
        if key.is_empty() {
            return Err("Metadata keys must not be empty".to_string());
        }
        validate_value(&key, raw_value)?;
        let value = raw_value.trim().to_string();
        if let Some(suffix) = key.strip_prefix(USER_METADATA_PREFIX) {
            if suffix.is_empty() {
                return Err(format!(
                    "Metadata key '{raw_key}' is missing a name after '{USER_METADATA_PREFIX}'"
                ));
            }
            if !suffix
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
            {
                return Err(format!(
                    "Invalid user metadata key '{raw_key}': use ASCII letters, digits, '.', '_' or '-'"
                ));
            }
            edit.user.insert(suffix.to_string(), value);
            continue;
        }
        if SUPPORTED_HEADERS.contains(&key.as_str()) {
            edit.headers.insert(key, value);
            continue;
        }
        if key == "expires" || key == "x-amz-expires" {
            return Err(
                "The S3 'Expires' header is not supported by metadata editing".to_string(),
            );
        }
        return Err(format!(
            "Unsupported metadata key '{raw_key}': only {} and {USER_METADATA_PREFIX}* keys can be written",
            SUPPORTED_HEADERS
                .iter()
                .map(|header| format!("'{header}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let user_bytes: usize = edit
        .user
        .iter()
        .map(|(key, value)| USER_METADATA_PREFIX.len() + key.len() + value.len())
        .sum();
    if user_bytes > MAX_USER_METADATA_BYTES {
        return Err(format!(
            "User metadata is {user_bytes} bytes; S3 allows at most {MAX_USER_METADATA_BYTES} bytes"
        ));
    }
    Ok(edit)
}

fn validate_value(key: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!(
            "Metadata value for '{key}' must not be empty: removing a metadata key is not supported"
        ));
    }
    if value.chars().any(|ch| ch.is_control()) {
        return Err(format!(
            "Metadata value for '{key}' must not contain control characters"
        ));
    }
    Ok(())
}

/// Derives the temporary rewrite key for one attempt. Kept out of the async
/// path so naming is unit-testable; `token` must be unique per attempt.
pub fn temp_remote(relative: &str, token: &str) -> String {
    format!("{}{TEMP_KEY_MARKER}-{token}", relative.trim_end_matches('/'))
}

/// Per-attempt token: wall-clock nanoseconds + pid + process-local counter.
/// Collisions inside a process are impossible; across processes the pid and
/// the pre-flight stat cover the rest.
fn temp_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}{:04x}{:04x}", nanos, std::process::id() & 0xffff, seq & 0xffff)
}

/// `files/setMetadata`: `path` + validated metadata map → two-hop rewrite.
///
/// `metadata` is the raw wire map (rclone vocabulary); validation happens
/// here so callers cannot bypass it.
pub async fn set_metadata(
    client: &RcClient,
    fs: &str,
    path: &str,
    root: &str,
    lock_to_root: bool,
    backend_type: &str,
    metadata: &BTreeMap<String, String>,
) -> Result<(), String> {
    let edit = parse_metadata_map(metadata)?;
    set_metadata_with_tokens(client, fs, path, root, lock_to_root, backend_type, &edit, temp_token)
        .await
}

async fn set_metadata_with_tokens<F>(
    client: &RcClient,
    fs: &str,
    path: &str,
    root: &str,
    lock_to_root: bool,
    backend_type: &str,
    edit: &MetadataEdit,
    mut next_token: F,
) -> Result<(), String>
where
    F: FnMut() -> String,
{
    ensure_supported_backend(backend_type)?;
    let relative = gate_write(root, lock_to_root, path)?;
    if relative.is_empty() {
        return Err("Cannot edit metadata of the connection root".to_string());
    }

    // Pre-flight: the path must be an existing file. `operations/copyfile`
    // would otherwise fail with a less actionable message, and a directory
    // source must not reach the copy hop at all.
    let item = stat_item(client, fs, &relative).await?;
    let Some(item) = item else {
        return Err(format!(
            "Failed to set metadata for '{relative}': path does not exist"
        ));
    };
    if item.get("IsDir").and_then(Value::as_bool).unwrap_or(false) {
        return Err(format!(
            "Failed to set metadata for '{relative}': path is a directory; metadata editing applies to files only"
        ));
    }

    let temp = pick_temp_remote(client, fs, &relative, &mut next_token).await?;

    let copy = json!({
        "srcFs": fs,
        "srcRemote": relative,
        "dstFs": fs,
        "dstRemote": temp,
        "_config": edit.copy_config(),
    });
    if let Err(error) = client.call("operations/copyfile", &copy).await {
        cleanup_temp(client, fs, &temp).await;
        return Err(format!(
            "Failed to write metadata for '{relative}' (temporary object removed): {error}"
        ));
    }

    // Required: without IgnoreTimes the move can decide the destination is
    // up to date and drop the temporary object without copying it.
    let restore = json!({
        "srcFs": fs,
        "srcRemote": temp,
        "dstFs": fs,
        "dstRemote": relative,
        "_config": { "IgnoreTimes": true },
    });
    if let Err(error) = client.call("operations/movefile", &restore).await {
        cleanup_temp(client, fs, &temp).await;
        return Err(format!(
            "Failed to restore '{relative}' after the metadata rewrite (temporary object removed): {error}"
        ));
    }
    Ok(())
}

async fn stat_item(client: &RcClient, fs: &str, remote: &str) -> Result<Option<Value>, String> {
    let response = client
        .operations_stat(fs, remote)
        .await
        .map_err(|error| format!("Failed to stat '{remote}': {error}"))?;
    Ok(response
        .get("item")
        .filter(|item| !item.is_null())
        .cloned())
}

/// Allocates a temporary key that is guaranteed absent at allocation time.
/// A collision (stale object left by an old crash, or an astronomically
/// unlucky token clash) retries with a fresh token; nothing is overwritten.
async fn pick_temp_remote<F>(
    client: &RcClient,
    fs: &str,
    relative: &str,
    next_token: &mut F,
) -> Result<String, String>
where
    F: FnMut() -> String,
{
    for attempt in 0..MAX_TEMP_ATTEMPTS {
        let temp = temp_remote(relative, &next_token());
        if stat_item(client, fs, &temp).await?.is_none() {
            return Ok(temp);
        }
        if attempt + 1 == MAX_TEMP_ATTEMPTS {
            break;
        }
    }
    Err(format!(
        "Failed to set metadata for '{relative}': could not allocate a temporary object key \
         ({MAX_TEMP_ATTEMPTS} collisions); try again"
    ))
}

/// Best-effort removal of the temporary object. A failure here must not hide
/// the primary error (and the orphan is documented as a residual risk).
async fn cleanup_temp(client: &RcClient, fs: &str, temp: &str) {
    let _ = client
        .call("operations/deletefile", &json!({ "fs": fs, "remote": temp }))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    // -- wire map parsing ----------------------------------------------------

    #[test]
    fn splits_headers_and_user_metadata() {
        let edit = parse_metadata_map(&map(&[
            ("content-type", "application/json"),
            ("Cache-Control", "max-age=60"),
            ("x-amz-meta-owner", "bob"),
        ]))
        .expect("valid map");
        assert_eq!(
            edit.headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(
            edit.headers.get("cache-control").map(String::as_str),
            Some("max-age=60")
        );
        assert_eq!(edit.user.get("owner").map(String::as_str), Some("bob"));
    }

    #[test]
    fn metadata_set_uses_the_s3_wire_vocabulary() {
        let edit = parse_metadata_map(&map(&[
            ("content-type", "application/json"),
            ("x-amz-meta-owner", "bob"),
        ]))
        .expect("valid map");
        let set = edit.metadata_set();
        assert_eq!(set.get("content-type").map(String::as_str), Some("application/json"));
        assert_eq!(set.get("x-amz-meta-owner").map(String::as_str), Some("bob"));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn copy_config_uses_go_field_names_only() {
        let edit = parse_metadata_map(&map(&[("x-amz-meta-owner", "bob")])).expect("valid");
        let config = edit.copy_config();
        assert_eq!(config.get("Metadata"), Some(&Value::Bool(true)));
        assert_eq!(
            config
                .get("MetadataSet")
                .and_then(|value| value.get("x-amz-meta-owner"))
                .and_then(Value::as_str),
            Some("bob")
        );
        // snake_case is silently ignored by rclone — make sure it never leaks
        // into the payload (the exact failure mode the evidence doc records).
        assert!(config.get("metadata_set").is_none());
        assert!(config.get("metadata").is_none());
    }

    #[test]
    fn empty_map_is_rejected() {
        let error = parse_metadata_map(&BTreeMap::new()).expect_err("must reject");
        assert!(error.contains("No metadata changes supplied"), "{error}");
    }

    #[test]
    fn empty_and_whitespace_values_are_rejected() {
        for value in ["", "   ", "\t"] {
            let error = parse_metadata_map(&map(&[("x-amz-meta-owner", value)]))
                .expect_err("must reject empty value");
            assert!(error.contains("removing a metadata key is not supported"), "{error}");
        }
    }

    #[test]
    fn control_characters_in_values_are_rejected() {
        let error = parse_metadata_map(&map(&[("x-amz-meta-owner", "bob\r\nX-Evil: 1")]))
            .expect_err("must reject CRLF");
        assert!(error.contains("control characters"), "{error}");
    }

    #[test]
    fn expires_is_rejected_with_a_specific_message() {
        let error =
            parse_metadata_map(&map(&[("expires", "Thu, 01 Jan 1970 00:00:00 GMT")]))
                .expect_err("must reject");
        assert!(error.contains("'Expires' header is not supported"), "{error}");
    }

    #[test]
    fn unknown_keys_are_rejected_with_the_allowed_vocabulary() {
        let error = parse_metadata_map(&map(&[("x-goog-meta-owner", "bob")])).expect_err("reject");
        assert!(error.contains("Unsupported metadata key"), "{error}");
        assert!(error.contains("content-type"), "{error}");
        assert!(error.contains("x-amz-meta-"), "{error}");
    }

    #[test]
    fn bare_prefix_and_bad_charset_are_rejected() {
        let error = parse_metadata_map(&map(&[("x-amz-meta-", "bob")])).expect_err("reject");
        assert!(error.contains("missing a name"), "{error}");
        let error = parse_metadata_map(&map(&[("x-amz-meta-bo b", "bob")])).expect_err("reject");
        assert!(error.contains("Invalid user metadata key"), "{error}");
        let error = parse_metadata_map(&map(&[("x-amz-meta-bo/b", "bob")])).expect_err("reject");
        assert!(error.contains("Invalid user metadata key"), "{error}");
    }

    #[test]
    fn user_metadata_size_limit_is_enforced() {
        let big = "v".repeat(MAX_USER_METADATA_BYTES);
        let error = parse_metadata_map(&map(&[("x-amz-meta-big", &big)])).expect_err("reject");
        assert!(error.contains("at most 2048 bytes"), "{error}");
        // Exactly at the limit is fine.
        let exact = "v".repeat(MAX_USER_METADATA_BYTES - USER_METADATA_PREFIX.len() - "big".len());
        assert!(parse_metadata_map(&map(&[("x-amz-meta-big", &exact)])).is_ok());
    }

    // -- capability gating ---------------------------------------------------

    #[test]
    fn only_s3_family_claims_the_capability() {
        for backend in ["s3", "oss", "cos", "obs"] {
            assert!(metadata_write_supported(backend), "{backend}");
        }
        for backend in ["local", "fs", "webdav", "ftp", "sftp", "smb", "azureblob", "gcs", "b2"] {
            assert!(!metadata_write_supported(backend), "{backend}");
        }
    }

    // -- temporary key naming ------------------------------------------------

    #[test]
    fn temp_remote_keeps_the_object_prefix_and_marks_the_key() {
        assert_eq!(
            temp_remote("dir/a.txt", "abc123"),
            "dir/a.txt.dbx-meta-tmp-abc123"
        );
        assert_eq!(
            temp_remote("a.txt/", "abc123"),
            "a.txt.dbx-meta-tmp-abc123"
        );
    }

    #[test]
    fn generated_tokens_are_distinct_and_path_safe() {
        let first = temp_token();
        let second = temp_token();
        assert_ne!(first, second);
        assert!(first.chars().all(|ch| ch.is_ascii_hexdigit()), "{first}");
        assert!(!temp_remote("a.txt", &first).contains('/'));
    }

    // -- two-step rewrite over a stub rc server ------------------------------

    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    type Recorded = Arc<Mutex<Vec<(String, Value)>>>;

    /// Minimal rc stand-in: reads `count` JSON posts and replies with whatever
    /// `respond` returns for (path, body). Records every request for asserts.
    async fn stub_rc<F>(
        count: usize,
        respond: F,
    ) -> (RcClient, Recorded, tokio::task::JoinHandle<()>)
    where
        F: Fn(&str, &Value) -> Value + Send + Sync + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub");
        let port = listener.local_addr().expect("addr").port();
        let recorded: Recorded = Arc::new(Mutex::new(Vec::new()));
        let sink = recorded.clone();
        let respond = Arc::new(respond);
        let handle = tokio::spawn(async move {
            for _ in 0..count {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                let mut header_end = None;
                loop {
                    let Ok(read) = socket.read(&mut chunk).await else {
                        break;
                    };
                    if read == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    if header_end.is_none() {
                        header_end = buffer
                            .windows(4)
                            .position(|window| window == b"\r\n\r\n")
                            .map(|position| position + 4);
                    }
                    let Some(end) = header_end else { continue };
                    let headers = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buffer.len() >= end + length {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&buffer).to_string();
                let path = text
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let body = text
                    .find("\r\n\r\n")
                    .map(|index| &text[index + 4..])
                    .unwrap_or("")
                    .trim_end_matches(char::is_whitespace);
                let params: Value = serde_json::from_str(body).unwrap_or(Value::Null);
                sink.lock().expect("record").push((path.clone(), params.clone()));
                let response = respond(path.trim_start_matches('/'), &params);
                let payload = response.to_string();
                let message = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = socket.write_all(message.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (
            RcClient::new(format!("http://127.0.0.1:{port}"), "u".into(), "p".into()),
            recorded,
            handle,
        )
    }

    fn file_item() -> Value {
        json!({ "item": { "Path": "a.txt", "Name": "a.txt", "Size": 3, "IsDir": false } })
    }

    fn ok() -> Value {
        json!({})
    }

    fn error_response() -> Value {
        json!({ "error": "boom" })
    }

    #[tokio::test]
    async fn two_step_rewrite_sends_the_pinned_config_blocks() {
        // stat(source) → missing temp → copyfile → movefile.
        let stat_calls = Arc::new(AtomicU64::new(0));
        let counter = stat_calls.clone();
        let (client, recorded, server) = stub_rc(4, move |path, _params| match path {
            "operations/stat" => {
                if counter.fetch_add(1, Ordering::Relaxed) == 0 {
                    file_item()
                } else {
                    json!({ "item": null })
                }
            }
            "operations/copyfile" | "operations/movefile" => ok(),
            _ => ok(),
        })
        .await;

        let edit = parse_metadata_map(&map(&[
            ("content-type", "application/json"),
            ("x-amz-meta-owner", "bob"),
        ]))
        .expect("valid");
        set_metadata_with_tokens(
            &client,
            "dbx:",
            "/dir/a.txt",
            "",
            false,
            "s3",
            &edit,
            {
                let mut tokens = vec!["t1".to_string()].into_iter();
                move || tokens.next().unwrap_or_else(|| "tx".to_string())
            },
        )
        .await
        .expect("two-step rewrite");
        server.await.expect("stub task");

        let requests = recorded.lock().expect("recorded").clone();
        assert_eq!(requests.len(), 4, "{requests:#?}");
        let (copy_path, copy) = &requests[2];
        assert_eq!(copy_path, "/operations/copyfile");
        assert_eq!(copy["srcRemote"], json!("dir/a.txt"));
        assert_eq!(copy["dstRemote"], json!("dir/a.txt.dbx-meta-tmp-t1"));
        assert_eq!(copy["_config"]["Metadata"], json!(true));
        assert_eq!(
            copy["_config"]["MetadataSet"]["content-type"],
            json!("application/json")
        );
        assert_eq!(copy["_config"]["MetadataSet"]["x-amz-meta-owner"], json!("bob"));
        assert!(copy["_config"].get("metadata_set").is_none());

        let (move_path, restore) = &requests[3];
        assert_eq!(move_path, "/operations/movefile");
        assert_eq!(restore["srcRemote"], json!("dir/a.txt.dbx-meta-tmp-t1"));
        assert_eq!(restore["dstRemote"], json!("dir/a.txt"));
        assert_eq!(restore["_config"]["IgnoreTimes"], json!(true));
        // No accidental second metadata directive on the restore hop.
        assert!(restore["_config"].get("Metadata").is_none());
    }

    #[tokio::test]
    async fn temp_key_collision_retries_with_a_fresh_token() {
        // stat(source) → stat(t1)=exists → stat(t2)=missing → copy → move.
        let stat_calls = Arc::new(AtomicU64::new(0));
        let counter = stat_calls.clone();
        let (client, recorded, server) = stub_rc(5, move |path, params| match path {
            "operations/stat" => {
                let call = counter.fetch_add(1, Ordering::Relaxed);
                let remote = params["remote"].as_str().unwrap_or("");
                if call == 0 {
                    file_item()
                } else if remote.ends_with("-t1") {
                    file_item()
                } else {
                    json!({ "item": null })
                }
            }
            _ => ok(),
        })
        .await;

        let edit = parse_metadata_map(&map(&[("x-amz-meta-owner", "bob")])).expect("valid");
        set_metadata_with_tokens(
            &client,
            "dbx:",
            "a.txt",
            "",
            false,
            "s3",
            &edit,
            {
                let mut tokens = vec!["t1".to_string(), "t2".to_string()].into_iter();
                move || tokens.next().unwrap_or_default()
            },
        )
        .await
        .expect("collision retry");
        server.await.expect("stub task");
        let requests = recorded.lock().expect("recorded").clone();
        let copy = &requests[3].1;
        assert_eq!(copy["dstRemote"], json!("a.txt.dbx-meta-tmp-t2"));
    }

    #[tokio::test]
    async fn copy_failure_removes_the_temporary_key() {
        let stat_calls = Arc::new(AtomicU64::new(0));
        let counter = stat_calls.clone();
        let (client, recorded, server) = stub_rc(4, move |path, _params| match path {
            "operations/stat" => {
                if counter.fetch_add(1, Ordering::Relaxed) == 0 {
                    file_item()
                } else {
                    json!({ "item": null })
                }
            }
            "operations/copyfile" => error_response(),
            "operations/deletefile" => ok(),
            _ => ok(),
        })
        .await;

        let edit = parse_metadata_map(&map(&[("x-amz-meta-owner", "bob")])).expect("valid");
        let error = set_metadata_with_tokens(
            &client,
            "dbx:",
            "a.txt",
            "",
            false,
            "s3",
            &edit,
            || "t1".to_string(),
        )
        .await
        .expect_err("copy failure");
        assert!(error.contains("temporary object removed"), "{error}");
        server.await.expect("stub task");

        let requests = recorded.lock().expect("recorded").clone();
        assert_eq!(requests.len(), 4, "{requests:#?}");
        assert_eq!(requests[3].0, "/operations/deletefile");
        assert_eq!(requests[3].1["remote"], json!("a.txt.dbx-meta-tmp-t1"));
    }

    #[tokio::test]
    async fn restore_failure_removes_the_temporary_key() {
        let stat_calls = Arc::new(AtomicU64::new(0));
        let counter = stat_calls.clone();
        let (client, recorded, server) = stub_rc(5, move |path, _params| match path {
            "operations/stat" => {
                if counter.fetch_add(1, Ordering::Relaxed) == 0 {
                    file_item()
                } else {
                    json!({ "item": null })
                }
            }
            "operations/copyfile" => ok(),
            "operations/movefile" => error_response(),
            "operations/deletefile" => ok(),
            _ => ok(),
        })
        .await;

        let edit = parse_metadata_map(&map(&[("x-amz-meta-owner", "bob")])).expect("valid");
        let error = set_metadata_with_tokens(
            &client,
            "dbx:",
            "a.txt",
            "",
            false,
            "s3",
            &edit,
            || "t1".to_string(),
        )
        .await
        .expect_err("restore failure");
        assert!(error.contains("Failed to restore"), "{error}");
        server.await.expect("stub task");

        let requests = recorded.lock().expect("recorded").clone();
        assert_eq!(requests.len(), 5, "{requests:#?}");
        assert_eq!(requests[4].0, "/operations/deletefile");
        assert_eq!(requests[4].1["remote"], json!("a.txt.dbx-meta-tmp-t1"));
    }

    #[tokio::test]
    async fn unsupported_backend_fails_before_any_rc_traffic() {
        let client = RcClient::new(
            "http://127.0.0.1:1".to_string(),
            "u".to_string(),
            "p".to_string(),
        );
        let edit = parse_metadata_map(&map(&[("x-amz-meta-owner", "bob")])).expect("valid");
        let error = set_metadata_with_tokens(
            &client,
            "/",
            "a.txt",
            "",
            false,
            "local",
            &edit,
            || "t1".to_string(),
        )
        .await
        .expect_err("unsupported backend");
        assert!(error.contains("not supported for the 'local' backend"), "{error}");
    }

    #[tokio::test]
    async fn directory_targets_are_rejected_before_the_copy_hop() {
        let (client, recorded, server) = stub_rc(1, move |_path, _params| {
            json!({ "item": { "Path": "dir", "Name": "dir", "IsDir": true } })
        })
        .await;
        let edit = parse_metadata_map(&map(&[("x-amz-meta-owner", "bob")])).expect("valid");
        let error = set_metadata_with_tokens(
            &client,
            "dbx:",
            "dir",
            "",
            false,
            "s3",
            &edit,
            || "t1".to_string(),
        )
        .await
        .expect_err("directory");
        assert!(error.contains("path is a directory"), "{error}");
        server.await.expect("stub task");
        let requests = recorded.lock().expect("recorded").clone();
        assert_eq!(requests.len(), 1, "{requests:#?}");
        assert_eq!(requests[0].0, "/operations/stat");
    }
}
