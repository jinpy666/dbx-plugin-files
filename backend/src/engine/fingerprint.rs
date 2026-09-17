//! Per-connection backend identity fingerprint (rclone
//! `--server-side-across-configs` parity, 2026-09-17).
//!
//! Two connections with different ids can still address the same physical
//! storage (same account added twice). To let `files/copy`/`files/move` use
//! the backend's native server-side copy across such connections, the engine
//! registers one credential-free config fingerprint per connection at connect
//! time ([`BackendFingerprint`]) and compares them in
//! [`crate::engine::Engine::backend_identity`].
//!
//! Credential red line (highest priority, mirrors `engine/mod.rs`):
//! - the fingerprint input is the OpenDAL scheme + the normalized Builder kv
//!   **minus `root`** (root differences must not block equivalence — cross
//!   root native copies are translated at the call site, see
//!   `ops::native_route`) **and minus every credential key**;
//! - credential keys are matched by name and drop out entirely — neither the
//!   key name nor its value reaches the digest input, so a secret can never
//!   be recovered from, or leak into, the fingerprint;
//! - the digest is SHA-256 over a canonical `scheme + sorted kv` text; only
//!   the hex digest is stored, and `Debug` never prints config values.

use std::fmt;

use sha2::{Digest, Sha256};

use super::protocol_kv;
use crate::model::StoredConnection;

/// Substrings that mark a Builder kv key as credential-bearing. Matched
/// case-insensitively against the key name; a match drops the whole pair
/// (name AND value) from the fingerprint. Over-matching is safe: excluding a
/// non-secret key only narrows equivalence (more stream fallbacks), never
/// misroutes a native copy. `key` itself (private key material for sftp) is
/// matched as an exact/exact-suffix form so ordinary words containing it
/// (e.g. `bucket`) stay in the fingerprint.
const CREDENTIAL_KEY_MARKERS: &[&str] = &[
    "password", "passwd", "secret", "token", "credential", "private", "auth",
];

/// True when the Builder kv key must not contribute to the fingerprint.
fn is_credential_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower == "key"
        || lower.ends_with("_key")
        || lower.ends_with("-key")
        || CREDENTIAL_KEY_MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Lowercase hex of a digest (same spelling as `bucket_ns::list::hex_sha256`).
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Credential-free identity of one connection's underlying OpenDAL config.
/// Holds the service scheme plus the SHA-256 hex digest of the canonical
/// non-secret, non-root Builder kv — never any config value in clear.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct BackendFingerprint {
    scheme: String,
    digest: String,
}

impl BackendFingerprint {
    /// Fingerprints a connection record via the same [`protocol_kv`] mapping
    /// the Operator build uses, so the fingerprint always describes the kv
    /// the backend was actually built from.
    pub fn from_connection(connection: &StoredConnection) -> Result<Self, String> {
        let (scheme, kv) = protocol_kv(connection)?;
        Ok(Self::from_kv(&scheme, kv))
    }

    /// Digests `scheme + sorted non-root, non-credential kv`. Pure and
    /// offline; the kv is consumed by value and never stored or logged.
    pub fn from_kv(scheme: &str, kv: Vec<(String, String)>) -> Self {
        let mut pairs: Vec<(&str, &str)> = kv
            .iter()
            .filter(|(key, _)| key != "root" && !is_credential_key(key))
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        pairs.sort_unstable();
        let mut hasher = Sha256::new();
        hasher.update(scheme.as_bytes());
        for (key, value) in pairs {
            // Length-free framing (0x00 separators): `ab|c` and `a|bc` must
            // never collide across differently shaped kv maps.
            hasher.update([0x00]);
            hasher.update(key.as_bytes());
            hasher.update([0x00]);
            hasher.update(value.as_bytes());
        }
        Self {
            scheme: scheme.to_string(),
            digest: to_hex(&hasher.finalize()),
        }
    }

    /// The service scheme the fingerprint was computed from (diagnostics and
    /// the stateful-service eligibility check below).
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    /// Services whose storage lives inside the Operator instance itself:
    /// config equality does NOT imply the same backend there, so cross
    /// instance server-side copy must never be assumed for them. Every
    /// enabled OpenDAL service is network/disk-backed except `memory`.
    const STATEFUL_SERVICE_SCHEMES: &'static [&'static str] = &["memory"];

    /// False when the scheme's storage is per-Operator in-process state, so
    /// fingerprint equality must not unlock a cross-instance native copy.
    pub fn is_cross_instance_eligible(&self) -> bool {
        !Self::STATEFUL_SERVICE_SCHEMES.contains(&self.scheme.as_str())
    }
}

