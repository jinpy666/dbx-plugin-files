//! `opendal::raw::Access` implementation on top of [`SftpNativePool`]
//! (dual-stack decision 2026-08-31; mirrors `engine::smb::access`).
//!
//! Root-mapping red line (learned from the SMB real-machine regression):
//! EVERY operation — deleter included — must translate OpenDAL-relative
//! paths through the configured root before touching the wire. Unlike the
//! SMB adapter (which works share-relative), SFTP paths are absolute; a
//! path that skips the root mapping would silently operate outside the
//! confined subtree on the server's root (`STATUS`-style no-op deletes,
//! wrong-target deletes).

use std::sync::Arc;

use opendal::raw::*;
use opendal::{Buffer, Capability, EntryMode, Error, ErrorKind, Metadata, Result};

use super::pool::{SftpNativePool, SftpOp, SftpOpResult};
use super::SFTP_NATIVE_SCHEME;

/// Sequential chunk served per `oio::Read::read` call. SFTP reads are
/// request/response round trips, so this bounds per-flight size (russh-sftp
/// does not pipeline reads).
const SFTP_READ_CHUNK: u64 = 1024 * 1024;

/// Maps a tokio io error from an SFTP file handle onto the OpenDAL taxonomy.
/// The crate flattens its own error type into `std::io::Error` at the
/// AsyncRead/AsyncWrite boundary, and its Display carries status text only —
/// never credentials.
fn map_io_error(error: std::io::Error) -> Error {
    Error::new(ErrorKind::Unexpected, format!("sftp-native: {error}"))
}

/// The native SFTP adapter: one pool (lazy single session) + capability
/// declaration. Constructed by `SftpNativeBuilder::build` via
/// `Operator::new(builder)` (static dispatch — "sftp-native" is not an
/// OpenDAL scheme).
pub(super) struct SftpNativeAccess {
    pool: Arc<SftpNativePool>,
    /// Normalized OpenDAL root: an optional sub-path of the remote
    /// filesystem (`normalize_root` format, e.g. `/pub/`).
    root: String,
    info: Arc<AccessorInfo>,
}

impl std::fmt::Debug for SftpNativeAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpNativeAccess")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

/// Converts an OpenDAL-relative path into an absolute SFTP path.
/// `build_abs_path` joins root+path WITHOUT a leading slash (its "abs" is
/// backend-namespace relative), while SFTP resolves relative paths against
/// the server-side session cwd — not the configured root. So the leading
/// `/` is re-added here, making every wire path absolute inside the root.
pub(super) fn sftp_path(root: &str, path: &str) -> String {
    let joined = build_abs_path(root, path).trim_matches('/').to_string();
    if joined.is_empty() {
        "/".to_string()
    } else {
        format!("/{joined}")
    }
}

impl SftpNativeAccess {
    pub(super) fn new(pool: Arc<SftpNativePool>, root: String, name: &str) -> Self {
        let info = AccessorInfo::default();
        info.set_scheme(SFTP_NATIVE_SCHEME);
        info.set_name(name);
        info.set_root(&root);
        // rename=true (SSH_FXP_RENAME is native) → files/move runs server-side;
        // copy=false → same-connection copies degrade to the engine's read→write
        // job; presign=false → files/publicLink reports the backend as
        // unsupported (both existing engine paths).
        info.set_native_capability(Capability {
            stat: true,
            read: true,
            write: true,
            write_can_empty: true,
            write_can_multi: true,
            create_dir: true,
            delete: true,
            delete_with_recursive: true,
            list: true,
            list_with_recursive: true,
            rename: true,
            shared: true,
            ..Default::default()
        });
        Self {
            pool,
            root,
            info: Arc::new(info),
        }
    }
}

impl Access for SftpNativeAccess {
    type Reader = SftpReader;
    type Writer = SftpWriter;
    type Lister = SftpLister;
    type Deleter = oio::OneShotDeleter<SftpDeleter>;
    type Copier = ();

    fn info(&self) -> Arc<AccessorInfo> {
        self.info.clone()
    }

