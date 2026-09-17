//! Bucket namespace: bucket-less object storage connections expose every
//! bucket as a pseudo-directory at the connection root (design 2026-09-17,
//! mirrors the SMB server-share namespace in `engine/smb`):
//!
//! ```
//! StoredConnection(protocol="s3", bucket="")
//!   → engine::build_operator → Operator::new(BucketNsBuilder)?
//!   → `/` lists buckets natively (list.rs); the first path segment selects
//!     the bucket and every other operation delegates to a per-bucket child
//!     operator built from the same Builder kv (bucket key injected, cached).
//! ```
//!
//! - bucket filled → untouched pass-through: `build_operator` never reaches
//!   this module (zero behavior change for existing connections).
//! - `gcs` stays bucket-required in phase 1: its ListBuckets needs a
//!   service-account JWT → OAuth2 token exchange, not a signed GET.
//!
//! Credential red line (same as smb/sftp-native): the base kv and the list
//! params live in process memory only — never persisted, logged, or
//! rendered into errors (list.rs redacts endpoints before echoing).

mod list;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opendal::raw::*;
use opendal::{
    Buffer, Builder, BytesRange, Capability, Error, ErrorKind, Metadata, MetadataBuilder,
    OperationContext, Operator, Result,
};

use crate::model::StoredConnection;

/// Sequential window served per read call (matches the transfer layer's
/// 4 MiB chunking; each window is one ranged child request).
const NS_READ_CHUNK: u64 = 4 * 1024 * 1024;

/// Quick protocols whose service needs a bucket but whose cloud API can
/// enumerate buckets natively. `gcs` is phase 2 (OAuth token exchange).
pub fn is_bucket_namespace_protocol(protocol: &str) -> bool {
    matches!(protocol, "s3" | "oss" | "cos" | "obs" | "azblob")
}

/// The Builder kv key each underlying service reads its bucket from
/// (`container` for Azure Blob, `bucket` everywhere else).
pub fn bucket_config_key(protocol: &str) -> &'static str {
    if protocol == "azblob" {
        "container"
    } else {
        "bucket"
    }
}

/// The service field a namespace-capable connection stores its bucket in.
fn bucket_value(connection: &StoredConnection) -> &str {
    if connection.protocol == "azblob" {
        &connection.container
    } else {
        &connection.bucket
    }
}

/// True when the connection exposes the bucket namespace: a namespace
/// protocol with its bucket left empty (bucket-optional form contract).
pub fn namespace_mode(connection: &StoredConnection) -> bool {
    is_bucket_namespace_protocol(&connection.protocol) && bucket_value(connection).trim().is_empty()
}

/// Native ListBuckets probe for `connection/test` — a real reachability +
/// permission check instead of `Operator::check()`'s root stat, which would
/// answer virtually in namespace mode (same false-positive lesson as the
/// SMB share listing, engine/mod.rs).
pub async fn probe(
    connection: &StoredConnection,
    timeout: Duration,
) -> std::result::Result<Vec<String>, String> {
    let params = list::ListParams::from_connection(connection);
    list::list_buckets(&params, timeout)
        .await
        .map_err(|error| error.to_message())
}

/// Builder configuration for the bucket namespace adapter (OpenDAL
/// `Configurator` shape; mirrors `smb::SmbConfig`).
#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(default)]
#[non_exhaustive]
pub struct BucketNsConfig {
    /// Underlying OpenDAL service scheme (`s3`/`oss`/`cos`/`obs`/`azblob`).
    scheme: Option<String>,
    /// Builder kv key the service reads its bucket/container from.
    bucket_key: Option<String>,
    /// Builder kv of the underlying service WITHOUT the bucket key (root,
    /// endpoint, credentials — already key-mapped by `protocol_kv`).
    base_kv: Vec<(String, String)>,
    /// Snapshot of the connection fields the native listing signs with.
    #[serde(skip)]
    list: list::ListParams,
    /// Timeout applied to native bucket listings (seconds).
    timeout_secs: Option<u64>,
}

impl opendal::Configurator for BucketNsConfig {
    type Builder = BucketNsBuilder;

    fn into_builder(self) -> Self::Builder {
        BucketNsBuilder { config: self }
    }
}

/// Builder for the bucket namespace adapter. Building is synchronous and
/// offline: bucket listing and child operators dial lazily per operation.
#[derive(Debug, Default)]
pub struct BucketNsBuilder {
    config: BucketNsConfig,
}

