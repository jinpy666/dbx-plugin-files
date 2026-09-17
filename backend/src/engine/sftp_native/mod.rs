//! Native SFTP Access adapter (russh + russh-sftp) for the `sftp-native`
//! quick protocol — the dual-stack companion to OpenDAL's `sftp` service.
//!
//! OpenDAL 0.57's sftp service shells out to the `ssh` binary and is
//! keyfile-only (`password` has no matching config option), which locks out
//! password-authenticated accounts — the norm on enterprise MFT servers.
//! This module wraps `russh` + `russh-sftp` (the ssh plugin's stack, where
//! password auth is proven) behind `opendal::raw::Access`, mirroring the
//! `engine::smb` adapter pattern:
//!
//! ```
//! StoredConnection(protocol="sftp-native")
//!   → engine::build_operator → Operator::new(SftpNativeBuilder)?.finish()
//!   → connection table / gates / audit / capabilities — all protocol-agnostic
//! ```
//!
//! Dual-stack decision (2026-08-31): the OpenDAL `sftp` protocol stays
//! untouched; `sftp-native` is additive. If a later OpenDAL release grows
//! password support, the protocol can migrate back to `via_iter("sftp", …)`
//! and this module retires.
//!
//! Layout:
//! - [`SftpNativeBuilder`] / [`SftpNativeConfig`]: Builder + Configurator
//!   path; constructed directly via `Operator::new(builder)?.finish()`
//!   ("sftp-native" is not an OpenDAL scheme).
//! - [`pool::SftpNativeAccess`] backing: lazy dial + session lifecycle
//!   (`pool.rs`).
//! - [`access::SftpNativeAccess`]: the `Access` implementation (`access.rs`).
//!
//! Credential red line (same as SMB §2.4): `password`/key material only ever
//! live in the in-memory builder and the pool's connect params; they are
//! never persisted, put into environment variables, or rendered into
//! logs/errors.

mod access;
mod pool;

use opendal::raw::{normalize_root, Service};
use opendal::{Builder, Configurator, Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

// The access/pool items are `pub(super)` (visible inside `engine::sftp_native`);
// no re-exports — `build` paths them directly.

/// Scheme string surfaced through `AccessorInfo` / `files/capabilities`.
pub(super) const SFTP_NATIVE_SCHEME: &str = "sftp-native";

/// Default SFTP TCP port.
pub(super) const SFTP_DEFAULT_PORT: u16 = 22;

/// Host-key verification strategy. Mirrors the OpenSSH CLI semantics the
/// manifest labels promise:
/// - `Strict`: the server key must match an entry in the sidecar's
///   `known_hosts` file (`<data_dir>/known_hosts`); a missing file or entry
///   rejects the dial.
/// - `Tolerate` (default): entries that exist must match (a changed key is
///   rejected), unknown hosts are accepted — accept-new semantics. Learned
///   keys are NOT persisted (the sidecar keeps no ssh state of its own).
/// - `Trust`: accept any server key.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostKeyStrategy {
    Strict,
    #[default]
    Tolerate,
    Trust,
}

impl HostKeyStrategy {
    /// Parses the manifest's `known_hosts_strategy` select values
    /// (case-insensitive; OpenDAL's `accept` alias maps to Tolerate).
    pub(super) fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "strict" => Self::Strict,
            "trust" | "accept" => Self::Trust,
            _ => Self::Tolerate,
        }
    }
}

/// Builder configuration for the native SFTP adapter (OpenDAL `Configurator`
/// shape). Field names double as the kv keys accepted by
/// `Configurator::from_iter`.
#[derive(Default, Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(default)]
#[non_exhaustive]
pub struct SftpNativeConfig {
    /// Optional sub-path inside the remote filesystem; empty = server root.
    pub root: Option<String>,
    /// `host[:port]` or `ssh://user@host[:port]`; port defaults to 22. A
    /// `user@` prefix is accepted when the explicit user fields are empty.
    pub endpoint: Option<String>,
    /// Login user (`external_config.user`).
    pub user: Option<String>,
    /// Login user alias (`external_config.username`), for form symmetry with
    /// webdav/smb.
    pub username: Option<String>,
    /// Login password (secret; memory only). Password auth falls back to
    /// keyboard-interactive automatically (PAM gateways).
    pub password: Option<String>,
    /// Private key content or path (`external_config.key`, same semantics as
    /// the OpenDAL sftp service).
    pub key: Option<String>,
    /// `Strict` / `Tolerate` / `Trust` (case-insensitive).
    pub known_hosts_strategy: Option<String>,
}

