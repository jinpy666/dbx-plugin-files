//! Native bucket enumeration (ListBuckets) for bucket-namespace connections.
//!
//! OpenDAL 0.57 object-storage services require a bucket and expose no
//! "list buckets" operation, so each namespace-capable service gets a small
//! signed GET-service request here (design 2026-09-17):
//!
//! | service | request                     | signature            |
//! |---------|-----------------------------|----------------------|
//! | s3      | `GET {endpoint}/`           | SigV4 (HMAC-SHA256)  |
//! | oss     | `GET {endpoint}/`           | `OSS ak:sig` HMAC-SHA1 |
//! | obs     | `GET {endpoint}/`           | `OBS ak:sig` HMAC-SHA1 |
//! | cos     | `GET {endpoint}/` (GET Service) | q-sign sha1      |
//! | azblob  | `GET {endpoint}/?comp=list` | SharedKey HMAC-SHA256 |
//!
//! Signing helpers are pure (timestamps passed in) and locked to fixed
//! vectors generated with Python's hashlib, so provider-side signature drift
//! fails CI instead of a connect dialog. gcs stays out (ListBuckets needs a
//! service-account JWT → OAuth2 token exchange; phase 2).
//!
//! Red line: secrets only ever flow into the in-memory signature input and
//! the request headers; they are never logged, and errors carry at most the
//! redacted endpoint host plus the provider's own error code/message.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use sha2::{Digest, Sha256};

/// Slim in-memory snapshot of the connection fields a ListBuckets request
/// signs with. Assembled once at build/probe time; secrets stay in memory.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ListParams {
    pub(crate) protocol: String,
    pub(crate) endpoint: String,
    pub(crate) region: String,
    pub(crate) access_key_id: String,
    pub(crate) secret_access_key: String,
    pub(crate) secret_id: String,
    pub(crate) secret_key: String,
    pub(crate) security_token: String,
    pub(crate) account_name: String,
    pub(crate) account_key: String,
}

impl ListParams {
    pub(crate) fn from_connection(connection: &crate::model::StoredConnection) -> Self {
        Self {
            protocol: connection.protocol.clone(),
            endpoint: connection.endpoint.clone(),
            region: connection.region.clone(),
            access_key_id: connection.access_key_id.clone(),
            secret_access_key: connection.secret_access_key.clone(),
            secret_id: connection.secret_id.clone(),
            secret_key: connection.secret_key.clone(),
            security_token: connection.security_token.clone(),
            account_name: connection.account_name.clone(),
            account_key: connection.account_key.clone(),
        }
    }
}

type HmacSha1 = Hmac<Sha1>;
type HmacSha256 = Hmac<Sha256>;

/// SHA-256 of the empty payload (SigV4 `x-amz-content-sha256` for GET).
const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Azure API version sent as `x-ms-version` (List Containers needs one).
const AZBLOB_API_VERSION: &str = "2025-01-05";

/// Default endpoint for s3 connections that leave the endpoint empty (the
/// form keeps it optional for AWS); the list call needs a concrete host.
const S3_DEFAULT_ENDPOINT: &str = "https://s3.amazonaws.com";

/// Why a bucket listing failed. `PermissionDenied` gets the actionable
/// "fill in the bucket instead" hint because narrowed IAM policies without
/// list-buckets rights are the common real-world case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BucketListError {
    PermissionDenied(String),
    NotFound(String),
    Other(String),
}

impl BucketListError {
    /// User-facing message; endpoint hosts only (never credentials).
    pub(crate) fn to_message(&self) -> String {
        let hint = "fill in the Bucket field to connect to a specific bucket directly";
        match self {
            BucketListError::PermissionDenied(detail) => {
                format!("listing buckets is not permitted for these credentials ({detail}); {hint}")
            }
            BucketListError::NotFound(detail) => {
                format!("bucket listing failed, endpoint or account unreachable ({detail}); {hint}")
            }
            BucketListError::Other(detail) => format!("bucket listing failed: {detail}"),
        }
    }
}

