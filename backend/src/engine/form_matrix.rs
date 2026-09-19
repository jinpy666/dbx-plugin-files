//! Connection-form × engine combination matrix (test-only).
//!
//! The manifest's connection form is the plugin contract: `visible_when`
//! decides which fields a user can set per protocol, `required_when` what
//! the host enforces before submit. This module replays that contract
//! against the engine so form×backend drift fails CI instead of a user's
//! connect dialog:
//!
//! - every combination the form allows (including "all visible fields
//!   filled" and "any visible optional field left empty") must parse AND
//!   build an Operator offline — no network, no panic;
//! - stale values left over from a previous protocol selection (the host
//!   keeps the whole `config` binding) must never break a build nor leak
//!   into another protocol's Builder kv;
//! - hostile scalar values (whitespace, zero/fractional timeouts, broken
//!   custom-service JSON) must produce clear errors, never panics.
//!
//! Both fixed regressions this module pins: fs with an empty root (the fs
//! service rejects an unset root while the form leaves root optional) and
//! the sftp quick protocol with the form's `Tolerate` strategy (the OpenDAL
//! service only speaks strict|accept|add and failed the build outright).

use super::*;
use serde_json::{json, Map, Value};

/// Throwaway ed25519 key generated for this matrix (never a real secret);
/// sftp-native loads key material at build time, so the form-complete
/// scenario needs a parseable key.
const TEST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACDYWlbov2OqpMHARd1lAkDHd+/t+jRS1Sz8e8CvJ6aQowAAAKBQ6icJUOon
CQAAAAtzc2gtZWQyNTUxOQAAACDYWlbov2OqpMHARd1lAkDHd+/t+jRS1Sz8e8CvJ6aQow
AAAEDbSbBLBlSXxC9fzxYV8VUnMyj5c0H0uPQ747xowWHDkNhaVui/Y6qkwcBF3WUCQMd3
7+36NFLVLPx7wK8nppCjAAAAGmRieC1maWxlcy1mb3JtLW1hdHJpeC10ZXN0AQID
-----END OPENSSH PRIVATE KEY-----";

// ---------------------------------------------------------------- form model

fn manifest() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../manifest.json");
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("manifest.json readable at {path}: {error}"));
    serde_json::from_str(&raw).expect("manifest.json parses as JSON")
}

struct FormField {
    key: String,
    ftype: String,
    binding: String,
    visible_when: Option<(String, Vec<String>)>,
    required_when: Option<(String, Vec<String>)>,
}

fn form_fields() -> Vec<FormField> {
    let manifest = manifest();
    let provider = manifest["contributions"]
        .as_array()
        .expect("contributions array")
        .iter()
        .find(|item| item["type"] == "connection-provider")
        .expect("connection-provider contribution")
        .clone();
    provider["fields"]
        .as_array()
        .expect("fields array")
        .iter()
        .map(|field| FormField {
            key: field["key"].as_str().expect("field key").to_string(),
            ftype: field["type"].as_str().unwrap_or_default().to_string(),
            binding: field["binding"].as_str().unwrap_or_default().to_string(),
            visible_when: condition_of(field.get("visible_when")),
            required_when: condition_of(field.get("required_when")),
        })
        .collect()
}

/// Keep the manifest-driven matrix scoped to protocols currently advertised by
/// the checked-in form. Backend-only protocols are covered by focused engine
/// tests until the integrator wires their fields into the shared manifest.
fn advertised_protocols() -> Vec<String> {
    let manifest = manifest();
    manifest["contributions"]
        .as_array()
        .expect("contributions array")
        .iter()
        .find(|item| item["type"] == "connection-provider")
        .expect("connection-provider")["fields"]
        .as_array()
        .expect("fields array")
        .iter()
        .find(|field| field["key"] == "protocol")
        .expect("protocol field")["options"]
        .as_array()
        .expect("protocol options")
        .iter()
        .map(|option| option["value"].as_str().expect("option value").to_string())
        .collect()
}

fn condition_of(condition: Option<&Value>) -> Option<(String, Vec<String>)> {
    condition.map(|condition| {
        let field = condition["field"].as_str().expect("condition.field");
        let one_of = condition["one_of"]
            .as_array()
            .expect("condition.one_of")
            .iter()
            .map(|value| value.as_str().expect("one_of value").to_string())
            .collect();
        (field.to_string(), one_of)
    })
}

