//! Connection-level policy gates and egress URL validation for the files
//! plugin.
//!
//! Three orthogonal policy surfaces live here (implementation plan §9, §6.1):
//!
//! 1. **Path whitelist** — every user-supplied path is sanitized and, when
//!    `lock_to_root` is set, required to stay inside the configured `root`.
//!    Traversal (`..`) that escapes the visible space is always rejected,
//!    independent of `lock_to_root`.
//! 2. **Write gates** — `read_only` rejects every mutating operation;
//!    `allow_delete` additionally rejects `delete`/`rmdir`/`purge` (and the
//!    implicit source deletion of `rename`/`move`).
//! 3. **Egress URL validation** — HTTP-class endpoints must be `http`/`https`
//!    with a well-formed host. Loopback/private/reserved addresses are
//!    rejected for every URL that is *not* directly user-configured
//!    (presign results, redirect targets, directory-derived URLs). Explicitly
//!    user-configured endpoints stay trusted (the plugin's core scenario is
//!    connecting to an intranet MinIO/NAS) but are flagged for audit.
//!
//! All functions return plain `String` errors; `main.rs` wraps them into
//! plugin error code `-32000`.

// Wired into the crate from `engine/ops.rs` (main.rs is frozen), so several
// public helpers are consumed only by tests / the consolidated call sites.
#![allow(dead_code)]

use serde_json::Value;

/// Redirect following is disabled by default (§6.1 rule 4). The single
/// source for the preview/write caps lives in `model`
/// (`MAX_PREVIEW_BYTES` / `MAX_INLINE_WRITE_BYTES`).
pub const FOLLOW_REDIRECTS_DEFAULT: bool = false;

// ---------------------------------------------------------------------------
// Path policy
// ---------------------------------------------------------------------------

/// Connection-scoped path policy: root binding and write gates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPolicy {
    /// Canonical absolute root (leading `/`, no trailing slash except the
    /// bare root `/`). Empty/unset input normalizes to `/`.
    pub root: String,
    /// When true, absolute-form paths must stay inside `root`.
    pub lock_to_root: bool,
    /// When true, every mutating operation is rejected.
    pub read_only: bool,
    /// When false, delete/rmdir/purge (and rename's implicit delete) are
    /// rejected.
    pub allow_delete: bool,
}

/// A path that passed sanitization, in both representations the engine needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    /// Canonical absolute path within the backend-visible space (leading
    /// `/`, no trailing slash; `/` denotes the root itself).
    pub absolute: String,
    /// Operator-relative path handed to OpenDAL (`""` denotes the root).
    pub relative: String,
}

impl PathPolicy {
    /// Builds the policy from a connection `external_config` map. Missing
    /// fields fall back to the manifest defaults (`root` empty, gates off).
    pub fn from_config(config: &serde_json::Map<String, Value>) -> Self {
        let root = config
            .get("root")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(canonical_root)
            .unwrap_or_else(|| "/".to_string());
        Self {
            root,
            lock_to_root: config
                .get("lock_to_root")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            read_only: config
                .get("read_only")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            allow_delete: config
                .get("allow_delete")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        }
    }

    /// A permissive policy used by tests and internal transports.
    pub fn permissive() -> Self {
        Self {
            root: "/".to_string(),
            lock_to_root: false,
            read_only: false,
            allow_delete: true,
        }
    }

    /// Builds the policy from explicit parts — the bridge used by the ops
    /// layer (`engine::ops::Gate`) so gate and policy share one implementation.
    pub fn from_parts(
        root: &str,
        lock_to_root: bool,
        read_only: bool,
        allow_delete: bool,
    ) -> Self {
        Self {
            root: canonical_root(root),
            lock_to_root,
            read_only,
            allow_delete,
        }
    }

    /// Builds the policy from a stored connection record (post-lifecycle).
    pub fn from_connection(connection: &crate::model::StoredConnection) -> Self {
        Self {
            root: canonical_root(&connection.root),
            lock_to_root: connection.lock_to_root,
            read_only: connection.read_only,
            allow_delete: connection.allow_delete,
        }
    }

    /// Sanitizes a user path and resolves it against the configured root.
    ///
    /// Input forms:
    /// - `""` or `"/"` — the root itself;
    /// - `"a/b"` — relative to the configured root;
    /// - `"/a/b"` — absolute within the backend-visible space.
    ///
    /// When `lock_to_root` is set and a root is configured, absolute-form
    /// paths must equal the root or lie beneath it. Rejected: NUL/control
    /// characters, backslashes (ambiguous across fs backends), and any `..`
    /// that escapes the visible space.
    pub fn resolve(&self, path: &str) -> Result<ResolvedPath, String> {
        let sanitized = sanitize(path)?;
        // Inputs without a leading slash are relative to the configured root
        // (doc example: "sub/f" with root "/mnt/nas" → "/mnt/nas/sub/f");
        // absolute inputs are taken as-is within the backend-visible space.
        let relative_input = !path.starts_with('/');
        let absolute = if sanitized == "/" {
            // "" or "/" denotes the root itself.
            self.root.clone()
        } else if relative_input && self.root != "/" {
            format!("{}{}", self.root, sanitized)
        } else {
            sanitized
        };

        if self.lock_to_root && self.root != "/" {
            let under_root = absolute == self.root
                || absolute.starts_with(&format!("{}/", self.root));
            if !under_root {
                return Err(format!(
                    "path '{path}' is outside the locked connection root '{}'",
                    self.root
                ));
            }
        }

        let relative = if self.root == "/" {
            absolute.trim_start_matches('/').to_string()
        } else if absolute == self.root {
            String::new()
        } else if let Some(rest) = absolute.strip_prefix(&format!("{}/", self.root)) {
            rest.to_string()
        } else {
            // Only reachable with lock_to_root=false: physically confined by
            // the Operator root configured in the engine.
            absolute.trim_start_matches('/').to_string()
        };

        Ok(ResolvedPath { absolute, relative })
    }