    /// mkdir -p: create every level; existing levels are detected via stat
    /// (SFTP servers answer SSH_FX_FAILURE — not a dedicated "exists" code —
    /// for mkdir on an existing directory, so stat-first is the portable
    /// shape). The per-level prefixes keep the LEADING SLASH: a relative
    /// prefix would resolve against the server-side session cwd and create
    /// the tree somewhere else entirely.
    async fn create_dir(&self, path: &str, _: OpCreateDir) -> Result<RpCreateDir> {
        let target = sftp_path(&self.root, path);
        let mut prefix = String::new();
        for segment in target.split('/').filter(|segment| !segment.is_empty()) {
            prefix = format!("{prefix}/{segment}");
            match self
                .pool
                .call(SftpOp::Stat { path: &prefix })
                .await
            {
                Ok(SftpOpResult::Stat(attributes)) if attributes.is_dir() => continue,
                Ok(SftpOpResult::Stat(_)) => {
                    return Err(Error::new(
                        ErrorKind::AlreadyExists,
                        format!("sftp-native: '{prefix}' already exists and is not a directory"),
                    ))
                }
                Ok(_) => return Err(unexpected_result("stat")),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            match self
                .pool
                .call(SftpOp::CreateDir { path: &prefix })
                .await
            {
                Ok(_) => {}
                // Racing creators: an existing level is as good as created.
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Ok(RpCreateDir::default())
    }

    async fn stat(&self, path: &str, _: OpStat) -> Result<RpStat> {
        let target = sftp_path(&self.root, path);
        // A real network stat for the root too (unlike the SMB adapter's
        // local DIR answer) so `connection/test` actually probes the server
        // instead of passing vacuously — the false-positive lesson from the
        // SMB real-machine run.
        let info = match self.pool.call(SftpOp::Stat { path: &target }).await? {
            SftpOpResult::Stat(info) => info,
            _ => return Err(unexpected_result("stat")),
        };
        let mut metadata = Metadata::new(if info.is_dir() {
            EntryMode::DIR
        } else {
            EntryMode::FILE
        });
        if let Some(size) = info.size {
            metadata.set_content_length(size);
        }
        // `mtime` is Unix seconds; opendal's raw Timestamp wraps jiff and
        // converts from SystemTime.
        if let Some(mtime) = info.mtime {
            if let Ok(timestamp) = Timestamp::try_from(std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::from(mtime))) {
                metadata.set_last_modified(timestamp);
            }
        }
        Ok(RpStat::new(metadata))
    }

    /// Opens a positioned sequential reader; the OpRead range decides the
    /// served window (`oio::Read` drains it sequentially).
    async fn read(&self, path: &str, args: OpRead) -> Result<(RpRead, Self::Reader)> {
        let target = sftp_path(&self.root, path);
        let reader = self.pool.open_reader(&target, args.range().offset()).await?;
        Ok((
            RpRead::default(),
            SftpReader {
                reader: Some(reader),
                remaining: args.range().size(),
            },
        ))
    }

    /// Opens a truncating streaming writer (overwrites by truncation).
    async fn write(&self, path: &str, _: OpWrite) -> Result<(RpWrite, Self::Writer)> {
        let target = sftp_path(&self.root, path);
        let writer = self.pool.open_writer(&target).await?;
        Ok((RpWrite::default(), SftpWriter { writer: Some(writer), written: 0 }))
    }

    async fn delete(&self) -> Result<(RpDelete, Self::Deleter)> {
        Ok((
            RpDelete::default(),
            oio::OneShotDeleter::new(SftpDeleter {
                pool: self.pool.clone(),
                root: self.root.clone(),
            }),
        ))
    }

    /// QUERY_DIRECTORY-equivalent buffered enumeration; recursion is
    /// flattened by the lister itself (dir stack) when `args.recursive()` is
    /// set.
    async fn list(&self, path: &str, args: OpList) -> Result<(RpList, Self::Lister)> {
        let target = sftp_path(&self.root, path);
        Ok((
            RpList::default(),
            SftpLister::new(self.pool.clone(), self.root.clone(), target, args.recursive()),
        ))
    }

    /// SSH_FXP_RENAME; works for files and directories. SFTP rename does not
    /// overwrite: an existing target surfaces as a protocol error (OpenSSH
    /// answers SSH_FX_FAILURE), matching the engine's no-clobber behavior for
    /// `files/rename` on other no-overwrite backends.
    async fn rename(&self, from: &str, to: &str, _: OpRename) -> Result<RpRename> {
        let (source, target) = (sftp_path(&self.root, from), sftp_path(&self.root, to));
        match self
            .pool
            .call(SftpOp::Rename {
                from: &source,
                to: &target,
            })
            .await?
        {
            SftpOpResult::Unit => Ok(RpRename::default()),
            _ => Err(unexpected_result("rename")),
        }
    }
}

/// Guard against a pool dispatch result not matching the requested op.
/// Unreachable by construction; kept so exhaustive matches stay cheap.
fn unexpected_result(op: &str) -> Error {
    Error::new(
        ErrorKind::Unexpected,
        format!("sftp-native: unexpected result from {op}"),
    )
}

/// Sequential reader over an SFTP file handle (tokio AsyncRead). EOF closes
/// the handle eagerly so the server-side handle does not linger until the
/// session drops.
pub(super) struct SftpReader {
    /// `None` once EOF / the requested range was drained.
    reader: Option<russh_sftp::client::fs::File>,
    /// Bytes left in the requested OpRead range; `None` = read to EOF.
    remaining: Option<u64>,
}

impl oio::Read for SftpReader {
    async fn read(&mut self) -> Result<Buffer> {
        use tokio::io::AsyncReadExt as _;
        let Some(reader) = self.reader.as_mut() else {
            return Ok(Buffer::new());
        };
        let budget = self.remaining.unwrap_or(SFTP_READ_CHUNK).min(SFTP_READ_CHUNK);
        if budget == 0 {
            self.take_reader_close().await;
            return Ok(Buffer::new());
        }
        let mut chunk = vec![0u8; budget as usize];
        let read = reader.read(&mut chunk).await.map_err(map_io_error)?;
        if read == 0 {
            self.take_reader_close().await;
            return Ok(Buffer::new());
        }
        chunk.truncate(read);
        if let Some(remaining) = self.remaining.as_mut() {
            *remaining -= read as u64;
            // Draining an exact range: close as soon as nothing is left.
            if *remaining == 0 {
                self.take_reader_close().await;
            }
        }
        Ok(Buffer::from(chunk))
    }
}

impl SftpReader {
    /// Takes the handle out and shuts it down (flush + close on the wire).
    /// Best-effort: a server that already reaped the handle must not fail
    /// the read stream at EOF.
    async fn take_reader_close(&mut self) {
        if let Some(mut reader) = self.reader.take() {
            use tokio::io::AsyncWriteExt;
            let _ = reader.shutdown().await;
        }
    }
}

/// Pipelined chunk writer over an SFTP file handle (tokio AsyncWrite).
pub(super) struct SftpWriter {
    writer: Option<russh_sftp::client::fs::File>,
    written: u64,
}

// Sound: every `oio::Write` access to `SftpWriter` goes through `&mut self`,
// same pattern as the upstream ftp service's writer.
unsafe impl Sync for SftpWriter {}

impl oio::Write for SftpWriter {
    async fn write(&mut self, bs: Buffer) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| Error::new(ErrorKind::Unexpected, "sftp-native writer already closed"))?;
        writer
            .write_all(bs.to_vec().as_slice())
            .await
            .map_err(map_io_error)?;
        self.written += bs.len() as u64;
        Ok(())
    }