impl BucketNsBuilder {
    /// Creates an empty builder (all options unset).
    pub fn new() -> Self {
        Self::default()
    }

    /// Wraps a pre-built configuration (test helper shares the exact
    /// construction path with the fluent setters).
    pub fn from_config(config: BucketNsConfig) -> Self {
        Self { config }
    }

    /// Underlying OpenDAL service scheme.
    pub fn scheme(mut self, value: &str) -> Self {
        self.config.scheme = Some(value.to_string());
        self
    }

    /// Builder kv key the service reads its bucket/container from.
    pub fn bucket_key(mut self, value: &str) -> Self {
        self.config.bucket_key = Some(value.to_string());
        self
    }

    /// Service Builder kv without the bucket key.
    pub fn base_kv(mut self, kv: Vec<(String, String)>) -> Self {
        self.config.base_kv = kv;
        self
    }

    /// Snapshot of the connection the native listing signs with.
    pub fn list_connection(mut self, connection: &StoredConnection) -> Self {
        self.config.list = list::ListParams::from_connection(connection);
        self
    }

    /// Timeout for native bucket listings.
    pub fn timeout(mut self, value: Duration) -> Self {
        self.config.timeout_secs = Some(value.as_secs().max(1));
        self
    }
}

impl Builder for BucketNsBuilder {
    type Config = BucketNsConfig;

    fn build(self) -> Result<impl Service> {
        build_access(self.config)
    }
}

/// Shared construction path (Builder + tests): synchronous, offline.
fn build_access(config: BucketNsConfig) -> Result<BucketNsAccess> {
    let scheme = config.scheme.clone().ok_or_else(|| {
        Error::new(
            ErrorKind::ConfigInvalid,
            "bucket namespace requires the underlying service scheme",
        )
    })?;
    let bucket_key = config
        .bucket_key
        .clone()
        .unwrap_or_else(|| "bucket".to_string());
    let timeout = Duration::from_secs(config.timeout_secs.unwrap_or(30).max(1));
    // The namespace reports the underlying scheme so the form matrix's
    // scheme assertions hold for bucket-filled and bucket-less alike.
    // `ServiceInfo::new` stores the scheme string by reference ('static); the
    // namespace protocols are a fixed set, so only an out-of-contract scheme
    // leaks.
    let scheme_static: &'static str = match scheme.as_str() {
        "s3" => "s3",
        "oss" => "oss",
        "cos" => "cos",
        "obs" => "obs",
        "azblob" => "azblob",
        other => Box::leak(other.to_string().into_boxed_str()),
    };
    // The namespace must not promise more than the child service delivers:
    // the engine plans moves/copies off these flags (a claimed rename that
    // the child lacks turns every move into a hard error instead of the
    // copy+delete degrade job — real-MinIO regression, smoke 2026-09-17).
    // A throwaway child operator is built offline (no dial) for introspection.
    let mut probe_kv = config.base_kv.clone();
    probe_kv.push((bucket_key.clone(), "bucket-namespace-capability-probe".to_string()));
    let child = super::build_registered_operator(&scheme, probe_kv)
        .map(|operator| operator.info().capability())
        .unwrap_or_default();
    // copy=false → copies (cross-bucket included) degrade to the engine's
    // read→write job; presign=false → files/publicLink reports unsupported.
    // stat/list stay on (namespace root + virtual bucket dirs answer locally).
    let capability = Capability {
        stat: true,
        read: child.read,
        write: child.write,
        write_can_empty: child.write_can_empty,
        write_can_multi: child.write_can_multi,
        create_dir: true,
        delete: child.delete,
        delete_with_recursive: child.delete_with_recursive,
        list: true,
        list_with_recursive: child.list_with_recursive,
        rename: child.rename,
        shared: true,
        ..Default::default()
    };
    Ok(BucketNsAccess {
        children: Arc::new(Mutex::new(HashMap::new())),
        info: ServiceInfo::new(scheme_static, "/", format!("{scheme} buckets")),
        capability,
        base_kv: Arc::new(config.base_kv),
        list_params: Arc::new(config.list),
        scheme,
        bucket_key,
        timeout,
    })
}

