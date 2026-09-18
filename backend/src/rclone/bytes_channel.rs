//! Binary transfer byte channel for the rclone engine (F-RCLONE phase B).
//!
//! Upload side: the host pushes 256KiB frames (8-byte BE offset prefix is
//! stripped by `main.rs::handle_binary`'s successor) into a local staging
//! file via [`UploadStaging`]; `finish` places the staged bytes at the exact
//! remote path through rcd and removes the staging file. Download side:
//! [`download_pump`] streams an rcd rc-serve body and re-emits it in
//! ≤[`TRANSFER_CHUNK_SIZE`] slices to a caller callback (the wiring layer
//! writes those slices back onto the host binary channel);
//! [`download_to_file`] is the `save_to_local` variant writing straight to
//! disk. [`remote_size`] prefetches the object size for download/start.
//!
//! # Live rcd behavior this module encodes around (rclone 1.75.1, verified
//! against a real `rclone rcd --rc-serve`)
//!
//! 1. rc-serve GET requires the URL form `/[{fs}]/{remote}` — the rcserver
//!    parses GET paths with `^\[(.*?)\](.*)$` (literal brackets, `fs/rc/
//!    rcserver/rcserver.go::handleGet`). A plain `{base}/{fs}/{remote}` URL
//!    (what `rc.rs::serve_url` currently builds) always answers 404. This
//!    module wraps the fs string in brackets itself (single choke point:
//!    [`serve_fs`]). If the integrator fixes `serve_url` to emit the bracket
//!    form, drop the wrapper — do not keep both or the fs gets double-
//!    bracketed.
//! 2. `operations/uploadfile` replies `{}` (no item to verify) and uploads
//!    the multipart part to `path.Join(remote, <part filename>)` (`fs/
//!    operations/rc.go`: `Rcat(ctx, f, path.Join(remote, p.FileName()), ...)`).
//!    `rc.rs::operations_uploadfile` pins the part filename to `payload`, so
//!    it can only address `<remote>/payload`. To land the file at an exact
//!    remote path, [`upload_staged_exact`] uploads into a transient staging
//!    directory and finishes with a server-side `operations/movefile` to the
//!    target (`{}` on success, same-fs = rename/copy+delete inside rclone).
//!
//! Progress/counters are exposed as plain numbers (`received`, pump totals);
//! event emission and throttling belong to the wiring layer. Errors are
//! uniformly `String`.

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde_json::json;

use super::rc::{RcClient, RcError};
use crate::model::TRANSFER_CHUNK_SIZE;

// ---------------------------------------------------------------------------
// Upload staging
// ---------------------------------------------------------------------------

/// Local staging sink for one upload task: frames are appended at their
/// running offset (strict continuity) into a temp file that `finish` streams
/// to the remote through rcd.
///
/// Lifecycle: `start` → N × `append` → `finish` (upload + cleanup) or
/// `abort`. Dropping without `abort` also removes the staging file, so a
/// lost task cannot leak temp bytes.
pub struct UploadStaging {
    task_id: String,
    path: PathBuf,
    file: Option<std::fs::File>,
    received: u64,
    expected: u64,
}