    async fn close(&mut self) -> Result<Metadata> {
        use tokio::io::AsyncWriteExt;
        let mut writer = self
            .writer
            .take()
            .ok_or_else(|| Error::new(ErrorKind::Unexpected, "sftp-native writer already closed"))?;
        writer.shutdown().await.map_err(map_io_error)?;
        let mut metadata = Metadata::new(EntryMode::FILE);
        metadata.set_content_length(self.written);
        Ok(metadata)
    }

    async fn abort(&mut self) -> Result<()> {
        if let Some(mut writer) = self.writer.take() {
            // No truncate-back primitive on SFTP: drop the handle with the
            // partial content in place (OpenDAL abort semantics allow the
            // target to hold garbage after an aborted write).
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
        }
        Ok(())
    }
}

/// Buffered streaming lister; recursive listing walks a dir stack through
/// the pool (directory ops serialize).
pub(super) struct SftpLister {
    pool: Arc<SftpNativePool>,
    root: String,
    /// Absolute directory to expand on the next `next()` call.
    pending: Option<String>,
    /// Entries already fetched from the pending directory.
    queue: std::vec::IntoIter<oio::Entry>,
    /// Directories left to expand when `recursive` is set.
    stack: Vec<String>,
    recursive: bool,
}

impl SftpLister {
    fn new(pool: Arc<SftpNativePool>, root: String, path: String, recursive: bool) -> Self {
        Self {
            pool,
            root,
            pending: Some(path),
            queue: Vec::new().into_iter(),
            stack: Vec::new(),
            recursive,
        }
    }

