//! OpenDAL custom Access adapter for SMB2/3 shares (F5-SMB).
//!
//! OpenDAL has no `services-smb`, so `smb2 =0.20.1` (pure-Rust SMB2/3 client)
//! is wrapped behind the `opendal::raw::Access` trait here and behaves as a
//! sixth quick protocol for the engine (IMPL_PLAN_SMB §1):
//!
//! ```
//! StoredConnection(protocol="smb")
//!   → engine::build_operator → Operator::new(SmbBuilder)?.finish()
//!   → connection table / gates / audit / capabilities — all protocol-agnostic
//! ```
//!
//! Layout:
//! - [`SmbBuilder`] / [`SmbConfig`]: Builder + Configurator path; constructed
//!   directly via `Operator::new(builder)?.finish()` (not the `via_iter`
//!   registry — "smb" is not an OpenDAL scheme).
//! - [`SmbAccess`]: the `Access` implementation (`access.rs`).
//! - [`SmbPool`]: lazy connection + client lifecycle (`pool.rs`).
//!
//! Credential red line (§2.4): `username`/`password`/`domain` only ever live
//! in the in-memory builder and the pool's connect params; they are never
//! persisted, put into environment variables, or rendered into logs/errors.

mod access;
mod pool;

use opendal::raw::{normalize_root, Service};
use opendal::{Builder, Configurator, Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};

// The access/pool items are `pub(super)` (visible inside `engine::smb`); no
// re-exports — `build` paths them directly.

/// Scheme string surfaced through `AccessorInfo` / `files/capabilities`.
pub(super) const SMB_SCHEME: &str = "smb";

/// Builder configuration for the SMB adapter (OpenDAL `Configurator` shape).
/// Field names double as the kv keys accepted by `Configurator::from_iter`.
#[derive(Default, Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(default)]
#[non_exhaustive]
pub struct SmbConfig {
    /// Optional sub-path inside the share; empty = share root.
    pub root: Option<String>,
    /// `host[:port]` or `smb://host[:port]`; port defaults to 445.
    pub endpoint: Option<String>,
    /// Optional share name (tree connect target). When absent, the operator
    /// lists server shares at `/` and uses the first path component as the
    /// selected share.
    pub share: Option<String>,
    /// NTLM username; empty = guest.
    pub username: Option<String>,
    /// NTLM password (secret; memory only).
    pub password: Option<String>,
    /// NTLM domain / workgroup; optional.
    pub domain: Option<String>,
}

impl Configurator for SmbConfig {
    type Builder = SmbBuilder;

    fn into_builder(self) -> Self::Builder {
        SmbBuilder { config: self }
    }
}

/// Builder for the SMB adapter; consumes [`SmbConfig`] into an
/// [`SmbAccess`](access::SmbAccess). Building is synchronous and does not
/// touch the network — the SMB handshake dials lazily on the first operation
/// (IMPL_PLAN_SMB §2.1).
#[derive(Debug, Default)]
pub struct SmbBuilder {
    config: SmbConfig,
}

impl SmbBuilder {
    /// Creates an empty builder (all options unset).
    pub fn new() -> Self {
        Self::default()
    }

    /// Optional sub-path inside the share (`normalize_root` applied at build).
    pub fn root(mut self, value: &str) -> Self {
        self.config.root = Some(value.to_string());
        self
    }

    /// `host[:port]` or `smb://host[:port]`.
    pub fn endpoint(mut self, value: &str) -> Self {
        self.config.endpoint = Some(value.to_string());
        self
    }

    /// Optional share name (tree connect target); empty enables server-level
    /// browsing and path-based share selection.
    pub fn share(mut self, value: &str) -> Self {
        self.config.share = Some(value.to_string());
        self
    }

    /// NTLM username; empty = guest.
    pub fn username(mut self, value: &str) -> Self {
        self.config.username = Some(value.to_string());
        self
    }

    /// NTLM password (secret; memory only).
    pub fn password(mut self, value: &str) -> Self {
        self.config.password = Some(value.to_string());
        self
    }

    /// NTLM domain / workgroup; optional.
    pub fn domain(mut self, value: &str) -> Self {
        self.config.domain = Some(value.to_string());
        self
    }
}

impl Builder for SmbBuilder {
    type Config = SmbConfig;