impl fmt::Display for BucketListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_message())
    }
}

// ------------------------------------------------------------------ signing

fn hmac_sha1(key: &[u8], message: &str) -> Vec<u8> {
    let mut mac = HmacSha1::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(message.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn hmac_sha256(key: &[u8], message: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(message.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn hex_sha1(message: &str) -> String {
    let digest = Sha1::digest(message.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_sha256(message: &str) -> String {
    let digest = Sha256::digest(message.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// SigV4 `Authorization` header for `GET /` (ListAllMyBuckets), path-style.
pub(crate) fn s3_authorization(
    access_key: &str,
    secret_key: &str,
    host: &str,
    region: &str,
    amz_date: &str,
    date_stamp: &str,
) -> String {
    let canonical_headers = format!(
        "host:{host}\nx-amz-content-sha256:{EMPTY_PAYLOAD_SHA256}\nx-amz-date:{amz_date}\n"
    );
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_request = format!(
        "GET\n/\n\n{canonical_headers}\n{signed_headers}\n{EMPTY_PAYLOAD_SHA256}"
    );
    let scope = format!("{date_stamp}/{region}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex_sha256(&canonical_request)
    );
    // Signing key: HMAC chain datestamp → region → service → terminator.
    let mut key = hmac_sha256(format!("AWS4{secret_key}").as_bytes(), date_stamp);
    key = hmac_sha256(&key, region);
    key = hmac_sha256(&key, "s3");
    key = hmac_sha256(&key, "aws4_request");
    let signature: String = hmac_sha256(&key, &string_to_sign)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, \
         Signature={signature}"
    )
}

/// AWS-Signature-v2 style header shared by oss (`OSS`) and obs (`OBS`):
/// `VERB\nContent-MD5\nContent-Type\nDate\nCanonicalizedResource` over
/// HMAC-SHA1, base64-encoded. The list call uses an empty resource (`/`).
pub(crate) fn aws_v2_style_signature(
    secret_key: &str,
    date: &str,
) -> String {
    let string_to_sign = format!("GET\n\n\n{date}\n/");
    base64::engine::general_purpose::STANDARD.encode(hmac_sha1(secret_key.as_bytes(), &string_to_sign))
}

/// COS `q-sign` authorization for GET Service (single-shot signature with
/// empty header/param lists — the request itself carries no signed extras).
pub(crate) fn cos_authorization(secret_id: &str, secret_key: &str, key_time: &str) -> String {
    // SignKey = HMAC-SHA1(SecretKey, KeyTime), hex-encoded (not re-hashed).
    let sign_key = hex_encode(&hmac_sha1(secret_key.as_bytes(), key_time));
    let http_string = "get\n/\n\n\n";
    let string_to_sign = format!("sha1\n{key_time}\n{}\n", hex_sha1(http_string));
    let signature = hex_encode(&hmac_sha1(sign_key.as_bytes(), &string_to_sign));
    format!(
        "q-sign-algorithm=sha1&q-ak={secret_id}&q-sign-time={key_time}&q-key-time={key_time}\
         &q-header-list=&q-url-param-list=&q-signature={signature}"
    )
}

/// Lowercase hex encoding of raw bytes (HMAC outputs render as hex).
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Azure SharedKey authorization for `GET /?comp=list`. The string-to-sign
/// is VERB + 11 empty standard headers (each `\n`-terminated) + the
/// canonicalized `x-ms-*` headers + the canonicalized resource.
pub(crate) fn azblob_authorization(
    account_name: &str,
    account_key_base64: &str,
    date: &str,
) -> Result<String, BucketListError> {
    let key = base64::engine::general_purpose::STANDARD
        .decode(account_key_base64.trim())
        .map_err(|error| {
            BucketListError::Other(format!(
                "invalid Azure account key (not base64): {error}"
            ))
        })?;
    let canonicalized_headers =
        format!("x-ms-date:{date}\nx-ms-version:{AZBLOB_API_VERSION}\n");
    let canonicalized_resource = format!("/{account_name}/\ncomp:list");
    // VERB + the 11 empty standard headers (Content-Encoding … Range), each
    // newline-terminated, then the canonicalized x-ms-* headers + resource.
    let mut string_to_sign = String::from("GET");
    for _ in 0..12 {
        string_to_sign.push('\n');
    }
    string_to_sign.push_str(&canonicalized_headers);
    string_to_sign.push_str(&canonicalized_resource);
    let signature =
        base64::engine::general_purpose::STANDARD.encode(hmac_sha256(&key, &string_to_sign));
    Ok(format!("SharedKey {account_name}:{signature}"))
}

// --------------------------------------------------------------- timestamps

/// SigV4 timestamps: `(amz_date "YYYYMMDDTHHMMSSZ", date_stamp "YYYYMMDD")`.
pub(crate) fn amz_timestamps(now: SystemTime) -> (String, String) {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_secs();
    let utc = chrono::DateTime::from_timestamp(seconds as i64, 0)
        .expect("unix seconds are representable");
    (
        utc.format("%Y%m%dT%H%M%SZ").to_string(),
        utc.format("%Y%m%d").to_string(),
    )
}

/// RFC 1123 GMT date header (`Thu, 17 Sep 2026 00:00:00 GMT`).
pub(crate) fn http_date(now: SystemTime) -> String {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_secs();
    let utc = chrono::DateTime::from_timestamp(seconds as i64, 0)
        .expect("unix seconds are representable");
    utc.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// COS `KeyTime` window `"{start};{end}"` (unix seconds, 10 minutes wide).
pub(crate) fn cos_key_time(now: SystemTime) -> String {
    let start = now
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_secs();
    format!("{};{}", start, start + 600)
}

// ------------------------------------------------------------------ request

fn endpoint_base(params: &ListParams) -> Result<String, BucketListError> {
    let base = params.endpoint.trim().trim_end_matches('/');
    let base = if base.is_empty() {
        // s3 keeps the endpoint optional (AWS default); every other
        // namespace service requires it in the form.
        S3_DEFAULT_ENDPOINT.to_string()
    } else {
        base.to_string()
    };
    Ok(base)
}

/// Host authority of the endpoint for SigV4 (`host` header must match).
fn endpoint_host(endpoint: &str) -> String {
    let mut value = endpoint.trim();
    if let Some((_, rest)) = value.split_once("://") {
        value = rest;
    }
    if let Some((_, rest)) = value.rsplit_once('@') {
        value = rest;
    }
    let authority = value.split('/').next().unwrap_or_default();
    authority.to_ascii_lowercase()
}

/// Lists the buckets visible to the connection's credentials.
///
/// The HTTP client is built per call: bucket listings are rare (namespace
/// root, connection test) and the pool would outlive the secrets otherwise.
pub(crate) async fn list_buckets(
    params: &ListParams,
    timeout: Duration,
) -> Result<Vec<String>, BucketListError> {
    let now = SystemTime::now();
    let base = endpoint_base(params)?;
    let client = reqwest::Client::builder()
        .connect_timeout(timeout)
        .timeout(timeout)
        .build()
        .map_err(|error| BucketListError::Other(format!("HTTP client: {error}")))?;

    let (group, request) = match params.protocol.as_str() {
        "s3" => {
            let (amz_date, date_stamp) = amz_timestamps(now);
            let region = if params.region.trim().is_empty() {
                "us-east-1"
            } else {
                params.region.trim()
            };
            let authorization = s3_authorization(
                &params.access_key_id,
                &params.secret_access_key,
                &endpoint_host(&base),
                region,
                &amz_date,
                &date_stamp,
            );
            let request = client
                .get(format!("{base}/"))
                .header("x-amz-date", amz_date)
                .header("x-amz-content-sha256", EMPTY_PAYLOAD_SHA256)
                .header("authorization", authorization);
            ("Bucket", request)
        }
        "oss" | "obs" => {
            let date = http_date(now);
            let signature = aws_v2_style_signature(&params.secret_access_key, &date);
            let prefix = if params.protocol == "oss" {
                "OSS"
            } else {
                "OBS"
            };
            let request = client
                .get(format!("{base}/"))
                .header("date", date)
                .header(
                    "authorization",
                    format!("{prefix} {}:{signature}", params.access_key_id),
                );
            ("Bucket", request)
        }
        "cos" => {
            let key_time = cos_key_time(now);
            let authorization =
                cos_authorization(&params.secret_id, &params.secret_key, &key_time);
            let request = client
                .get(format!("{base}/"))
                .header("authorization", authorization);
            ("Bucket", request)
        }
        "azblob" => {
            let date = http_date(now);
            let authorization =
                azblob_authorization(&params.account_name, &params.account_key, &date)?;
            let request = client
                .get(format!("{base}/?comp=list"))
                .header("x-ms-date", date)
                .header("x-ms-version", AZBLOB_API_VERSION)
                .header("authorization", authorization);
            ("Container", request)
        }
        other => {
            return Err(BucketListError::Other(format!(
                "protocol '{other}' does not support bucket listing"
            )))
        }
    };

    let response = request.send().await.map_err(|error| {
        BucketListError::Other(format!(
            "request to {} failed: {error}",
            crate::engine::redact_url(&base)
        ))
    })?;
    let status = response.status();
    let body = response.text().await.map_err(|error| {
        BucketListError::Other(format!("reading listing response: {error}"))
    })?;
    if !status.is_success() {
        let (code, message) = extract_error(&body);
        let detail = match (code, message) {
            (Some(code), Some(message)) => format!("{code}: {message}"),
            (Some(code), None) => code,
            (None, Some(message)) => message,
            (None, None) => format!("HTTP {}", status.as_u16()),
        };
        return Err(match status.as_u16() {
            401 | 403 => BucketListError::PermissionDenied(detail),
            404 => BucketListError::NotFound(detail),
            _ => BucketListError::Other(detail),
        });
    }
    parse_bucket_names(&body, group).map_err(BucketListError::Other)
}

/// Pulls `<Code>`/`<Message>` (or `<Error>` payload) out of a provider error
/// body; both optional and tolerant of plain-text bodies.
fn extract_error(body: &str) -> (Option<String>, Option<String>) {
    (text_of(body, "Code"), text_of(body, "Message"))
}

/// Collects the `<Name>` of every `<{group}>` element (`Bucket` for
/// S3-family XML, `Container` for Azure). Namespaces and metadata elements
/// are ignored; ordering is the provider's.
fn parse_bucket_names(xml: &str, group: &str) -> Result<Vec<String>, String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut names = Vec::new();
    let mut in_group = false;
    let mut in_name = false;
    // Accumulates the <Name> text across events: quick-xml 0.42 splits
    // general entity references (`&amp;`) into their own GeneralRef events,
    // so a single bucket name can span Text + GeneralRef + Text.
    let mut current = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                // quick-xml 0.42: bind LocalName first, then deref to &str.
                let local = element.local_name();
                if local.as_ref() == group {
                    in_group = true;
                } else if local.as_ref() == "Name" && in_group {
                    in_name = true;
                }
            }
            Ok(Event::Text(text)) => {
                if in_name {
                    // xml10_content only normalizes EOLs in 0.42; entity
                    // decoding happens on the GeneralRef events (and the
                    // final trim covers pretty-printed padding).
                    current.push_str(&text.xml10_content());
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                if in_name {
                    let raw = format!("&{};", reference.xml10_content());
                    let decoded = quick_xml::escape::unescape(&raw).map_err(|error| {
                        format!("malformed bucket listing XML: bad escape: {error}")
                    })?;
                    current.push_str(&decoded);
                }
            }
            Ok(Event::End(element)) => {
                let local = element.local_name();
                if local.as_ref() == group {
                    in_group = false;
                } else if local.as_ref() == "Name" {
                    if in_name {
                        let decoded = current.trim();
                        if !decoded.is_empty() {
                            names.push(decoded.to_string());
                        }
                        current.clear();
                    }
                    in_name = false;
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => return Err(format!("malformed bucket listing XML: {error}")),
        }
    }
    if in_group || in_name {
        return Err("malformed bucket listing XML: unclosed elements".to_string());
    }
    // An empty listing is valid, but a body that never mentions the group
    // element is not a bucket listing at all (plain text/HTML responses).
    if names.is_empty() && !xml.contains(group) {
        return Err(format!(
            "response does not look like a {group} listing"
        ));
    }
    Ok(names)
}

/// First `<tag>text</tag>` occurrence, tolerant of namespace prefixes.
fn text_of(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    let text = xml[start..end].trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// `2026-09-17T00:00:00Z`.
    fn fixed_now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1789603200)
    }

    const FIXED_HTTP_DATE: &str = "Thu, 17 Sep 2026 00:00:00 GMT";

    #[test]
    fn timestamps_render_in_provider_formats() {
        assert_eq!(
            amz_timestamps(fixed_now()),
            ("20260917T000000Z".to_string(), "20260917".to_string())
        );
        assert_eq!(http_date(fixed_now()), FIXED_HTTP_DATE);
        assert_eq!(cos_key_time(fixed_now()), "1789603200;1789603800");
    }

    /// Vector generated with Python hashlib/hmac for the exact same inputs;
    /// any drift in the canonical request or the key chain breaks this first.
    #[test]
    fn s3_sigv4_matches_reference_vector() {
        let authorization = s3_authorization(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "s3.us-east-1.amazonaws.com",
            "us-east-1",
            "20260917T000000Z",
            "20260917",
        );
        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260917/us-east-1/s3/aws4_request, \
             SignedHeaders=host;x-amz-content-sha256;x-amz-date, \
             Signature=88f63b3e7cca5971e1acd00632f56905ad37a666d01a443d4dbf8fd90d195b32"
        );
    }

    #[test]
    fn oss_and_obs_v2_signatures_match_reference_vectors() {
        // Same string-to-sign, different keys → different signatures.
        let date = FIXED_HTTP_DATE;
        assert_eq!(
            aws_v2_style_signature("osssk", date),
            "KyQcY6wkRKeGDufbioWs6AJXbq4="
        );
        assert_eq!(
            aws_v2_style_signature("obssk", date),
            "cICVJB9ax0PRGt9o23eBUP8nAxU="
        );
    }

    #[test]
    fn cos_q_sign_matches_reference_vector() {
        let key_time = "1726531200;1726531800";
        let authorization = cos_authorization("COSID", "coskey", key_time);
        assert_eq!(
            authorization,
            "q-sign-algorithm=sha1&q-ak=COSID&q-sign-time=1726531200;1726531800\
             &q-key-time=1726531200;1726531800&q-header-list=&q-url-param-list=\
             &q-signature=8e2621b4f6e6b7773f6caf7c93a51671ee2ad58f"
        );
    }

    #[test]
    fn azblob_shared_key_matches_reference_vector() {
        let date = FIXED_HTTP_DATE;
        let authorization =
            azblob_authorization("acct", "YXprZXktMDEyMzQ1Njc4OQ==", date).unwrap();
        assert_eq!(
            authorization,
            "SharedKey acct:0ETykIlDHIZlxer0GpzTzZoAfIYSzXX36qvKCrcwa3E="
        );
        let error = azblob_authorization("acct", "not base64!!", date).unwrap_err();
        assert!(
            error.to_message().contains("base64"),
            "invalid key must explain itself: {error}"
        );
    }

    #[test]
    fn endpoint_host_extracts_authority() {
        assert_eq!(endpoint_host("https://s3.amazonaws.com"), "s3.amazonaws.com");
        assert_eq!(
            endpoint_host("http://127.0.0.1:9000/"),
            "127.0.0.1:9000"
        );
        assert_eq!(endpoint_host("HTTPS://MinIO.Local:9443"), "minio.local:9443");
    }

    #[test]
    fn bucket_names_parse_across_provider_shapes() {
        // AWS/oss/cos share the ListAllMyBucketsResult shape.
        let s3 = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListAllMyBucketsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Owner><ID>x</ID><DisplayName>y</DisplayName></Owner>
  <Buckets>
    <Bucket><CreationDate>2026-01-01T00:00:00Z</CreationDate><Name>alpha</Name></Bucket>
    <Bucket><CreationDate>2026-02-01T00:00:00Z</CreationDate><Name>beta.data</Name></Bucket>
  </Buckets>
</ListAllMyBucketsResult>"#;
        assert_eq!(
            parse_bucket_names(s3, "Bucket").unwrap(),
            vec!["alpha".to_string(), "beta.data".to_string()]
        );

        // Azure wraps containers in EnumerationResults.
        let azblob = r#"<?xml version="1.0" encoding="utf-8"?>
<EnumerationResults ServiceEndpoint="https://acct.blob.core.windows.net/">
  <Containers>
    <Container><Name>media</Name><Properties /></Container>
    <Container><Name>logs</Name><Properties /></Container>
  </Containers>
  <NextMarker />
</EnumerationResults>"#;
        assert_eq!(
            parse_bucket_names(azblob, "Container").unwrap(),
            vec!["media".to_string(), "logs".to_string()]
        );

        // Empty listing is valid and yields no names.
        let empty = r#"<ListAllMyBucketsResult><Buckets /></ListAllMyBucketsResult>"#;
        assert!(parse_bucket_names(empty, "Bucket").unwrap().is_empty());
    }

    #[test]
    fn bucket_names_decode_entities_and_normalize_eols() {
        // quick-xml 0.42 path: xml10_content decodes entities and normalizes
        // CRLF EOLs; CRLF between elements keeps whitespace-only nodes out of
        // the names (trim_text), and trim() guards pretty-printed names.
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\r\n\
                   <ListAllMyBucketsResult><Buckets>\r\n\
                   <Bucket><Name>weird&amp;name</Name></Bucket>\r\n\
                   <Bucket><Name>\r\n  spaced  </Name></Bucket>\r\n\
                   </Buckets></ListAllMyBucketsResult>";
        assert_eq!(
            parse_bucket_names(xml, "Bucket").unwrap(),
            vec!["weird&name".to_string(), "spaced".to_string()]
        );
    }

    #[test]
    fn malformed_and_error_bodies_are_handled() {
        assert!(parse_bucket_names("<Buckets><Bucket><Name>x", "Bucket").is_err());
        assert!(parse_bucket_names("not xml at all", "Bucket").is_err());

        let error_body = r#"<Error><Code>AccessDenied</Code><Message>denied</Message></Error>"#;
        assert_eq!(
            extract_error(error_body),
            (
                Some("AccessDenied".to_string()),
                Some("denied".to_string())
            )
        );
        assert_eq!(extract_error("plain gateway text"), (None, None));
    }

    #[test]
    fn permission_errors_carry_the_fill_bucket_hint() {
        let message = BucketListError::PermissionDenied("AccessDenied: denied".into()).to_message();
        assert!(message.contains("fill in the Bucket field"), "{message}");
        assert!(message.contains("AccessDenied"), "{message}");
    }
}
