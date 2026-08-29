//! `opendal::raw::Access` implementation on top of [`SmbPool`]
//! (IMPL_PLAN_SMB §2.2 operation mapping).

use std::sync::Arc;

use opendal::raw::*;
use opendal::{Buffer, Capability, EntryMode, Error, ErrorKind, Metadata, Result};

use super::pool::{map_smb_error, SmbOp, SmbOpResult, SmbPool};
use super::SMB_SCHEME;

/// Sequential chunk served per `oio::Read::read` call. The smb2 reader splits
/// requests larger than the negotiated `MaxReadSize` on its own, so this only
/// bounds our buffering, not the wire requests.
const SMB_READ_CHUNK: u64 = 1024 * 1024;

/// The SMB adapter: one pool (lazy single client) + capability declaration.
/// Constructed by `SmbBuilder::build` via `Operator::new(builder)` (static
/// dispatch — "smb" is not a registered OpenDAL scheme).
pub(super) struct SmbAccess {
    pool: Arc<SmbPool>,
    /// Normalized OpenDAL root: an optional sub-path inside the share
    /// (`normalize_root` format, e.g. `/sub/dir/`).
    root: String,
    info: Arc<AccessorInfo>,
}

impl std::fmt::Debug for SmbAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmbAccess")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl SmbAccess {
    pub(super) fn new(pool: Arc<SmbPool>, root: String, share: &str) -> Self {
        let info = AccessorInfo::default();
        info.set_scheme(SMB_SCHEME);
        info.set_name(share);
        info.set_root(&root);
        // copy=false → same-connection copies degrade to the engine's
        // read→write job; presign=false → files/publicLink reports the
        // backend as unsupported (both existing engine paths, §2.2).
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

    /// Converts an OpenDAL path into a share-relative SMB path (no leading or
    /// trailing `/`; empty string = the configured root inside the share).
    /// OpenDAL-relative input never starts with `/` and keeps the trailing
    /// `/` for directory paths.
    fn smb_path(&self, path: &str) -> String {
        build_abs_path(&self.root, path)
            .trim_matches('/')
            .to_string()
    }
}

impl Access for SmbAccess {
    type Reader = SmbReader;
    type Writer = SmbWriter;
    type Lister = SmbLister;
    type Deleter = oio::OneShotDeleter<SmbDeleter>;
    type Copier = ();

    fn info(&self) -> Arc<AccessorInfo> {
        self.info.clone()
    }