impl UploadStaging {
    /// Opens a fresh staging file for `task_id`. `expected_size` of 0 means
    /// "unknown" and disables the strict size checks in `append`/`finish`.
    pub fn start(task_id: &str, expected_size: u64) -> Result<Self, String> {
        let dir = std::env::temp_dir().join(format!("dbx-files-upload-{}", std::process::id()));
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("Failed to create staging dir '{}': {error}", dir.display()))?;
        // uuid suffix: even a retried/colliding task id must never clobber
        // another in-flight staging file in the shared per-process dir.
        let path = dir.join(format!(
            "{}-{}.staging",
            sanitize_task_id(task_id),
            uuid::Uuid::new_v4().simple()
        ));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .map_err(|error| {
                format!("Failed to open staging file '{}': {error}", path.display())
            })?;
        Ok(Self {
            task_id: task_id.to_string(),
            path,
            file: Some(file),
            received: 0,
            expected: expected_size,
        })
    }

    /// Appends one frame. `offset` must equal the number of bytes already
    /// received (the host binary channel is strictly sequential) — gaps,
    /// overlaps and rewinds are protocol violations and error out. With a
    /// known `expected_size`, frames may not push the file past it.
    /// Returns the cumulative received byte count (the wiring layer feeds
    /// this straight into its progress bookkeeping).
    pub fn append(&mut self, offset: u64, data: &[u8]) -> Result<u64, String> {
        if offset != self.received {
            return Err(format!(
                "upload frame out of order for task '{}': offset {offset} but {} bytes \
                 already received",
                self.task_id, self.received
            ));
        }
        let new_total = self.received.saturating_add(data.len() as u64);
        if self.expected > 0 && new_total > self.expected {
            return Err(format!(
                "upload frame for task '{}' exceeds the declared size: {} bytes received \
                 would pass the expected {}",
                self.task_id, new_total, self.expected
            ));
        }
        if !data.is_empty() {
            let file = self
                .file
                .as_mut()
                .ok_or_else(|| format!("upload staging for task '{}' already closed", self.task_id))?;
            file.seek(SeekFrom::Start(offset))
                .map_err(|error| format!("Failed to seek staging file for task '{}': {error}", self.task_id))?;
            file.write_all(data)
                .map_err(|error| format!("Failed to write staging file for task '{}': {error}", self.task_id))?;
        }
        self.received = new_total;
        Ok(self.received)
    }

    /// Bytes received so far.
    pub fn received(&self) -> u64 {
        self.received
    }

    /// Declared total size (0 = unknown).
    pub fn expected(&self) -> u64 {
        self.expected
    }

    /// Local staging file location (diagnostics/tests; removed on finish,
    /// abort or drop).
    pub fn staging_path(&self) -> &Path {
        &self.path
    }

    /// Verifies the received byte count against the declared size, streams
    /// the staging file to `fs:remote` through rcd and removes the staging
    /// file (on success *and* failure — a failed finish is not retryable
    /// because `self` is consumed). Returns the uploaded byte count, i.e.
    /// the staging file size; the `operations/uploadfile` response carries
    /// no item to cross-check against (live rcd answers `{}`).
    pub async fn finish(
        mut self,
        client: &RcClient,
        fs: &str,
        remote: &str,
        mime: Option<&str>,
    ) -> Result<u64, String> {
        if self.expected > 0 && self.received != self.expected {
            let message = format!(
                "upload size mismatch for task '{}': expected {} bytes but received {}",
                self.task_id, self.expected, self.received
            );
            self.cleanup();
            return Err(message);
        }
        if let Some(file) = self.file.as_ref() {
            file.sync_all().map_err(|error| {
                format!("Failed to flush staging file for task '{}': {error}", self.task_id)
            })?;
        }
        drop(self.file.take());
        let result = upload_staged_exact(client, fs, &self.path, remote, mime).await;
        self.cleanup();
        result?;
        Ok(self.received)
    }

    /// Discards the staging file (idempotent — also runs on Drop). No rc
    /// traffic: before `finish` nothing has been placed on the remote.
    pub fn abort(mut self) {
        self.cleanup();
    }

    /// Removes the staging file and, best-effort, the now-possibly-empty
    /// per-process staging directory. Safe to call repeatedly and while the
    /// file handle is still open (the handle is closed first so Windows
    /// delete works).
    fn cleanup(&mut self) {
        drop(self.file.take());
        let _ = std::fs::remove_file(&self.path);
        if let Some(parent) = self.path.parent() {
            // Succeeds only when empty — other tasks may still stage here.
            let _ = std::fs::remove_dir(parent);
        }
    }
}