/// Host/verify.mjs condition semantics: an empty controlling value disables
/// the condition, otherwise the current value must be in `one_of`.
fn condition_active(
    condition: &Option<(String, Vec<String>)>,
    values: &Map<String, Value>,
) -> bool {
    match condition {
        None => true,
        Some((field, one_of)) => {
            let current = values
                .get(field)
                .map(value_as_string)
                .unwrap_or_default();
            !current.trim().is_empty() && one_of.iter().any(|value| value == &current)
        }
    }
}

fn value_as_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(flag) => flag.to_string(),
        other => other.to_string(),
    }
}

/// A valid sample value per field; `endpoint` is protocol-shaped because the
/// scheme hygiene differs per backend. `root` points at a writable temp dir:
/// the fs service creates the root directory at build time, so a path like
/// `/srv` would fail on read-only host filesystems.
fn sample_value(key: &str, protocol: &str) -> Value {
    match key {
        "display_name" => json!("Matrix"),
        "protocol" => json!(protocol),
        "endpoint" => json!(match protocol {
            "s3" | "gcs" | "obs" | "oss" | "webdav" | "koofr" | "pcloud" | "seafile" => "http://127.0.0.1:9000",
            "azblob" => "https://account.blob.core.windows.net",
            "cos" => "https://cos.ap-guangzhou.myqcloud.com",
            "ftp" => "ftp://127.0.0.1:2121",
            "sftp" | "sftp-native" => "127.0.0.1:22",
            "smb" => "nas.local:445",
            _ => "",
        }),
        "bucket" => json!("demo"),
        "container" => json!("demo-container"),
        "account_name" => json!("account"),
        "account_key" => json!("YWNjb3VudC1rZXk="),
        "credential" => json!("credential"),
        "scope" => json!("https://www.googleapis.com/auth/devstorage.read_write"),
        "region" => json!("us-east-1"),
        "access_key_id" => json!("ak"),
        "secret_access_key" => json!("sk"),
        "secret_id" => json!("secret-id"),
        "secret_key" => json!("secret-key"),
        "security_token" => json!("security-token"),
        "access_token" => json!("access-token"),
        "client_id" => json!("client-id"),
        "client_secret" => json!("client-secret"),
        "refresh_token" => json!(match protocol {
            "dropbox" | "gdrive" | "onedrive" => "",
            _ => "refresh-token",
        }),
        "drive_type" => json!("resource"),
        "email" => json!("alice@example.com"),
        "repo_name" => json!("library"),
        "share" => json!("media"),
        "username" => json!("alice"),
        "user" => json!("bob"),
        "domain" => json!("WORKGROUP"),
        "password" => json!("pw"),
        "key" => json!(TEST_KEY),
        "service" => json!("memory"),
        "config" => json!(format!("{{\"root\":{}}}", serde_json::json!(matrix_root()))),
        "root" => json!(matrix_root()),
        "known_hosts_strategy" => json!("Tolerate"),
        // Proxy fields ship default "off"; the dependent host/port/username/
        // password samples stay inert because the flat parser short-circuits
        // on the off type (hidden-field superset semantics, see below).
        "proxy_type" => json!("off"),
        "proxy_host" => json!("127.0.0.1"),
        "proxy_port" => json!("1080"),
        "proxy_username" => json!("proxyuser"),
        "proxy_password" => json!("proxy-pw"),
        // Ships default "": no tunnel unless the user types a jump chain.
        "tunnel_jump_hosts" => json!(""),
        "tunnel_identity_file" => json!(""),
        "timeout_secs" => json!(30),
        "read_only" => json!(false),
        "allow_delete" => json!(true),
        "lock_to_root" => json!(false),
        "enable_virtual_host_style" => json!(false),
        other => panic!("form field '{other}' has no matrix sample value"),
    }
}

/// Shared writable root for the fs samples; kept alive (and cleaned up) for
/// the whole test process via the `TempDir` guard.
fn matrix_root() -> &'static str {
    static ROOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let dir = tempfile::TempDir::with_prefix("dbx-files-form-matrix")
            .expect("temp dir for matrix roots");
        let path = dir.path().join("root").to_string_lossy().into_owned();
        std::mem::forget(dir); // keep the directory alive until process exit
        path
    })
}