    /// Read-side check: path whitelist only.
    pub fn check_read(&self, path: &str) -> Result<ResolvedPath, String> {
        self.resolve(path)
    }

    /// Write-side check: path whitelist + `read_only` gate. Applies to
    /// `write`, `mkdir`, and copy/move targets.
    pub fn check_write(&self, path: &str) -> Result<ResolvedPath, String> {
        let resolved = self.resolve(path)?;
        if self.read_only {
            return Err("connection is read-only: write operations are rejected".to_string());
        }
        Ok(resolved)
    }

    /// Delete-side check: path whitelist + `read_only` + `allow_delete`.
    /// Applies to `delete` and `rmdir`.
    pub fn check_delete(&self, path: &str) -> Result<ResolvedPath, String> {
        let resolved = self.resolve(path)?;
        self.ensure_delete_allowed("delete")?;
        Ok(resolved)
    }

    /// Purge check: delete gates plus the hard refusal to purge the root
    /// itself (`""`, `"/"`, or the configured root).
    pub fn check_purge(&self, path: &str) -> Result<ResolvedPath, String> {
        let resolved = self.resolve(path)?;
        if resolved.absolute == "/" || resolved.absolute == self.root {
            return Err(format!(
                "refusing to purge the connection root '{}': purge requires a subdirectory",
                resolved.absolute
            ));
        }
        self.ensure_delete_allowed("purge")?;
        Ok(resolved)
    }

    /// Copy check: readable source, writable target. No delete gate — the
    /// source survives a copy.
    pub fn check_copy(
        &self,
        source: &str,
        target: &str,
    ) -> Result<(ResolvedPath, ResolvedPath), String> {
        let source = self.resolve(source)?;
        let target = self.check_write(target)?;
        Ok((source, target))
    }

    /// Rename/move check: write gate on the target plus the delete gate on
    /// the implicit source deletion.
    pub fn check_rename(
        &self,
        source: &str,
        target: &str,
    ) -> Result<(ResolvedPath, ResolvedPath), String> {
        let source = self.resolve(source)?;
        let target = self.check_write(target)?;
        self.ensure_delete_allowed("rename/move")?;
        Ok((source, target))
    }

    fn ensure_delete_allowed(&self, operation: &str) -> Result<(), String> {
        if self.read_only {
            return Err(format!(
                "connection is read-only: {operation} operations are rejected"
            ));
        }
        if !self.allow_delete {
            return Err(format!(
                "connection forbids deletion (allow_delete=false): {operation} is rejected"
            ));
        }
        Ok(())
    }

    /// Public form of the delete gate, for composed operations whose source
    /// and target live on different connections (e.g. cross-connection move
    /// deletes on the source policy while writing under the target policy).
    pub fn check_delete_allowed(&self, operation: &str) -> Result<(), String> {
        self.ensure_delete_allowed(operation)
    }
}

/// Normalizes a configured root into canonical absolute form.
fn canonical_root(root: &str) -> String {
    let trimmed = root.trim_end_matches('/');
    let canonical = sanitize(trimmed).unwrap_or_else(|_| "/".to_string());
    if canonical.is_empty() {
        "/".to_string()
    } else {
        canonical
    }
}

/// Sanitizes a raw path into canonical absolute form (`/a/b`, `/` for root).
/// Public so the engine ops layer can re-check request paths as a second
/// line of defense, independent of the gate wiring in `main.rs`.
pub fn sanitize_path(path: &str) -> Result<String, String> {
    sanitize(path)
}

/// Sanitizes a raw path into canonical absolute form (`/a/b`, `/` for root).
fn sanitize(path: &str) -> Result<String, String> {
    if path.chars().any(|ch| ch == '\\' || ch.is_control()) {
        return Err(format!(
            "path '{path}' contains backslashes or control characters"
        ));
    }
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.pop().is_none() {
                    return Err(format!("path '{path}' escapes the connection root"));
                }
            }
            other => segments.push(other),
        }
    }
    if segments.is_empty() {
        Ok("/".to_string())
    } else {
        Ok(format!("/{}", segments.join("/")))
    }
}

// ---------------------------------------------------------------------------
// Egress URL validation (§6.1)
// ---------------------------------------------------------------------------