impl Configurator for SftpNativeConfig {
    type Builder = SftpNativeBuilder;

    fn into_builder(self) -> Self::Builder {
        SftpNativeBuilder { config: self }
    }
}

/// Builder for the native SFTP adapter; consumes [`SftpNativeConfig`] into
/// the [`pool`] accessor. Building is synchronous; key loading happens here
/// so a bad key fails at connect time instead of the first operation — but
/// the SSH handshake itself dials lazily on the first operation.
#[derive(Debug, Default)]
pub struct SftpNativeBuilder {
    config: SftpNativeConfig,
}

impl SftpNativeBuilder {
    /// Creates an empty builder (all options unset).
    pub fn new() -> Self {
        Self::default()
    }

    /// Optional sub-path inside the remote filesystem (`normalize_root`
    /// applied at build).
    pub fn root(mut self, value: &str) -> Self {
        self.config.root = Some(value.to_string());
        self
    }

    /// `host[:port]` or `ssh://user@host[:port]`.
    pub fn endpoint(mut self, value: &str) -> Self {
        self.config.endpoint = Some(value.to_string());
        self
    }

    /// Login user (`user` field).
    pub fn user(mut self, value: &str) -> Self {
        self.config.user = Some(value.to_string());
        self
    }

    /// Login user alias (`username` field).
    pub fn username(mut self, value: &str) -> Self {
        self.config.username = Some(value.to_string());
        self
    }

    /// Login password (secret; memory only).
    pub fn password(mut self, value: &str) -> Self {
        self.config.password = Some(value.to_string());
        self
    }

    /// Private key content or path.
    pub fn key(mut self, value: &str) -> Self {
        self.config.key = Some(value.to_string());
        self
    }

    /// `Strict` / `Tolerate` / `Trust` host-key strategy.
    pub fn known_hosts_strategy(mut self, value: &str) -> Self {
        self.config.known_hosts_strategy = Some(value.to_string());
        self
    }
}

impl Builder for SftpNativeBuilder {
    type Config = SftpNativeConfig;

    fn build(self) -> Result<impl Service> {
        let config = self.config;
        let endpoint = config.endpoint.clone().unwrap_or_default();
        let (host, port, endpoint_user) = parse_sftp_native_endpoint(&endpoint)?;
        let user = [config.user.as_deref(), config.username.as_deref(), endpoint_user.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|value| !value.is_empty())
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::ConfigInvalid,
                    "sftp-native user is required (via 'user', 'username' or user@endpoint)",
                )
            })?
            .to_string();
        let user_for_name = user.clone();

        // Exactly one credential channel: password (with keyboard-interactive
        // fallback) or a decoded private key. Both empty is a config error.
        let password = config.password.unwrap_or_default();
        let key_source = config.key.unwrap_or_default();
        let credentials = if !password.is_empty() {
            pool::SftpNativeAuth::Password(password)
        } else if !key_source.is_empty() {
            let key = load_private_key(&key_source)?;
            pool::SftpNativeAuth::Key(Arc::new(key))
        } else {
            return Err(Error::new(
                ErrorKind::ConfigInvalid,
                "sftp-native requires a password or a private key",
            ));
        };

        let strategy = HostKeyStrategy::parse(&config.known_hosts_strategy.unwrap_or_default());
        // The OpenDAL root is an optional sub-path of the remote filesystem;
        // locking semantics (`lock_to_root`) are enforced by the engine
        // policy layer.
        let root = normalize_root(config.root.as_deref().unwrap_or("/"));
        let params = pool::SftpNativeConnectParams {
            host,
            port,
            user,
            credentials,
            strategy,
            known_hosts_path: default_known_hosts_path(),
        };
        Ok(access::SftpNativeAccess::new(
            pool::SftpNativePool::new(params),
            root,
            &user_for_name,
        ))
    }
}