/// Resolves an OpenDAL path into `(bucket, child-relative path)`. The root
/// itself (`None`) is the namespace listing; the first path segment selects
/// the bucket and the remainder is handed to the child operator unchanged.
/// Only leading slashes are stripped — a directory marker's trailing `/`
/// survives ("media/dir/" → rest "dir/") so directory ops keep their hint;
/// empty rest = the bucket root.
fn resolve(path: &str) -> Result<Option<(String, String)>> {
    let clean = path.trim_start_matches('/');
    if clean.is_empty() {
        return Ok(None);
    }
    let (bucket, rest) = clean.split_once('/').unwrap_or((clean, ""));
    if bucket.is_empty() {
        return Err(Error::new(
            ErrorKind::ConfigInvalid,
            "bucket namespace path is empty",
        ));
    }
    Ok(Some((
        bucket.to_string(),
        rest.trim_start_matches('/').to_string(),
    )))
}

/// The bucket namespace service: one child operator per selected bucket,
/// built lazily from the shared base kv and cached for the connection's
/// lifetime.
struct BucketNsAccess {
    scheme: String,
    bucket_key: String,
    base_kv: Arc<Vec<(String, String)>>,
    list_params: Arc<list::ListParams>,
    timeout: Duration,
    children: Arc<Mutex<HashMap<String, Operator>>>,
    info: ServiceInfo,
    capability: Capability,
}

impl std::fmt::Debug for BucketNsAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BucketNsAccess")
            .field("scheme", &self.scheme)
            .finish_non_exhaustive()
    }
}

impl BucketNsAccess {
    /// Clones (or lazily builds) the child operator for a bucket. Building
    /// is offline (`via_iter` dials on first use), so the mutex is never
    /// held across an await.
    fn child(&self, bucket: &str) -> Result<Operator> {
        child_operator(
            &self.scheme,
            &self.bucket_key,
            &self.base_kv,
            &self.children,
            bucket,
        )
    }

    /// `(bucket, rest)` with the invariant that a path inside the bucket was
    /// selected (rest non-empty). Shared by the object-only operations.
    fn require_object(&self, path: &str, operation: &str) -> Result<(String, String)> {
        match resolve(path)? {
            None => Err(Error::new(
                ErrorKind::ConfigInvalid,
                format!(
                    "object storage {operation} requires a selected bucket; the connection \
                     root lists all buckets"
                ),
            )),
            Some((bucket, rest)) if rest.is_empty() => Err(Error::new(
                ErrorKind::ConfigInvalid,
                format!(
                    "object storage {operation} requires a path inside a bucket \
                     ('{bucket}' names the bucket itself)"
                ),
            )),
            Some((bucket, rest)) => Ok((bucket, rest)),
        }
    }
}

/// Child-operator cache lookup/build shared by the access and the deleter.
fn child_operator(
    scheme: &str,
    bucket_key: &str,
    base_kv: &[ (String, String) ],
    children: &Mutex<HashMap<String, Operator>>,
    bucket: &str,
) -> Result<Operator> {
    let mut children = children.lock().map_err(|_| {
        Error::new(ErrorKind::Unexpected, "bucket namespace cache poisoned")
    })?;
    if let Some(operator) = children.get(bucket) {
        return Ok(operator.clone());
    }
    let mut kv = base_kv.to_vec();
    kv.push((bucket_key.to_string(), bucket.to_string()));
    let operator = super::build_registered_operator(scheme, kv).map_err(|error| {
        Error::new(
            ErrorKind::ConfigInvalid,
            format!("failed to build the operator for bucket '{bucket}': {error}"),
        )
    })?;
    children.insert(bucket.to_string(), operator.clone());
    Ok(operator)
}

impl Service for BucketNsAccess {
    type Reader = NsReader;
    type Writer = NsWriter;
    type Lister = NsLister;
    type Deleter = oio::OneShotDeleter<NsDeleter>;
    type Copier = ();
    type Composer = ();

    fn info(&self) -> ServiceInfo {
        self.info.clone()
    }

    fn capability(&self) -> Capability {
        self.capability.clone()
    }