    /// Fetches one directory level, converts entries to OpenDAL paths
    /// relative to the adapter root, and queues subdirectories for expansion.
    async fn load(&mut self, dir: &str) -> Result<()> {
        let entries = match self.pool.call(SftpOp::List { path: dir }).await? {
            SftpOpResult::List(entries) => entries,
            _ => return Err(unexpected_result("list")),
        };
        let mut queue = Vec::with_capacity(entries.len());
        for (name, is_dir, size, mtime) in entries {
            // SFTP servers may include the dot entries.
            if name == "." || name == ".." {
                continue;
            }
            let child = if dir == "/" {
                format!("/{name}")
            } else {
                format!("{dir}/{name}")
            };
            // Absolute → OpenDAL-relative path (build_rel_path strips the root
            // prefix); directories keep their trailing `/`.
            let mut rel = build_rel_path(&self.root, &child);
            if rel.is_empty() {
                rel = "/".to_string();
            } else if is_dir {
                rel.push('/');
            }
            if is_dir && self.recursive {
                self.stack.push(child);
            }
            let mut metadata = Metadata::new(if is_dir {
                EntryMode::DIR
            } else {
                EntryMode::FILE
            });
            if let Some(size) = size {
                metadata.set_content_length(size);
            }
            if let Some(mtime) = mtime {
                if let Ok(timestamp) = Timestamp::try_from(
                    std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::from(mtime)),
                ) {
                    metadata.set_last_modified(timestamp);
                }
            }
            queue.push(oio::Entry::new(&rel, metadata));
        }
        self.queue = queue.into_iter();
        Ok(())
    }
}

impl oio::List for SftpLister {
    async fn next(&mut self) -> Result<Option<oio::Entry>> {
        loop {
            if let Some(entry) = self.queue.next() {
                return Ok(Some(entry));
            }
            if let Some(dir) = self.pending.take() {
                self.load(&dir).await?;
                continue;
            }
            match self.stack.pop() {
                Some(dir) => self.pending = Some(dir),
                None => return Ok(None),
            }
        }
    }
}

/// One-shot deleter; recursive deletes walk the subtree bottom-up.
///
/// THE root-mapping red line (SMB real-machine lesson, SmbDeleter regression):
/// both the single and recursive paths translate OpenDAL-relative paths
/// through [`sftp_path`] BEFORE any wire call. A deleter that skips the
/// mapping operates on the server root instead of the confined subtree —
/// silent no-op deletes at best, wrong-target deletes at worst.
pub(super) struct SftpDeleter {
    pool: Arc<SftpNativePool>,
    root: String,
}