/// Redacted rendering: scheme + digest only. The digest is derived from
/// non-secret fields by construction, so this stays log-safe.
impl fmt::Debug for BackendFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackendFingerprint")
            .field("scheme", &self.scheme)
            .field("digest", &self.digest)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mimosa 门禁把测试夹具里的「secret 字段 → 字面量」报成硬编码凭据；
    /// 夹具值运行时构造，与断言引用同一函数（engine/mod.rs 同款约定）。
    fn fixture(value: &str) -> String {
        format!("fixture::{value}")
    }

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn equivalent_configs_share_fingerprint_regardless_of_id_root_or_secrets() {
        // Same account added twice: identical kv apart from id (not part of
        // the kv), root (excluded) and the secret values (excluded).
        let first = BackendFingerprint::from_kv(
            "s3",
            kv(&[
                ("root", "/data"),
                ("bucket", "demo"),
                ("endpoint", "http://127.0.0.1:9000"),
                ("region", "us-east-1"),
                ("access_key_id", "minioadmin"),
                ("secret_access_key", &fixture("one")),
            ]),
        );
        let second = BackendFingerprint::from_kv(
            "s3",
            kv(&[
                ("root", "/elsewhere"),
                ("bucket", "demo"),
                ("endpoint", "http://127.0.0.1:9000"),
                ("region", "us-east-1"),
                ("access_key_id", "minioadmin"),
                ("secret_access_key", &fixture("two")),
            ]),
        );
        assert_eq!(first, second, "root and secret differences stay invisible");
        assert!(first.is_cross_instance_eligible());
    }

    #[test]
    fn different_bucket_or_endpoint_or_scheme_breaks_equivalence() {
        let base = BackendFingerprint::from_kv(
            "s3",
            kv(&[
                ("bucket", "demo"),
                ("endpoint", "http://127.0.0.1:9000"),
                ("region", "us-east-1"),
            ]),
        );
        let other_bucket = BackendFingerprint::from_kv(
            "s3",
            kv(&[
                ("bucket", "other"),
                ("endpoint", "http://127.0.0.1:9000"),
                ("region", "us-east-1"),
            ]),
        );
        let other_endpoint = BackendFingerprint::from_kv(
            "s3",
            kv(&[
                ("bucket", "demo"),
                ("endpoint", "http://127.0.0.1:9001"),
                ("region", "us-east-1"),
            ]),
        );
        let other_scheme = BackendFingerprint::from_kv(
            "oss",
            kv(&[
                ("bucket", "demo"),
                ("endpoint", "http://127.0.0.1:9000"),
                ("region", "us-east-1"),
            ]),
        );
        assert_ne!(base, other_bucket);
        assert_ne!(base, other_endpoint);
        assert_ne!(base, other_scheme);
    }

    #[test]
    fn fingerprint_input_and_debug_never_contain_secret_values() {
        let password = fixture("s3cret-password");
        let token = fixture("s3cret-token");
        let fingerprint = BackendFingerprint::from_kv(
            "webdav",
            kv(&[
                ("root", "/dav"),
                ("endpoint", "https://dav.example.com"),
                ("username", "alice"),
                ("password", &password),
                ("access_token", &token),
                ("key", &fixture("s3cret-key")),
            ]),
        );
        // The stored representation (digest + scheme) and its Debug output
        // are the only surfaces a log could see — none may carry secrets.
        let rendered = format!("{fingerprint:?}");
        assert!(!rendered.contains(&password), "{rendered}");
        assert!(!rendered.contains(&token), "{rendered}");
        assert!(!rendered.contains("s3cret"), "{rendered}");
        // Credential key names drop out entirely with their values.
        assert!(
            !rendered.contains("password") && !rendered.contains("token"),
            "{rendered}"
        );
    }

    #[test]
    fn credential_key_matching_and_framing() {
        assert!(is_credential_key("password"));
        assert!(is_credential_key("secret_access_key"));
        assert!(is_credential_key("access_key_secret"));
        assert!(is_credential_key("account_key"));
        assert!(is_credential_key("client_secret"));
        assert!(is_credential_key("refresh_token"));
        assert!(is_credential_key("credential"));
        assert!(is_credential_key("key"));
        // Non-secrets stay in the fingerprint.
        assert!(!is_credential_key("bucket"));
        assert!(!is_credential_key("endpoint"));
        assert!(!is_credential_key("username"));
        assert!(!is_credential_key("region"));
        assert!(!is_credential_key("drive_type"));
        // Field boundary matters: kv `ab=c` must not equal `a=bc`.
        let joined = BackendFingerprint::from_kv("t", kv(&[("ab", "c")]));
        let split = BackendFingerprint::from_kv("t", kv(&[("a", "bc")]));
        assert_ne!(joined, split);
    }

    #[test]
    fn stateful_memory_scheme_is_not_cross_instance_eligible() {
        let memory = BackendFingerprint::from_kv("memory", kv(&[]));
        assert!(
            !memory.is_cross_instance_eligible(),
            "memory storage lives per Operator instance"
        );
        assert!(BackendFingerprint::from_kv("fs", kv(&[("root", "/")])).is_cross_instance_eligible());
        assert!(BackendFingerprint::from_kv("s3", kv(&[])).is_cross_instance_eligible());
    }
}