    /// mkdir -p inside the selected bucket. The bucket root itself answers
    /// Ok without network: buckets pre-exist (OpenDAL create_dir is
    /// mkdir -p, and the transfer walkers ensure ancestors all the way up to
    /// the bucket — the SMB adapter makes the same share-root choice).
    /// Creating a *new* bucket surfaces later as the provider's NoSuchBucket
    /// on the first object write.
    async fn create_dir(&self, _ctx: &OperationContext, path: &str, _: OpCreateDir) -> Result<RpCreateDir> {
        let Some((bucket, rest)) = resolve(path)? else {
            return Err(Error::new(
                ErrorKind::ConfigInvalid,
                "object storage create_dir requires a selected bucket; the connection root \
                 lists all buckets",
            ));
        };
        if !rest.is_empty() {
            self.child(&bucket)?.create_dir(&rest).await?;
        }
        Ok(RpCreateDir::default())
    }

    /// Namespace root and bucket roots answer virtually (no network) so
    /// breadcrumbs stay cheap; existence/permission errors surface on the
    /// first real list inside the bucket.
    async fn stat(&self, _ctx: &OperationContext, path: &str, _: OpStat) -> Result<RpStat> {
        let metadata = match resolve(path)? {
            None => MetadataBuilder::dir().build(),
            Some((_bucket, rest)) if rest.is_empty() => MetadataBuilder::dir().build(),
            Some((bucket, rest)) => self.child(&bucket)?.stat(&rest).await?,
        };
        Ok(RpStat::new(metadata))
    }

    /// Returns a ranged reader over the selected bucket; the child dials on
    /// the first read/open.
    fn read(&self, _ctx: &OperationContext, path: &str, _args: OpRead) -> Result<Self::Reader> {
        let (bucket, rest) = self.require_object(path, "read")?;
        Ok(NsReader {
            child: self.child(&bucket)?,
            path: rest,
            total: Mutex::new(None),
        })
    }

    /// Returns a pipelined streaming writer into the selected bucket (the
    /// child's Writer handles multipart semantics per service); the child
    /// writer opens lazily on the first chunk.
    fn write(&self, _ctx: &OperationContext, path: &str, _args: OpWrite) -> Result<Self::Writer> {
        let (bucket, rest) = self.require_object(path, "write")?;
        Ok(NsWriter {
            child: self.child(&bucket)?,
            path: rest,
            writer: None,
        })
    }

    fn delete(&self, _ctx: &OperationContext) -> Result<Self::Deleter> {
        Ok(oio::OneShotDeleter::new(NsDeleter {
            scheme: self.scheme.clone(),
            bucket_key: self.bucket_key.clone(),
            base_kv: self.base_kv.clone(),
            children: self.children.clone(),
        }))
    }

    /// Namespace root → native bucket listing; any deeper path delegates to
    /// the child operator's lister with the bucket prefixed back onto the
    /// entry paths (same namespace mapping as the SMB share lister). The
    /// listing work happens lazily on the lister's first `next()` (0.59
    /// `Service::list` is a synchronous constructor).
    fn list(&self, _ctx: &OperationContext, path: &str, args: OpList) -> Result<Self::Lister> {
        let source = match resolve(path)? {
            None => NsSource::Buckets {
                params: (*self.list_params).clone(),
                timeout: self.timeout,
            },
            Some((bucket, rest)) => NsSource::Child {
                child: self.child(&bucket)?,
                bucket,
                rest,
                recursive: args.recursive(),
            },
        };
        Ok(NsLister {
            source: Some(source),
            queue: Vec::new().into_iter(),
        })
    }

    fn copy(
        &self,
        _ctx: &OperationContext,
        _from: &str,
        _to: &str,
        _args: OpCopy,
    ) -> Result<Self::Copier> {
        // Capability copy=false gates this before it is ever reached; the
        // engine degrades copies to its read→write job.
        Err(Error::new(
            ErrorKind::Unsupported,
            "bucket namespace copy is not supported",
        ))
    }

    /// Rename within one bucket delegates to the child; crossing buckets
    /// (or renaming a bucket itself) is refused — cross-bucket moves go
    /// through the engine's copy jobs.
    async fn rename(
        &self,
        _ctx: &OperationContext,
        from: &str,
        to: &str,
        _: OpRename,
    ) -> Result<RpRename> {
        let (source_bucket, source) = self.require_object(from, "rename")?;
        let (target_bucket, target) = self.require_object(to, "rename")?;
        if source_bucket != target_bucket {
            return Err(Error::new(
                ErrorKind::ConfigInvalid,
                "object storage rename cannot cross buckets",
            ));
        }
        self.child(&source_bucket)?
            .rename(&source, &target)
            .await?;
        Ok(RpRename::default())
    }