/// Where a URL came from. Only [`UrlOrigin::UserConfigured`] URLs may target
/// intranet addresses; everything else (presign results, redirect targets,
/// directory-derived URLs) is validated strictly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlOrigin {
    /// Typed by the user into the connection form; trusted but audit-flagged
    /// when it points at a non-public address.
    UserConfigured,
    /// Derived at runtime (presign output, redirect target, listing URL);
    /// loopback/private/reserved targets are rejected.
    Derived,
}

/// Address classification driving the derived-URL decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostClass {
    Loopback,
    Private,
    Reserved,
    Public,
}

impl HostClass {
    /// True when the address must be rejected for derived-origin URLs.
    pub fn is_restricted(self) -> bool {
        !matches!(self, HostClass::Public)
    }
}

/// Outcome of a passed egress check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EgressVerdict {
    /// Set when a user-configured endpoint points at a restricted address;
    /// audit pipelines must emit a marker for such connections.
    pub audit_flag: bool,
}

/// Validates an outbound URL according to §6.1:
///
/// 1. scheme must be `http`/`https` (case-insensitive; other schemes fail);
/// 2. the host must be structurally valid (dotted-quad IPv4, bracketed IPv6,
///    or an ASCII domain, with an optional `:port`);
/// 3. for [`UrlOrigin::Derived`], loopback/private/reserved hosts are
///    rejected; for [`UrlOrigin::UserConfigured`] they pass with an audit
///    flag;
/// 4. userinfo (`user@host`) is rejected for derived URLs so that
///    `http://expected.example@127.0.0.1/`-style confusion cannot smuggle a
///    host past review.
pub fn validate_egress_url(url: &str, origin: UrlOrigin) -> Result<EgressVerdict, String> {
    let (scheme, rest) = split_scheme(url)?;
    match scheme.as_str() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "egress URL scheme '{other}://' is not allowed (only http/https)"
            ))
        }
    }

    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .ok_or_else(|| format!("egress URL '{url}' has no host"))?;

    let (userinfo, host_port) = match authority.rfind('@') {
        Some(index) => (Some(&authority[..index]), &authority[index + 1..]),
        None => (None, authority),
    };
    if userinfo.is_some() && origin == UrlOrigin::Derived {
        return Err(format!(
            "egress URL '{url}' carries userinfo, which is not allowed for derived URLs"
        ));
    }

    let (host, port) = split_host_port(host_port)?;
    let class = classify_host(host)?;
    let _ = port;

    if class.is_restricted() {
        return match origin {
            UrlOrigin::UserConfigured => Ok(EgressVerdict { audit_flag: true }),
            UrlOrigin::Derived => Err(format!(
                "egress URL '{url}' targets a restricted ({class:?}) host '{host}', which is rejected for non-user-configured URLs"
            )),
        };
    }
    Ok(EgressVerdict { audit_flag: false })
}

/// Validates a redirect target. Redirect targets are never trusted — even
/// when the original request went to a user-configured endpoint, the target
/// is treated as derived and must pass the strict checks. Callers must gate
/// the actual follow on their configured redirect policy (default off,
/// [`FOLLOW_REDIRECTS_DEFAULT`]).
pub fn validate_redirect_target(url: &str) -> Result<EgressVerdict, String> {
    validate_egress_url(url, UrlOrigin::Derived)
}

fn split_scheme(url: &str) -> Result<(String, &str), String> {
    let index = url
        .find("://")
        .ok_or_else(|| format!("egress URL '{url}' is missing a scheme"))?;
    let scheme = url[..index].to_ascii_lowercase();
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '+' || ch == '-' || ch == '.')
    {
        return Err(format!("egress URL '{url}' has a malformed scheme"));
    }
    Ok((scheme, &url[index + 3..]))
}

fn split_host_port(host_port: &str) -> Result<(&str, Option<u16>), String> {
    if host_port.starts_with('[') {
        let close = host_port.find(']').ok_or_else(|| {
            format!("egress URL host '{host_port}' has an unterminated IPv6 bracket")
        })?;
        let host = &host_port[1..close];
        let tail = &host_port[close + 1..];
        let port = parse_port(tail.strip_prefix(':'), host_port)?;
        return Ok((host, port));
    }
    match host_port.rfind(':') {
        Some(index) => {
            let host = &host_port[..index];
            let port = parse_port(Some(&host_port[index + 1..]), host_port)?;
            Ok((host, port))
        }
        None => Ok((host_port, None)),
    }
}

fn parse_port(raw: Option<&str>, context: &str) -> Result<Option<u16>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    if !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("egress URL '{context}' has a non-numeric port"));
    }
    let value: u32 = raw
        .parse()
        .map_err(|_| format!("egress URL '{context}' has an out-of-range port"))?;
    u16::try_from(value)
        .ok()
        .filter(|port| *port > 0)
        .map(Some)
        .ok_or_else(|| format!("egress URL '{context}' has an invalid port (must be 1-65535)"))
}