/// Parses the endpoint into `(host, port, endpoint_user)`. Accepts bare
/// `host[:port]` (same shape as smb/ftp) or `ssh://[user@]host[:port]`; any
/// other scheme and an empty or invalid host/port is a config error. Port
/// defaults to 22. IPv6 hosts must be bracketed in `ssh://` URLs.
pub(super) fn parse_sftp_native_endpoint(endpoint: &str) -> Result<(String, u16, Option<String>)> {
    let text = endpoint.trim();
    let (scheme_ok, body) = match text.split_once("://") {
        Some((scheme, rest)) => (
            scheme.eq_ignore_ascii_case("ssh"),
            rest.to_string(),
        ),
        None => (true, text.to_string()),
    };
    if !scheme_ok {
        return Err(Error::new(
            ErrorKind::ConfigInvalid,
            "sftp-native endpoint must be a bare host[:port] or ssh://[user@]host[:port]",
        ));
    }
    let body = body.trim_end_matches('/');
    let (authority, path) = match body.split_once('/') {
        Some((authority, path)) => (authority, path),
        None => (body, ""),
    };
    let _ = path; // a path suffix is tolerated and ignored (root comes from `root`)
    if authority.is_empty() {
        return Err(Error::new(
            ErrorKind::ConfigInvalid,
            "sftp-native endpoint host must not be empty",
        ));
    }
    // Split an optional user@ prefix before host parsing (no password-in-URL
    // support: credentials travel through connection_secrets only).
    let (endpoint_user, host_part) = match authority.rsplit_once('@') {
        Some((user, host)) if !user.is_empty() && !host.is_empty() => (Some(user), host),
        Some(_) => {
            return Err(Error::new(
                ErrorKind::ConfigInvalid,
                format!("sftp-native endpoint '{authority}' is not a valid [user@]host[:port]"),
            ))
        }
        None => (None, authority),
    };
    // Bracketed IPv6: "[::1]:22"; a bare bracketed host keeps default port.
    let (host, port) = if let Some(inner) = host_part.strip_prefix('[') {
        let (inside, rest) = inner
            .split_once(']')
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::ConfigInvalid,
                    format!("sftp-native endpoint '{host_part}' has an unterminated IPv6 bracket"),
                )
            })?
            ;
        let port = match rest.strip_prefix(':') {
            Some(port) if !port.is_empty() => port.parse::<u16>().map_err(|_| {
                Error::new(
                    ErrorKind::ConfigInvalid,
                    format!("sftp-native endpoint port '{port}' is not a valid port"),
                )
            })?,
            Some(_) => {
                return Err(Error::new(
                    ErrorKind::ConfigInvalid,
                    format!("sftp-native endpoint '{host_part}' is not a valid [host]:port"),
                ))
            }
            None => SFTP_DEFAULT_PORT,
        };
        (inside.to_string(), port)
    } else {
        match host_part.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() && !port.is_empty() => {
                let port = port.parse::<u16>().map_err(|_| {
                    Error::new(
                        ErrorKind::ConfigInvalid,
                        format!("sftp-native endpoint port '{port}' is not a valid port"),
                    )
                })?;
                (host.to_string(), port)
            }
            // Colon with an empty side (e.g. "host:" / ":22") is malformed.
            Some(_) => {
                return Err(Error::new(
                    ErrorKind::ConfigInvalid,
                    format!("sftp-native endpoint '{host_part}' is not a valid host[:port]"),
                ))
            }
            None => (host_part.to_string(), SFTP_DEFAULT_PORT),
        }
    };
    if host.is_empty() {
        return Err(Error::new(
            ErrorKind::ConfigInvalid,
            "sftp-native endpoint host must not be empty",
        ));
    }
    Ok((host, port, endpoint_user.map(str::to_string)))
}