    async fn presign(
        &self,
        _ctx: &OperationContext,
        _path: &str,
        _args: OpPresign,
    ) -> Result<RpPresign> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "bucket namespace presign is not supported",
        ))
    }
}

/// Maps a bucket listing failure onto OpenDAL error kinds (401/403 →
/// PermissionDenied with the "fill in the bucket" hint).
fn list_error_to_opendal(error: list::BucketListError) -> Error {
    let message = error.to_message();
    match error {
        list::BucketListError::PermissionDenied(_) => {
            Error::new(ErrorKind::PermissionDenied, message)
        }
        list::BucketListError::NotFound(_) => Error::new(ErrorKind::NotFound, message),
        list::BucketListError::Other(_) => Error::new(ErrorKind::Unexpected, message),
    }
}

/// Ranged reader: every `oio::Read::read` serves the requested window via a
/// ranged child request; unbounded ranges stat the object once (cached) and
/// drain to EOF in `NS_READ_CHUNK` windows.
struct NsReader {
    child: Operator,
    path: String,
    /// Object size cache for unbounded reads (std mutex: no await held while
    /// locked — the stat completes first).
    total: Mutex<Option<u64>>,
}

impl NsReader {
    /// Object size for unbounded windows, resolved once via the child stat.
    /// A poisoned lock is recovered by keeping the (unobservable) cached
    /// value — the size cache is advisory and the stat result is stable.
    async fn total(&self) -> Result<u64> {
        let cached = *self
            .total
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(total) = cached {
            return Ok(total);
        }
        let total = self.child.stat(&self.path).await?.content_length();
        *self
            .total
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(total);
        Ok(total)
    }

    /// Serves `[offset, offset + want)` — may return fewer bytes only at EOF.
    async fn read_window(&self, offset: u64, want: u64) -> Result<Vec<u8>> {
        let buffer = self
            .child
            .read_with(&self.path)
            .range(offset..offset + want)
            .await?;
        Ok(buffer.to_vec())
    }
}

impl oio::Read for NsReader {
    async fn read(&self, range: BytesRange) -> Result<(RpRead, Buffer)> {
        let offset = range.offset();
        let mut collected = Vec::new();
        match range.size() {
            Some(size) => {
                while (collected.len() as u64) < size {
                    let want = (size - collected.len() as u64).min(NS_READ_CHUNK);
                    let chunk = self.read_window(offset + collected.len() as u64, want).await?;
                    if chunk.is_empty() {
                        break;
                    }
                    collected.extend_from_slice(&chunk);
                }
            }
            None => {
                let total = self.total().await?;
                while (offset + collected.len() as u64) < total {
                    let want =
                        (total - offset - collected.len() as u64).min(NS_READ_CHUNK);
                    let chunk = self.read_window(offset + collected.len() as u64, want).await?;
                    if chunk.is_empty() {
                        break;
                    }
                    collected.extend_from_slice(&chunk);
                }
            }
        }
        Ok((RpRead::default(), Buffer::from(collected)))
    }

    async fn open(&self, range: BytesRange) -> Result<(RpRead, Box<dyn oio::ReadStreamDyn>)> {
        Ok((
            RpRead::default(),
            Box::new(NsStream {
                child: self.child.clone(),
                path: self.path.clone(),
                offset: range.offset(),
                remaining: range.size(),
                total: Arc::new(Mutex::new(None)),
            }) as Box<dyn oio::ReadStreamDyn>,
        ))
    }
}

/// Chunked drain stream behind `oio::Read::open`.
struct NsStream {
    child: Operator,
    path: String,
    offset: u64,
    remaining: Option<u64>,
    total: Arc<Mutex<Option<u64>>>,
}

impl oio::ReadStream for NsStream {
    async fn read(&mut self) -> Result<Buffer> {
        // Resolve the size cache without holding the std mutex across awaits
        // (poison recovery, same shape as transfers' conn_lock).
        let cached = *self
            .total
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let total = match cached {
            Some(total) => Some(total),
            None => {
                let total = self.child.stat(&self.path).await?.content_length();
                *self
                    .total
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(total);
                Some(total)
            }
        };
        if let Some(total) = total {
            if self.offset >= total {
                return Ok(Buffer::new());
            }
        }
        let want = match self.remaining {
            Some(left) => left.min(NS_READ_CHUNK),
            None => total.map_or(NS_READ_CHUNK, |total| (total - self.offset).min(NS_READ_CHUNK)),
        };
        let buffer = self
            .child
            .read_with(&self.path)
            .range(self.offset..self.offset + want)
            .await?;
        let served = buffer.len() as u64;
        if served == 0 {
            return Ok(Buffer::new());
        }
        self.offset += served;
        if let Some(remaining) = self.remaining.as_mut() {
            *remaining -= served.min(*remaining);
        }
        Ok(buffer)
    }
}