impl Drop for UploadStaging {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Staging file name fragment: task ids are host-generated, so strip
/// everything outside `[A-Za-z0-9._-]` and cap the length.
fn sanitize_task_id(task_id: &str) -> String {
    let cleaned: String = task_id
        .chars()
        .take(80)
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect();
    if cleaned.is_empty() {
        "task".to_string()
    } else {
        cleaned
    }
}

/// Places the local `staging_file` at the exact remote path `fs:remote`.
///
/// See the module docs for why this is uploadfile-into-staging-dir +
/// movefile instead of a single uploadfile call. Cleanup of the transient
/// remote staging directory is best-effort on every path; on exotic
/// mid-upload failures a `.dbx-files-upload-*` dot directory could survive
/// in the target's parent (visible hazard documented in the handoff notes).
async fn upload_staged_exact(
    client: &RcClient,
    fs: &str,
    staging_file: &Path,
    remote: &str,
    mime: Option<&str>,
) -> Result<(), String> {
    let (parent, name) = split_remote(remote);
    if name.is_empty() {
        return Err(format!("Invalid remote path '{remote}': empty file name"));
    }
    let stage_leaf = format!(".dbx-files-upload-{}", uuid::Uuid::new_v4().simple());
    let stage_dir = if parent.is_empty() {
        stage_leaf
    } else {
        format!("{parent}/{stage_leaf}")
    };
    let staged = format!("{stage_dir}/payload");

    let result = async {
        client
            .operations_uploadfile(fs, &stage_dir, staging_file, mime)
            .await
            .map_err(|error| format!("Failed to upload '{remote}': {error}"))?;
        client
            .call(
                "operations/movefile",
                &json!({
                    "srcFs": fs,
                    "srcRemote": &staged,
                    "dstFs": fs,
                    "dstRemote": remote,
                }),
            )
            .await
            .map_err(|error| format!("Failed to finalize upload of '{remote}': {error}"))?;
        Ok(())
    }
    .await;

    match result {
        // The move removed the staged file; only the empty dot dir remains.
        Ok(()) => {
            let _ = client
                .call("operations/rmdir", &json!({ "fs": fs, "remote": &stage_dir }))
                .await;
        }
        // Best-effort sweep of a half-finished staging dir.
        Err(_) => {
            let _ = client
                .call("operations/deletefile", &json!({ "fs": fs, "remote": &staged }))
                .await;
            let _ = client
                .call("operations/rmdir", &json!({ "fs": fs, "remote": &stage_dir }))
                .await;
        }
    }
    result
}

/// Splits `dir/name` at the last `/`; the parent may be empty (root).
fn split_remote(remote: &str) -> (&str, &str) {
    match remote.rfind('/') {
        Some(index) => (&remote[..index], &remote[index + 1..]),
        None => ("", remote),
    }
}

// ---------------------------------------------------------------------------
// Download pump
// ---------------------------------------------------------------------------

/// Streams `fs:remote` through rcd's rc-serve GET and hands the body to
/// `on_chunk` in slices of at most [`TRANSFER_CHUNK_SIZE`] bytes (network
/// chunks are variable-sized, so they are re-aggregated). Returns the total
/// byte count. A callback error aborts the pump and is returned verbatim —
/// the wiring layer uses that to stop forwarding frames to the host.
pub async fn download_pump(
    client: &RcClient,
    fs: &str,
    remote: &str,
    on_chunk: &mut (dyn FnMut(&[u8]) -> Result<(), String> + Send),
) -> Result<u64, String> {
    let mut response = open_serve(client, fs, remote).await?;
    let mut buffer: Vec<u8> = Vec::with_capacity(TRANSFER_CHUNK_SIZE);
    let mut total: u64 = 0;
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|error| format!("Failed to read '{remote}': {error}"))?;
        let Some(chunk) = chunk else { break };
        total += chunk.len() as u64;
        buffer.extend_from_slice(&chunk);
        while buffer.len() >= TRANSFER_CHUNK_SIZE {
            on_chunk(&buffer[..TRANSFER_CHUNK_SIZE])
                .map_err(|error| format!("download sink rejected data for '{remote}': {error}"))?;
            buffer.drain(..TRANSFER_CHUNK_SIZE);
        }
    }
    if !buffer.is_empty() {
        on_chunk(&buffer)
            .map_err(|error| format!("download sink rejected data for '{remote}': {error}"))?;
    }
    Ok(total)
}