/// Classifies a host string. Format legality (§6.1 rule 2) is enforced here:
/// dotted-quad IPv4, bracketed IPv6 (already unbracketed), or ASCII domain.
fn classify_host(host: &str) -> Result<HostClass, String> {
    if host.is_empty() {
        return Err("egress URL has an empty host".to_string());
    }
    if !host.is_ascii() {
        return Err(format!(
            "egress URL host '{host}' contains non-ASCII characters"
        ));
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.contains(':') {
        return classify_ipv6(host);
    }
    let lowered = host.to_ascii_lowercase();
    if lowered.split('.').any(|label| label.starts_with("0x")) {
        // Hex IP spellings (0x7f.0.0.1, 0x7f000001) resolve to loopback on
        // some resolvers — reject as an ambiguous numeric host.
        return Err(format!(
            "egress URL host '{host}' uses an ambiguous hexadecimal numeric spelling"
        ));
    }
    if looks_like_ipv4(host) {
        let octets = parse_ipv4(host)?;
        return Ok(classify_ipv4(&octets));
    }
    if host.bytes().all(|byte| byte.is_ascii_digit()) {
        // Bare integer ("2130706433") is a classic SSRF spelling of loopback
        // and never a valid domain.
        return Err(format!(
            "egress URL host '{host}' is a bare integer, not a valid domain"
        ));
    }
    validate_domain(host)?;
    if lowered == "localhost" || lowered.ends_with(".localhost") {
        return Ok(HostClass::Loopback);
    }
    if lowered.ends_with(".local")
        || lowered.ends_with(".internal")
        || lowered.ends_with(".localdomain")
    {
        return Ok(HostClass::Private);
    }
    Ok(HostClass::Public)
}

fn looks_like_ipv4(host: &str) -> bool {
    host.contains('.')
        && host.bytes().all(|byte| byte.is_ascii_digit() || byte == b'.')
        && host.bytes().any(|byte| byte.is_ascii_digit())
}

/// Parses a strict dotted-quad IPv4 address. Leading zeros, out-of-range
/// octets, and non-canonical spellings are all rejected to close bypass
/// vectors.
fn parse_ipv4(host: &str) -> Result<[u8; 4], String> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 {
        return Err(format!(
            "egress URL host '{host}' is not a valid dotted-quad IPv4 address"
        ));
    }
    let mut octets = [0u8; 4];
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() || part.len() > 3 {
            return Err(format!(
                "egress URL host '{host}' is not a valid dotted-quad IPv4 address"
            ));
        }
        if part.len() > 1 && part.starts_with('0') {
            return Err(format!(
                "egress URL host '{host}' contains a leading-zero octet (ambiguous IPv4 spelling)"
            ));
        }
        let value: u32 = part
            .parse()
            .map_err(|_| format!("egress URL host '{host}' is not a valid IPv4 address"))?;
        octets[index] = u8::try_from(value)
            .map_err(|_| format!("egress URL host '{host}' has an out-of-range octet"))?;
    }
    Ok(octets)
}

fn classify_ipv4(octets: &[u8; 4]) -> HostClass {
    let [a, b, _, _] = *octets;
    match (a, b) {
        (127, _) => HostClass::Loopback,
        (10, _) | (192, 168) => HostClass::Private,
        (172, 16..=31) => HostClass::Private,
        (169, 254) => HostClass::Private, // link-local
        (0, _) => HostClass::Reserved,
        (100, 64..=127) => HostClass::Reserved, // CGNAT
        (192, 0) => HostClass::Reserved,        // 192.0.0.0/24 + TEST-NET-1
        (198, 18..=19) => HostClass::Reserved,  // benchmarking
        (198, 51) => HostClass::Reserved,       // TEST-NET-2
        (203, 0) => HostClass::Reserved,        // TEST-NET-3
        (224..=255, _) => HostClass::Reserved,  // multicast + reserved + broadcast
        _ => HostClass::Public,
    }
}