/// Pipelined streaming writer; the child writer opens lazily on the first
/// chunk, and `close` opens it for never-written uploads so empty files
/// keep working (`write_can_empty`).
struct NsWriter {
    child: Operator,
    path: String,
    writer: Option<opendal::Writer>,
}

impl oio::Write for NsWriter {
    async fn write(&mut self, bs: Buffer) -> Result<()> {
        if self.writer.is_none() {
            self.writer = Some(self.child.writer(&self.path).await?);
        }
        self.writer
            .as_mut()
            .expect("writer initialized above")
            .write(bs)
            .await
    }

    async fn close(&mut self) -> Result<Metadata> {
        let mut writer = match self.writer.take() {
            Some(writer) => writer,
            None => self.child.writer(&self.path).await?,
        };
        writer.close().await
    }

    async fn abort(&mut self) -> Result<()> {
        if let Some(mut writer) = self.writer.take() {
            writer.abort().await?;
        }
        Ok(())
    }
}

/// Entry-queue lister: the bucket listing and the delegated child list run
/// lazily on the first `next()` (errors surface as lister errors), after
/// which entries drain from a plain queue.
struct NsLister {
    source: Option<NsSource>,
    queue: std::vec::IntoIter<oio::Entry>,
}

/// Where the lister's entries come from: the native bucket listing at the
/// namespace root, or a delegated child-operator list inside one bucket.
enum NsSource {
    Buckets {
        params: list::ListParams,
        timeout: Duration,
    },
    Child {
        child: Operator,
        bucket: String,
        rest: String,
        recursive: bool,
    },
}

impl NsLister {
    /// Loads the next directory/bucket level into the queue.
    async fn load(&mut self, source: NsSource) -> Result<()> {
        let entries: Vec<oio::Entry> = match source {
            NsSource::Buckets { params, timeout } => list::list_buckets(&params, timeout)
                .await
                .map_err(list_error_to_opendal)?
                .into_iter()
                .map(|bucket| {
                    oio::Entry::new(&format!("{bucket}/"), MetadataBuilder::dir().build())
                })
                .collect(),
            NsSource::Child {
                child,
                bucket,
                rest,
                recursive,
            } => {
                let raw = if recursive {
                    child.list_with(&rest).recursive(true).await?
                } else {
                    child.list(&rest).await?
                };
                raw.into_iter()
                    .map(|entry| {
                        oio::Entry::new(
                            &format!("{bucket}/{}", entry.path()),
                            entry.metadata().clone(),
                        )
                    })
                    .collect()
            }
        };
        self.queue = entries.into_iter();
        Ok(())
    }
}

impl oio::List for NsLister {
    async fn next(&mut self) -> Result<Option<oio::Entry>> {
        loop {
            if let Some(entry) = self.queue.next() {
                return Ok(Some(entry));
            }
            match self.source.take() {
                Some(source) => self.load(source).await?,
                None => return Ok(None),
            }
        }
    }
}

/// One-shot deleter delegating to the child operator; deleting the bucket
/// itself (empty rest) is refused — buckets are read-only pseudo-entries.
struct NsDeleter {
    scheme: String,
    bucket_key: String,
    base_kv: Arc<Vec<(String, String)>>,
    children: Arc<Mutex<HashMap<String, Operator>>>,
}