/// Materializes the `key` field into a decoded private key. The field holds
/// either PEM/OpenSSH key content (detected by the BEGIN marker) or a path to
/// a key file (`~/` expands against HOME/USERPROFILE). Encrypted keys are
/// rejected with a clear message: the sftp-native form carries no passphrase
/// field yet (parity with the OpenDAL sftp service).
pub(super) fn load_private_key(key_source: &str) -> Result<russh::keys::PrivateKey> {
    let text = key_source.trim();
    let key_text = if text.starts_with("-----BEGIN") {
        text.to_string()
    } else {
        let path = expand_private_key_path(text);
        std::fs::read_to_string(&path).map_err(|error| {
            Error::new(
                ErrorKind::ConfigInvalid,
                format!("sftp-native failed to read private key '{}': {error}", path.display()),
            )
        })?
    };
    russh::keys::decode_secret_key(&key_text, None).map_err(|error| {
        let hint = matches!(error, russh::keys::Error::KeyIsEncrypted)
            || key_text.contains("ENCRYPTED");
        if hint {
            Error::new(
                ErrorKind::ConfigInvalid,
                "sftp-native cannot decrypt this private key (passphrase-protected keys are not supported yet)",
            )
        } else {
            Error::new(
                ErrorKind::ConfigInvalid,
                format!("sftp-native failed to decode the private key: {error}"),
            )
        }
    })
}

/// `~/`-aware path expansion (same rule as the ssh plugin's key loader).
fn expand_private_key_path(path: &str) -> PathBuf {
    let trimmed = path.trim();
    let Some(remainder) = trimmed.strip_prefix("~/").or_else(|| trimmed.strip_prefix("~\\")) else {
        return PathBuf::from(trimmed);
    };
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(remainder)
}