    /// mkdir -p: create every level; existing directories are fine
    /// (`create_directory` collides map to Ok).
    async fn create_dir(&self, path: &str, _: OpCreateDir) -> Result<RpCreateDir> {
        let target = self.smb_path(path);
        if target.is_empty() {
            return Ok(RpCreateDir::default());
        }
        let mut prefix = String::new();
        for segment in target.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            match self.pool.call(SmbOp::CreateDir { path: &prefix }).await {
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Ok(RpCreateDir::default())
    }

    async fn stat(&self, path: &str, _: OpStat) -> Result<RpStat> {
        let target = self.smb_path(path);
        let metadata = if target.is_empty() {
            // The configured root inside the share always exists once the
            // tree connect succeeded.
            Metadata::new(EntryMode::DIR)
        } else {
            let info = match self.pool.call(SmbOp::Stat { path: &target }).await? {
                SmbOpResult::Stat(info) => info,
                _ => return Err(unexpected_result("stat")),
            };
            let mut metadata = Metadata::new(if info.is_directory {
                EntryMode::DIR
            } else {
                EntryMode::FILE
            });
            metadata.set_content_length(info.size);
            // `modified` is a Windows FILETIME (0 = unset); opendal's raw
            // Timestamp wraps jiff and converts from SystemTime.
            if info.modified.0 > 0 {
                if let Some(time) = info.modified.to_system_time() {
                    if let Ok(timestamp) = Timestamp::try_from(time) {
                        metadata.set_last_modified(timestamp);
                    }
                }
            }
            metadata
        };
        Ok(RpStat::new(metadata))
    }

    /// Opens a positioned streaming reader; the OpRead range decides the
    /// served window (`oio::Read` drains it sequentially).
    async fn read(&self, path: &str, args: OpRead) -> Result<(RpRead, Self::Reader)> {
        let target = self.smb_path(path);
        let reader = self.pool.open_reader(&target).await?;
        Ok((
            RpRead::default(),
            SmbReader {
                reader: Some(reader),
                pos: args.range().offset(),
                remaining: args.range().size(),
            },
        ))
    }

    /// Opens a pipelined streaming writer (truncating create).
    async fn write(&self, path: &str, _: OpWrite) -> Result<(RpWrite, Self::Writer)> {
        let target = self.smb_path(path);
        let writer = self.pool.open_writer(&target).await?;
        Ok((
            RpWrite::default(),
            SmbWriter {
                writer: Some(writer),
            },
        ))
    }

    async fn delete(&self) -> Result<(RpDelete, Self::Deleter)> {
        Ok((
            RpDelete::default(),
            oio::OneShotDeleter::new(SmbDeleter {
                pool: self.pool.clone(),
            }),
        ))
    }

    /// QUERY_DIRECTORY streaming enumeration; recursion is flattened by the
    /// lister itself (dir stack) when `args.recursive()` is set.
    async fn list(&self, path: &str, args: OpList) -> Result<(RpList, Self::Lister)> {
        let target = self.smb_path(path);
        Ok((
            RpList::default(),
            SmbLister::new(
                self.pool.clone(),
                self.root.clone(),
                target,
                args.recursive(),
            ),
        ))
    }

    /// SET_INFORMATION FileRenameInfo; works for files and directories.
    async fn rename(&self, from: &str, to: &str, _: OpRename) -> Result<RpRename> {
        let (source, target) = (self.smb_path(from), self.smb_path(to));
        match self
            .pool
            .call(SmbOp::Rename {
                from: &source,
                to: &target,
            })
            .await?
        {
            SmbOpResult::Unit => Ok(RpRename::default()),
            _ => Err(unexpected_result("rename")),
        }
    }
}

/// Guard against a pool dispatch result not matching the requested op.
/// Unreachable by construction; kept so exhaustive matches stay cheap.
fn unexpected_result(op: &str) -> Error {
    Error::new(
        ErrorKind::Unexpected,
        format!("smb: unexpected result from {op}"),
    )
}

/// Positioned sequential reader over an owned SMB file handle.
pub(super) struct SmbReader {
    /// `None` once the handle was closed (EOF / range exhausted). The smb2
    /// crate leaks the server-side handle when a `FileReader` is dropped
    /// without `close().await`, and `oio::Read` has no async close hook —
    /// so the close runs eagerly at EOF here. Early drops (error/cancel
    /// paths) still leak until session teardown, matching the crate's
    /// documented behavior.
    reader: Option<smb2::FileReader>,
    pos: u64,
    /// Bytes left in the requested OpRead range; `None` = read to EOF.
    remaining: Option<u64>,
}

impl oio::Read for SmbReader {
    async fn read(&mut self) -> Result<Buffer> {
        enum Outcome {
            Data(Vec<u8>),
            Eof,
        }
        let Some(reader) = self.reader.as_mut() else {
            return Ok(Buffer::new());
        };
        let outcome = match self.remaining {
            Some(0) => Outcome::Eof,
            Some(remaining) => {
                // read_at is pread-like and clamps at EOF; an empty return
                // means EOF.
                let data = reader
                    .read_at(self.pos, remaining.min(SMB_READ_CHUNK))
                    .await
                    .map_err(map_smb_error)?;
                if data.is_empty() {
                    Outcome::Eof
                } else {
                    self.pos += data.len() as u64;
                    self.remaining = Some(remaining - data.len() as u64);
                    Outcome::Data(data)
                }
            }
            None => {
                if self.pos >= reader.size() {
                    Outcome::Eof
                } else {
                    let data = reader
                        .read_at(self.pos, SMB_READ_CHUNK)
                        .await
                        .map_err(map_smb_error)?;
                    if data.is_empty() {
                        Outcome::Eof
                    } else {
                        self.pos += data.len() as u64;
                        Outcome::Data(data)
                    }
                }
            }
        };
        match outcome {
            Outcome::Data(data) => Ok(Buffer::from(data)),
            Outcome::Eof => {
                if let Some(reader) = self.reader.take() {
                    reader.close().await.map_err(map_smb_error)?;
                }
                Ok(Buffer::new())
            }
        }
    }
}

/// Pipelined chunk writer over an owned SMB file handle.
pub(super) struct SmbWriter {
    writer: Option<smb2::FileWriter>,
}

// Sound: every `oio::Write` access to `SmbWriter` goes through `&mut self`,
// so the interior `FuturesUnordered` (Send but not Sync) is never shared.
// Same pattern as the upstream ftp service's reader.
unsafe impl Sync for SmbWriter {}

impl oio::Write for SmbWriter {
    async fn write(&mut self, bs: Buffer) -> Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| Error::new(ErrorKind::Unexpected, "smb writer already closed"))?;
        writer
            .write_chunk(bs.to_vec().as_slice())
            .await
            .map_err(map_smb_error)
    }

    async fn close(&mut self) -> Result<Metadata> {
        let writer = self
            .writer
            .take()
            .ok_or_else(|| Error::new(ErrorKind::Unexpected, "smb writer already closed"))?;
        let written = writer.finish().await.map_err(map_smb_error)?;
        let mut metadata = Metadata::new(EntryMode::FILE);
        metadata.set_content_length(written);
        Ok(metadata)
    }

    async fn abort(&mut self) -> Result<()> {
        if let Some(writer) = self.writer.take() {
            writer.abort().await.map_err(map_smb_error)?;
        }
        Ok(())
    }
}