/// Host-shaped lifecycle params: the host submits the whole `config` binding,
/// so hidden fields keep their last value (stale superset input) while
/// non-empty secret-bound fields go to `connection_secrets`.
fn lifecycle_params(protocol: &str, overrides: &Map<String, Value>) -> Value {
    let fields = form_fields();
    let mut values: Map<String, Value> = fields
        .iter()
        .map(|field| (field.key.clone(), sample_value(&field.key, protocol)))
        .collect();
    for (key, value) in overrides {
        values.insert(key.clone(), value.clone());
    }
    values.insert("protocol".to_string(), json!(protocol));

    let mut external_config = Map::new();
    let mut connection_secrets = Map::new();
    let mut name = Value::Null;
    for field in &fields {
        let value = values.get(&field.key).cloned().unwrap_or(Value::Null);
        match field.binding.as_str() {
            "name" => name = value,
            "secret" => {
                let present = !value_as_string(&value).trim().is_empty()
                    || matches!(value, Value::Number(_) | Value::Bool(_));
                if present {
                    connection_secrets.insert(field.key.clone(), value);
                }
            }
            _ => {
                external_config.insert(field.key.clone(), value);
            }
        }
    }
    json!({
        "connection": {
            "id": "matrix",
            "name": name,
            "external_config": external_config,
            "connection_secrets": connection_secrets,
        }
    })
}

fn parse(params: &Value) -> StoredConnection {
    StoredConnection::from_lifecycle_params(params)
        .expect("matrix lifecycle params must always parse")
}

fn expected_scheme(protocol: &str) -> String {
    match protocol {
        "opendal-custom" => "memory".to_string(),
        other => other.to_string(),
    }
}

// ------------------------------------------------------------------- matrix

/// Every form-complete connection (all visible fields filled; hidden fields
/// keep stale values from another protocol) must parse and build offline.
#[test]
fn form_complete_connections_build_for_every_protocol() {
    for protocol in advertised_protocols() {
        let params = lifecycle_params(&protocol, &Map::new());
        let connection = parse(&params);
        match build_operator(&connection) {
            Ok(operator) => {
                assert_eq!(
                    operator.info().scheme(),
                    expected_scheme(&protocol),
                    "scheme mismatch for form-complete {protocol}"
                );
            }
            Err(error) => {
                // The OpenDAL sftp service (keyfile auth) is Unix-only; on
                // Windows the build must fail with the actionable
                // sftp-native hint instead.
                if cfg!(windows) && protocol == "sftp" {
                    assert!(
                        error.contains("sftp-native"),
                        "windows sftp must hint at sftp-native: {error}"
                    );
                } else {
                    panic!("form-complete {protocol} connection must build: {error}");
                }
            }
        }
    }
}

/// Any visible field the form lets stay empty must not break the build
/// (fs+empty-root regression); fields the host enforces via required_when
/// may fail the build, but only with a descriptive error.
#[test]
fn visible_field_left_empty_never_breaks_optional_builds() {
    for protocol in advertised_protocols() {
        for field in form_fields() {
            let blankable = matches!(
                field.ftype.as_str(),
                "text" | "password" | "textarea" | "number"
            );
            if !blankable {
                continue; // selects/booleans always carry a value
            }
            let visible = match &field.visible_when {
                None => true,
                Some((target, one_of)) => {
                    target == "protocol" && one_of.iter().any(|value| value == &protocol)
                }
            };
            if !visible {
                continue;
            }
            let mut overrides = Map::new();
            overrides.insert(
                field.key.clone(),
                if field.ftype == "number" {
                    Value::Null
                } else {
                    json!("")
                },
            );
            let params = lifecycle_params(&protocol, &overrides);
            let connection = parse(&params);
            let form_required = field
                .required_when
                .as_ref()
                .is_some_and(|(target, one_of)| {
                    target == "protocol" && one_of.iter().any(|value| value == &protocol)
                });
            match build_operator(&connection) {
                Ok(_) => {}
                Err(error) => {
                    if cfg!(windows) && protocol == "sftp" {
                        continue; // protocol itself unavailable, hint asserted elsewhere
                    }
                    // OAuth-backed services accept either an access token or
                    // a refresh-token flow; the manifest cannot express this
                    // cross-field OR requirement, so an empty one-of member
                    // is expected to fail the backend validation.
                    let oauth_one_of = matches!(
                        (protocol.as_str(), field.key.as_str()),
                        ("dropbox" | "gdrive" | "onedrive", "access_token" | "refresh_token")
                    );
                    assert!(
                        form_required || oauth_one_of,
                        "empty optional field must not break the build: {protocol}/{}: {error}",
                        field.key
                    );
                    assert!(
                        error.trim().len() > 10,
                        "required-field error must be descriptive: {protocol}/{}: {error}",
                        field.key
                    );
                }
            }
        }
    }
}