/// Sidecar-local known_hosts store for the Strict strategy. Lives next to the
/// plugin's other state (prefs/transfers/audit) so an operator can provision
/// it; an absent file simply rejects every Strict dial.
fn default_known_hosts_path() -> PathBuf {
    crate::store::Store::default_dir().join("known_hosts")
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendal::Operator;

    #[test]
    fn parse_sftp_native_endpoint_table() {
        let cases: Vec<(&str, (String, u16, Option<String>))> = vec![
            ("mft.local", ("mft.local".into(), 22, None)),
            ("mft.local:2222", ("mft.local".into(), 2222, None)),
            ("ssh://mft.local", ("mft.local".into(), 22, None)),
            ("ssh://mft.local:2222", ("mft.local".into(), 2222, None)),
            ("ssh://bob@mft.local:2222", ("mft.local".into(), 2222, Some("bob".into()))),
            ("bob@mft.local", ("mft.local".into(), 22, Some("bob".into()))),
            (" SSH://BOB@MFT.LOCAL/ ", ("MFT.LOCAL".into(), 22, Some("BOB".into()))),
            ("ssh://[::1]:2222", ("::1".into(), 2222, None)),
            ("[2001:db8::1]", ("2001:db8::1".into(), 22, None)),
        ];
        for (input, expected) in cases {
            assert_eq!(
                parse_sftp_native_endpoint(input).unwrap(),
                expected,
                "parse('{input}')"
            );
        }
        for bad in [
            "",
            "   ",
            "ssh://",
            "ssh:///path",
            "@mft.local",
            "bob@",
            "host:",
            ":22",
            "host:notaport",
            "ftp://mft.local",
            "http://10.0.0.1",
            "ssh://[::1",
        ] {
            assert!(parse_sftp_native_endpoint(bad).is_err(), "parse('{bad}') must fail");
        }
    }

    #[test]
    fn host_key_strategy_parsing() {
        assert_eq!(HostKeyStrategy::parse("Strict"), HostKeyStrategy::Strict);
        assert_eq!(HostKeyStrategy::parse(" strict "), HostKeyStrategy::Strict);
        assert_eq!(HostKeyStrategy::parse("Tolerate"), HostKeyStrategy::Tolerate);
        // OpenDAL's accept alias maps to the tolerant behavior.
        assert_eq!(HostKeyStrategy::parse("accept"), HostKeyStrategy::Trust);
        assert_eq!(HostKeyStrategy::parse("Trust"), HostKeyStrategy::Trust);
        // Empty / unknown falls back to the safe default.
        assert_eq!(HostKeyStrategy::parse(""), HostKeyStrategy::Tolerate);
        assert_eq!(HostKeyStrategy::parse("whatever"), HostKeyStrategy::Tolerate);
    }

    #[test]
    fn sftp_native_config_kv_parsing() {
        use opendal::Configurator as _;
        let config = SftpNativeConfig::from_iter(vec![
            ("endpoint".to_string(), "mft.local:2222".to_string()),
            ("user".to_string(), "bob".to_string()),
            ("password".to_string(), "secret".to_string()),
            ("root".to_string(), "/pub".to_string()),
            ("known_hosts_strategy".to_string(), "Strict".to_string()),
            // Unknown keys are ignored, same as the via_iter services.
            ("whatever".to_string(), "x".to_string()),
        ])
        .unwrap();
        assert_eq!(config.endpoint.as_deref(), Some("mft.local:2222"));
        assert_eq!(config.user.as_deref(), Some("bob"));
        assert_eq!(config.password.as_deref(), Some("secret"));
        assert_eq!(config.root.as_deref(), Some("/pub"));
        assert_eq!(config.known_hosts_strategy.as_deref(), Some("Strict"));

        let empty = SftpNativeConfig::from_iter(Vec::<(String, String)>::new()).unwrap();
        assert_eq!(empty.endpoint, None, "endpoint absence surfaces at build");
    }

    #[test]
    fn sftp_native_builder_builds_operator_and_declares_capabilities() {
        // Building must not touch the network: the SSH handshake dials lazily
        // on the first operation, so Operator::finish() succeeds offline.
        let builder = SftpNativeBuilder::new()
            .endpoint("mft.local:2222")
            .user("bob")
            .password("secret")
            .root("/pub")
            .known_hosts_strategy("Trust");
        let operator = Operator::new(builder).unwrap();

        let info = operator.info();
        assert_eq!(info.scheme(), SFTP_NATIVE_SCHEME);
        assert_eq!(info.root(), "/pub/", "normalized root");
        assert_eq!(info.name(), "bob");

        let capability = info.capability();
        assert!(capability.stat && capability.read && capability.write);
        assert!(capability.create_dir && capability.delete);
        assert!(capability.list && capability.rename);
        assert!(
            capability.delete_with_recursive && capability.list_with_recursive,
            "recursive delete/purge and recursive list are adapter-implemented"
        );
        // copy=false → engine degrades same-connection copies to read→write
        // jobs; presign=false → files/publicLink reports unsupported.
        assert!(!capability.copy);
        assert!(!capability.presign_read);
    }

    #[test]
    fn sftp_native_builder_requires_user() {
        let error = SftpNativeBuilder::new()
            .endpoint("mft.local")
            .password("secret")
            .build()
            .expect_err("missing user must fail");
        assert_eq!(error.kind(), ErrorKind::ConfigInvalid);
    }

    #[test]
    fn sftp_native_builder_requires_credentials() {
        let error = SftpNativeBuilder::new()
            .endpoint("bob@mft.local")
            .build()
            .expect_err("missing password and key must fail");
        assert_eq!(error.kind(), ErrorKind::ConfigInvalid);
    }

    #[test]
    fn sftp_native_builder_rejects_bad_endpoint_and_schemes() {
        let error = SftpNativeBuilder::new()
            .endpoint("http://mft.local")
            .user("bob")
            .password("secret")
            .build()
            .expect_err("http scheme must fail");
        assert_eq!(error.kind(), ErrorKind::ConfigInvalid);

        let error = SftpNativeBuilder::new()
            .endpoint("")
            .user("bob")
            .password("secret")
            .build()
            .expect_err("empty endpoint must fail");
        assert_eq!(error.kind(), ErrorKind::ConfigInvalid);
    }

    #[test]
    fn sftp_native_user_precedence() {
        // user > username > endpoint user@.
        let build = |endpoint: &str, user: &str, username: &str| {
            let mut builder = SftpNativeBuilder::new().endpoint(endpoint).password("secret");
            if !user.is_empty() {
                builder = builder.user(user);
            }
            if !username.is_empty() {
                builder = builder.username(username);
            }
            Operator::new(builder).unwrap().info().name().to_string()
        };
        assert_eq!(build("a@h", "u1", "u2"), "u1");
        assert_eq!(build("a@h", "", "u2"), "u2");
        assert_eq!(build("a@h", "", ""), "a");
    }

    #[test]
    fn sftp_native_load_private_key_rejects_unknown_content() {
        let error = load_private_key("not a key").expect_err("garbage must fail");
        assert_eq!(error.kind(), ErrorKind::ConfigInvalid);
        let error = load_private_key("~/definitely-missing-key").expect_err("missing path must fail");
        assert_eq!(error.kind(), ErrorKind::ConfigInvalid);
    }
}
