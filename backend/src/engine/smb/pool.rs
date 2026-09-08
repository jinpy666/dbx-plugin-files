//! SMB client lifecycle for the OpenDAL adapter (IMPL_PLAN_SMB §2.1).
//!
//! The `smb2` client is not `Clone` and its directory-class methods take
//! `&mut self`, so:
//! - the client + share tree live behind one `tokio::sync::Mutex` and every
//!   directory-class operation (stat/list/mkdir/delete/rename) serializes on
//!   it;
//! - streaming reads/writes are opened through the mutex (a CREATE round
//!   trip) and then run on independent `'static` file handles that own a
//!   cheap `Connection` clone, so transfer jobs never hold the mutex.
//!
//! Connections are lazy: `SmbBuilder::build` is synchronous while the SMB
//! handshake (negotiate + session setup + tree connect) is async, so the
//! dial happens on the first operation instead. Dead sessions are handled
//! first by the crate's `auto_reconnect`/`ReconnectPolicy`; a client that
//! still reports a lost connection is dropped and re-dialed on the next
//! call (`connection/connect` stays idempotent at the engine layer).
//!
//! Credential red line (§2.4): `SmbConnectParams` holds the password in
//! memory only; the manual `Debug` impls below keep credentials out of any
//! log/error formatting, and no `Display` path here ever renders them.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use opendal::{Error, ErrorKind};
use tokio::sync::Mutex;

/// Connect parameters for one SMB share. `Debug` is manual: credentials are
/// never rendered.
pub(super) struct SmbConnectParams {
    pub host: String,
    pub port: u16,
    /// Optional default share. Empty means server-level browsing: the first
    /// path component selects a share and the root lists available shares.
    pub share: Option<String>,
    pub username: String,
    pub password: String,
    pub domain: String,
}

impl fmt::Debug for SmbConnectParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmbConnectParams")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("share", &self.share)
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

/// One connected client: the SMB session plus its tree connect to the share.
struct SmbState {
    client: smb2::SmbClient,
    /// Trees are opened lazily and cached by share name. This lets one
    /// server-level Operator browse several Samba exports without storing a
    /// second credential-bearing connection.
    trees: HashMap<String, smb2::Tree>,
}

/// Lazily-dialed single-client pool behind the adapter.
pub(super) struct SmbPool {
    params: SmbConnectParams,
    state: Mutex<Option<SmbState>>,
}

impl fmt::Debug for SmbPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmbPool")
            .field("params", &self.params)
            .finish_non_exhaustive()
    }
}

/// Directory-class operations dispatched through the pool mutex. Kept as an
/// enum (instead of closures) so the connected-client borrow stays inside
/// `call` and no HRTB boxing is needed.
pub(super) enum SmbOp<'a> {
    ListShares,
    ConnectShare {
        share: &'a str,
    },
    Stat {
        share: &'a str,
        path: &'a str,
    },
    List {
        share: &'a str,
        path: &'a str,
    },
    CreateDir {
        share: &'a str,
        path: &'a str,
    },
    DeleteFile {
        share: &'a str,
        path: &'a str,
    },
    DeleteDirectory {
        share: &'a str,
        path: &'a str,
    },
    Rename {
        share: &'a str,
        from: &'a str,
        to: &'a str,
    },
}

/// Result of a dispatched [`SmbOp`] (only the ops that produce data carry it).
pub(super) enum SmbOpResult {
    Shares(Vec<smb2::ShareInfo>),
    Stat(smb2::FileInfo),
    List(Vec<smb2::DirectoryEntry>),
    Unit,
}

impl SmbPool {
    pub(super) fn new(params: SmbConnectParams) -> Arc<Self> {
        Arc::new(Self {
            params,
            state: Mutex::new(None),
        })
    }

    /// Dispatches a directory-class operation, dialing on first use.
    pub(super) async fn call(&self, op: SmbOp<'_>) -> opendal::Result<SmbOpResult> {
        let mut state = self.state.lock().await;
        if state.is_none() {
            *state = Some(dial(&self.params).await?);
        }
        let SmbState { client, trees } = state.as_mut().expect("connected above");
        let outcome = match op {
            SmbOp::ListShares => client.list_shares().await.map(SmbOpResult::Shares),
            SmbOp::ConnectShare { share } => ensure_tree(client, trees, share)
                .await
                .map(|_| SmbOpResult::Unit),
            SmbOp::Stat { share, path } => match ensure_tree(client, trees, share).await {
                Ok(tree) => client.stat(tree, path).await.map(SmbOpResult::Stat),
                Err(error) => Err(error),
            },
            SmbOp::List { share, path } => match ensure_tree(client, trees, share).await {
                Ok(tree) => client
                    .list_directory(tree, path)
                    .await
                    .map(SmbOpResult::List),
                Err(error) => Err(error),
            },
            SmbOp::CreateDir { share, path } => match ensure_tree(client, trees, share).await {
                Ok(tree) => client
                    .create_directory(tree, path)
                    .await
                    .map(|_| SmbOpResult::Unit),
                Err(error) => Err(error),
            },
            SmbOp::DeleteFile { share, path } => match ensure_tree(client, trees, share).await {
                Ok(tree) => client
                    .delete_file(tree, path)
                    .await
                    .map(|_| SmbOpResult::Unit),
                Err(error) => Err(error),
            },
            SmbOp::DeleteDirectory { share, path } => match ensure_tree(client, trees, share).await
            {
                Ok(tree) => client
                    .delete_directory(tree, path)
                    .await
                    .map(|_| SmbOpResult::Unit),
                Err(error) => Err(error),
            },
            SmbOp::Rename { share, from, to } => match ensure_tree(client, trees, share).await {
                Ok(tree) => client
                    .rename(tree, from, to)
                    .await
                    .map(|_| SmbOpResult::Unit),
                Err(error) => Err(error),
            },
        };
        // A client the crate's reviver could not keep alive is discarded;
        // the next call re-dials from scratch.
        if let Err(error) = &outcome {
            if matches!(
                error.kind(),
                smb2::ErrorKind::ConnectionLost | smb2::ErrorKind::SessionExpired
            ) {
                *state = None;
            }
        }
        outcome.map_err(map_smb_error)
    }

