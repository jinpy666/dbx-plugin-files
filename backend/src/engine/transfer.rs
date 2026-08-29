//! Streaming read/write primitives backing the binary transfer slots
//! (implementation doc §5.3/§8.3) plus the directory traversal used by the
//! self-built syncDir/copyDir jobs (§7).
//!
//! Split from `transfers.rs` so the job table stays protocol-focused while
//! every OpenDAL call lives here:
//! - uploads open a `Writer` with `chunk(4MiB).concurrent(4)`;
//! - downloads stat the size first, then open a (prefetching) `Reader` and
//!   serve `TRANSFER_CHUNK_SIZE` slices per binary frame;
//! - `walk_files` does manual recursion over `Operator::list` (no futures
//!   dependency needed — `Lister` implements `Stream` which is not
//!   re-exported and adding crates is out of scope).

#![allow(dead_code)]

use opendal::{Operator, Reader, Writer};

/// Upload writer chunking (§8.3): OpenDAL decides multipart upload sizing.
pub const UPLOAD_WRITER_CHUNK: usize = 4 * 1024 * 1024;
/// Upload writer in-flight chunk concurrency (§8.3).
pub const UPLOAD_WRITER_CONCURRENT: usize = 4;

/// Opens the upload `Writer` for `remote_path` (`chunk(4MiB).concurrent(4)`).
pub async fn open_upload_writer(operator: &Operator, remote_path: &str) -> Result<Writer, String> {
    operator
        .writer_with(remote_path)
        .chunk(UPLOAD_WRITER_CHUNK)
        .concurrent(UPLOAD_WRITER_CONCURRENT)
        .await
        .map_err(|error| format!("Failed to open upload writer for '{remote_path}': {error}"))
}

/// Opens the download side: stats `remote_path` first (fails on directories),
/// then opens the `Reader`. Returns `(reader, size)`; the reader prefetches
/// ahead of the pump loop on its own.
pub async fn open_download_reader(
    operator: &Operator,
    remote_path: &str,
) -> Result<(Reader, u64), String> {
    let metadata = operator
        .stat(remote_path)
        .await
        .map_err(|error| format!("Failed to stat '{remote_path}' for download: {error}"))?;
    if metadata.mode().is_dir() {
        return Err(format!("Cannot download '{remote_path}': it is a directory"));
    }
    let size = metadata.content_length();
    let reader = operator
        .reader(remote_path)
        .await
        .map_err(|error| format!("Failed to open download reader for '{remote_path}': {error}"))?;
    Ok((reader, size))
}

/// Reads the range `[offset, end)` (end is exclusive and must not exceed the
/// stat'ed file size — OpenDAL readers error on EOF-overrun ranges). An empty
/// `Vec` means EOF.
pub async fn read_chunk(reader: &Reader, offset: u64, end: u64) -> Result<Vec<u8>, String> {
    let buffer = reader
        .read(offset..end)
        .await
        .map_err(|error| format!("Download read failed at offset {offset}: {error}"))?;
    Ok(buffer.to_vec())
}

/// Encodes one wire frame: 8-byte BE offset + payload (ssh-sftp alignment).
pub fn frame(offset: u64, payload: &[u8]) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + payload.len());
    data.extend_from_slice(&offset.to_be_bytes());
    data.extend_from_slice(payload);
    data
}

/// One file discovered by [`walk_files`]: OpenDAL-relative path + size
/// (`content_length`; 0 when the backend does not report it).
pub type WalkedFile = (String, u64);

/// Recursively collects the files under `dir` (directories are recursed, not
/// returned). Paths are relative to `dir` itself — e.g. walking `src` yields
/// `a.txt`, `nested/b.txt` — so they join directly onto a target directory
/// (rclone copy semantics: `copyDir src → dst` produces `dst/a.txt`, not
/// `dst/src/a.txt`). Walking the root (`""`/`"/"`) yields root-relative
/// paths. Manual recursion via `list` per level.
pub async fn walk_files(operator: &Operator, dir: &str) -> Result<Vec<WalkedFile>, String> {
    let mut files = Vec::new();
    walk_inner(operator, dir, &mut files).await?;
    let prefix = dir.trim().trim_matches('/');
    if prefix.is_empty() {
        return Ok(files);
    }
    let prefix = format!("{prefix}/");
    Ok(files
        .into_iter()
        .map(|(path, size)| {
            let relative = path.strip_prefix(&prefix).unwrap_or(&path).to_string();
            (relative, size)
        })
        .collect())
}

