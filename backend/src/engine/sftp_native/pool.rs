//! SSH session lifecycle for the native SFTP adapter.
//!
//! One russh `Handle` + one `SftpSession` per connection, lazily dialed on
//! the first operation (the handshake is async while `Builder::build` is
//! sync). russh-sftp's session methods take the session exclusively, so the
//! session lives behind one `tokio::sync::Mutex` and directory ops serialize
//! on it; opened `File` handles own an independent session sender and stream
//! without touching the mutex (streaming transfers never hold it). A session
//! that reports a transport-level failure is discarded and the next call
//! re-dials (`connection/connect` stays idempotent at the engine layer).
//!
//! Host-key handling runs inside the russh `Handler::check_server_key`
//! callback according to the configured [`HostKeyStrategy`]: Strict verifies
//! against the sidecar `known_hosts` file, Tolerate is accept-new (changed
//! keys still reject), Trust accepts everything.
//!
//! Credential red line: `SftpNativeConnectParams` holds the password/key in
//! memory only; the manual `Debug` impl never renders credentials.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use opendal::{Error, ErrorKind};
use russh::client;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags, StatusCode};
use tokio::sync::Mutex;

use super::HostKeyStrategy;

type SftpError = russh_sftp::client::error::Error;

/// Auth channel resolved at build time: password (with keyboard-interactive
/// fallback) or a decoded private key.
pub(super) enum SftpNativeAuth {
    Password(String),
    Key(Arc<russh::keys::PrivateKey>),
}

impl fmt::Debug for SftpNativeAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Never render the credential itself.
            Self::Password(_) => f.write_str("Password(***)"),
            Self::Key(key) => f
                .debug_struct("Key")
                .field("algorithm", &key.algorithm().to_string())
                .finish(),
        }
    }
}

/// Connect parameters for one SFTP endpoint. `Debug` is manual: credentials
/// are never rendered.
pub(super) struct SftpNativeConnectParams {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub credentials: SftpNativeAuth,
    pub strategy: HostKeyStrategy,
    pub known_hosts_path: PathBuf,
}

impl fmt::Debug for SftpNativeConnectParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SftpNativeConnectParams")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("credentials", &self.credentials)
            .field("strategy", &format!("{:?}", self.strategy))
            .finish_non_exhaustive()
    }
}

/// One connected session: the authenticated SSH handle (kept alive for the
/// connection) plus its SFTP channel behind the shared mutex.
pub(super) struct SftpNativeState {
    handle: client::Handle<HostKeyGate>,
    sftp: Arc<Mutex<SftpSession>>,
}

/// Lazily-dialed single-session pool behind the adapter.
pub(super) struct SftpNativePool {
    params: SftpNativeConnectParams,
    state: Mutex<Option<SftpNativeState>>,
}

impl fmt::Debug for SftpNativePool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SftpNativePool")
            .field("params", &self.params)
            .finish_non_exhaustive()
    }
}