impl oio::OneShotDelete for NsDeleter {
    async fn delete_once(&self, path: String, args: OpDelete) -> Result<()> {
        let Some((bucket, rest)) = resolve(&path)? else {
            return Err(Error::new(
                ErrorKind::ConfigInvalid,
                "object storage delete requires a selected bucket; the connection root \
                 lists all buckets",
            ));
        };
        if rest.is_empty() {
            return Err(Error::new(
                ErrorKind::ConfigInvalid,
                format!("deleting bucket '{bucket}' is not supported"),
            ));
        }
        let child = child_operator(
            &self.scheme,
            &self.bucket_key,
            &self.base_kv,
            &self.children,
            &bucket,
        )?;
        child.delete_with(&rest).recursive(args.recursive()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendal::raw::oio::Read as _;

    fn config(scheme: &str) -> BucketNsConfig {
        BucketNsConfig {
            scheme: Some(scheme.to_string()),
            bucket_key: Some("bucket".to_string()),
            // s3 needs a region for the offline capability probe to build.
            base_kv: vec![("region".to_string(), "us-east-1".to_string())],
            list: list::ListParams {
                protocol: scheme.to_string(),
                ..list::ListParams::default()
            },
            timeout_secs: Some(5),
        }
    }

    #[test]
    fn namespace_mode_matches_protocols_and_empty_bucket() {
        let connection = |protocol: &str, bucket: &str| {
            StoredConnection::from_lifecycle_params(&serde_json::json!({
                "connection": {
                    "id": "ns",
                    "external_config": {
                        "protocol": protocol,
                        "bucket": bucket,
                        "container": bucket,
                    }
                }
            }))
            .unwrap()
        };
        for protocol in ["s3", "oss", "cos", "obs", "azblob"] {
            assert!(is_bucket_namespace_protocol(protocol), "{protocol}");
            assert!(namespace_mode(&connection(protocol, "")), "{protocol} empty");
            assert!(
                !namespace_mode(&connection(protocol, "demo")),
                "{protocol} filled stays pass-through"
            );
        }
        // gcs is phase 2: bucket stays required, never namespace mode.
        assert!(!is_bucket_namespace_protocol("gcs"));
        assert!(!namespace_mode(&connection("gcs", "")));
        assert!(!namespace_mode(&connection("fs", "")));

        assert_eq!(bucket_config_key("azblob"), "container");
        for protocol in ["s3", "oss", "cos", "obs"] {
            assert_eq!(bucket_config_key(protocol), "bucket");
        }
    }

    #[test]
    fn namespace_operators_build_offline_with_underlying_scheme() {
        for scheme in ["s3", "oss", "cos", "obs", "azblob"] {
            // opendal 0.58+: Operator::new returns the finished operator.
            let operator = Operator::new(BucketNsBuilder::from_config(config(scheme))).unwrap();
            assert_eq!(operator.info().scheme(), scheme, "{scheme}");
            assert_eq!(operator.info().root(), "/");
            assert!(
                operator.info().name().contains("buckets"),
                "{scheme} name: {}",
                operator.info().name()
            );
        }
    }

    #[test]
    fn namespace_capability_never_exceeds_the_child() {
        let capability = Operator::new(BucketNsBuilder::from_config(config("s3")))
            .unwrap()
            .info()
            .capability();
        // Child-derived flags (probed offline from the same base kv).
        let probe_kv = vec![
            ("bucket".to_string(), "probe".to_string()),
            ("region".to_string(), "us-east-1".to_string()),
        ];
        let child = super::super::build_registered_operator("s3", probe_kv)
            .unwrap()
            .info()
            .capability();
        println!(
            "PROBE child: rename={} copy={} delete={} read={} write={}",
            child.rename, child.copy, child.delete, child.read, child.write
        );
        println!(
            "PROBE namespace: rename={} copy={} delete={} read={} write={}",
            capability.rename, capability.copy, capability.delete, capability.read, capability.write
        );
        assert_eq!(capability.rename, child.rename, "rename tracks the child");
        assert_eq!(capability.write_can_multi, child.write_can_multi);
        assert_eq!(capability.delete, child.delete);
        // Namespace invariants regardless of the child.
        assert!(capability.stat && capability.list);
        assert!(capability.create_dir);
        assert!(!capability.copy, "copies degrade to the read→write job");
        assert!(!capability.presign_read);
    }

    #[test]
    fn builder_requires_scheme() {
        assert!(build_access(BucketNsConfig::default()).is_err());
        let result = BucketNsBuilder::new().bucket_key("bucket").build();
        assert!(result.is_err(), "missing scheme must fail the build");
    }

    #[test]
    fn resolve_selects_bucket_from_first_segment() {
        assert_eq!(resolve("/").unwrap(), None);
        assert_eq!(resolve("").unwrap(), None);
        assert_eq!(
            resolve("media").unwrap(),
            Some(("media".to_string(), String::new()))
        );
        assert_eq!(
            resolve("media/").unwrap(),
            Some(("media".to_string(), String::new()))
        );
        assert_eq!(
            resolve("media/dir/sub file.txt").unwrap(),
            Some(("media".to_string(), "dir/sub file.txt".to_string()))
        );
        assert_eq!(
            resolve("media/dir/").unwrap(),
            Some(("media".to_string(), "dir/".to_string()))
        );
    }

    #[test]
    fn require_object_refuses_namespace_and_bucket_roots() {
        let access = build_access(config("s3")).unwrap();

        let error = access.require_object("/", "read").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ConfigInvalid);
        assert!(error.to_string().contains("requires a selected bucket"));

        let error = access.require_object("media", "write").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ConfigInvalid);
        assert!(
            error.to_string().contains("inside a bucket"),
            "{error}"
        );

        // A real object path passes through untouched.
        let (bucket, rest) = access.require_object("media/dir/a.txt", "read").unwrap();
        assert_eq!(bucket, "media");
        assert_eq!(rest, "dir/a.txt");
    }