    fn build(self) -> Result<impl Service> {
        let config = self.config;
        let endpoint = config.endpoint.clone().unwrap_or_default();
        let (host, port) = parse_smb_endpoint(&endpoint)?;
        let (share, root) =
            split_share_and_root(&config.share.unwrap_or_default(), config.root.as_deref());
        if share.is_empty() && root != "/" {
            return Err(Error::new(
                ErrorKind::ConfigInvalid,
                "smb root requires a selected share; leave root empty when browsing server shares",
            ));
        }
        let share = (!share.is_empty()).then_some(share);
        let params = pool::SmbConnectParams {
            host,
            port,
            share: share.clone(),
            username: config.username.unwrap_or_default(),
            password: config.password.unwrap_or_default(),
            domain: config.domain.unwrap_or_default(),
        };
        Ok(access::SmbAccess::new(
            pool::SmbPool::new(params),
            root,
            share.as_deref(),
        ))
    }
}

/// Splits the configured share into the tree connect target and an OpenDAL
/// root prefix (real-machine regression, jobnote server 2026-09: tree
/// connect to a subdirectory of a share is rejected with
/// `STATUS_BAD_NETWORK_NAME`, so an Icewind/smbclient-style nested share
/// value like `Projects/UIPath/JobNotes/InputFile` must tree connect to
/// `Projects` and serve `/UIPath/JobNotes/InputFile` as the operator root).
/// Both `/` and `\` act as separators; empty segments collapse; the user
/// root (if any) is joined UNDER the nested prefix. Locking semantics
/// (`lock_to_root`) are enforced by the engine policy layer as before.
///
/// The form keeps `share` and `root` independently optional, so the
/// combination "share empty, root filled" is submittable; instead of a
/// dead-end build error it follows UNC semantics (`//host/share/path`) and
/// lifts the root's first segment to the share (root `/media/data` connects
/// to share `media` rooted at `/data`). Only a root of `/` (or empty) keeps
/// share-discovery mode.
fn split_share_and_root(share_value: &str, user_root: Option<&str>) -> (String, String) {
    fn segments(text: &str) -> impl Iterator<Item = &str> {
        text.split(['/', '\\']).map(str::trim).filter(|s| !s.is_empty())
    }

    let mut share_segments = segments(share_value);
    let share = share_segments.next().unwrap_or_default().to_string();
    let nested: Vec<&str> = share_segments.collect();

    if share.is_empty() {
        let root_segments: Vec<&str> = user_root
            .map(|root| segments(root).collect())
            .unwrap_or_default();
        return match root_segments.split_first() {
            Some((&first, rest)) => {
                let root = normalize_root(&format!("/{}", rest.join("/")));
                (first.to_string(), root)
            }
            None => (String::new(), "/".to_string()),
        };
    }

    let root = if nested.is_empty() {
        normalize_root(user_root.unwrap_or("/"))
    } else {
        normalize_root(&format!(
            "/{}/{}",
            nested.join("/"),
            user_root.unwrap_or("/").trim_matches('/')
        ))
    };
    (share, root)
}

/// Parses the endpoint into `(host, port)`. Accepts bare `host[:port]` (same
/// shape as the ftp protocol) or `smb://host[:port]`; any other scheme and an
/// empty or invalid host/port is a config error (§2.3). Port defaults to 445.
pub(super) fn parse_smb_endpoint(endpoint: &str) -> Result<(String, u16)> {
    let text = endpoint.trim();
    let body = match text.split_once("://") {
        Some((scheme, rest)) => {
            if !scheme.eq_ignore_ascii_case("smb") {
                return Err(Error::new(
                    ErrorKind::ConfigInvalid,
                    format!(
                        "smb endpoint must be a bare host[:port] or smb://host[:port] \
                         (got scheme '{scheme}://')"
                    ),
                ));
            }
            rest
        }
        None => text,
    };
    let body = body.trim_end_matches('/');
    // Only the authority (before any `/`) may carry host[:port]; a leading
    // `/` means an empty host (e.g. `smb:///share`).
    let authority = body.split('/').next().unwrap_or_default();
    if authority.is_empty() {
        return Err(Error::new(
            ErrorKind::ConfigInvalid,
            "smb endpoint host must not be empty",
        ));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !port.is_empty() => {
            let port = port.parse::<u16>().map_err(|_| {
                Error::new(
                    ErrorKind::ConfigInvalid,
                    format!("smb endpoint port '{port}' is not a valid port"),
                )
            })?;
            (host, port)
        }
        // Colon with an empty side (e.g. "host:" / ":445") is malformed.
        Some(_) => {
            return Err(Error::new(
                ErrorKind::ConfigInvalid,
                format!("smb endpoint '{authority}' is not a valid host[:port]"),
            ))
        }
        None => (authority, SMB_DEFAULT_PORT),
    };
    Ok((host.to_string(), port))
}