/// Validates an IPv6 address (loose structural check: hex groups separated by
/// `:`, at most one `::`) and classifies it.
fn classify_ipv6(host: &str) -> Result<HostClass, String> {
    let lowered = host.to_ascii_lowercase();

    // IPv4-mapped / -compatible forms (::ffff:127.0.0.1, ::127.0.0.1).
    if let Some((_, tail)) = lowered.rsplit_once(':') {
        if looks_like_ipv4(tail) {
            let octets = parse_ipv4(tail)?;
            return Ok(classify_ipv4(&octets));
        }
    }

    let double_colon = lowered.matches("::").count();
    if double_colon > 1 {
        return Err(format!(
            "egress URL host '{host}' is not a valid IPv6 address"
        ));
    }
    let groups: Vec<&str> = if double_colon == 1 {
        let (head, tail) = lowered.split_once("::").unwrap();
        let mut groups: Vec<&str> = head
            .split(':')
            .filter(|group| !group.is_empty())
            .collect();
        groups.extend(
            tail.split(':')
                .filter(|group| !group.is_empty()),
        );
        groups
    } else {
        lowered.split(':').collect()
    };
    if groups.is_empty() {
        // "::" — the unspecified address is reserved.
        return Ok(HostClass::Reserved);
    }
    if groups.len() > 8 {
        return Err(format!(
            "egress URL host '{host}' is not a valid IPv6 address"
        ));
    }
    for group in &groups {
        if group.is_empty()
            || group.len() > 4
            || !group.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(format!(
                "egress URL host '{host}' is not a valid IPv6 address"
            ));
        }
    }

    let segments: Vec<u16> = groups
        .iter()
        .map(|group| u16::from_str_radix(group, 16).unwrap_or(0))
        .collect();

    // :: (unspecified) is reserved; ::1 (loopback) is loopback in every
    // spelling, e.g. "::1" and "0:0:0:0:0:0:0:1".
    if segments.iter().all(|segment| *segment == 0) {
        return Ok(HostClass::Reserved);
    }
    if segments.last() == Some(&1) && segments[..segments.len() - 1].iter().all(|s| *s == 0) {
        return Ok(HostClass::Loopback);
    }
    match segments.first() {
        // fc00::/7 covers fc00..fdff (e.g. fd12::/8 unique-local addresses).
        Some(first) if (0xfc00..=0xfdff).contains(first) => {
            return Ok(HostClass::Private)
        }
        Some(first) if (0xfe80..=0xfebf).contains(first) => {
            return Ok(HostClass::Private)
        } // fe80::/10
        Some(0xff00) => return Ok(HostClass::Reserved),               // ff00::/8
        Some(0x0100) => return Ok(HostClass::Reserved),               // 100::/64 discard
        _ => {}
    }
    if segments.starts_with(&[0x2001, 0x0db8]) {
        return Ok(HostClass::Reserved); // 2001:db8::/32 documentation
    }
    Ok(HostClass::Public)
}