    /// Opens a positioned streaming reader. The returned handle owns its own
    /// `Connection` clone and never touches the pool mutex afterwards.
    pub(super) async fn open_reader(&self, path: &str) -> opendal::Result<smb2::FileReader> {
        self.open_reader_on_share(self.default_share()?, path).await
    }

    pub(super) async fn open_reader_on_share(
        &self,
        share: &str,
        path: &str,
    ) -> opendal::Result<smb2::FileReader> {
        let mut state = self.state.lock().await;
        if state.is_none() {
            *state = Some(dial(&self.params).await?);
        }
        let SmbState { client, trees } = state.as_mut().expect("connected above");
        let tree = ensure_tree(client, trees, share)
            .await
            .map_err(map_smb_error)?;
        client
            .open_file_reader(tree, path)
            .await
            .map_err(map_smb_error)
    }

    /// Opens a pipelined streaming writer (overwrites by truncation).
    pub(super) async fn open_writer(&self, path: &str) -> opendal::Result<smb2::FileWriter> {
        self.open_writer_on_share(self.default_share()?, path).await
    }

    pub(super) async fn open_writer_on_share(
        &self,
        share: &str,
        path: &str,
    ) -> opendal::Result<smb2::FileWriter> {
        let mut state = self.state.lock().await;
        if state.is_none() {
            *state = Some(dial(&self.params).await?);
        }
        let SmbState { client, trees } = state.as_mut().expect("connected above");
        let tree = ensure_tree(client, trees, share)
            .await
            .map_err(map_smb_error)?;
        client
            .create_file_writer(tree, path)
            .await
            .map_err(map_smb_error)
    }

    fn default_share(&self) -> opendal::Result<&str> {
        self.params.share.as_deref().ok_or_else(|| {
            Error::new(
                ErrorKind::ConfigInvalid,
                "smb share must be selected before file access",
            )
        })
    }
}

/// Returns a cached tree, opening it lazily when a server-level connection
/// first touches a share.
async fn ensure_tree<'a>(
    client: &mut smb2::SmbClient,
    trees: &'a mut HashMap<String, smb2::Tree>,
    share: &str,
) -> std::result::Result<&'a mut smb2::Tree, smb2::Error> {
    if !trees.contains_key(share) {
        let tree = client.connect_share(share).await?;
        trees.insert(share.to_string(), tree);
    }
    Ok(trees.get_mut(share).expect("tree inserted or cached"))
}

/// Runs the session handshake: TCP dial, negotiate, and session setup.
/// Tree connects are deliberately lazy because a server-level operator may
/// browse and use several exported shares with the same authenticated client.
async fn dial(params: &SmbConnectParams) -> opendal::Result<SmbState> {
    let config = smb2::ClientConfig {
        addr: format!("{}:{}", params.host, params.port),
        timeout: Duration::from_secs(30),
        username: params.username.clone(),
        password: params.password.clone(),
        domain: params.domain.clone(),
        // Arm the crate's SessionReviver so flapping sessions recover in
        // place; read-only ops replay, mutating ops surface errors (crate
        // defaults for the retry bounds).
        auto_reconnect: true,
        compression: true,
        dfs_enabled: true,
        dfs_target_overrides: HashMap::new(),
    };
    let client = smb2::SmbClient::connect(config)
        .await
        .map_err(map_smb_error)?;
    Ok(SmbState {
        client,
        trees: HashMap::new(),
    })
}

/// Maps an `smb2` error onto the OpenDAL error taxonomy. The crate's `Display`
/// never carries credentials (NT failures are NTSTATUS + command shaped), so
/// echoing the source message is safe.
pub(super) fn map_smb_error(error: smb2::Error) -> opendal::Error {
    let kind = match error.kind() {
        smb2::ErrorKind::NotFound => ErrorKind::NotFound,
        smb2::ErrorKind::AlreadyExists => ErrorKind::AlreadyExists,
        smb2::ErrorKind::AccessDenied
        | smb2::ErrorKind::AuthRequired
        | smb2::ErrorKind::SigningRequired => ErrorKind::PermissionDenied,
        smb2::ErrorKind::IsADirectory => ErrorKind::IsADirectory,
        smb2::ErrorKind::NotADirectory => ErrorKind::NotADirectory,
        _ => ErrorKind::Unexpected,
    };
    Error::new(kind, format!("smb: {error}"))
}