/// Default SMB/CIFS TCP port.
const SMB_DEFAULT_PORT: u16 = 445;

#[cfg(test)]
mod tests {
    use super::*;
    use opendal::raw::build_abs_path;
    use opendal::Operator;

    #[test]
    fn parse_smb_endpoint_table() {
        let cases: Vec<(&str, (String, u16))> = vec![
            ("nas.local", ("nas.local".to_string(), 445)),
            ("nas.local:445", ("nas.local".to_string(), 445)),
            ("nas.local:1445", ("nas.local".to_string(), 1445)),
            ("smb://nas.local", ("nas.local".to_string(), 445)),
            ("smb://nas.local:1445", ("nas.local".to_string(), 1445)),
            ("  smb://nas.local/  ", ("nas.local".to_string(), 445)),
            ("SMB://NAS.LOCAL", ("NAS.LOCAL".to_string(), 445)),
        ];
        for (input, expected) in cases {
            assert_eq!(
                parse_smb_endpoint(input).unwrap(),
                expected,
                "parse('{input}')"
            );
        }
        for bad in [
            "",
            "   ",
            "smb://",
            "smb:///share",
            "host:",
            ":445",
            "host:notaport",
        ] {
            assert!(parse_smb_endpoint(bad).is_err(), "parse('{bad}') must fail");
        }
        // Other schemes are rejected at parse time as well.
        for bad in ["file:///etc/passwd", "http://10.0.0.1", "ftp://x"] {
            let error = parse_smb_endpoint(bad).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::ConfigInvalid, "{bad}");
        }
    }

    #[test]
    fn smb_config_kv_parsing() {
        use opendal::Configurator as _;
        let config = SmbConfig::from_iter(vec![
            ("endpoint".to_string(), "nas.local:445".to_string()),
            ("share".to_string(), "media".to_string()),
            ("username".to_string(), "alice".to_string()),
            ("password".to_string(), "wonderland".to_string()),
            ("domain".to_string(), "WORKGROUP".to_string()),
            ("root".to_string(), "/archive".to_string()),
            // Unknown keys are ignored, same as the via_iter services.
            ("whatever".to_string(), "x".to_string()),
        ])
        .unwrap();
        assert_eq!(config.endpoint.as_deref(), Some("nas.local:445"));
        assert_eq!(config.share.as_deref(), Some("media"));
        assert_eq!(config.username.as_deref(), Some("alice"));
        assert_eq!(config.password.as_deref(), Some("wonderland"));
        assert_eq!(config.domain.as_deref(), Some("WORKGROUP"));
        assert_eq!(config.root.as_deref(), Some("/archive"));

        let empty = SmbConfig::from_iter(Vec::<(String, String)>::new()).unwrap();
        assert_eq!(empty.share, None, "share absence surfaces at build");
    }

    #[test]
    fn smb_builder_builds_operator_and_declares_capabilities() {
        // Building must not touch the network: the SMB handshake dials lazily
        // on the first operation, so Operator::finish() succeeds offline.
        let builder = SmbBuilder::new()
            .endpoint("nas.local:445")
            .share("media")
            .username("alice")
            .password("wonderland")
            .domain("WORKGROUP")
            .root("/archive");
        let operator = Operator::new(builder).unwrap();

        let info = operator.info();
        assert_eq!(info.scheme(), SMB_SCHEME);
        assert_eq!(info.root(), "/archive/", "normalized root");
        assert_eq!(info.name(), "media");

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
    fn smb_builder_allows_server_level_browsing_without_share() {
        let operator = Operator::new(SmbBuilder::new().endpoint("nas.local")).unwrap();
        assert_eq!(operator.info().scheme(), SMB_SCHEME);
        assert_eq!(operator.info().root(), "/");
    }

    #[test]
    fn smb_builder_lifts_filled_root_first_segment_to_share() {
        // The form keeps share and root independently optional; a filled root
        // plus share discovery follows UNC semantics instead of a dead-end
        // build error (form-combination fix, see split_share_and_root).
        let operator = Operator::new(
            SmbBuilder::new()
                .endpoint("nas.local")
                .root("/data/archive"),
        )
        .unwrap();
        assert_eq!(operator.info().name(), "data", "root head becomes the share");
        assert_eq!(operator.info().root(), "/archive/", "root tail becomes the root");

        // A plain `/` root keeps share-discovery mode.
        let discovery = Operator::new(SmbBuilder::new().endpoint("nas.local").root("/"))
            .unwrap();
        assert_eq!(discovery.info().root(), "/");
    }

    #[test]
    fn smb_builder_rejects_bad_endpoint() {
        let error = SmbBuilder::new()
            .endpoint("http://nas.local")
            .share("media")
            .build()
            .expect_err("http scheme must fail");
        assert_eq!(error.kind(), ErrorKind::ConfigInvalid);

        let error = SmbBuilder::new()
            .endpoint("")
            .share("media")
            .build()
            .expect_err("empty endpoint must fail");
        assert_eq!(error.kind(), ErrorKind::ConfigInvalid);
    }

    #[test]
    fn split_share_and_root_table() {
        // Plain shares keep the configured root unchanged.
        let cases: Vec<(&str, Option<&str>, (String, String))> = vec![
            ("media", None, ("media".into(), "/".into())),
            (
                "media",
                Some("/archive"),
                ("media".into(), "/archive/".into()),
            ),
            ("  media  ", None, ("media".into(), "/".into())),
            // Trailing/leading separators and backslash notation all collapse
            // onto the plain share.
            ("media/", None, ("media".into(), "/".into())),
            ("/media", None, ("media".into(), "/".into())),
            (
                r"Projects\UIPath",
                None,
                ("Projects".into(), "/UIPath/".into()),
            ),
            // Nested shares: first segment tree connects, the rest becomes
            // the root prefix (jobnote real-machine shape).
            (
                "Projects/UIPath/JobNotes/InputFile",
                None,
                ("Projects".into(), "/UIPath/JobNotes/InputFile/".into()),
            ),
            (
                "Projects/UIPath/JobNotes/InputFile",
                Some("/sub"),
                ("Projects".into(), "/UIPath/JobNotes/InputFile/sub/".into()),
            ),
            (
                "Projects//UIPath",
                None,
                ("Projects".into(), "/UIPath/".into()),
            ),
            ("Projects/", Some("x"), ("Projects".into(), "/x/".into())),
        ];
        for (share, root, expected) in cases {
            assert_eq!(
                split_share_and_root(share, root),
                expected,
                "split('{share}', {root:?})"
            );
        }
        // Separator-only values leave an empty share; with an empty root that
        // keeps share-discovery mode (build sees share=None, root "/").
        for bad in ["", "   ", "/", r"\\", "//"] {
            let (share, root) = split_share_and_root(bad, None);
            assert!(
                share.is_empty() && root == "/",
                "split('{bad}') must stay in discovery mode, got ({share:?}, {root:?})"
            );
        }
        // UNC-lift combinations: a filled root with no share names the share
        // with its first segment and serves the rest as the root.
        let lifts: Vec<(&str, Option<&str>, (String, String))> = vec![
            (  "", Some("/data"), ("data".into(), "/".into())),
            ("", Some("/data/archive"), ("data".into(), "/archive/".into())),
            ("", Some("data"), ("data".into(), "/".into())),
            ("", Some("  /  "), ("".into(), "/".into())),
        ];
        for (share, root_value, expected) in lifts {
            assert_eq!(
                split_share_and_root(share, root_value),
                expected,
                "lift('{share}', {root_value:?})"
            );
        }
    }

    #[test]
    fn smb_share_paths_normalize_through_root() {
        // smb_path is private to the access module; verify the root+path
        // algebra it relies on instead. OpenDAL-relative inputs never start
        // with `/`; the share-relative output keeps no slashes at the edges.
        let root = normalize_root("/archive");
        assert_eq!(root, "/archive/");
        // Root listing: "" (the operator normalizes "/" down to "").
        assert_eq!(build_abs_path(&root, "").trim_matches('/'), "archive");
        assert_eq!(
            build_abs_path(&root, "docs/a.txt").trim_matches('/'),
            "archive/docs/a.txt"
        );
        assert_eq!(
            build_abs_path(&root, "docs/").trim_matches('/'),
            "archive/docs",
            "directory paths keep their trailing slash pre-trim"
        );
        let share_root = normalize_root("/");
        assert_eq!(
            build_abs_path(&share_root, "").trim_matches('/'),
            "",
            "share root maps to the empty SMB path"
        );
    }
}