/// A stale superset (every config-bound field filled, whatever the protocol)
/// must never leak foreign keys into a protocol's Builder kv: switching the
/// protocol select in the form cannot smuggle e.g. `password` into sftp.
#[test]
fn stale_cross_protocol_values_never_leak_into_builder_kv() {
    let allowed: &[(&str, &[&str])] = &[
        ("fs", &["root"]),
        (
            "s3",
            &[
                "root",
                "bucket",
                "endpoint",
                "region",
                "access_key_id",
                "secret_access_key",
                "enable_virtual_host_style",
            ],
        ),
        ("gcs", &[
            "root", "bucket", "endpoint", "credential", "scope",
        ]),
        ("azblob", &[
            "root", "container", "endpoint", "account_name", "account_key",
        ]),
        ("obs", &[
            "root", "bucket", "endpoint", "access_key_id", "secret_access_key",
        ]),
        (
            "oss",
            &["root", "bucket", "endpoint", "access_key_id", "access_key_secret"],
        ),
        ("webdav", &["root", "endpoint", "username", "password"]),
        ("ftp", &["root", "endpoint", "user", "password"]),
        (
            "sftp",
            &["root", "endpoint", "user", "key", "known_hosts_strategy"],
        ),
    ];
    for (protocol, expected) in allowed {
        let connection = parse(&lifecycle_params(protocol, &Map::new()));
        let (_, kv) = protocol_kv(&connection).expect("form-complete kv must build");
        for (key, _) in kv {
            assert!(
                expected.contains(&key.as_str()),
                "{protocol}: unexpected Builder key '{key}' (cross-protocol leak)"
            );
        }
    }
}

/// The sftp quick protocol's strategy select maps onto the OpenDAL service's
/// strict|accept|add vocabulary — the form's `Tolerate` default previously
/// failed the build with "unknown known_hosts strategy".
#[test]
fn sftp_known_hosts_strategy_maps_onto_opendal_vocabulary() {
    for (form_value, expected) in [
        ("Tolerate", "accept"),
        ("Strict", "strict"),
        ("Trust", "accept"), // degrades: the service has no blanket-trust mode
        ("tolerate", "accept"),
        ("legacy-garbage", "accept"),
    ] {
        let mut overrides = Map::new();
        overrides.insert("known_hosts_strategy".to_string(), json!(form_value));
        let connection = parse(&lifecycle_params("sftp", &overrides));
        let (_, kv) = protocol_kv(&connection).expect("sftp kv must build");
        let strategy = kv
            .iter()
            .find(|(key, _)| key == "known_hosts_strategy")
            .map(|(_, value)| value.as_str())
            .expect("strategy key present");
        assert_eq!(
            strategy, expected,
            "form strategy '{form_value}' must map to '{expected}'"
        );
    }
}

/// fs with an untouched (empty) root field means the whole filesystem — the
/// same semantics as the built-in local connection — instead of the fs
/// service's cryptic "root is not specified" build error.
#[test]
fn fs_empty_root_defaults_to_filesystem_root() {
    let mut overrides = Map::new();
    overrides.insert("root".to_string(), json!(""));
    let connection = parse(&lifecycle_params("fs", &overrides));
    let (_, kv) = protocol_kv(&connection).expect("fs with empty root must build");
    let root = kv
        .iter()
        .find(|(key, _)| key == "root")
        .map(|(_, value)| value.as_str())
        .expect("root key present");
    assert_eq!(root, "/");

    build_operator(&connection).expect("fs operator builds with the defaulted root");
}