/// `save_to_local` variant: streams `fs:remote` into the local file at
/// `dest` (parent directory is the caller's job) and returns the total byte
/// count.
pub async fn download_to_file(
    client: &RcClient,
    fs: &str,
    remote: &str,
    dest: &Path,
) -> Result<u64, String> {
    let file = std::fs::File::create(dest)
        .map_err(|error| format!("Failed to create '{}': {error}", dest.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    let total = {
        let mut sink = |chunk: &[u8]| -> Result<(), String> {
            writer
                .write_all(chunk)
                .map_err(|error| format!("Failed to write '{}': {error}", dest.display()))
        };
        download_pump(client, fs, remote, &mut sink).await?
    };
    writer
        .flush()
        .map_err(|error| format!("Failed to flush '{}': {error}", dest.display()))?;
    Ok(total)
}

/// Object size via `operations/stat` for the download/start metadata
/// prefetch. A missing object is `Ok(None)` (`{"item": null}` is rcd's
/// not-found answer), everything else is an error string.
pub async fn remote_size(client: &RcClient, fs: &str, remote: &str) -> Result<Option<u64>, String> {
    let response = client
        .operations_stat(fs, remote)
        .await
        .map_err(|error| format!("Failed to stat '{remote}': {error}"))?;
    let item = response.get("item").filter(|item| !item.is_null());
    Ok(item.and_then(|item| item.get("Size")).and_then(serde_json::Value::as_u64))
}

// ---------------------------------------------------------------------------
// rc-serve plumbing
// ---------------------------------------------------------------------------

/// rc-serve GET URLs must carry the fs string inside literal brackets
/// (`/[{fs}]/{remote}`); see the module docs. This is the single choke
/// point — drop the wrapping once `rc.rs::serve_url` emits the bracket form
/// itself.
fn serve_fs(fs: &str) -> String {
    format!("[{fs}]")
}

/// Opens the rc-serve GET for `fs:remote`, mapping rcd's 404 (missing
/// object) to a clear error. Any non-2xx becomes `RcError::Http` inside a
/// `String`.
async fn open_serve(
    client: &RcClient,
    fs: &str,
    remote: &str,
) -> Result<reqwest::Response, String> {
    if remote.trim_matches('/').is_empty() {
        return Err(format!("Invalid download path '{remote}': empty remote path"));
    }
    match client.serve_get(&serve_fs(fs), remote, None).await {
        Ok(response) => Ok(response),
        Err(RcError::Http { status: 404, .. }) => Err(format!(
            "Failed to download '{remote}': file does not exist (rc-serve 404)"
        )),
        Err(error) => Err(format!("Failed to download '{remote}': {error}")),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// One live rcd per test on an ephemeral port + a fresh tempdir fs
    /// (same pattern as `ops.rs` tests). Returns `None` when no rclone
    /// binary is available so CI without the binary stays green.
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
            let handle = super::super::proc::RcdHandle::start(&binary)
                .await
                .expect("rcd should spawn");
            let client = handle.client();
            std::mem::forget(handle); // tests are process-exit scoped
            let dir = tempfile::tempdir().expect("tempdir");
            Some(Live {
                client,
                fs: dir.path().to_string_lossy().to_string(),
                _dir: dir,
            })
        }
    }

    /// Deterministic payload with length not aligned to the chunk size so
    /// the pump must emit a partial final slice.
    fn deterministic_payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Uploads `payload` to `fs:remote` through the staging path in
    /// TRANSFER_CHUNK_SIZE frames; returns the uploaded byte count.
    async fn stage_upload(client: &RcClient, fs: &str, remote: &str, payload: &[u8]) -> u64 {
        let mut staging =
            UploadStaging::start("helper", payload.len() as u64).expect("staging start");
        for (index, frame) in payload.chunks(TRANSFER_CHUNK_SIZE).enumerate() {
            staging
                .append(index as u64 * TRANSFER_CHUNK_SIZE as u64, frame)
                .expect("append frame");
        }
        staging
            .finish(client, fs, remote, Some("application/octet-stream"))
            .await
            .expect("finish upload")
    }

    // -- offline unit tests (no rcd required) ----------------------------

    #[test]
    fn append_enforces_continuity_and_declared_size() {
        let mut staging = UploadStaging::start("unit-a", 100).expect("start");
        let path = staging.staging_path().to_path_buf();
        assert_eq!(staging.expected(), 100);
        assert_eq!(staging.append(0, &[0u8; 40]).unwrap(), 40);
        assert_eq!(staging.received(), 40);
        // Gap (offset jumps past received) is rejected.
        assert!(staging.append(500, &[0u8; 10]).is_err());
        // Overlap (offset rewinds) is rejected.
        assert!(staging.append(20, &[0u8; 10]).is_err());
        // Frame pushing past the declared size is rejected.
        assert!(staging.append(40, &[0u8; 61]).is_err());
        // Empty frame at the right offset is a legal no-op.
        assert_eq!(staging.append(40, &[]).unwrap(), 40);
        staging.abort();
        assert!(!path.exists(), "abort removes staging");
    }

    #[test]
    fn unknown_expected_size_disables_size_guard() {
        let mut staging = UploadStaging::start("unit-b", 0).expect("start");
        assert_eq!(staging.expected(), 0);
        staging.append(0, &[0u8; 300]).expect("oversized frame ok when size unknown");
        assert_eq!(staging.received(), 300);
        staging.abort();
    }

    #[test]
    fn drop_cleans_up_staging_file() {
        let path;
        {
            let staging = UploadStaging::start("unit-c", 0).expect("start");
            path = staging.staging_path().to_path_buf();
            assert!(path.exists());
        }
        assert!(!path.exists(), "Drop must remove the staging file");
    }

    #[test]
    fn sanitize_task_id_strips_path_metacharacters() {
        assert_eq!(sanitize_task_id("ab/cd:ef"), "ab_cd_ef");
        assert_eq!(sanitize_task_id("ok-id_1.staging"), "ok-id_1.staging");
        assert_eq!(sanitize_task_id(""), "task");
        assert_eq!(sanitize_task_id("///"), "___");
    }

    #[test]
    fn split_remote_splits_at_last_slash() {
        assert_eq!(split_remote("dir/sub/file.bin"), ("dir/sub", "file.bin"));
        assert_eq!(split_remote("file.bin"), ("", "file.bin"));
        assert_eq!(split_remote("dir/"), ("dir", ""));
    }

    // -- live tests (real rcd) -------------------------------------------

    #[tokio::test]
    async fn live_upload_three_frames_finishes_and_cleans_staging() {
        let Some(live) = Live::start().await else { return };
        let payload = deterministic_payload(100_000);

        let mut staging = UploadStaging::start("task-3frames", payload.len() as u64)
            .expect("staging start");
        let path = staging.staging_path().to_path_buf();
        staging.append(0, &payload[0..40_000]).expect("frame 1");
        staging.append(40_000, &payload[40_000..80_000]).expect("frame 2");
        staging.append(80_000, &payload[80_000..]).expect("frame 3");
        assert_eq!(staging.received(), payload.len() as u64);

        let uploaded = staging
            .finish(&live.client, &live.fs, "bc-out.bin", Some("application/octet-stream"))
            .await
            .expect("finish");
        assert_eq!(uploaded, payload.len() as u64);
        assert!(!path.exists(), "finish removes the staging file");

        // The file must sit at the exact remote path (not <dir>/payload).
        let size = remote_size(&live.client, &live.fs, "bc-out.bin")
            .await
            .expect("stat uploaded file");
        assert_eq!(size, Some(payload.len() as u64));
    }

    #[tokio::test]
    async fn live_finish_size_mismatch_is_rejected() {
        let Some(live) = Live::start().await else { return };
        let mut staging = UploadStaging::start("task-mismatch", 100).expect("start");
        staging.append(0, &[0u8; 40]).expect("append");
        let error = staging.finish(&live.client, &live.fs, "bc-mismatch.bin", None)
            .await
            .expect_err("finish must reject size mismatch");
        assert!(error.contains("size mismatch"), "{error}");
        // Nothing landed remotely.
        assert_eq!(
            remote_size(&live.client, &live.fs, "bc-mismatch.bin").await.expect("stat"),
            None
        );
    }

    #[tokio::test]
    async fn live_download_pump_streams_exact_bytes_and_aborts_on_sink_error() {
        let Some(live) = Live::start().await else { return };
        let payload = deterministic_payload(1024 * 1024 + 137);
        stage_upload(&live.client, &live.fs, "bc-pump.bin", &payload).await;

        let shared = Arc::new(Mutex::new((Vec::<u8>::new(), 0usize, 0u32)));
        let sink_shared = Arc::clone(&shared);
        let mut sink = move |chunk: &[u8]| -> Result<(), String> {
            let mut guard = sink_shared.lock().expect("lock");
            guard.0.extend_from_slice(chunk);
            guard.1 = guard.1.max(chunk.len());
            guard.2 += 1;
            Ok(())
        };
        let total = download_pump(&live.client, &live.fs, "bc-pump.bin", &mut sink)
            .await
            .expect("pump");
        assert_eq!(total, payload.len() as u64);
        let (bytes, max_slice, calls) = Arc::try_unwrap(shared)
            .map(|mutex| mutex.into_inner().expect("join"))
            .unwrap_or_else(|arc| arc.lock().expect("lock").clone());
        assert_eq!(bytes, payload, "pumped bytes must match byte for byte");
        assert!(max_slice <= TRANSFER_CHUNK_SIZE, "slice {max_slice} over chunk limit");
        assert!(calls >= 2, "expected multiple slices, got {calls}");

        // A rejecting sink aborts the pump with the sink's error.
        let mut rejecting = |chunk: &[u8]| -> Result<(), String> {
            let _ = chunk;
            Err("sink closed".to_string())
        };
        let error = download_pump(&live.client, &live.fs, "bc-pump.bin", &mut rejecting)
            .await
            .expect_err("sink error must abort");
        assert!(error.contains("sink closed"), "{error}");
    }

    #[tokio::test]
    async fn live_download_to_file_writes_exact_content() {
        let Some(live) = Live::start().await else { return };
        let payload = deterministic_payload(300_000);
        stage_upload(&live.client, &live.fs, "bc-file.bin", &payload).await;

        let dest_dir = tempfile::tempdir().expect("dest tempdir");
        let dest = dest_dir.path().join("saved.bin");
        let total = download_to_file(&live.client, &live.fs, "bc-file.bin", &dest)
            .await
            .expect("download to file");
        assert_eq!(total, payload.len() as u64);
        let saved = std::fs::read(&dest).expect("read saved file");
        assert_eq!(saved, payload);
    }

    #[tokio::test]
    async fn live_download_missing_file_is_clear_error() {
        let Some(live) = Live::start().await else { return };
        let mut sink = |chunk: &[u8]| -> Result<(), String> {
            let _ = chunk;
            Ok(())
        };
        let error = download_pump(&live.client, &live.fs, "bc-nope.bin", &mut sink)
            .await
            .expect_err("missing file must error");
        assert!(error.contains("does not exist"), "{error}");
    }

    #[tokio::test]
    async fn live_remote_size_some_and_none() {
        let Some(live) = Live::start().await else { return };
        stage_upload(&live.client, &live.fs, "bc-size.bin", &[7u8; 1234]).await;
        assert_eq!(
            remote_size(&live.client, &live.fs, "bc-size.bin").await.expect("stat"),
            Some(1234)
        );
        assert_eq!(
            remote_size(&live.client, &live.fs, "bc-missing.bin")
                .await
                .expect("missing is Ok(None)"),
            None
        );
    }
}