async fn walk_inner(
    operator: &Operator,
    dir: &str,
    files: &mut Vec<WalkedFile>,
) -> Result<(), String> {
    let prefix = dir.trim().trim_matches('/');
    let list_path = if prefix.is_empty() {
        "/".to_string()
    } else {
        format!("/{prefix}/")
    };
    let entries = operator
        .list(&list_path)
        .await
        .map_err(|error| format!("Failed to list '{prefix}': {error}"))?;
    for entry in entries {
        let path = entry.path().to_string();
        // Some backends emit a marker entry for the listed directory itself.
        let self_marker = if prefix.is_empty() {
            path == "/"
        } else {
            path == prefix || path == format!("{prefix}/")
        };
        if self_marker {
            continue;
        }
        let metadata = entry.metadata();
        if metadata.mode().is_dir() {
            // Box::pin: async recursion without a size-cycle warning.
            Box::pin(walk_inner(operator, &path, files)).await?;
        } else {
            files.push((path, metadata.content_length()));
        }
    }
    Ok(())
}

/// Parent directory of an OpenDAL-relative file path (`""` for the root).
pub fn parent_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(index) => path[..index].to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TRANSFER_CHUNK_SIZE;

    fn memory_operator() -> Operator {
        opendal::Operator::via_iter("memory", Vec::<(String, String)>::new()).unwrap()
    }

    #[test]
    fn frames_carry_be_offset_prefix() {
        let payload = frame(7, b"abc");
        assert_eq!(payload.len(), 11);
        assert_eq!(&payload[..8], &7u64.to_be_bytes());
        assert_eq!(&payload[8..], b"abc");
        // Round-trips through the frozen upload-frame parser.
        let (offset, decoded) = crate::transfers::parse_upload_frame(&payload).unwrap();
        assert_eq!(offset, 7);
        assert_eq!(decoded, b"abc");
    }

    #[test]
    fn parent_dir_strips_one_level() {
        assert_eq!(parent_dir("a/b/c.txt"), "a/b");
        assert_eq!(parent_dir("top.txt"), "");
    }

    #[tokio::test]
    async fn walk_files_recurses_and_reports_sizes() {
        let operator = memory_operator();
        operator.write("dir/one.txt", "12345").await.unwrap();
        operator.write("dir/inner/two.bin", "xy").await.unwrap();
        operator.write("root.bin", "z").await.unwrap();
        operator.create_dir("dir/empty/").await.unwrap();

        let files = walk_files(&operator, "dir").await.unwrap();
        let mut names: Vec<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["inner/two.bin", "one.txt"], "relative to the walked dir");
        let one = files.iter().find(|(path, _)| path == "one.txt").unwrap();
        assert_eq!(one.1, 5);

        let root_files = walk_files(&operator, "").await.unwrap();
        assert_eq!(root_files.len(), 3, "root walk sees every file");
    }

    #[tokio::test]
    async fn upload_download_roundtrip_through_primitives() {
        let operator = memory_operator();
        let payload: Vec<u8> = (0..=255u8).cycle().take(600 * 1024).collect();
        let mut writer = open_upload_writer(&operator, "blob.bin").await.unwrap();
        for chunk in payload.chunks(100 * 1024) {
            writer.write(chunk.to_vec()).await.unwrap();
        }
        let metadata = writer.close().await.unwrap();
        assert_eq!(metadata.content_length(), payload.len() as u64);

        let (reader, size) = open_download_reader(&operator, "blob.bin").await.unwrap();
        assert_eq!(size, payload.len() as u64);
        let mut assembled = Vec::new();
        let mut offset = 0u64;
        while offset < size {
            let end = (offset + TRANSFER_CHUNK_SIZE as u64).min(size);
            let chunk = read_chunk(&reader, offset, end).await.unwrap();
            if chunk.is_empty() {
                break;
            }
            offset += chunk.len() as u64;
            assembled.extend_from_slice(&chunk);
        }
        assert_eq!(assembled, payload);
    }

    #[tokio::test]
    async fn download_reader_rejects_directories() {
        let operator = memory_operator();
        operator.create_dir("some-dir/").await.unwrap();
        assert!(open_download_reader(&operator, "some-dir").await.is_err());
    }
}