/// Bucket namespace (design 2026-09-17): an empty bucket/container on
/// s3/oss/cos/obs/azblob must build the namespace operator offline and keep
/// reporting the underlying scheme (so scheme-based assertions and
/// capabilities stay uniform); gcs without a bucket keeps failing the build
/// with a descriptive error (bucket stays form-required there).
#[test]
fn bucket_namespace_connections_build_for_every_namespace_protocol() {
    for protocol in ["s3", "oss", "cos", "obs", "azblob"] {
        let mut overrides = Map::new();
        overrides.insert("bucket".to_string(), json!(""));
        overrides.insert("container".to_string(), json!(""));
        let connection = parse(&lifecycle_params(protocol, &overrides));
        let operator = build_operator(&connection).unwrap_or_else(|error| {
            panic!("bucket-less {protocol} must build the namespace operator: {error}")
        });
        assert_eq!(
            operator.info().scheme(),
            protocol,
            "namespace operator reports the underlying scheme for {protocol}"
        );
        let capability = operator.info().capability();
        // copy follows the child service verbatim (all five namespace
        // children advertise native copy): same-bucket copies run server-side
        // and cross-bucket copies stream inside the namespace adapter.
        assert!(
            capability.copy,
            "{protocol}: namespace copy follows the child's native copy"
        );
    }

    let mut overrides = Map::new();
    overrides.insert("bucket".to_string(), json!(""));
    let gcs = parse(&lifecycle_params("gcs", &overrides));
    let error = build_operator(&gcs).expect_err("gcs without a bucket must fail the build");
    assert!(
        error.trim().len() > 10,
        "gcs bucket error must be descriptive: {error}"
    );
}

/// Hostile scalar combinations must parse to sane defaults and never panic:
/// the number input may reach us as 0, negative, fractional, string or
/// overflow; timeouts fall back to 30s.
#[test]
fn timeout_secs_edge_values_fall_back_to_default() {
    for (raw, expected) in [
        (json!(null), 30),
        (json!(0), 30),
        (json!(1), 1),
        (json!(30.5), 30),
        (json!(-5), 30),
        (json!("45"), 30),
        (json!(u64::MAX), u64::MAX),
        (json!(1e30), 30), // beyond u64
    ] {
        let mut overrides = Map::new();
        overrides.insert("timeout_secs".to_string(), raw);
        let connection = parse(&lifecycle_params("fs", &overrides));
        assert_eq!(
            connection.timeout_secs, expected,
            "timeout edge {overrides:?}"
        );
    }
}

/// The custom-service textarea accepts object/JSON-string shapes plus an
/// emptied field; broken JSON and non-objects fail with clear messages.
#[test]
fn custom_config_shapes_parse_and_build_or_explain() {
    let case = |config: Value| {
        let mut overrides = Map::new();
        overrides.insert("config".to_string(), config);
        parse(&lifecycle_params("opendal-custom", &overrides))
    };

    let cleared = case(json!(""));
    assert!(
        cleared.custom_config.as_object().unwrap().is_empty(),
        "cleared textarea means no service config"
    );

    let object = case(json!({ "bucket": "demo" }));
    assert_eq!(object.custom_config["bucket"], "demo");

    let text = case(json!("{\"bucket\": \"demo\"}"));
    assert_eq!(text.custom_config["bucket"], "demo");

    let malformed = || {
        let mut overrides = Map::new();
        overrides.insert("config".to_string(), json!("{not json"));
        StoredConnection::from_lifecycle_params(&lifecycle_params(
            "opendal-custom",
            &overrides,
        ))
    };
    let error = malformed().unwrap_err();
    assert!(
        error.contains("Invalid service config JSON"),
        "broken JSON must explain itself: {error}"
    );

    for hostile in [json!("[1,2]"), json!(42), json!(true)] {
        let mut overrides = Map::new();
        overrides.insert("config".to_string(), hostile.clone());
        let error = StoredConnection::from_lifecycle_params(&lifecycle_params(
            "opendal-custom",
            &overrides,
        ))
        .unwrap_err();
        assert!(
            error.contains("object") && error.contains("config"),
            "non-object config {hostile} must explain itself: {error}"
        );
    }
}

/// Whitespace-only scalar input (a form can always submit it) must never
/// panic; endpoints that survive the hygiene check keep building.
#[test]
fn whitespace_only_scalars_never_panic_the_builder() {
    for key in ["endpoint", "user", "username", "root", "bucket", "share"] {
        for protocol in [
            "fs", "s3", "gcs", "azblob", "obs", "oss", "webdav", "ftp", "smb", "sftp-native",
        ] {
            let mut overrides = Map::new();
            overrides.insert(key.to_string(), json!("   "));
            let connection = parse(&lifecycle_params(protocol, &overrides));
            // Either builds, or fails with a descriptive error — no panic.
            if let Err(error) = build_operator(&connection) {
                assert!(
                    error.trim().len() > 10,
                    "whitespace {protocol}/{key} error must be descriptive: {error}"
                );
            }
        }
    }
}
