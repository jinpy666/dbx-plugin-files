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
    /// `Some` keeps the historical direct-share behavior. `None` exposes a
    /// server namespace: `/` lists shares and the first path segment selects
    /// one for subsequent file operations.
    share: Option<String>,
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

/// Maps an OpenDAL path onto a share-relative SMB wire path (no leading or
/// trailing `/`; empty string = the configured root inside the share).
/// Module-level so the deleter shares the exact mapping used by stat/read/
/// write/create_dir/list/rename. RED LINE: every wire-facing path must go
/// through this helper — a rootless op would hit same-named paths at the
/// SHARE root (silent no-op deletes at best, wrong-target deletes at worst;
/// real-machine regression recorded in docs/PROGRESS-F5-SMB.zh-CN.md §5).
pub(super) fn smb_path(root: &str, path: &str) -> String {
    build_abs_path(root, path).trim_matches('/').to_string()
}

impl SmbAccess {
    pub(super) fn new(pool: Arc<SmbPool>, root: String, share: Option<&str>) -> Self {
        let info = AccessorInfo::default();
        info.set_scheme(SMB_SCHEME);
        info.set_name(share.unwrap_or("SMB server"));
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
            share: share.map(str::to_string),
            root,
            info: Arc::new(info),
        }
    }

    /// Converts an OpenDAL path into a share-relative SMB path (no leading or
    /// trailing `/`; empty string = the configured root inside the share).
    /// OpenDAL-relative input never starts with `/` and keeps the trailing
    /// `/` for directory paths. Delegates to the module-level [`smb_path`].
    fn smb_path(&self, path: &str) -> String {
        smb_path(&self.root, path)
    }

    /// Resolves an OpenDAL path into `(share, share-relative wire path)`.
    /// Direct-share operators use the configured share; server-level
    /// operators use the first path component as the selected share.
    fn resolve_path(&self, path: &str) -> Result<Option<(String, String)>> {
        if let Some(share) = &self.share {
            return Ok(Some((share.clone(), self.smb_path(path))));
        }
        let clean = path.trim_matches('/');
        if clean.is_empty() {
            return Ok(None);
        }
        let (share, rest) = clean.split_once('/').unwrap_or((clean, ""));
        if share.is_empty() {
            return Err(Error::new(
                ErrorKind::ConfigInvalid,
                "smb share path is empty",
            ));
        }
        Ok(Some((share.to_string(), smb_path("/", rest))))
    }

    fn require_path(&self, path: &str, operation: &str) -> Result<(String, String)> {
        self.resolve_path(path)?.ok_or_else(|| {
            Error::new(
                ErrorKind::ConfigInvalid,
                format!("smb {operation} requires a selected share"),
            )
        })
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
        let (share, target) = self.require_path(path, "create_dir")?;
        if target.is_empty() {
            return Ok(RpCreateDir::default());
        }
        let mut prefix = String::new();
        for segment in target.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            match self
                .pool
                .call(SmbOp::CreateDir {
                    share: &share,
                    path: &prefix,
                })
                .await
            {
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Ok(RpCreateDir::default())
    }

    async fn stat(&self, path: &str, _: OpStat) -> Result<RpStat> {
        let metadata = match self.resolve_path(path)? {
            None => Metadata::new(EntryMode::DIR),
            Some((share, target)) if target.is_empty() => {
                // A root stat cannot be sent as an SMB CREATE. Connecting the
                // selected share still validates it before returning the virtual
                // directory metadata.
                self.pool
                    .call(SmbOp::ConnectShare { share: &share })
                    .await?;
                Metadata::new(EntryMode::DIR)
            }
            Some((share, target)) => {
                let info = match self
                    .pool
                    .call(SmbOp::Stat {
                        share: &share,
                        path: &target,
                    })
                    .await?
                {
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
            }
        };
        Ok(RpStat::new(metadata))
    }

    /// Opens a positioned streaming reader; the OpRead range decides the
    /// served window (`oio::Read` drains it sequentially).
    async fn read(&self, path: &str, args: OpRead) -> Result<(RpRead, Self::Reader)> {
        let (share, target) = self.require_path(path, "read")?;
        let reader = self.pool.open_reader_on_share(&share, &target).await?;
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
        let (share, target) = self.require_path(path, "write")?;
        let writer = self.pool.open_writer_on_share(&share, &target).await?;
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
                root: self.root.clone(),
                share: self.share.clone(),
            }),
        ))
    }

    /// QUERY_DIRECTORY streaming enumeration; recursion is flattened by the
    /// lister itself (dir stack) when `args.recursive()` is set.
    async fn list(&self, path: &str, args: OpList) -> Result<(RpList, Self::Lister)> {
        if self.share.is_none() && path.trim_matches('/').is_empty() {
            return Ok((RpList::default(), SmbLister::new_shares(self.pool.clone())));
        }
        let (share, target) = self.require_path(path, "list")?;
        Ok((
            RpList::default(),
            SmbLister::new(
                self.pool.clone(),
                self.root.clone(),
                share,
                target,
                self.share.is_none(),
                args.recursive(),
            ),
        ))
    }

    /// SET_INFORMATION FileRenameInfo; works for files and directories.
    async fn rename(&self, from: &str, to: &str, _: OpRename) -> Result<RpRename> {
        let (source_share, source) = self.require_path(from, "rename")?;
        let (target_share, target) = self.require_path(to, "rename")?;
        if source_share != target_share {
            return Err(Error::new(
                ErrorKind::ConfigInvalid,
                "smb rename cannot cross shares",
            ));
        }
        match self
            .pool
            .call(SmbOp::Rename {
                share: &source_share,
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
    share: Option<String>,
    /// `true` for the server root, where the lister returns exported shares
    /// rather than filesystem entries.
    server_root: bool,
    /// Directory to expand on the next `next()` call (share-relative, no
    /// leading/trailing `/`; empty = share root).
    pending: Option<String>,
    /// Entries already fetched from the pending directory.
    queue: std::vec::IntoIter<oio::Entry>,
    /// Directories left to expand when `recursive` is set.
    stack: Vec<String>,
    recursive: bool,
    /// Prefix the selected share back onto entries for server-level
    /// namespace browsing. Direct-share operators keep the legacy relative
    /// paths and leave this empty.
    namespace_share: Option<String>,
}

impl SmbLister {
    fn new(
        pool: Arc<SmbPool>,
        root: String,
        share: String,
        path: String,
        namespace_share: bool,
        recursive: bool,
    ) -> Self {
        Self {
            pool,
            root,
            share: Some(share.clone()),
            server_root: false,
            namespace_share: namespace_share.then(|| share.clone()),
            pending: Some(path),
            queue: Vec::new().into_iter(),
            stack: Vec::new(),
            recursive,
        }
    }

    fn new_shares(pool: Arc<SmbPool>) -> Self {
        Self {
            pool,
            root: "/".to_string(),
            share: None,
            server_root: true,
            pending: Some(String::new()),
            queue: Vec::new().into_iter(),
            stack: Vec::new(),
            recursive: false,
            namespace_share: None,
        }
    }

    /// Fetches one directory level, converts entries to OpenDAL paths relative
    /// to the adapter root, and queues subdirectories for expansion.
    async fn load(&mut self, dir: &str) -> Result<()> {
        if self.server_root {
            let shares = match self.pool.call(SmbOp::ListShares).await? {
                SmbOpResult::Shares(shares) => shares,
                _ => return Err(unexpected_result("list shares")),
            };
            let mut queue = Vec::with_capacity(shares.len());
            for share in shares {
                if share.name.is_empty() || share.name == "." || share.name == ".." {
                    continue;
                }
                queue.push(oio::Entry::new(
                    &format!("/{}/", share.name),
                    Metadata::new(EntryMode::DIR),
                ));
            }
            self.queue = queue.into_iter();
            return Ok(());
        }
        let share = self.share.as_deref().expect("directory lister has share");
        let entries = match self.pool.call(SmbOp::List { share, path: dir }).await? {
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
            let share_absolute = match &self.namespace_share {
                Some(share) => format!("/{share}/{child}"),
                None => format!("/{child}"),
            };
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
///
/// Holds the configured root: every `delete_once` wire path is mapped through
/// [`smb_path`]. RED LINE (real-machine regression): a rootless deleter sent
/// OpenDAL-relative paths straight to the wire, so on root-configured
/// connections `delete`/`rmdir` reported success while deleting nothing
/// (the mis-aimed NotFound was swallowed by the idempotent-delete mapping),
/// `purge` failed with `STATUS_OBJECT_NAME_NOT_FOUND`, rename-dir degrade
/// jobs left the source behind — and a same-named path at the share root
/// would have been deleted instead.
pub(super) struct SmbDeleter {
    pool: Arc<SmbPool>,
    root: String,
    share: Option<String>,
}

impl SmbDeleter {
    fn resolve_path(&self, path: &str) -> Result<(String, String)> {
        if let Some(share) = &self.share {
            return Ok((share.clone(), smb_path(&self.root, path)));
        }
        let clean = path.trim_matches('/');
        let (share, rest) = clean.split_once('/').unwrap_or((clean, ""));
        if share.is_empty() {
            return Err(Error::new(
                ErrorKind::ConfigInvalid,
                "smb delete requires a selected share",
            ));
        }
        Ok((share.to_string(), smb_path("/", rest)))
    }

    /// Deletes one object; falls back to `delete_directory` for directory
    /// paths, and maps NotFound to Ok (delete is idempotent per the Access
    /// contract). `path` must already be the mapped share-relative wire path
    /// (see [`smb_path`]).
    ///
    /// `is_dir_hint` comes from the OpenDAL path shape (directories carry a
    /// trailing `/`): SMB Create rejects names with a trailing slash
    /// (`STATUS_OBJECT_NAME_INVALID`, which maps to `InvalidName`, not
    /// `IsADirectory`), so a hinted path must go straight to the directory
    /// op instead of relying on the file-op fallback.
    async fn delete_single(&self, share: &str, path: &str, is_dir_hint: bool) -> Result<()> {
        if !is_dir_hint {
            match self.pool.call(SmbOp::DeleteFile { share, path }).await {
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == ErrorKind::IsADirectory => {}
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        match self.pool.call(SmbOp::DeleteDirectory { share, path }).await {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Iterative bottom-up subtree purge (explicit stack, no async recursion):
    /// files are deleted immediately, directories only after their children.
    async fn delete_recursive(&self, share: &str, root: &str) -> Result<()> {
        // (path, is_dir, children already deleted)
        let mut stack: Vec<(String, bool, bool)> = vec![(root.to_string(), true, false)];
        while let Some((path, is_dir, expanded)) = stack.pop() {
            if !is_dir {
                self.delete_single(share, &path, false).await?;
                continue;
            }
            if expanded {
                match self
                    .pool
                    .call(SmbOp::DeleteDirectory { share, path: &path })
                    .await
                {
                    Ok(_) => {}
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                continue;
            }
            // Re-queue this directory for deletion after its children, then
            // queue the children (LIFO order deletes children first).
            stack.push((path.clone(), true, true));
            let entries = match self.pool.call(SmbOp::List { share, path: &path }).await? {
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
        // OpenDAL directory paths carry a trailing `/`; keep the dir-ness as
        // an explicit hint — smb_path strips the slash itself, and the
        // non-recursive delete needs the hint to go straight to the
        // directory op (SMB Create rejects trailing-slash names). The root
        // mapping happens HERE so both the single and recursive paths
        // operate inside the configured root.
        let is_dir = path.ends_with('/');
        let (share, target) = self.resolve_path(&path)?;
        if args.recursive() {
            self.delete_recursive(&share, &target).await
        } else {
            self.delete_single(&share, &target, is_dir).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smb_paths_map_through_root_absolutely() {
        // The whole point of the red line: relative OpenDAL paths map onto
        // share-relative SMB paths inside the configured root (deleter
        // included — see smb_deleter_type_carries_root).
        let root = normalize_root("/archive");
        assert_eq!(smb_path(&root, ""), "archive", "root itself");
        assert_eq!(smb_path(&root, "docs/a.txt"), "archive/docs/a.txt");
        assert_eq!(
            smb_path(&root, "docs/"),
            "archive/docs",
            "dir hint is wire-irrelevant"
        );
        // Share-root connections map onto the share root unchanged.
        let share_root = normalize_root("/");
        assert_eq!(smb_path(&share_root, "a.txt"), "a.txt");
    }

    #[test]
    fn server_level_paths_select_share_from_first_component() {
        let pool = SmbPool::new(super::super::pool::SmbConnectParams {
            host: "nas.local".into(),
            port: 445,
            share: None,
            username: String::new(),
            password: String::new(),
            domain: String::new(),
        });
        let access = SmbAccess::new(pool, "/".to_string(), None);
        assert_eq!(access.resolve_path("/").unwrap(), None);
        assert_eq!(
            access.resolve_path("Projects/docs/a.txt").unwrap(),
            Some(("Projects".to_string(), "docs/a.txt".to_string()))
        );
        assert_eq!(
            access.resolve_path("Projects/").unwrap(),
            Some(("Projects".to_string(), String::new()))
        );
    }

    #[test]
    fn smb_deleter_type_carries_root() {
        // Compile-time guard of the red line: the deleter holds the root so
        // delete_once can map paths (the real-machine bug was a rootless
        // deleter: silent no-op deletes + wrong-target deletes on
        // root-configured connections). Runtime behavior is exercised by
        // the smoke's root-set sections and the real-machine run.
        let pool = SmbPool::new(super::super::pool::SmbConnectParams {
            host: "nas.local".into(),
            port: 445,
            share: Some("Projects".into()),
            username: "bob".into(),
            password: String::new(),
            domain: String::new(),
        });
        let deleter = SmbDeleter {
            pool,
            root: "/archive".to_string(),
            share: Some("Projects".to_string()),
        };
        assert_eq!(deleter.root, "/archive");
    }
}