/// QUERY_DIRECTORY streaming lister; recursive listing walks a dir stack
/// through the pool (IMPL_PLAN_SMB §2.1: directory ops serialize).
pub(super) struct SmbLister {
    pool: Arc<SmbPool>,
    root: String,
    /// Directory to expand on the next `next()` call (share-relative, no
    /// leading/trailing `/`; empty = share root).
    pending: Option<String>,
    /// Entries already fetched from the pending directory.
    queue: std::vec::IntoIter<oio::Entry>,
    /// Directories left to expand when `recursive` is set.
    stack: Vec<String>,
    recursive: bool,
}

impl SmbLister {
    fn new(pool: Arc<SmbPool>, root: String, path: String, recursive: bool) -> Self {
        Self {
            pool,
            root,
            pending: Some(path),
            queue: Vec::new().into_iter(),
            stack: Vec::new(),
            recursive,
        }
    }

    /// Fetches one directory level, converts entries to OpenDAL paths relative
    /// to the adapter root, and queues subdirectories for expansion.
    async fn load(&mut self, dir: &str) -> Result<()> {
        let entries = match self.pool.call(SmbOp::List { path: dir }).await? {
            SmbOpResult::List(entries) => entries,
            _ => return Err(unexpected_result("list")),
        };
        let mut queue = Vec::with_capacity(entries.len());
        for entry in entries {
            let name = entry.name.as_str();
            // QUERY_DIRECTORY may include the dot entries depending on server.
            if name == "." || name == ".." {
                continue;
            }
            let child = if dir.is_empty() {
                name.to_string()
            } else {
                format!("{dir}/{name}")
            };
            // Share-absolute → OpenDAL-relative path (build_rel_path strips
            // the root prefix); directories keep their trailing `/`.
            let share_absolute = format!("/{child}");
            let mut rel = build_rel_path(&self.root, &share_absolute);
            if rel.is_empty() {
                rel = "/".to_string();
            } else if entry.is_directory {
                rel.push('/');
            }
            if entry.is_directory && self.recursive {
                self.stack.push(child);
            }
            let mode = if entry.is_directory {
                EntryMode::DIR
            } else {
                EntryMode::FILE
            };
            queue.push(oio::Entry::new(&rel, Metadata::new(mode)));
        }
        self.queue = queue.into_iter();
        Ok(())
    }
}

impl oio::List for SmbLister {
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
pub(super) struct SmbDeleter {
    pool: Arc<SmbPool>,
}

impl SmbDeleter {
    /// Deletes one object; falls back to `delete_directory` for directory
    /// paths, and maps NotFound to Ok (delete is idempotent per the Access
    /// contract).
    ///
    /// `is_dir_hint` comes from the OpenDAL path shape (directories carry a
    /// trailing `/`): SMB Create rejects names with a trailing slash
    /// (`STATUS_OBJECT_NAME_INVALID`, which maps to `InvalidName`, not
    /// `IsADirectory`), so a hinted path must go straight to the directory
    /// op instead of relying on the file-op fallback.
    async fn delete_single(&self, path: &str, is_dir_hint: bool) -> Result<()> {
        if !is_dir_hint {
            match self.pool.call(SmbOp::DeleteFile { path }).await {
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == ErrorKind::IsADirectory => {}
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        match self.pool.call(SmbOp::DeleteDirectory { path }).await {
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
                match self.pool.call(SmbOp::DeleteDirectory { path: &path }).await {
                    Ok(_) => {}
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                continue;
            }
            // Re-queue this directory for deletion after its children, then
            // queue the children (LIFO order deletes children first).
            stack.push((path.clone(), true, true));
            let entries = match self.pool.call(SmbOp::List { path: &path }).await? {
                SmbOpResult::List(entries) => entries,
                _ => return Err(unexpected_result("list")),
            };
            for entry in entries {
                let name = entry.name.as_str();
                if name == "." || name == ".." {
                    continue;
                }
                let child = if path.is_empty() {
                    name.to_string()
                } else {
                    format!("{path}/{name}")
                };
                stack.push((child, entry.is_directory, false));
            }
        }
        Ok(())
    }
}

impl oio::OneShotDelete for SmbDeleter {
    async fn delete_once(&self, path: String, args: OpDelete) -> Result<()> {
        // OpenDAL directory paths carry a trailing `/`; normalize before any
        // SMB call (trailing-slash names are rejected on the wire) and keep
        // the dir-ness as an explicit hint for the non-recursive delete.
        let is_dir = path.ends_with('/');
        let path = path.trim_end_matches('/');
        if args.recursive() {
            self.delete_recursive(path).await
        } else {
            self.delete_single(path, is_dir).await
        }
    }
}