impl SftpDeleter {
    /// Deletes one object; falls back to `remove_dir` for directory paths,
    /// and maps NotFound to Ok (delete is idempotent per the Access
    /// contract).
    ///
    /// `is_dir_hint` comes from the OpenDAL path shape (directories carry a
    /// trailing `/`); the engine's `files/rmdir` empties the directory first,
    /// so a hinted remove_dir on a non-empty directory surfaces the server's
    /// failure honestly instead of guessing.
    async fn delete_single(&self, path: &str, is_dir_hint: bool) -> Result<()> {
        if !is_dir_hint {
            match self.pool.call(SftpOp::RemoveFile { path }).await {
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        match self.pool.call(SftpOp::RemoveDir { path }).await {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Iterative bottom-up subtree purge (explicit stack, no async recursion):
    /// files are deleted immediately, directories only after their children.
    async fn delete_recursive(&self, root: &str) -> Result<()> {
        // (path, is_dir, children already deleted)
        let mut stack: Vec<(String, bool, bool)> = vec![(root.to_string(), true, false)];
        while let Some((path, is_dir, expanded)) = stack.pop() {
            if !is_dir {
                self.delete_single(&path, false).await?;
                continue;
            }
            if expanded {
                match self.pool.call(SftpOp::RemoveDir { path: &path }).await {
                    Ok(_) => {}
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                continue;
            }
            // Re-queue this directory for deletion after its children, then
            // queue the children (LIFO order deletes children first).
            stack.push((path.clone(), true, true));
            let entries = match self.pool.call(SftpOp::List { path: &path }).await? {
                SftpOpResult::List(entries) => entries,
                _ => return Err(unexpected_result("list")),
            };
            for (name, is_dir, _, _) in entries {
                if name == "." || name == ".." {
                    continue;
                }
                let child = if path == "/" {
                    format!("/{name}")
                } else {
                    format!("{path}/{name}")
                };
                stack.push((child, is_dir, false));
            }
        }
        Ok(())
    }
}

impl oio::OneShotDelete for SftpDeleter {
    async fn delete_once(&self, path: String, args: OpDelete) -> Result<()> {
        // OpenDAL directory paths carry a trailing `/`; normalize before any
        // SFTP call and keep the dir-ness as an explicit hint for the
        // non-recursive delete. The root mapping happens HERE so both the
        // single and recursive paths operate inside the configured root.
        let is_dir = path.ends_with('/');
        let target = sftp_path(&self.root, &path);
        if args.recursive() {
            self.delete_recursive(&target).await
        } else {
            self.delete_single(&target, is_dir).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sftp_paths_map_through_root_absolutely() {
        // The whole point of the red line: relative OpenDAL paths map onto
        // ABSOLUTE SFTP paths inside the configured root.
        let root = normalize_root("/pub");
        assert_eq!(sftp_path(&root, ""), "/pub", "root itself");
        assert_eq!(sftp_path(&root, "/"), "/pub", "root listing");
        assert_eq!(sftp_path(&root, "a.txt"), "/pub/a.txt");
        assert_eq!(sftp_path(&root, "dir/b.txt"), "/pub/dir/b.txt");
        assert_eq!(sftp_path(&root, "dir/"), "/pub/dir", "dir hint is wire-irrelevant");
        // Share-root connections map onto the server root.
        let root_root = normalize_root("/");
        assert_eq!(sftp_path(&root_root, ""), "/");
        assert_eq!(sftp_path(&root_root, "a.txt"), "/a.txt");
    }

    #[test]
    fn sftp_deleter_type_carries_root() {
        // Compile-time guard of the red line: the deleter holds the root so
        // delete_once can map paths (the SMB adapter's bug was a rootless
        // deleter). Runtime behavior is exercised by the smoke's root-set
        // sections and the real-machine run.
        let pool = SftpNativePool::new(super::super::pool::SftpNativeConnectParams {
            host: "mft.local".into(),
            port: 22,
            user: "bob".into(),
            credentials: super::super::pool::SftpNativeAuth::Password("secret".into()),
            strategy: super::super::HostKeyStrategy::Trust,
            known_hosts_path: std::path::PathBuf::from("/dev/null"),
        });
        let deleter = SftpDeleter {
            pool,
            root: "/pub".to_string(),
        };
        assert_eq!(deleter.root, "/pub");
    }
}