/// Directory-class operations dispatched through the pool mutex. File handles
/// (read/write streams) bypass the enum via [`SftpNativePool::open_reader`]
/// / [`SftpNativePool::open_writer`].
pub(super) enum SftpOp<'a> {
    Stat { path: &'a str },
    List { path: &'a str },
    CreateDir { path: &'a str },
    RemoveFile { path: &'a str },
    RemoveDir { path: &'a str },
    Rename { from: &'a str, to: &'a str },
}

/// Result of a dispatched [`SftpOp`].
pub(super) enum SftpOpResult {
    Stat(FileAttributes),
    /// Buffered directory entries: (name, is_dir, size, mtime).
    List(Vec<(String, bool, Option<u64>, Option<u32>)>),
    Unit,
}

impl SftpNativePool {
    pub(super) fn new(params: SftpNativeConnectParams) -> Arc<Self> {
        Arc::new(Self {
            params,
            state: Mutex::new(None),
        })
    }

    /// Dispatches a directory-class operation, dialing on first use. The pool
    /// mutex is held across the await: directory ops serialize, mirroring the
    /// SMB pool's semantics.
    pub(super) async fn call(&self, op: SftpOp<'_>) -> opendal::Result<SftpOpResult> {
        let mut guard = self.state.lock().await;
        if guard.is_none() {
            *guard = Some(dial(&self.params).await?);
        }
        let sftp = guard.as_ref().expect("connected above").sftp.clone();
        let outcome = dispatch(&sftp, op).await;
        // Transport-level failures (timeout, broken stream, protocol desync)
        // poison the session: drop it so the next call re-dials from scratch.
        // Protocol-level answers (NoSuchFile, PermissionDenied, …) keep the
        // session alive.
        if let Err(error) = &outcome {
            if is_transport_error(error) {
                *guard = None;
            }
        }
        outcome.map_err(map_sftp_error)
    }

    /// Opens a sequential read stream positioned at `offset`. The returned
    /// handle owns an independent session sender and never touches the pool
    /// mutex afterwards.
    pub(super) async fn open_reader(
        &self,
        path: &str,
        offset: u64,
    ) -> opendal::Result<russh_sftp::client::fs::File> {
        let mut guard = self.state.lock().await;
        if guard.is_none() {
            *guard = Some(dial(&self.params).await?);
        }
        let sftp = guard.as_ref().expect("connected above").sftp.clone();
        let mut file = sftp.lock().await.open(path).await.map_err(map_sftp_error)?;
        if offset > 0 {
            use tokio::io::AsyncSeekExt;
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|error| {
                    Error::new(ErrorKind::Unexpected, format!("sftp-native seek failed: {error}"))
                })?;
        }
        Ok(file)
    }

    /// Opens a truncating write stream (OpenDAL write semantics: overwrite).
    pub(super) async fn open_writer(
        &self,
        path: &str,
    ) -> opendal::Result<russh_sftp::client::fs::File> {
        let mut guard = self.state.lock().await;
        if guard.is_none() {
            *guard = Some(dial(&self.params).await?);
        }
        let sftp = guard.as_ref().expect("connected above").sftp.clone();
        let file = sftp
            .lock()
            .await
            .open_with_flags(path, OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE)
            .await;
        file.map_err(map_sftp_error)
    }
}

/// Dispatches one directory-class op against the shared session. Transport
/// errors stay typed so the caller can decide on a re-dial.
async fn dispatch(
    sftp: &Arc<Mutex<SftpSession>>,
    op: SftpOp<'_>,
) -> Result<SftpOpResult, SftpError> {
    let sftp = sftp.lock().await;
    match op {
        SftpOp::Stat { path } => {
            Ok(SftpOpResult::Stat(sftp.metadata(path).await?))
        }
        SftpOp::List { path } => {
            let entries = sftp
                .read_dir(path)
                .await?
                .map(|entry| {
                    let name = entry.file_name();
                    let is_dir = entry.file_type() == FileType::Dir;
                    let metadata = entry.metadata();
                    (name, is_dir, metadata.size, metadata.mtime)
                })
                .collect();
            Ok(SftpOpResult::List(entries))
        }
        SftpOp::CreateDir { path } => {
            sftp.create_dir(path).await?;
            Ok(SftpOpResult::Unit)
        }
        SftpOp::RemoveFile { path } => {
            sftp.remove_file(path).await?;
            Ok(SftpOpResult::Unit)
        }
        SftpOp::RemoveDir { path } => {
            sftp.remove_dir(path).await?;
            Ok(SftpOpResult::Unit)
        }
        SftpOp::Rename { from, to } => {
            sftp.rename(from, to).await?;
            Ok(SftpOpResult::Unit)
        }
    }
}

/// True for errors that mean the session itself is unusable (as opposed to a
/// protocol-level answer like NoSuchFile that keeps the session valid).
fn is_transport_error(error: &SftpError) -> bool {
    match error {
        SftpError::Timeout
        | SftpError::IO(_)
        | SftpError::UnexpectedPacket
        | SftpError::UnexpectedBehavior(_)
        | SftpError::Limited(_) => true,
        SftpError::Status(status) => status.status_code == StatusCode::NoConnection,
    }
}

/// Runs the full handshake: TCP dial (with host-key gate), authenticate
/// (password → keyboard-interactive fallback, or public key), open the SFTP
/// subsystem channel.
async fn dial(params: &SftpNativeConnectParams) -> opendal::Result<SftpNativeState> {
    // Strict needs the store up front so a missing known_hosts fails with a
    // clear message instead of a bare handshake rejection.
    if params.strategy == HostKeyStrategy::Strict && !params.known_hosts_path.exists() {
        return Err(Error::new(
            ErrorKind::ConfigInvalid,
            format!(
                "sftp-native Strict strategy requires a known_hosts file at {}",
                params.known_hosts_path.display()
            ),
        ));
    }
    let config = Arc::new(client::Config {
        nodelay: true,
        // Long transfers benefit from liveness detection; matches the ssh
        // plugin's 3-unanswered-probe rule.
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 3,
        ..Default::default()
    });
    let handler = HostKeyGate {
        host: params.host.clone(),
        port: params.port,
        strategy: params.strategy,
        known_hosts_path: params.known_hosts_path.clone(),
    };
    let mut handle = client::connect(config, (params.host.as_str(), params.port), handler)
        .await
        .map_err(|error| {
            Error::new(
                ErrorKind::Unexpected,
                format!(
                    "sftp-native connect to {}:{} failed: {error}",
                    params.host, params.port
                ),
            )
        })?;
    authenticate(&mut handle, params).await?;

    let channel = handle
        .channel_open_session()
        .await
        .map_err(|error| {
            Error::new(ErrorKind::Unexpected, format!("sftp-native channel open failed: {error}"))
        })?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|error| {
            Error::new(
                ErrorKind::Unexpected,
                format!("sftp-native subsystem request failed: {error}"),
            )
        })?;
    // Enterprise MFT servers in the wild choke on big single SSH_FXP_WRITE
    // packets (a 147 KiB write hung this adapter's first real server while
    // 128 KiB passed; the OpenSSH CLI ships 32 KiB chunks by default). Cap
    // the wire packet at 64 KiB — the crate derives max read/write lengths
    // from it — and keep a small write window instead of the crate default
    // of 8: real servers mis-handle deep write pipelining too. russh-sftp 3.0
    // adds read pipelining (left at the crate default) and a write packet
    // length that follows max_packet_len unless overridden.
    let sftp_config = russh_sftp::client::Config {
        max_packet_len: 64 * 1024,
        max_concurrent_writes: 4,
        request_timeout_secs: 30,
        ..Default::default()
    };
    let sftp = SftpSession::new_with_config(channel.into_stream(), sftp_config)
        .await
        .map_err(|error| {
            Error::new(ErrorKind::Unexpected, format!("sftp-native session init failed: {error}"))
        })?;

    Ok(SftpNativeState {
        handle,
        sftp: Arc::new(Mutex::new(sftp)),
    })
}

/// Authenticates the established transport: password with a
/// keyboard-interactive fallback (PAM gateways keep password auth behind
/// KbdInteractiveAuthentication), or public key.
async fn authenticate(
    handle: &mut client::Handle<HostKeyGate>,
    params: &SftpNativeConnectParams,
) -> opendal::Result<()> {
    match &params.credentials {
        SftpNativeAuth::Password(password) => {
            let result = handle
                .authenticate_password(&params.user, password)
                .await
                .map_err(|error| {
                    Error::new(
                        ErrorKind::Unexpected,
                        format!("sftp-native password auth failed: {error}"),
                    )
                })?;
            if result.success() {
                return Ok(());
            }
            authenticate_keyboard_interactive(handle, params, password).await
        }
        SftpNativeAuth::Key(key) => {
            let hash = handle
                .best_supported_rsa_hash()
                .await
                .ok()
                .flatten()
                .flatten();
            let result = handle
                .authenticate_publickey(
                    &params.user,
                    russh::keys::PrivateKeyWithHashAlg::new(Arc::clone(key), hash),
                )
                .await
                .map_err(|error| {
                    Error::new(
                        ErrorKind::Unexpected,
                        format!("sftp-native key auth failed: {error}"),
                    )
                })?;
            if result.success() {
                Ok(())
            } else {
                Err(Error::new(
                    ErrorKind::PermissionDenied,
                    format!("sftp-native rejected the private key for user '{}'", params.user),
                ))
            }
        }
    }
}

/// Answers every keyboard-interactive prompt with the configured password
/// (bounded rounds; headless sidecar — no interactive TOTP orchestration
/// here, that stays an ssh-plugin workbench feature).
async fn authenticate_keyboard_interactive(
    handle: &mut client::Handle<HostKeyGate>,
    params: &SftpNativeConnectParams,
    password: &str,
) -> opendal::Result<()> {
    use russh::client::KeyboardInteractiveAuthResponse;
    let mut round = handle
        .authenticate_keyboard_interactive_start(&params.user, None)
        .await
        .map_err(|error| {
            Error::new(
                ErrorKind::Unexpected,
                format!("sftp-native keyboard-interactive auth failed: {error}"),
            )
        })?;
    const MAX_ROUNDS: usize = 4;
    for _ in 0..MAX_ROUNDS {
        match round {
            KeyboardInteractiveAuthResponse::Success => return Ok(()),
            KeyboardInteractiveAuthResponse::Failure { .. } => break,
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let answers = vec![password.to_string(); prompts.len()];
                round = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(|error| {
                        Error::new(
                            ErrorKind::Unexpected,
                            format!("sftp-native keyboard-interactive auth failed: {error}"),
                        )
                    })?;
            }
        }
    }
    Err(Error::new(
        ErrorKind::PermissionDenied,
        format!("sftp-native rejected the password for user '{}'", params.user),
    ))
}

/// russh `Handler` that gates the handshake on the configured host-key
/// strategy. The known_hosts lookup happens inside `check_server_key`, which
/// russh awaits during the key exchange.
pub(super) struct HostKeyGate {
    host: String,
    port: u16,
    strategy: HostKeyStrategy,
    known_hosts_path: PathBuf,
}

impl client::Handler for HostKeyGate {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // Certificate hosts have no known_hosts line format in this adapter;
        // they only pass the blanket Trust strategy.
        let russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } = server_key else {
            return Ok(self.strategy == HostKeyStrategy::Trust);
        };
        match self.strategy {
            HostKeyStrategy::Trust => Ok(true),
            HostKeyStrategy::Strict => Ok(russh::keys::check_known_hosts_path(
                &self.host,
                self.port,
                key,
                &self.known_hosts_path,
            )
            .unwrap_or(false)),
            // accept-new semantics: known keys must still match (a changed
            // key → `check_known_hosts_path` Err(KeyChanged) → reject);
            // unknown hosts pass. Learned keys are not persisted.
            HostKeyStrategy::Tolerate => {
                let known = russh::keys::check_known_hosts_path(
                    &self.host,
                    self.port,
                    key,
                    &self.known_hosts_path,
                );
                Ok(matches!(known, Ok(true) | Ok(false)))
            }
        }
    }
}

/// Maps a russh-sftp client error onto the OpenDAL error taxonomy. The
/// crate's `Display` carries status codes/messages only — never credentials
/// — so echoing the source message is safe.
pub(super) fn map_sftp_error(error: SftpError) -> opendal::Error {
    let kind = match &error {
        SftpError::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => ErrorKind::NotFound,
            StatusCode::PermissionDenied => ErrorKind::PermissionDenied,
            _ => ErrorKind::Unexpected,
        },
        _ => ErrorKind::Unexpected,
    };
    Error::new(kind, format!("sftp-native: {error}"))
}