fn validate_domain(host: &str) -> Result<(), String> {
    if host.len() > 253 {
        return Err(format!(
            "egress URL host '{host}' exceeds the 253-character limit"
        ));
    }
    for label in host.split('.') {
        if label.is_empty() {
            return Err(format!(
                "egress URL host '{host}' contains an empty label"
            ));
        }
        if !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(format!(
                "egress URL host '{host}' contains characters outside [A-Za-z0-9-]"
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "egress URL host '{host}' has a label starting or ending with '-'"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Path policy --------------------------------------------------------

    fn policy(root: &str, lock: bool, read_only: bool, allow_delete: bool) -> PathPolicy {
        PathPolicy {
            root: root.to_string(),
            lock_to_root: lock,
            read_only,
            allow_delete,
        }
    }

    #[test]
    fn from_config_defaults_and_root_normalization() {
        let config = serde_json::Map::new();
        let policy = PathPolicy::from_config(&config);
        assert_eq!(policy.root, "/");
        assert!(!policy.lock_to_root);
        assert!(!policy.read_only);
        assert!(policy.allow_delete);

        let config: serde_json::Map<String, Value> = [
            ("root", Value::from("/mnt/nas/")),
            ("lock_to_root", Value::from(true)),
            ("read_only", Value::from(true)),
            ("allow_delete", Value::from(false)),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect();
        let policy = PathPolicy::from_config(&config);
        assert_eq!(policy.root, "/mnt/nas");
        assert!(policy.lock_to_root);
        assert!(policy.read_only);
        assert!(!policy.allow_delete);

        // Relative root spellings are canonicalized too.
        let config: serde_json::Map<String, Value> = [("root", Value::from("mnt/nas"))]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();
        assert_eq!(PathPolicy::from_config(&config).root, "/mnt/nas");
    }

    #[test]
    fn resolve_permissive_paths() {
        let policy = policy("/", false, false, true);
        let cases = [
            ("", "/", ""),
            ("/", "/", ""),
            ("a/b", "/a/b", "a/b"),
            ("/a/b", "/a/b", "a/b"),
            ("a/../b", "/b", "b"),
            ("a//b///c", "/a/b/c", "a/b/c"),
            ("./a", "/a", "a"),
            ("a/", "/a", "a"),
        ];
        for (input, absolute, relative) in cases {
            let resolved = policy.resolve(input).unwrap_or_else(|err| {
                panic!("resolve('{input}') should pass: {err}")
            });
            assert_eq!(resolved.absolute, absolute, "input '{input}'");
            assert_eq!(resolved.relative, relative, "input '{input}'");
        }
    }

    #[test]
    fn resolve_rejects_traversal_and_ambiguous_characters() {
        let policy = policy("/", false, false, true);
        for input in [
            "../x",
            "a/../../x",
            "a\\..\\b",
            "a\\b",
            "a\0b",
            "a\nb",
            "a/../b/../../../c",
        ] {
            assert!(
                policy.resolve(input).is_err(),
                "resolve('{input}') should be rejected"
            );
        }
    }

    #[test]
    fn resolve_with_root_and_lock_to_root() {
        let policy = policy("/mnt/nas", true, false, true);
        let cases = [
            ("", "/mnt/nas", ""),
            ("/mnt/nas", "/mnt/nas", ""),
            ("sub/f", "/mnt/nas/sub/f", "sub/f"),
            ("/mnt/nas/sub", "/mnt/nas/sub", "sub"),
            ("a/../sub", "/mnt/nas/sub", "sub"),
        ];
        for (input, absolute, relative) in cases {
            let resolved = policy
                .resolve(input)
                .unwrap_or_else(|err| panic!("resolve('{input}') should pass: {err}"));
            assert_eq!(resolved.absolute, absolute, "input '{input}'");
            assert_eq!(resolved.relative, relative, "input '{input}'");
        }
        for input in ["/etc/passwd", "/mnt/nasx/steal", "../escape", "/"] {
            // "/" resolves to the root itself and is fine — only absolute
            // forms *outside* the root must fail.
            if input == "/" {
                assert!(policy.resolve(input).is_ok());
            } else {
                assert!(
                    policy.resolve(input).is_err(),
                    "resolve('{input}') should be rejected by lock_to_root"
                );
            }
        }
    }

    #[test]
    fn resolve_without_lock_allows_absolute_outside_root() {
        let policy = policy("/mnt/nas", false, false, true);
        let resolved = policy.resolve("/etc/x").expect("unlocked absolute pass");
        assert_eq!(resolved.absolute, "/etc/x");
        // Physically confined by the Operator root; the engine hands the
        // operator-relative remainder to OpenDAL.
        assert_eq!(resolved.relative, "etc/x");
    }

    #[test]
    fn write_gate_read_only() {
        let policy = policy("/", false, true, true);
        assert!(policy.check_read("a").is_ok());
        for path in ["a", "b/c"] {
            let error = policy.check_write(path).unwrap_err();
            assert!(error.contains("read-only"), "unexpected message: {error}");
        }
        let error = policy.check_rename("a", "b").unwrap_err();
        assert!(error.contains("read-only"), "unexpected message: {error}");
    }

    #[test]
    fn delete_gate_allow_delete() {
        // Locals must not be named `policy`: that would shadow the `policy(..)`
        // helper for the rest of the block.
        let denying = policy("/", false, false, false);
        assert!(denying.check_read("a").is_ok());
        assert!(denying.check_write("a").is_ok(), "copy target stays allowed");
        assert!(denying.check_copy("a", "b").is_ok());
        for (label, result) in [
            ("delete", denying.check_delete("a").map(|_| ())),
            ("purge", denying.check_purge("a").map(|_| ())),
            ("rename", denying.check_rename("a", "b").map(|_| ())),
        ] {
            let error = result.unwrap_err();
            assert!(
                error.contains("allow_delete=false"),
                "{label}: unexpected message: {error}"
            );
        }

        let permissive = policy("/", false, false, true);
        assert!(permissive.check_delete("a").is_ok());
        assert!(permissive.check_purge("sub").is_ok());
        assert!(permissive.check_rename("a", "b").is_ok());
    }

    #[test]
    fn delete_gate_combinations_pin_read_only_precedence() {
        // Both form flags deny at once (the form can submit this combination
        // when the host read_only flag ORs in on top of allow_delete=false):
        // the read-only message must win everywhere, so callers never see the
        // allow_delete hint for a connection that rejects ALL writes anyway.
        let both = policy("/", false, true, false);
        assert!(both.check_read("a").is_ok());
        let error = both.check_write("a").unwrap_err();
        assert!(error.contains("read-only"), "write: {error}");
        assert!(!error.contains("allow_delete"), "write: {error}");
        for (label, result) in [
            ("delete", both.check_delete("a").map(|_| ())),
            ("purge", both.check_purge("a").map(|_| ())),
            ("rename", both.check_rename("a", "b").map(|_| ())),
            ("delete_allowed", both.check_delete_allowed("delete").map(|_| ())),
        ] {
            let error = result.unwrap_err();
            assert!(
                error.contains("read-only") && !error.contains("allow_delete=false"),
                "{label}: read-only gate must take precedence: {error}"
            );
        }

        // The mirror combination (writable + deletable) stays fully open.
        let open = policy("/", false, false, true);
        assert!(open.check_write("a").is_ok());
        assert!(open.check_delete("a").is_ok());
        assert!(open.check_rename("a", "b").is_ok());
    }

    #[test]
    fn purge_refuses_root() {
        let policy = policy("/mnt/nas", false, false, true);
        for input in ["", "/", "/mnt/nas"] {
            let error = policy.check_purge(input).unwrap_err();
            assert!(
                error.contains("refusing to purge"),
                "purge('{input}'): unexpected message: {error}"
            );
        }
        // Subdirectories pass.
        assert!(policy.check_purge("sub").is_ok());
        assert!(policy.check_purge("/mnt/nas/sub").is_ok());
    }

    /// 第五轮（可靠性纵深）路径门对抗输入表。两类结果：
    /// - **拒绝**：任何能逃逸可视空间或含歧义字节的拼写；
    /// - **字面透传（设计内保守行为）**：协议不做 `~` 展开、URL 解码与
    ///   unicode NFC/NFD 归一化——这些拼写只会寻址到「字面同名」条目，
    ///   无法走私遍历或根绕过；`~`/`%2e%2e`/NFD 拼写在后端就是一个
    ///   同名文件。该语义同时钉住 MCP 面（mcp.rs validate_path_shape）
    ///   与本文件的 split('-segment') 归一化两条路径。
    #[test]
    fn sanitize_path_adversarial_inputs_reject_or_stay_literal() {
        // 拒绝：遍历逃逸与歧义字节（`..` 段在栈空时必须报错，不许吞）。
        for input in [
            "/..",
            "../x",
            "a/../../etc",
            "/a/../..",
            "a\\..\\b",
            "a\0b",
            "a\nb",
        ] {
            assert!(
                sanitize_path(input).is_err(),
                "sanitize('{input}') should be rejected"
            );
        }
        // `.` 段等价吞并（与 MCP 面的 fail-fast 拒绝是记录在案的双面语义）。
        assert_eq!(sanitize_path("/a/./b").unwrap(), "/a/b");
        assert_eq!(sanitize_path(".").unwrap(), "/");
        // 字面透传：不展开、不解码、不归一化（相对输入补前导 `/` 属既有
        // 根相对语义，`~` 依旧是字面名）。
        for (input, literal) in [
            ("~", "/~"),
            ("/home/u/~x", "/home/u/~x"),
            ("/etc/file.", "/etc/file."),
            ("/dir/%2e%2e/next", "/dir/%2e%2e/next"),
            ("/caf\u{e9}", "/caf\u{e9}"),   // NFC
            ("/cafe\u{301}", "/cafe\u{301}"), // NFD 组合（与 NFC 不同字节 → 不同条目）
            ("/data/报告 v2.txt", "/data/报告 v2.txt"), // 空格与 unicode 文件名合法
        ] {
            assert_eq!(
                sanitize_path(input).unwrap(),
                literal,
                "'{input}' must pass through literally"
            );
        }
        // 根红线在对抗拼写下依然成立：`/.` `//.` 归一化后即根 → purge 拒绝。
        let rooted = policy("/mnt/nas", false, false, true);
        for input in ["/.", "//.", "/./", "/mnt/nas/."] {
            let error = rooted.check_purge(input).unwrap_err();
            assert!(
                error.contains("refusing to purge"),
                "purge('{input}') bypassed the root red line: {error}"
            );
        }
    }

    // -- Egress URL validation ----------------------------------------------

    enum Expect {
        Allow,
        AllowAudited,
        Reject(&'static str),
    }

    fn run_cases(cases: &[(&str, UrlOrigin, Expect)]) {
        for (url, origin, expect) in cases {
            let outcome = validate_egress_url(url, *origin);
            match expect {
                Expect::Allow => assert_eq!(
                    outcome.as_ref().map(|verdict| verdict.audit_flag),
                    Ok(false),
                    "expected plain allow for '{url}', got {outcome:?}"
                ),
                Expect::AllowAudited => assert_eq!(
                    outcome.as_ref().map(|verdict| verdict.audit_flag),
                    Ok(true),
                    "expected audited allow for '{url}', got {outcome:?}"
                ),
                Expect::Reject(fragment) => {
                    let error = outcome.err().unwrap_or_else(|| {
                        panic!("expected rejection for '{url}', got Ok")
                    });
                    assert!(
                        error.contains(fragment),
                        "rejection for '{url}' should mention '{fragment}', got: {error}"
                    );
                }
            }
        }
    }

    #[test]
    fn egress_public_urls_pass_for_both_origins() {
        let mut cases: Vec<(&str, UrlOrigin, Expect)> = Vec::new();
        let public = [
            "http://8.8.8.8",
            "https://8.8.8.8/",
            "https://example.com",
            "http://example.com:8443/x?y=1#z",
            "http://[2606:4700::1111]/",
            "http://[::ffff:8.8.8.8]/",
        ];
        for url in public {
            cases.push((url, UrlOrigin::UserConfigured, Expect::Allow));
            cases.push((url, UrlOrigin::Derived, Expect::Allow));
        }
        run_cases(&cases);
    }

    #[test]
    fn egress_user_configured_intranet_passes_with_audit_flag() {
        run_cases(&[
            ("http://127.0.0.1:9000", UrlOrigin::UserConfigured, Expect::AllowAudited),
            ("http://10.0.0.5", UrlOrigin::UserConfigured, Expect::AllowAudited),
            ("http://172.16.0.1", UrlOrigin::UserConfigured, Expect::AllowAudited),
            ("http://172.31.255.254", UrlOrigin::UserConfigured, Expect::AllowAudited),
            ("http://192.168.1.1", UrlOrigin::UserConfigured, Expect::AllowAudited),
            ("http://169.254.169.254", UrlOrigin::UserConfigured, Expect::AllowAudited),
            ("http://0.0.0.0", UrlOrigin::UserConfigured, Expect::AllowAudited),
            ("http://localhost", UrlOrigin::UserConfigured, Expect::AllowAudited),
            ("http://LOCALHOST:9000", UrlOrigin::UserConfigured, Expect::AllowAudited),
            ("http://my.minio.internal", UrlOrigin::UserConfigured, Expect::AllowAudited),
            ("http://nas.local", UrlOrigin::UserConfigured, Expect::AllowAudited),
            ("http://[fc00::1]:9000", UrlOrigin::UserConfigured, Expect::AllowAudited),
            ("http://[fd12::1]", UrlOrigin::UserConfigured, Expect::AllowAudited),
            ("http://[fe80::1]", UrlOrigin::UserConfigured, Expect::AllowAudited),
        ]);
    }

    #[test]
    fn egress_derived_intranet_and_reserved_rejected() {
        run_cases(&[
            ("http://127.0.0.1:9000", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://10.1.2.3", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://172.20.0.1", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://192.168.0.10", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://169.254.169.254/latest", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://224.0.0.1", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://100.64.0.1", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://0.0.0.0", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://localhost", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://x.localhost", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://svc.internal", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://printer.local", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://[::1]", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://[0:0:0:0:0:0:0:1]", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://[fc00::abcd]", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://[fd00::1]", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://[fe80::1]", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://[::]", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://[2001:db8::1]", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://[::ffff:127.0.0.1]", UrlOrigin::Derived, Expect::Reject("restricted")),
        ]);
    }

    #[test]
    fn egress_scheme_rules() {
        run_cases(&[
            ("HTTP://8.8.8.8", UrlOrigin::Derived, Expect::Allow),
            ("HTTPS://EXAMPLE.COM", UrlOrigin::Derived, Expect::Allow),
            ("ftp://8.8.8.8", UrlOrigin::Derived, Expect::Reject("scheme")),
            ("FTP://8.8.8.8", UrlOrigin::Derived, Expect::Reject("scheme")),
            ("file:///etc/passwd", UrlOrigin::Derived, Expect::Reject("scheme")),
            ("gopher://8.8.8.8", UrlOrigin::Derived, Expect::Reject("scheme")),
            ("javascript:alert(1)", UrlOrigin::Derived, Expect::Reject("missing a scheme")),
            ("", UrlOrigin::Derived, Expect::Reject("missing a scheme")),
            ("/etc/passwd", UrlOrigin::Derived, Expect::Reject("missing a scheme")),
        ]);
    }

    #[test]
    fn egress_host_format_and_bypass_attempts() {
        run_cases(&[
            ("http://2130706433", UrlOrigin::Derived, Expect::Reject("bare integer")),
            ("http://0x7f.0.0.1", UrlOrigin::Derived, Expect::Reject("hexadecimal")),
            ("http://0x7f000001", UrlOrigin::Derived, Expect::Reject("hexadecimal")),
            ("http://127.0.0.1.", UrlOrigin::Derived, Expect::Reject("restricted")),
            ("http://127.000.000.001", UrlOrigin::Derived, Expect::Reject("leading-zero")),
            ("http://127.1", UrlOrigin::Derived, Expect::Reject("dotted-quad")),
            ("http://256.1.1.1", UrlOrigin::Derived, Expect::Reject("out-of-range octet")),
            ("http://foo@127.0.0.1", UrlOrigin::Derived, Expect::Reject("userinfo")),
            ("http://foo@127.0.0.1", UrlOrigin::UserConfigured, Expect::AllowAudited),
            ("http://", UrlOrigin::UserConfigured, Expect::Reject("empty host")),
            ("http:///path", UrlOrigin::UserConfigured, Expect::Reject("empty host")),
            ("http://8.8.8.8:0", UrlOrigin::Derived, Expect::Reject("port")),
            ("http://8.8.8.8:99999", UrlOrigin::Derived, Expect::Reject("port")),
            ("http://8.8.8.8:abc", UrlOrigin::Derived, Expect::Reject("port")),
            ("http://ex ample.com", UrlOrigin::Derived, Expect::Reject("outside [A-Za-z0-9-]")),
            ("http://ex_ample.com", UrlOrigin::Derived, Expect::Reject("outside [A-Za-z0-9-]")),
            ("http://bücher.de", UrlOrigin::Derived, Expect::Reject("non-ASCII")),
            ("http://-a.b", UrlOrigin::Derived, Expect::Reject("starting or ending with '-'")),
            ("http://a-.b", UrlOrigin::Derived, Expect::Reject("starting or ending with '-'")),
            ("http://a..b", UrlOrigin::Derived, Expect::Reject("empty label")),
            ("http://[::1", UrlOrigin::Derived, Expect::Reject("bracket")),
        ]);
    }

    #[test]
    fn egress_redirect_targets_are_strict_by_default() {
        assert!(!FOLLOW_REDIRECTS_DEFAULT);
        assert!(validate_redirect_target("http://8.8.8.8/ok").is_ok());
        let error = validate_redirect_target("http://127.0.0.1/steal").unwrap_err();
        assert!(error.contains("restricted"), "unexpected message: {error}");
        // Redirect targets never inherit the trusted origin of the original
        // request.
        let error = validate_redirect_target("http://192.168.1.1/").unwrap_err();
        assert!(error.contains("restricted"), "unexpected message: {error}");
    }
}