    #[test]
    fn child_operators_build_offline_and_cache() {
        // memory is a registered offline service, so the child build path is
        // exercised end-to-end without network.
        let access = build_access(config("memory")).unwrap();
        let first = access.child("one").unwrap();
        assert_eq!(first.info().scheme(), "memory");
        let second = access.child("one").unwrap();
        assert_eq!(first.info().root(), second.info().root());
        assert_eq!(access.children.lock().unwrap().len(), 1, "cached per bucket");
        access.child("two").unwrap();
        assert_eq!(access.children.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn create_dir_is_idempotent_at_bucket_root_and_refused_at_namespace_root() {
        // Bucket root: mkdir -p semantics — the bucket pre-exists, so the
        // transfer walkers' ancestor creation succeeds (mirrors the SMB
        // share-root choice).
        let access = build_access(config("memory")).unwrap();
        let ctx = opendal::OperationContext::default();
        access
            .create_dir(&ctx, "media", opendal::raw::OpCreateDir::default())
            .await
            .expect("bucket-root create_dir answers Ok");
        // Namespace root still refuses: there is no bucket selected.
        let error = access
            .create_dir(&ctx, "/", opendal::raw::OpCreateDir::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ConfigInvalid);
        assert!(error.to_string().contains("requires a selected bucket"));
    }

    #[tokio::test]
    async fn ns_reader_serves_bounded_windows_unbounded_drains_and_eof() {
        // 0.59 reader semantics, offline: the memory child provides a real
        // backend; the payload spans two NS_READ_CHUNK windows plus a tail so
        // the multi-window loop is exercised in both bounded and unbounded
        // (size-cache + EOF drain) modes.
        let access = build_access(config("memory")).unwrap();
        let child = access.child("media").unwrap();
        let total = (NS_READ_CHUNK * 2 + 4096) as usize;
        let payload: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
        child.write("big.bin", payload.clone()).await.unwrap();

        let reader = NsReader {
            child: child.clone(),
            path: "big.bin".to_string(),
            total: Mutex::new(None),
        };

        // Bounded range spanning two windows lands exactly the asked slice.
        let window = NS_READ_CHUNK + 4096;
        let (_, head) = reader.read(BytesRange::from(0..window)).await.unwrap();
        assert_eq!(head.len() as u64, window);
        assert_eq!(head.to_vec(), payload[..window as usize]);

        // Bounded range deep into the final window.
        let (_, tail_bounded) = reader
            .read(BytesRange::from((total as u64 - 1024)..total as u64))
            .await
            .unwrap();
        assert_eq!(tail_bounded.to_vec(), payload[total - 1024..]);

        // Unbounded range drains from the offset to EOF (size cache stats the
        // child exactly once, then answers from cache).
        let (_, drained) = reader
            .read(BytesRange::from((total as u64 - 100)..))
            .await
            .unwrap();
        assert_eq!(drained.len(), 100);
        assert_eq!(drained.to_vec(), payload[total - 100..]);
        assert_eq!(*reader.total.lock().unwrap(), Some(total as u64));

        // Offset at EOF answers empty instead of erroring.
        let (_, past) = reader.read(BytesRange::from(total as u64..)).await.unwrap();
        assert!(past.is_empty());
    }
}
