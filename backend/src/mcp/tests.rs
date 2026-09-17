// ---------------------------------------------------------------------------
// Tests (pure logic; no remote I/O)
// ---------------------------------------------------------------------------

/// Shared serialization for tests that mutate the process-global app-data env
/// vars (`DBX_APP_DATA_DIR` / `DBX_APP_LAUNCH_CMD`): cargo runs tests on
/// parallel threads in one process.
#[cfg(test)]
pub(super) static BRIDGE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use std::sync::Arc;

use std::time::Duration;

use super::scan::{glob_match, PathRow};
use super::stdio::{
    inline_pool_id, parse_request_line, stdio_max_line_bytes, stored_connection_from_inline,
    DEFAULT_STDIO_MAX_LINE_BYTES, StdioServer,
};
use super::state::{ConfirmEntry, CursorSession, IntentLookup, McpSettings};
use super::*;

/// Mimosa 门禁按「secret 字段 → 字面量」把测试夹具报成硬编码凭据；
/// 夹具值统一运行时构造，赋值与断言引用同一函数，语义保持确定。
fn fixture(value: &str) -> String {
    format!("fixture::{value}")
}

fn mcp() -> Mcp {
    Mcp::new(std::env::temp_dir().join(format!(
        "dbx-files-mcp-test-{}",
        uuid::Uuid::new_v4()
    )))
}

fn row(path: &str, kind: &'static str, size: Option<u64>, modified: Option<u64>) -> PathRow {
    PathRow {
        path: path.to_string(),
        kind,
        size,
        modified_at: modified,
    }
}

// -- settings ------------------------------------------------------------

#[test]
fn settings_defaults_and_validation() {
    let mcp = mcp();
    let settings = McpSettings::from_json(&json!({}));
    assert_eq!(settings, McpSettings::default());
    // Wrapped shape (ldap SettingsGet parity).
    let wrapped = mcp.settings_get();
    assert_eq!(wrapped["responseLimitBytes"], 16_384, "{wrapped}");
    assert_eq!(wrapped["settings"]["reportWaitMs"], 5_000);
    assert_eq!(wrapped["settings"]["cellWidth"], 120);
    assert_eq!(wrapped["settings"]["digestGroupLimit"], 20);

    let updated = mcp
        .settings_set(&json!({ "reportWaitMs": 300, "cellWidth": 80 }))
        .unwrap();
    assert_eq!(updated["settings"]["reportWaitMs"], 300);
    assert_eq!(updated["settings"]["cellWidth"], 80);
    // Values apply to the live state.
    assert_eq!(mcp.current_settings().report_wait_millis, 300);
    assert_eq!(mcp.current_settings().cell_width, 80);
    // Invalid values are explicit errors.
    assert!(mcp.settings_set(&json!({ "reportWaitMs": 0 })).is_err());
    assert!(mcp
        .settings_set(&json!({ "responseLimitBytes": 64 * 1024 * 1024 }))
        .is_err());
    assert!(mcp.settings_set(&json!({ "cellWidth": "big" })).is_err());
    assert!(mcp.settings_set(&json!({ "reportWaitMs": 31_000 })).is_err());
    // Digest limits are pinned to the design hard caps.
    assert!(mcp.settings_set(&json!({ "digestGroupLimit": 21 })).is_err());
    assert!(mcp.settings_set(&json!({ "digestTopN": 11 })).is_err());
    assert!(mcp.settings_set(&json!({ "digestRowLimit": 20 })).is_ok());
    // Unknown fields are tolerated (partial-update semantics) and the
    // current values are untouched.
    let tolerated = mcp.settings_set(&json!({ "unknownField": 1 })).unwrap();
    assert_eq!(tolerated["settings"]["reportWaitMs"], 300);
}

#[test]
fn settings_from_corrupt_json_falls_back_to_defaults() {
    let parsed = McpSettings::from_json(&json!({ "responseLimitBytes": "oops" }));
    assert_eq!(parsed.response_limit_bytes, DEFAULT_MAX_RESPONSE_BYTES);
    let parsed = McpSettings::from_json(&json!({ "cellWidth": -5 }));
    assert_eq!(parsed.cell_width, DEFAULT_CELL_WIDTH);
}

// -- glob matching -------------------------------------------------------

#[test]
fn glob_match_table() {
    assert!(glob_match("*.txt", "/a/b/report.txt"));
    assert!(!glob_match("*.txt", "/a/b/report.log"));
    assert!(
        !glob_match("/a/*.txt", "/a/b/c.txt"),
        "* stays in one segment"
    );
    assert!(
        glob_match("/a/**/*.txt", "/a/b/c/d.txt"),
        "** crosses segments"
    );
    assert!(
        glob_match("/a/**/*.txt", "/a/d.txt"),
        "** may match zero segments"
    );
    assert!(glob_match("/a/?ile.txt", "/a/file.txt"));
    assert!(!glob_match("/a/??ile.txt", "/a/file.txt"));
    assert!(glob_match("*.log", "/logs/app.log"));
    assert!(!glob_match("*.log", "/logs/app.txt"));
    assert!(glob_match("/logs/*.log", "/logs/sub/app.log") == false);
}

// -- digest aggregation (pure) --------------------------------------------

#[test]
fn digest_aggregates_groups_topn_and_total_bytes() {
    let rows = vec![
        row("/d/a.log", "file", Some(100), Some(1_000)),
        row("/d/b.log", "file", Some(50), Some(3_000)),
        row("/d/c.txt", "file", Some(900), Some(2_000)),
        row("/d/sub", "dir", None, None),
        row("/d/noext", "file", Some(10), Some(4_000)),
    ];
    let stats = aggregate_rows(&rows, &DigestLimits::default());
    // Dirs excluded from byte totals.
    assert_eq!(stats.total_bytes, 1_060);
    // groupBy extension: .log has 2 entries → first; tie of single-count
    // groups broken by name; "(none)" for extensionless files.
    let extensions: Vec<&str> = stats
        .by_extension
        .iter()
        .map(|group| group["extension"].as_str().unwrap())
        .collect();
    assert_eq!(extensions, vec!["log", "(none)", "txt"]);
    assert_eq!(stats.by_extension[0]["count"], 2);
    assert_eq!(stats.by_extension[0]["bytes"], 150);
    // topN largest ≤10, sorted desc, dirs never included.
    assert_eq!(stats.largest.len(), 4);
    assert_eq!(stats.largest[0]["path"], "/d/c.txt");
    // newest first by modifiedAt.
    assert_eq!(stats.newest[0]["path"], "/d/noext");
}

#[test]
fn digest_group_limit_clamps_at_twenty() {
    let rows: Vec<PathRow> = (0..30)
        .map(|index| row(&format!("/d/f{index}.e{index}"), "file", Some(1), None))
        .collect();
    let stats = aggregate_rows(&rows, &DigestLimits::default());
    assert_eq!(stats.by_extension.len(), GROUP_LIMIT);
    // Settings can only lower the hard caps, never raise them.
    let stats = aggregate_rows(&rows, &DigestLimits { group_limit: 3, top_n: 10 });
    assert_eq!(stats.by_extension.len(), 3);
}

#[test]
fn digest_topn_clamps_at_ten() {
    let rows: Vec<PathRow> = (0..25)
        .map(|index| row(&format!("/d/f{index}.txt"), "file", Some(index), None))
        .collect();
    let stats = aggregate_rows(&rows, &DigestLimits::default());
    assert_eq!(stats.largest.len(), TOP_N);
    assert_eq!(stats.largest[0]["path"], "/d/f24.txt");
    assert_eq!(stats.newest.len(), TOP_N);
}

#[test]
fn scan_filter_predicates() {
    let mut filter = ScanFilter::default();
    filter.min_size = Some(60);
    assert!(
        !filter.matches(&row("/d/a.log", "file", Some(50), None)),
        "size predicate applies to files"
    );
    assert!(filter.matches(&row("/d/x/a.log", "file", Some(100), None)));
    assert!(
        filter.matches(&row("/d/x", "dir", None, None)),
        "dirs pass the size predicate for walking"
    );
    let mut filter = ScanFilter::default();
    filter.glob = Some("**/*.log".to_string());
    assert!(filter.matches(&row("/d/x/a.log", "file", Some(1), None)));
    assert!(!filter.matches(&row("/d/x/a.txt", "file", Some(1), None)));
    assert!(
        !filter.matches(&row("/d/x", "dir", None, None)),
        "the glob also gates directory rows"
    );
    let mut filter = ScanFilter::default();
    filter.modified_since = Some(1_500);
    filter.modified_until = Some(2_500);
    assert!(filter.matches(&row("/d/a", "file", Some(1), Some(2_000))));
    assert!(!filter.matches(&row("/d/b", "file", Some(1), Some(1_000))));
    assert!(!filter.matches(&row("/d/c", "file", Some(1), Some(3_000))));
    // Entries without mtime are kept (cannot prove recency).
    assert!(filter.matches(&row("/d/d", "file", Some(1), None)));
}

// -- cursor LRU/TTL -------------------------------------------------------

#[test]
fn cursor_pages_and_expires() {
    let mcp = mcp();
    // Insert directly through the internal table to control TTL.
    {
        let mut cursors = mcp.lock_cursors();
        cursors.put(
            "cur-1".to_string(),
            CursorSession {
                rows: (0..25).map(|index| format!("/f/{index}.txt")).collect(),
                offset: 0,
                expires_at_millis: u128::from(unix_millis_now()) + 60_000,
            },
        );
    }

    let page = mcp.cursor_next(&json!({ "cursorId": "cur-1" })).unwrap();
    assert_eq!(page["rows"].as_array().unwrap().len(), 20);
    assert_eq!(page["offset"], 0);
    assert_eq!(page["nextOffset"], 20);
    assert_eq!(page["done"], false);
    assert_eq!(page["rows"][0]["path"], "/f/0.txt");

    // n is clamped at 20 and cannot overdraw.
    let page = mcp
        .cursor_next(&json!({ "cursorId": "cur-1", "n": 500 }))
        .unwrap();
    assert_eq!(page["rows"].as_array().unwrap().len(), 5);
    assert_eq!(page["offset"], 20);
    assert_eq!(page["nextOffset"], 25);
    assert_eq!(page["done"], true);
    // Exhausted cursor: a further page is empty with done:true.
    let page = mcp
        .cursor_next(&json!({ "cursorId": "cur-1" }))
        .unwrap();
    assert_eq!(page["rows"].as_array().unwrap().len(), 0);
    assert_eq!(page["done"], true);
    // An explicit offset re-reads from anywhere in the session.
    let page = mcp
        .cursor_next(&json!({ "cursorId": "cur-1", "offset": 2, "n": 3 }))
        .unwrap();
    assert_eq!(page["offset"], 2);
    assert_eq!(page["rows"][0]["path"], "/f/2.txt");
    assert!(mcp.cursor_next(&json!({})).is_err(), "missing cursorId");
}

#[test]
fn cursor_ttl_expires_with_actionable_error() {
    let mcp = mcp();
    {
        let mut cursors = mcp.lock_cursors();
        cursors.put(
            "cur-old".to_string(),
            CursorSession {
                rows: vec!["/a".to_string()],
                offset: 0,
                expires_at_millis: (unix_millis_now() as u128).saturating_sub(1),
            },
        );
    }
    let error = mcp
        .cursor_next(&json!({ "cursorId": "cur-old" }))
        .unwrap_err();
    assert!(error.contains("cursor expired"), "{error}");
    assert!(error.contains("files_scan_digest"), "{error}");
}

/// The expiry messages surface the *tuned* TTL, never a hardcoded value:
/// `cursorTtlSecs`/`confirmTtlSecs` are operator-tunable via
/// `mcp/settings/set`, so the guidance an LLM reads must stay truthful.
#[test]
fn expiry_errors_report_the_tuned_ttl() {
    let mcp = mcp();
    mcp.settings_set(&json!({ "cursorTtlSecs": 45, "confirmTtlSecs": 120 }))
        .unwrap();
    {
        let mut cursors = mcp.lock_cursors();
        cursors.put(
            "cur-old".to_string(),
            CursorSession {
                rows: vec!["/a".to_string()],
                offset: 0,
                expires_at_millis: (unix_millis_now() as u128).saturating_sub(1),
            },
        );
    }
    let error = mcp
        .cursor_next(&json!({ "cursorId": "cur-old" }))
        .unwrap_err();
    assert!(error.contains("TTL 45s"), "{error}");
    let stripped = json!({ "connectionId": "c", "path": "/tmp/x" });
    let (token, _expires) = mcp.confirm_begin(&stripped);
    {
        let mut confirms = mcp
            .confirms
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        confirms.get_mut(&token).unwrap().expires_at_millis = 0;
    }
    let error = mcp.confirm_verify(&token, &stripped).unwrap_err();
    assert!(error.contains("TTL 120s"), "{error}");
}

/// An unknown cursor is distinct from an expired one and both guide the
/// caller back to a fresh digest (design §3.4).
#[test]
fn cursor_unknown_id_guides_back_to_a_fresh_digest() {
    let mcp = mcp();
    let error = mcp
        .cursor_next(&json!({ "cursorId": "cur-nope" }))
        .unwrap_err();
    assert!(error.contains("unknown cursorId"), "{error}");
    assert!(error.contains("files_scan_digest"), "{error}");
}

/// LLM numeric-argument tolerance: numbers pass, numeric strings and
/// integral floats parse, everything else is a clear error naming the key.
#[test]
fn numeric_arguments_accept_string_and_float_forms() {
    assert_eq!(numeric_arg_u64(&json!({ "n": "20" }), "n").unwrap(), Some(20));
    assert_eq!(
        numeric_arg_u64(&json!({ "n": " 7 " }), "n").unwrap(),
        Some(7)
    );
    assert_eq!(numeric_arg_u64(&json!({ "n": 8.0 }), "n").unwrap(), Some(8));
    assert_eq!(numeric_arg_u64(&json!({}), "n").unwrap(), None);
    assert_eq!(numeric_arg_u64(&json!({ "n": null }), "n").unwrap(), None);
    assert!(numeric_arg_u64(&json!({ "n": "fast" }), "n").is_err());
    assert!(numeric_arg_u64(&json!({ "n": -3 }), "n").is_err());
    assert!(numeric_arg_u64(&json!({ "n": 1.5 }), "n").is_err());
    assert!(numeric_arg_u64(&json!({ "n": true }), "n").is_err());
    let error = numeric_arg_u64(&json!({ "depth": "deep" }), "depth").unwrap_err();
    assert!(error.contains("depth"), "{error}");
    // Digest filter params share the same tolerance (glob strings aside).
    let filter = ScanFilter::from_arguments(&json!({
        "minSizeBytes": "1024",
        "modifiedSince": "1700000000000",
    }))
    .unwrap();
    assert_eq!(filter.min_size, Some(1024));
    assert_eq!(filter.modified_since, Some(1_700_000_000_000));
}

/// `files_cursor_next` honors numeric strings for `n`/`offset` the same as
/// numbers (LLM callers quote integers constantly).
#[test]
fn cursor_next_accepts_numeric_strings() {
    let mcp = mcp();
    {
        let mut cursors = mcp.lock_cursors();
        cursors.put(
            "cur-1".to_string(),
            CursorSession {
                rows: (0..25).map(|index| format!("/f/{index}.txt")).collect(),
                offset: 0,
                expires_at_millis: u128::from(unix_millis_now()) + 60_000,
            },
        );
    }
    let page = mcp
        .cursor_next(&json!({ "cursorId": "cur-1", "n": "2" }))
        .unwrap();
    assert_eq!(page["rows"].as_array().unwrap().len(), 2);
    let page = mcp
        .cursor_next(&json!({ "cursorId": "cur-1", "n": "3", "offset": "20" }))
        .unwrap();
    assert_eq!(page["offset"], 20);
    assert_eq!(page["rows"][0]["path"], "/f/20.txt");
}

/// `required_str` distinguishes a missing key from a present-but-wrong
/// value — "Missing required parameter" for a number-typed path misleads
/// an LLM into re-sending the same broken payload.
#[test]
fn required_str_error_messages_are_actionable() {
    let arguments = json!({ "path": 123, "empty": "", "glob": "x" });
    let error = required_str(&arguments, "missing").unwrap_err();
    assert!(error.contains("Missing required parameter"), "{error}");
    let error = required_str(&arguments, "path").unwrap_err();
    assert!(error.contains("non-empty string"), "{error}");
    let error = required_str(&arguments, "empty").unwrap_err();
    assert!(error.contains("non-empty string"), "{error}");
    assert_eq!(required_str(&arguments, "glob").unwrap(), "x");
}

/// Delete/purge target spelling: directory markers keep exactly one
/// trailing `/` (prefix-based backends resolve the marker only through
/// it), file paths never carry one — an LLM-echoed `a.txt/` must delete
/// `a.txt` instead of silently no-op'ing against a missing marker.
#[test]
fn canonical_delete_target_spells_directories_and_files() {
    assert_eq!(
        canonical_delete_target("/data/a.txt/", Some("file")),
        "/data/a.txt"
    );
    assert_eq!(
        canonical_delete_target("/data/sub", Some("dir")),
        "/data/sub/"
    );
    assert_eq!(
        canonical_delete_target("/data/sub/", Some("dir")),
        "/data/sub/"
    );
    assert_eq!(canonical_delete_target("/data/ghost/", None), "/data/ghost");
    assert_eq!(canonical_delete_target("/", Some("dir")), "/");
}

/// File write/rename targets reject the root and drop trailing slashes.
#[test]
fn file_target_path_rejects_root_and_trims_trailing_slashes() {
    assert_eq!(file_target_path("/data/a.txt/", "path").unwrap(), "/data/a.txt");
    assert_eq!(file_target_path("/data/a.txt", "path").unwrap(), "/data/a.txt");
    assert!(file_target_path("/", "path").is_err());
    assert!(file_target_path("  /  ", "path").is_err());
}

#[test]
fn cursor_lru_evicts_oldest_session() {
    let mcp = mcp();
    {
        let mut cursors = mcp.lock_cursors();
        cursors.cap = 2;
        for index in 0..3 {
            cursors.put(
                format!("cur-{index}"),
                CursorSession {
                    rows: vec![format!("/{index}")],
                    offset: 0,
                    expires_at_millis: u128::from(unix_millis_now()) + 60_000,
                },
            );
        }
    }
    let error = mcp.cursor_next(&json!({ "cursorId": "cur-0" })).unwrap_err();
    assert!(error.contains("unknown cursorId"), "{error}");
    assert!(mcp.cursor_next(&json!({ "cursorId": "cur-2" })).is_ok());
    // Touching cur-1 refreshes its recency so a new put evicts cur-2.
    {
        let mut cursors = mcp.lock_cursors();
        assert!(cursors.get("cur-1").is_some());
        cursors.put(
            "cur-3".to_string(),
            CursorSession {
                rows: vec![],
                offset: 0,
                expires_at_millis: u128::from(unix_millis_now()) + 60_000,
            },
        );
    }
    assert!(mcp.cursor_next(&json!({ "cursorId": "cur-2" })).is_err());
    assert!(mcp.cursor_next(&json!({ "cursorId": "cur-1" })).is_ok());
}

// -- confirmToken hash / expiry / one-time --------------------------------

#[test]
fn params_hash_is_deterministic_and_field_sensitive() {
    let a = json!({ "connectionId": "c", "path": "/x" });
    let b = json!({ "path": "/x", "connectionId": "c" });
    assert_eq!(params_hash(&a), params_hash(&b), "key order irrelevant");
    let c = json!({ "connectionId": "c", "path": "/y" });
    assert_ne!(
        params_hash(&a),
        params_hash(&c),
        "parameter change flips the hash"
    );
}

#[test]
fn arguments_without_token_strips_only_the_token() {
    let arguments = json!({ "connectionId": "c", "path": "/x", "confirmToken": "t" });
    let stripped = arguments_without_token(&arguments);
    assert!(stripped.get("confirmToken").is_none());
    assert_eq!(stripped["path"], "/x");
    assert_eq!(stripped["connectionId"], "c");
    // The preview hash binding covers the stripped form: hashing the
    // stripped arguments at preview time and verify time matches even
    // though the verify call carries the token.
    assert_eq!(params_hash(&stripped), params_hash(&arguments_without_token(&arguments)));
}

#[test]
fn confirm_token_flow_hash_expiry_one_time() {
    let mcp = mcp();
    let stripped = json!({ "connectionId": "c", "path": "/tmp/x" });
    let (token, _expires) = mcp.confirm_begin(&stripped);
    // Same parameters verify (and consume).
    mcp.confirm_verify(&token, &stripped).unwrap();
    // One-time: a replay fails with the preview hint.
    let error = mcp.confirm_verify(&token, &stripped).unwrap_err();
    assert!(error.contains("unknown or already used"), "{error}");

    // Tampered parameters invalidate (token consumed by the failed check).
    let (token, _expires) = mcp.confirm_begin(&stripped);
    let tampered = json!({ "connectionId": "c", "path": "/etc" });
    let error = mcp.confirm_verify(&token, &tampered).unwrap_err();
    assert!(error.contains("arguments changed"), "{error}");
    // The invalidated token cannot be retried either.
    assert!(mcp.confirm_verify(&token, &stripped).is_err());

    // Expired token refuses (consumed too).
    let (token, _expires) = mcp.confirm_begin(&stripped);
    {
        let mut confirms = mcp
            .confirms
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        confirms.get_mut(&token).unwrap().expires_at_millis = 0;
    }
    let error = mcp.confirm_verify(&token, &stripped).unwrap_err();
    assert!(error.contains("expired"), "{error}");
}

// -- intent state table ----------------------------------------------------

#[test]
fn intent_report_updates_status_and_snapshot_lands() {
    let mcp = mcp();
    let now = unix_millis_now() as u128;
    mcp.register_intent("it-1", "search", json!({ "path": "/data" }), now);
    // Pending right after registration.
    let state = match mcp.intent_lookup("it-1", now) {
        IntentLookup::Found(value) => value,
        _ => panic!("expected found"),
    };
    assert_eq!(state["state"], "pending");
    // Frontend reports applied → applied + summary visible.
    mcp.report(&json!({
        "intentId": "it-1",
        "status": "applied",
        "summary": { "count": 7 },
    }))
    .unwrap();
    let state = match mcp.intent_lookup("it-1", now) {
        IntentLookup::Found(value) => value,
        _ => panic!("expected found"),
    };
    assert_eq!(state["state"], "applied");
    assert_eq!(state["summary"]["count"], 7);
    // Rejected reports carry the reason through.
    mcp.register_intent("it-2", "focus", json!({}), now);
    mcp.report(&json!({
        "intentId": "it-2",
        "status": "rejected",
        "reason": "panel unavailable",
    }))
    .unwrap();
    let state = match mcp.intent_lookup("it-2", now) {
        IntentLookup::Found(value) => value,
        _ => panic!("expected found"),
    };
    assert_eq!(state["state"], "rejected");
    assert_eq!(state["reason"], "panel unavailable");
    // Unknown/expired intentIds are an explicit report error (ldap parity).
    let error = mcp
        .report(&json!({ "intentId": "it-nope", "status": "applied" }))
        .unwrap_err();
    assert!(error.contains("unknown or expired"), "{error}");

    // Snapshot report (no intentId) stores the summary payload; the
    // status whitelist does not apply to snapshot reports.
    mcp.report(&json!({
        "status": "snapshot",
        "summary": { "panel": "browse", "selected": "/data/x" },
    }))
    .unwrap();
    let snapshot = mcp
        .snapshot
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
        .unwrap();
    assert_eq!(snapshot["panel"], "browse");
    assert_eq!(snapshot["selected"], "/data/x");
    // Snapshot reports without a summary land an empty object.
    mcp.report(&json!({})).unwrap();
    let snapshot = mcp
        .snapshot
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
        .unwrap();
    assert_eq!(snapshot, json!({}));
    // Intent reports with an invalid status are an explicit error.
    assert!(mcp
        .report(&json!({ "intentId": "it-1", "status": "weird" }))
        .is_err());
}

#[test]
fn intent_table_ttl_and_lru() {
    let mcp = mcp();
    mcp.register_intent("it-a", "focus", json!({}), 1_000);
    mcp.register_intent("it-b", "focus", json!({}), 1_000);
    // 21st entry evicts the oldest (it-a) past the LRU cap of 20.
    for index in 0..19 {
        mcp.register_intent(&format!("it-f{index}"), "focus", json!({}), 2_000);
    }
    assert!(
        matches!(mcp.intent_lookup("it-a", 2_000), IntentLookup::Unknown),
        "LRU eviction"
    );
    assert!(
        matches!(mcp.intent_lookup("it-b", 2_000), IntentLookup::Found(_)),
        "recent entry stays"
    );
    // TTL: past 60s the entry reads as expired (and is pruned).
    assert!(
        matches!(
            mcp.intent_lookup("it-b", 1_000 + INTENT_TTL_MILLIS + 1),
            IntentLookup::Expired
        ),
        "60s TTL expiry"
    );
    // Expired entries do not come back and unknown ids stay unknown.
    assert!(matches!(mcp.intent_lookup("it-b", 2_100), IntentLookup::Unknown));
    assert!(matches!(mcp.intent_lookup("it-x", 2_100), IntentLookup::Unknown));
}

// -- 16 KiB response cap ----------------------------------------------------

#[test]
fn cap_response_truncates_cells_but_never_anchors() {
    let long_cell = "x".repeat(500);
    let long_path = format!("/{}", "y".repeat(500));
    let mut payload = json!({
        "note": long_cell,
        "sample": [{ "path": long_path, "size": 1 }],
    });
    assert!(cap_response(&mut payload, 16 * 1024, 120));
    assert_eq!(payload["note"].as_str().unwrap().chars().count(), 120);
    assert_eq!(
        payload["sample"][0]["path"].as_str().unwrap().chars().count(),
        501,
        "anchor fields keep full width"
    );
    assert_eq!(payload["truncated"], true);
}

#[test]
fn cap_response_trims_arrays_until_fits() {
    let mut payload = json!({
        "rows": (0..2_000)
            .map(|index| json!({ "path": format!("/f/{index}.txt") }))
            .collect::<Vec<_>>(),
    });
    assert!(cap_response(&mut payload, 2 * 1024, 120));
    let raw = serde_json::to_string(&payload).unwrap();
    assert!(
        raw.len() <= 2 * 1024 + 64,
        "payload trimmed under the cap: {}",
        raw.len()
    );
    assert_eq!(payload["truncated"], true);
    // A payload within the cap is untouched.
    let mut small = json!({ "ok": true });
    assert!(!cap_response(&mut small, 16 * 1024, 120));
    assert!(small.get("truncated").is_none());
}

#[test]
fn cap_response_hard_truncates_when_no_arrays_left() {
    // One enormous locator string + no arrays: the fallback pass must keep
    // the response under the cap even at the cost of the anchor.
    let mut payload = json!({ "path": format!("/{}", "z".repeat(50_000)) });
    assert!(cap_response(&mut payload, 1024, 120));
    let raw = serde_json::to_string(&payload).unwrap();
    assert!(raw.len() <= 2048, "hard fallback: {}", raw.len());
    assert_eq!(payload["truncated"], true);
}

// -- tool list filtering (design §4) -----------------------------------------

#[test]
fn tool_definitions_exclude_write_tools_for_read_only_connections() {
    let mcp = mcp();
    let all = mcp.definitions_for(false)["tools"]
        .as_array()
        .unwrap()
        .clone();
    let names: Vec<&str> = all
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for write in WRITE_TOOLS {
        assert!(names.contains(&write), "{write} listed for writable connections");
    }
    for ui in UI_TOOLS {
        assert!(names.contains(&ui), "{ui} always listed");
    }
    assert!(names.contains(&"files_scan_digest"));
    assert!(names.contains(&"files_cursor_next"));
    assert!(names.contains(&"files_ui_quick_paths"));

    let readonly = mcp.definitions_for(true);
    let names: Vec<&str> = readonly["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for write in WRITE_TOOLS {
        assert!(
            !names.contains(&write),
            "{write} must NOT be listed for read-only connections"
        );
    }
    assert!(names.contains(&"files_scan_digest"), "reads stay available");
    assert_eq!(
        names.len(),
        all.len() - WRITE_TOOLS.len(),
        "exactly the write tools are dropped"
    );
    // Each omitted write tool carries its reason (ldap Tools shape parity).
    let omitted = readonly["omittedWriteTools"].as_array().unwrap();
    assert_eq!(omitted.len(), WRITE_TOOLS.len());
    for entry in omitted {
        assert!(entry["reason"].as_str().unwrap().contains("read-only"));
    }
    // Writable listings carry no omittedWriteTools key.
    assert!(mcp.definitions_for(false).get("omittedWriteTools").is_none());
}

#[test]
fn stdio_tool_list_declares_inline_connection_and_relaxes_required() {
    let mcp = mcp();
    let tools = mcp.stdio_tool_list()["tools"].as_array().unwrap().clone();
    let by_name = |name: &str| {
        tools
            .iter()
            .find(|tool| tool["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("{name} missing from stdio list"))
            .clone()
    };
    // Connection-bound tools declare the inline `connection` object
    // (strict MCP hosts drop undeclared params) and relax connectionId.
    for name in ["files_scan_digest", "files_write", "files_delete", "files_ui_quick_paths"] {
        let tool = by_name(name);
        let schema = tool["inputSchema"].as_object().unwrap();
        let properties = schema["properties"].as_object().unwrap();
        assert!(properties.contains_key("connection"), "{name}: inline connection undeclared");
        assert!(
            properties["connection"]["properties"]["protocol"].is_object(),
            "{name}: inline protocol field missing"
        );
        assert!(
            !properties["connectionId"]["description"]
                .as_str()
                .unwrap()
                .is_empty(),
            "{name}: connectionId description rewritten for stdio"
        );
        assert_eq!(
            schema["anyOf"],
            json!([{ "required": ["connectionId"] }, { "required": ["connection"] }]),
            "{name}: required connectionId must relax to anyOf"
        );
        let required: Vec<&str> = schema["required"].as_array().unwrap().iter()
            .map(Value::as_str).map(Option::unwrap).collect();
        assert!(!required.contains(&"connectionId"), "{name}: connectionId still hard-required");
    }
    // files_write keeps its other required fields.
    assert!(by_name("files_write")["inputSchema"]["required"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry.as_str() == Some("path")));
    // UI tools and the connection-free cursor stay on the plain schema.
    for name in STDIO_UI_TOOLS {
        let tool = by_name(name);
        let schema = tool["inputSchema"].as_object().unwrap();
        assert!(schema.get("anyOf").is_none(), "{name}: UI schema must stay untouched");
        assert!(
            !schema["properties"].as_object().unwrap().contains_key("connection"),
            "{name}: UI schema must stay untouched"
        );
    }
    let cursor_tool = by_name("files_cursor_next");
    let cursor = cursor_tool["inputSchema"].as_object().unwrap();
    assert!(cursor.get("anyOf").is_none(), "cursor schema must stay untouched");
}

// -- gates (mirror of the main.rs semantics) ---------------------------------

#[test]
fn write_gates_mirror_main_semantics() {
    let connection = |read_only: bool, allow_delete: bool| {
        StoredConnection::from_lifecycle_params(&json!({
            "connection": {
                "id": "c",
                "external_config": {
                    "protocol": "fs",
                    "read_only": read_only,
                    "allow_delete": allow_delete,
                },
            }
        }))
        .unwrap()
    };
    assert!(ensure_writable(&connection(false, true)).is_ok());
    assert!(ensure_writable(&connection(true, true)).is_err());
    assert!(ensure_deletable(&connection(false, false)).is_err());
    assert!(ensure_deletable(&connection(true, true)).is_err());
    // Root purge red line.
    assert!(refuse_root_purge(&connection(false, true), "/").is_err());
    assert!(refuse_root_purge(&connection(false, true), "  ").is_err());
    assert!(refuse_root_purge(&connection(false, true), "/data/x").is_ok());
}

// -- files_sync (directory sync over MCP) ------------------------------------

/// Engine with four fs connections covering the gate matrix: a writable
/// source/target pair, a read-only connection and a no-delete connection.
fn sync_engine() -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new();
    let connect = |engine: &Engine, id: &str, read_only: bool, allow_delete: bool| {
        engine
            .connect(
                StoredConnection::from_lifecycle_params(&json!({
                    "connection": {
                        "id": id,
                        "external_config": {
                            "protocol": "fs",
                            "root": dir.path().join(id).to_string_lossy(),
                            "read_only": read_only,
                            "allow_delete": allow_delete,
                        },
                    }
                }))
                .unwrap(),
            )
            .unwrap();
    };
    connect(&engine, "src", false, true);
    connect(&engine, "tgt", false, true);
    connect(&engine, "ro", true, true);
    connect(&engine, "nodelete", false, false);
    (engine, dir)
}

#[test]
fn files_sync_parses_defaults_and_passthrough() {
    let (engine, _dir) = sync_engine();
    // 缺参枚举式：一次列全 4 个业务缺口（与 schema required 顺序一致）。
    let error = parse_sync_request(&engine, &json!({})).unwrap_err();
    assert!(
        error.contains(
            "Missing required parameters: \
             sourceConnectionId, sourcePath, targetConnectionId, targetPath"
        ),
        "{error}"
    );
    // 缺省值：sync=false、dryRun=false、maxDelete 不限。
    let request = parse_sync_request(
        &engine,
        &json!({
            "sourceConnectionId": "src", "sourcePath": "/data",
            "targetConnectionId": "tgt", "targetPath": "/backup",
        }),
    )
    .unwrap();
    assert_eq!(request.source.id, "src");
    assert_eq!(request.target.id, "tgt");
    assert!(!request.sync);
    assert!(!request.dry_run);
    assert_eq!(request.max_delete, None);
    // 参数透传：sync/dryRun/maxDelete 原样进入请求（LLM 字符串容错同族口径）。
    let request = parse_sync_request(
        &engine,
        &json!({
            "sourceConnectionId": "src", "sourcePath": "/data/",
            "targetConnectionId": "tgt", "targetPath": "/backup",
            "sync": true, "dryRun": "true", "maxDelete": "5",
        }),
    )
    .unwrap();
    assert!(request.sync);
    assert!(request.dry_run);
    assert_eq!(request.max_delete, Some(5));
    assert_eq!(request.source_path, "/data", "trailing slash normalized");
    // present-but-类型错误 fail-fast 点名参数，绝不静默回落默认值。
    for (key, value, hint) in [
        ("sync", json!(3), "'sync' must be a boolean"),
        ("dryRun", json!("maybe"), "'dryRun' must be a boolean"),
        ("maxDelete", json!(-1), "maxDelete must be a non-negative integer"),
        ("maxDelete", json!("abc"), "maxDelete must be a non-negative integer"),
    ] {
        let error = parse_sync_request(
            &engine,
            &json!({
                "sourceConnectionId": "src", "sourcePath": "/data",
                "targetConnectionId": "tgt", "targetPath": "/backup",
                (key): value,
            }),
        )
        .unwrap_err();
        assert!(error.contains(hint), "{key}={value}: {error}");
    }
    // 路径形状硬门（.. 段拒绝）。
    let error = parse_sync_request(
        &engine,
        &json!({
            "sourceConnectionId": "src", "sourcePath": "/data/../secret",
            "targetConnectionId": "tgt", "targetPath": "/backup",
        }),
    )
    .unwrap_err();
    assert!(error.contains("'..'"), "{error}");
    // '/' 双侧合法（整树同步，rclone 风格）。
    let request = parse_sync_request(
        &engine,
        &json!({
            "sourceConnectionId": "src", "sourcePath": "/",
            "targetConnectionId": "tgt", "targetPath": "/",
        }),
    )
    .unwrap();
    assert_eq!(request.source_path, "/");
    assert_eq!(request.target_path, "/");
}

#[test]
fn files_sync_gates_mirror_the_workbench_delete_rules() {
    let (engine, _dir) = sync_engine();
    let args = |source: &str, target: &str, sync: bool| {
        json!({
            "sourceConnectionId": source, "sourcePath": "/data",
            "targetConnectionId": target, "targetPath": "/backup",
            "sync": sync,
        })
    };
    // 只读目标拒绝（纯 copy 也一样——目标要写）。
    let error = parse_sync_request(&engine, &args("src", "ro", false)).unwrap_err();
    assert!(error.contains("read-only"), "{error}");
    // sync=true 要求 allow_delete：与 transfers::validate_dir_job_gates 同语义。
    let error = parse_sync_request(&engine, &args("src", "nodelete", true)).unwrap_err();
    assert!(error.contains("allow_delete=false"), "{error}");
    // dryRun 不豁免门禁（dry-run 仍要规划删除，工作台路径同样拒绝）。
    let error = parse_sync_request(
        &engine,
        &json!({
            "sourceConnectionId": "src", "sourcePath": "/data",
            "targetConnectionId": "nodelete", "targetPath": "/backup",
            "sync": true, "dryRun": true,
        }),
    )
    .unwrap_err();
    assert!(error.contains("allow_delete=false"), "{error}");
    // 纯 copy 到 allow_delete=false 的目标放行（不删除任何东西）。
    let request = parse_sync_request(&engine, &args("src", "nodelete", false)).unwrap();
    assert!(!request.sync);
    // 源只读合法（从只读连接向外同步是正当拓扑）。
    let request = parse_sync_request(&engine, &args("ro", "tgt", false)).unwrap();
    assert_eq!(request.source.id, "ro");
}

/// 工具清单：files_sync 双侧连接工具，不受单一连接只读清单过滤影响
/// （门禁在调用时按目标连接执行）；schema required 顺序与缺参枚举一致，
/// 删除警告写进描述，dryRun 建议写进参数。
#[test]
fn files_sync_is_always_listed_with_the_full_schema() {
    let mcp = mcp();
    let definition = |read_only: bool| {
        mcp.definitions_for(read_only)["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "files_sync")
            .cloned()
            .expect("files_sync registered for writable listings")
    };
    let writable = definition(false);
    let schema = writable["inputSchema"].as_object().unwrap();
    assert_eq!(
        schema["required"],
        json!(["sourceConnectionId", "sourcePath", "targetConnectionId", "targetPath"]),
        "required order matches the missing_required enumeration"
    );
    let properties = schema["properties"].as_object().unwrap();
    for key in ["sync", "dryRun", "maxDelete"] {
        assert!(properties.contains_key(key), "{key} documented in the schema");
    }
    // 「sync 会删除目标多余文件」必须显式写进工具描述。
    assert!(
        definition(false)["description"].as_str().unwrap().contains("DELETED"),
        "sync delete warning must be in the description"
    );
    assert!(properties["sync"]["description"]
        .as_str()
        .unwrap()
        .contains("dryRun"));
    // 只读连接清单同样注册（目标是参数，不是注册连接）。
    assert!(definition(true)["inputSchema"].is_object());
    // 双连接工具不吃通用 connectionId/inline connection 放宽（stdio 原样 schema）。
    let stdio = mcp
        .stdio_tool_list()["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "files_sync")
        .cloned()
        .expect("files_sync registered in the stdio list");
    let stdio_schema = stdio["inputSchema"].as_object().unwrap();
    assert!(stdio_schema.get("anyOf").is_none(), "files_sync keeps its plain schema");
    assert!(
        !stdio_schema["properties"].as_object().unwrap().contains_key("connection"),
        "no generic inline connection injection for the dual-connection tool"
    );
}

// -- standalone stdio server (`--mcp`) ---------------------------------------

fn stdio_server() -> (StdioServer, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let server = StdioServer {
        mcp: Arc::new(Mcp::new(data_dir.clone())),
        engine: Arc::new(Engine::new()),
        transfers: Arc::new(JobTable::new()),
        store: Arc::new(Store::new(data_dir)),
        // Hermetic: no real DBX app bridge on the test box — the bridge
        // fallback decision/forward paths are covered by the dedicated
        // mock-bridge tests below.
        bridge_fallback: false,
        bridge_ensure_wait: Duration::from_millis(300),
    };
    (server, dir)
}

fn stdio_dispatch(server: &StdioServer, method: &str, params: Value) -> Option<Value> {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(server.dispatch(json!({
            "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
        })))
}

fn stdio_result(server: &StdioServer, method: &str, params: Value) -> Value {
    let response = stdio_dispatch(server, method, params).expect("request must be answered");
    assert!(response.get("error").is_none(), "{response}");
    response["result"].clone()
}

fn stdio_error(server: &StdioServer, method: &str, params: Value) -> String {
    // Tool-level errors are MCP isError results (ldap/kafka stdio
    // parity); structural mistakes (-32602 tier) stay JSON-RPC errors.
    // Both shapes carry one actionable text — extract either.
    let response = stdio_dispatch(server, method, params).expect("request must be answered");
    if let Some(message) = response["error"]["message"].as_str() {
        return message.to_string();
    }
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Unwraps the MCP content envelope into the tool payload (the shared
/// `finalize_payload` shape: `{content:[{type:text}], isError:false}`).
fn unwrap_envelope(result: &Value) -> Value {
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .expect("content envelope");
    assert_eq!(result["isError"], false, "{result}");
    serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn stdio_line_parsing_skips_blank_and_reports_parse_errors() {
    assert_eq!(parse_request_line("").unwrap(), None);
    assert_eq!(parse_request_line("   ").unwrap(), None);
    let parsed = parse_request_line(r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#)
        .unwrap()
        .unwrap();
    assert_eq!(parsed["id"], 7);
    assert_eq!(parsed["method"], "ping");
    let error = parse_request_line("not json").unwrap_err();
    assert_eq!(error["error"]["code"], -32700);
    assert!(error["error"]["message"].as_str().unwrap().contains("Parse error"));
    assert_eq!(error["id"], Value::Null);
}

#[test]
fn stdio_protocol_handshake_and_shapes() {
    let (server, _dir) = stdio_server();
    // initialize: MCP 2024-11-05, serverInfo name = io.dbx.files.
    let result = stdio_result(&server, "initialize", json!({}));
    assert_eq!(result["protocolVersion"], "2024-11-05", "{result}");
    assert_eq!(result["serverInfo"]["name"], "io.dbx.files");
    assert!(result["serverInfo"]["version"].is_string());
    assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
    assert_eq!(stdio_result(&server, "ping", json!({})), json!({}));
    // Notifications are never answered (notifications/initialized included).
    let answer = tokio::runtime::Runtime::new().unwrap().block_on(
        server.dispatch(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })),
    );
    assert!(answer.is_none(), "{answer:?}");
    // tools/list exposes the full tool surface with schemas.
    let tools = stdio_result(&server, "tools/list", json!({}))["tools"].clone();
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for expected in ["files_ui_focus", "files_scan_digest", "files_cursor_next", "files_delete"] {
        assert!(names.contains(&expected), "{expected} missing from {names:?}");
    }
    // Unknown method → standard JSON-RPC -32601 (Method not found),
    // family-wide with ssh/ldap/kafka; the smoke SKIP gate keys on the
    // message text ("Method not found"), S1 pins the numeric code.
    let response = stdio_dispatch(&server, "mcp/nonexistent", json!({})).unwrap();
    assert_eq!(response["error"]["code"], -32601);
    assert!(response["error"]["message"].as_str().unwrap().contains("Method not found"));
    // Structural tools/call problems are Invalid params (-32602): missing
    // tool name and a non-object arguments payload.
    let response = stdio_dispatch(&server, "tools/call", json!({})).unwrap();
    assert_eq!(response["error"]["code"], -32602, "{response}");
    assert!(response["error"]["message"].as_str().unwrap().contains("tool name"));
    let response = stdio_dispatch(
        &server,
        "tools/call",
        json!({ "name": "files_scan_digest", "arguments": "not-an-object" }),
    )
    .unwrap();
    assert_eq!(response["error"]["code"], -32602, "{response}");
    assert!(response["error"]["message"].as_str().unwrap().contains("arguments must be a JSON object"));
    // Tool-level errors are MCP isError results (ldap/kafka stdio
    // parity), not protocol-level JSON-RPC errors.
    let response = stdio_dispatch(
        &server,
        "tools/call",
        json!({ "name": "files_nonexistent", "arguments": {} }),
    )
    .unwrap();
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert!(response["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Unknown tool"));
    assert!(response.get("error").is_none(), "{response}");
    // A request without an id is invalid JSON-RPC → -32600 with null id.
    let answer = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(server.dispatch(json!({ "jsonrpc": "2.0", "method": "ping" })))
        .unwrap();
    assert_eq!(answer["id"], Value::Null);
    assert_eq!(answer["error"]["code"], -32600);
    // tools/call without a tool name.
    let error = stdio_error(&server, "tools/call", json!({}));
    assert!(error.contains("tool name"), "{error}");
}

#[test]
fn stdio_ui_tools_answer_unavailable() {
    let (server, _dir) = stdio_server();
    for (tool, arguments) in [
        ("files_ui_focus", json!({ "panel": "browse" })),
        ("files_ui_search", json!({ "path": "/data" })),
        ("files_ui_select", json!({ "path": "/data/a.txt" })),
        ("files_ui_state", json!({})),
    ] {
        let error = stdio_error(
            &server,
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        );
        assert!(error.contains("UNAVAILABLE"), "{tool}: {error}");
        assert!(error.contains(tool), "{tool}: {error}");
        assert!(error.contains("files_scan_digest"), "{tool}: {error}");
    }
    // Non-UI tools are untouched by the gate: a digest call surfaces its
    // real (argument) error instead of UNAVAILABLE.
    let error = stdio_error(
        &server,
        "tools/call",
        json!({ "name": "files_scan_digest", "arguments": { "path": "/" } }),
    );
    assert!(!error.contains("UNAVAILABLE"), "{error}");
}

/// files_sync stdio 路由：参数齐备且门禁通过时，调用一路推进到 enqueue
/// 边界，仅在事件通道处显式拒绝（stdio 无 emitter，不假死不静默）；缺参
/// 与 allow_delete 门禁在事件通道检查之前先行报错。双连接 id 池化进
/// engine（与 bridge_forward_plan 测试同一手法）。
#[test]
fn files_sync_stdio_reaches_the_enqueue_boundary_then_refuses() {
    let (server, dir) = stdio_server();
    let fsroot = dir.path().join("sync-fsroot");
    std::fs::create_dir_all(fsroot.join("src")).unwrap();
    let connect = |id: &str, read_only: bool, allow_delete: bool| {
        server
            .engine
            .connect(
                StoredConnection::from_lifecycle_params(&json!({
                    "connection": {
                        "id": id,
                        "external_config": {
                            "protocol": "fs",
                            "root": fsroot.join(id).to_string_lossy(),
                            "read_only": read_only,
                            "allow_delete": allow_delete,
                        },
                    }
                }))
                .unwrap(),
            )
            .unwrap();
    };
    connect("sync-src", false, true);
    connect("sync-tgt", false, true);
    connect("sync-nodelete", false, false);
    // 门禁全过 → 路由到 enqueue 边界（sync/dryRun/maxDelete 全量透传的
    // 参数形状被接受），仅因无事件通道显式拒绝。
    let error = stdio_error(
        &server,
        "tools/call",
        json!({
            "name": "files_sync",
            "arguments": {
                "sourceConnectionId": "sync-src", "sourcePath": "/data",
                "targetConnectionId": "sync-tgt", "targetPath": "/backup",
                "sync": true, "dryRun": false, "maxDelete": 10,
            },
        }),
    );
    assert!(error.contains("standalone stdio"), "{error}");
    assert!(error.contains("files_sync"), "{error}");
    // 缺 source/target 业务参数：先于事件通道，枚举报参错。
    let error = stdio_error(
        &server,
        "tools/call",
        json!({ "name": "files_sync", "arguments": {} }),
    );
    assert!(
        error.contains(
            "Missing required parameters: \
             sourceConnectionId, sourcePath, targetConnectionId, targetPath"
        ),
        "{error}"
    );
    // allow_delete 门禁：先于事件通道拒绝（复用既有删除门禁语义）。
    let error = stdio_error(
        &server,
        "tools/call",
        json!({
            "name": "files_sync",
            "arguments": {
                "sourceConnectionId": "sync-src", "sourcePath": "/data",
                "targetConnectionId": "sync-nodelete", "targetPath": "/backup",
                "sync": true,
            },
        }),
    );
    assert!(error.contains("allow_delete=false"), "{error}");
}

// -- stdio bridge fallback (L1; ssh bridge_forward_plan 同构 + ldap M14) --

/// stdio server with the bridge fallback ON (mock-bridge tests; each test
/// controls `DBX_APP_DATA_DIR` / `DBX_APP_LAUNCH_CMD` under the shared
/// env lock).
fn stdio_bridge_server() -> (StdioServer, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let server = StdioServer {
        mcp: Arc::new(Mcp::new(data_dir.clone())),
        engine: Arc::new(Engine::new()),
        transfers: Arc::new(JobTable::new()),
        store: Arc::new(Store::new(data_dir)),
        bridge_fallback: true,
        bridge_ensure_wait: Duration::from_millis(500),
    };
    (server, dir)
}

/// L1 forward decision: pooled inline ids and `__local__` stay local,
/// session tools and inline-`connection` calls never forward, and only a
/// genuinely unpooled saved-connection id forwards with its arguments.
#[test]
fn bridge_forward_plan_forwards_only_unpooled_connection_ids() {
    let (server, _dir) = stdio_bridge_server();
    let pooled = stored_connection_from_inline(&json!({
        "protocol": "local", "root": "/tmp/plan-x",
    }))
    .unwrap();
    server.engine.connect(pooled.clone()).unwrap();

    let plan = server.bridge_forward_plan(
        "files_scan_digest",
        &json!({ "connectionId": pooled.id, "path": "/" }),
    );
    assert!(plan.is_none(), "pooled inline id stays local");

    let plan = server.bridge_forward_plan(
        "files_scan_digest",
        &json!({ "connectionId": "__local__", "path": "/" }),
    );
    assert!(plan.is_none(), "__local__ stays local");

    let plan = server.bridge_forward_plan("files_cursor_next", &json!({ "cursorId": "cur-x" }));
    assert!(plan.is_none(), "session tools stay local");

    let plan = server.bridge_forward_plan(
        "files_scan_digest",
        &json!({
            "connection": { "protocol": "local", "root": "/tmp/plan-y" },
            "connectionId": "saved-9", "path": "/",
        }),
    );
    assert!(plan.is_none(), "inline connection owns the call");

    let (id, forwarded) = server
        .bridge_forward_plan(
            "files_scan_digest",
            &json!({ "connectionId": "saved-jane", "path": "/" }),
        )
        .expect("unpooled saved id must forward");
    assert_eq!(id, "saved-jane");
    assert_eq!(forwarded["path"], "/", "arguments forward untouched");

    let plan = server.bridge_forward_plan("files_scan_digest", &json!({ "path": "/" }));
    assert!(plan.is_none(), "no connectionId: local guidance path");
}

/// Mock-bridge happy path: an unpooled connectionId forwards the
/// snake_case five-field contract to the mock app, the app-side MCP
/// envelope passes through as the stdio result, and the local pool stays
/// unpolluted.
#[test]
fn stdio_bridge_forward_passes_the_app_envelope_through() {
    let _guard = BRIDGE_ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let envelope = r#"{"content":[{"type":"text","text":"{\"matched\":7,\"connectionId\":\"saved-jane\"}"}],"isError":false}"#;
    let (port, calls) = appbridge::spawn_mock_bridge(200, envelope);
    let app_data = tempfile::tempdir().unwrap();
    std::fs::write(app_data.path().join("mcp-bridge-port"), port.to_string()).unwrap();
    std::env::set_var("DBX_APP_DATA_DIR", app_data.path());
    std::env::set_var("DBX_APP_LAUNCH_CMD", ":");

    let (server, _dir) = stdio_bridge_server();
    let response = stdio_dispatch(
        &server,
        "tools/call",
        json!({
            "name": "files_scan_digest",
            "arguments": { "connectionId": "saved-jane", "path": "/data", "glob": "*" },
        }),
    )
    .unwrap();

    std::env::remove_var("DBX_APP_DATA_DIR");
    std::env::remove_var("DBX_APP_LAUNCH_CMD");

    assert!(response.get("error").is_none(), "{response}");
    let payload = unwrap_envelope(&response["result"]);
    assert_eq!(payload["matched"], 7, "{payload}");
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "bridge called exactly once: {calls:?}");
    assert_eq!(calls[0]["plugin_id"], "io.dbx.files", "{calls:?}");
    assert_eq!(calls[0]["connection_id"], "saved-jane", "{calls:?}");
    assert_eq!(calls[0]["tool"], "files_scan_digest", "{calls:?}");
    assert_eq!(calls[0]["arguments"]["path"], "/data", "{calls:?}");
    // 转发不污染本地池。
    assert!(server.engine.connection("saved-jane").is_err());
}

/// A non-envelope app answer (defensive shape) wraps as a success
/// payload instead of being buried one JSON level deeper.
#[test]
fn stdio_bridge_forward_wraps_a_non_envelope_payload_as_success() {
    let _guard = BRIDGE_ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (port, _calls) = appbridge::spawn_mock_bridge(200, r#"{"unexpected":"shape"}"#);
    let app_data = tempfile::tempdir().unwrap();
    std::fs::write(app_data.path().join("mcp-bridge-port"), port.to_string()).unwrap();
    std::env::set_var("DBX_APP_DATA_DIR", app_data.path());
    std::env::set_var("DBX_APP_LAUNCH_CMD", ":");

    let (server, _dir) = stdio_bridge_server();
    let response = stdio_dispatch(
        &server,
        "tools/call",
        json!({
            "name": "files_scan_digest",
            "arguments": { "connectionId": "saved-1", "path": "/" },
        }),
    )
    .unwrap();

    std::env::remove_var("DBX_APP_DATA_DIR");
    std::env::remove_var("DBX_APP_LAUNCH_CMD");

    assert!(response.get("error").is_none(), "{response}");
    let payload = unwrap_envelope(&response["result"]);
    assert_eq!(payload["unexpected"], "shape", "{payload}");
}

/// Bridge unreachable (unpublished port, no-op launch): the forward leg
/// fails closed into the merged guidance error — bridge reason plus the
/// inline-credential ways out named with files' own parameter fields.
#[test]
fn stdio_bridge_unreachable_fails_closed_with_inline_guidance() {
    let _guard = BRIDGE_ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let app_data = tempfile::tempdir().unwrap(); // 空目录：端口文件不存在
    std::env::set_var("DBX_APP_DATA_DIR", app_data.path());
    std::env::set_var("DBX_APP_LAUNCH_CMD", ":");
    let (server, _dir) = stdio_bridge_server();
    let error = stdio_error(
        &server,
        "tools/call",
        json!({
            "name": "files_scan_digest",
            "arguments": { "connectionId": "ghost", "path": "/" },
        }),
    );
    std::env::remove_var("DBX_APP_DATA_DIR");
    std::env::remove_var("DBX_APP_LAUNCH_CMD");

    assert!(error.contains("Unknown connectionId 'ghost'"), "{error}");
    assert!(error.contains("DBX app bridge"), "{error}");
    assert!(error.contains("inline connection parameters"), "{error}");
    // files 特化出路：内联参数字段点名（protocol/secretAccessKey）。
    assert!(error.contains("\"protocol\""), "{error}");
    assert!(error.contains("secretAccessKey"), "{error}");
}

#[test]
fn inline_pool_key_is_hash_stable_and_sensitive() {
    let a = json!({ "protocol": "local", "root": "/tmp/x" });
    let b = json!({ "root": "/tmp/x", "protocol": "local" });
    assert_eq!(inline_pool_id(&a), inline_pool_id(&b), "key order irrelevant");
    assert!(inline_pool_id(&a).starts_with("mcp-inline-"));
    let c = json!({ "protocol": "local", "root": "/tmp/y" });
    assert_ne!(inline_pool_id(&a), inline_pool_id(&c));
    // Credentials are part of the key: two secrets never share a pool id.
    let secret = json!({ "protocol": "s3", "bucket": "b", "secretAccessKey": "k1" });
    let secret2 = json!({ "protocol": "s3", "bucket": "b", "secretAccessKey": "k2" });
    assert_ne!(inline_pool_id(&secret), inline_pool_id(&secret2));
}

#[test]
fn inline_connection_maps_camel_case_form_fields() {
    let connection = stored_connection_from_inline(&json!({
        "protocol": "localFs",
        "root": "/data",
        "name": "smoke",
        "readOnly": true,
        "allowDelete": false,
        "lockToRoot": true,
        "timeoutSecs": 42,
    }))
    .unwrap();
    assert_eq!(connection.protocol, "fs", "localFs aliases fs");
    assert_eq!(connection.root, "/data");
    assert!(connection.read_only);
    assert!(!connection.allow_delete);
    assert!(connection.lock_to_root);
    assert_eq!(connection.timeout_secs, 42);
    assert!(connection.id.starts_with("mcp-inline-"), "{}", connection.id);

    let s3 = stored_connection_from_inline(&json!({
        "protocol": "s3",
        "bucket": "demo",
        "endpoint": "http://127.0.0.1:9000",
        "region": "us-east-1",
        "accessKeyId": "minioadmin",
        "secretAccessKey": fixture("minioadmin"),
        "id": "inline-c1",
    }))
    .unwrap();
    assert_eq!(s3.id, "inline-c1", "explicit id wins over the pool hash");
    assert_eq!(s3.bucket, "demo");
    assert_eq!(s3.access_key_id, "minioadmin");
    assert_eq!(s3.secret_access_key, fixture("minioadmin"));
    assert_eq!(s3.endpoint, "http://127.0.0.1:9000");

    let cos = stored_connection_from_inline(&json!({
        "protocol": "cos",
        "bucket": "demo-1250000000",
        "endpoint": "https://cos.ap-guangzhou.myqcloud.com",
        "secretId": fixture("throwaway-secret-id"),
        "secretKey": fixture("throwaway-secret-key"),
        "securityToken": fixture("throwaway-security-token"),
    }))
    .unwrap();
    assert_eq!(cos.protocol, "cos");
    assert_eq!(cos.secret_id, fixture("throwaway-secret-id"));
    assert_eq!(cos.secret_key, fixture("throwaway-secret-key"));
    assert_eq!(cos.security_token, fixture("throwaway-security-token"));

    let missing = stored_connection_from_inline(&json!({ "root": "/data" })).unwrap_err();
    assert!(missing.contains("protocol"), "{missing}");
    let unknown = stored_connection_from_inline(&json!({ "protocol": "gopher" })).unwrap_err();
    assert!(unknown.contains("Unsupported protocol"), "{unknown}");
    let not_object = stored_connection_from_inline(&json!("fs")).unwrap_err();
    assert!(not_object.contains("object"), "{not_object}");
}

/// Form↔MCP consistency: every camelCase key the inline parser maps into
/// the lifecycle payload must be declared in the `tools/list` connection
/// schema. Strict MCP hosts silently drop undeclared keys, so a drifted
/// schema would quietly change connection behavior (e.g. drop the private
/// key and fall back to anonymous) instead of erroring.
#[test]
fn inline_connection_schema_covers_every_mapped_key() {
    let properties = inline_connection_properties();
    let properties = properties.as_object().expect("schema properties object");
    let mapped: &[&str] = &[
        // config-bound keys, in stored_connection_from_inline mapping order
        "root",
        "bucket",
        "endpoint",
        "region",
        "container",
        "accountName",
        "scope",
        "accessKeyId",
        "enableVirtualHostStyle",
        "username",
        "user",
        "share",
        "domain",
        "knownHostsStrategy",
        "readOnly",
        "allowDelete",
        "lockToRoot",
        "timeoutSecs",
        "service",
        "config",
        "clientId",
        "driveType",
        "email",
        "repoName",
        // secret-bound keys
        "secretAccessKey",
        "credential",
        "accountKey",
        "secretId",
        "secretKey",
        "securityToken",
        "password",
        "key",
        "accessToken",
        "clientSecret",
        "refreshToken",
    ];
    for key in mapped {
        assert!(
            properties.contains_key(*key),
            "inline key '{key}' is mapped but undeclared in tools/list schema; \
             strict MCP hosts would silently drop it"
        );
    }
    // protocol (validated below), id and name are transport fields — the
    // only schema entries allowed outside the mapping.
    let extra: Vec<&str> = properties
        .keys()
        .map(String::as_str)
        .filter(|key| *key != "protocol" && !mapped.contains(key))
        .collect();
    assert_eq!(extra.len(), 2, "undeclared schema keys: {extra:?}");
    assert!(properties.contains_key("id") && properties.contains_key("name"));
    // The protocol description must enumerate every engine protocol so an
    // LLM caller can never be offered a value the engine would reject.
    let description = properties["protocol"]["description"]
        .as_str()
        .expect("protocol description");
    for protocol in crate::model::PROTOCOLS {
        assert!(
            description.contains(protocol),
            "protocol '{protocol}' missing from the inline schema description"
        );
    }
}

/// Manifest↔MCP leg of the contract triangle: every connection-provider
/// field in manifest.json must be reachable through the inline connection
/// (config-bound → the camelCase config map, secret-bound → the secret
/// map, display_name → `name`), and no mapping entry may exist without a
/// manifest field behind it. Catches "field added to the form but never
/// wired into MCP" drift.
#[test]
fn manifest_fields_are_fully_covered_by_inline_mapping() {
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../manifest.json"
        ))
        .expect("manifest.json readable"),
    )
    .expect("manifest.json parses");
    let fields = manifest["contributions"]
        .as_array()
        .expect("contributions")
        .iter()
        .find(|item| item["type"] == "connection-provider")
        .expect("connection-provider")["fields"]
        .as_array()
        .expect("fields");
    let config_map: &[(&str, &str)] = &[
        ("root", "root"),
        ("bucket", "bucket"),
        ("endpoint", "endpoint"),
        ("region", "region"),
        ("container", "container"),
        ("account_name", "accountName"),
        ("scope", "scope"),
        ("access_key_id", "accessKeyId"),
        ("enable_virtual_host_style", "enableVirtualHostStyle"),
        ("username", "username"),
        ("user", "user"),
        ("share", "share"),
        ("domain", "domain"),
        ("known_hosts_strategy", "knownHostsStrategy"),
        ("read_only", "readOnly"),
        ("allow_delete", "allowDelete"),
        ("lock_to_root", "lockToRoot"),
        ("timeout_secs", "timeoutSecs"),
        ("service", "service"),
        ("config", "config"),
        ("client_id", "clientId"),
        ("drive_type", "driveType"),
        ("email", "email"),
        ("repo_name", "repoName"),
    ];
    let secret_map: &[(&str, &str)] = &[
        ("secret_access_key", "secretAccessKey"),
        ("credential", "credential"),
        ("account_key", "accountKey"),
        ("secret_id", "secretId"),
        ("secret_key", "secretKey"),
        ("security_token", "securityToken"),
        ("password", "password"),
        ("key", "key"),
        ("access_token", "accessToken"),
        ("client_secret", "clientSecret"),
        ("refresh_token", "refreshToken"),
    ];
    let mut covered: Vec<&str> = Vec::new();
    for field in fields {
        let key = field["key"].as_str().expect("field key");
        match field["binding"].as_str().expect("field binding") {
            "name" => assert_eq!(key, "display_name", "unexpected name-bound field"),
            "secret" => assert!(
                secret_map.iter().any(|(source, _)| *source == key),
                "secret field '{key}' missing from the inline secret mapping"
            ),
            _ if key == "protocol" => {}
            _ => assert!(
                config_map.iter().any(|(source, _)| *source == key),
                "config field '{key}' missing from the inline mapping; \
                 MCP callers could never set it"
            ),
        }
        covered.push(key);
    }
    for (source, _) in config_map.iter().chain(secret_map) {
        assert!(
            covered.contains(source),
            "inline mapping key '{source}' has no manifest field behind it"
        );
    }
}

/// The full stdio loop against a real localFs connection: mkdir/write via
/// inline credentials, digest + cursor paging, two-phase delete with the
/// parameter-hash confirmToken, pooled connectionId stability, the dir
/// rename stdio refusal and the missing/unknown connectionId guidance.
#[test]
fn stdio_localfs_inline_round_trip_digest_cursor_two_phase() {
    let (server, dir) = stdio_server();
    // The fs root is separate from the plugin data dir: the audit trail
    // must not land inside the scanned tree.
    let fsroot = dir.path().join("fsroot");
    std::fs::create_dir_all(&fsroot).unwrap();
    let root = fsroot.to_string_lossy().to_string();
    let connection = json!({ "protocol": "local", "root": root, "name": "smoke" });

    // 建树走 stdio MCP 写工具本身（mkdir/write 单阶段直执行）。
    unwrap_envelope(&stdio_result(
        &server,
        "tools/call",
        json!({
            "name": "files_mkdir",
            "arguments": { "connection": connection, "path": "/data/sub" },
        }),
    ));
    for name in ["a.txt", "b.log"] {
        unwrap_envelope(&stdio_result(
            &server,
            "tools/call",
            json!({
                "name": "files_write",
                "arguments": {
                    "connection": connection,
                    "path": format!("/data/{name}"),
                    "dataBase64": BASE64_STANDARD.encode(format!("content {name}")),
                },
            }),
        ));
    }

    // digest：递归扫描 + 本地聚合 + cursor 物化。
    let digest = unwrap_envelope(&stdio_result(
        &server,
        "tools/call",
        json!({
            "name": "files_scan_digest",
            "arguments": { "connection": connection, "path": "/data", "glob": "*" },
        }),
    ));
    assert_eq!(digest["matched"], 3, "{digest}");
    assert_eq!(digest["scanned"], 3, "{digest}");
    assert!(digest["cursorId"].as_str().unwrap().starts_with("cur-"));
    // 池化键稳定：同一 connection 参数的重复调用复用同一 engine 条目。
    let again = unwrap_envelope(&stdio_result(
        &server,
        "tools/call",
        json!({
            "name": "files_scan_digest",
            "arguments": { "connection": connection, "path": "/data", "format": "rows" },
        }),
    ));
    assert_eq!(digest["connectionId"], again["connectionId"], "pooled inline id");

    // cursor 翻页：不带 connectionId（纯会话查找）。
    let cursor_id = digest["cursorId"].as_str().unwrap();
    let page = unwrap_envelope(&stdio_result(
        &server,
        "tools/call",
        json!({
            "name": "files_cursor_next",
            "arguments": { "cursorId": cursor_id, "n": 2 },
        }),
    ));
    assert_eq!(page["rows"].as_array().unwrap().len(), 2, "{page}");
    assert_eq!(page["done"], false);

    // 两阶段 delete：preview → 参数改动作废 → 重新预览 → token 确认执行。
    let target = "/data/a.txt";
    let preview = unwrap_envelope(&stdio_result(
        &server,
        "tools/call",
        json!({
            "name": "files_delete",
            "arguments": { "connection": connection, "path": target },
        }),
    ));
    assert_eq!(preview["preview"]["path"], target, "{preview}");
    let token = preview["confirmToken"].as_str().unwrap().to_string();
    let error = stdio_error(
        &server,
        "tools/call",
        json!({
            "name": "files_delete",
            "arguments": { "connection": connection, "path": "/data/b.log", "confirmToken": token },
        }),
    );
    assert!(error.contains("arguments changed"), "{error}");
    let preview2 = unwrap_envelope(&stdio_result(
        &server,
        "tools/call",
        json!({
            "name": "files_delete",
            "arguments": { "connection": connection, "path": target },
        }),
    ));
    let confirmed = unwrap_envelope(&stdio_result(
        &server,
        "tools/call",
        json!({
            "name": "files_delete",
            "arguments": {
                "connection": connection,
                "path": target,
                "confirmToken": preview2["confirmToken"],
            },
        }),
    ));
    assert_eq!(confirmed["success"], true, "{confirmed}");
    assert!(
        !fsroot.join("data/a.txt").exists(),
        "file deleted on disk"
    );
    // 一次性：同 token 重放 → unknown。
    let error = stdio_error(
        &server,
        "tools/call",
        json!({
            "name": "files_delete",
            "arguments": {
                "connection": connection,
                "path": target,
                "confirmToken": preview2["confirmToken"],
            },
        }),
    );
    assert!(error.contains("unknown or already used"), "{error}");

    // 目录 rename 降级 job 需要工作台事件通道：stdio 下明确报错（不假死）。
    let error = stdio_error(
        &server,
        "tools/call",
        json!({
            "name": "files_rename",
            "arguments": { "connection": connection, "path": "/data/sub", "newPath": "/data/sub2" },
        }),
    );
    assert!(error.contains("standalone stdio"), "{error}");

    // 连接寻址引导：缺 connectionId / 未注册 id 都给出可执行出路。
    let error = stdio_error(
        &server,
        "tools/call",
        json!({ "name": "files_scan_digest", "arguments": { "path": "/" } }),
    );
    assert!(error.contains("Missing required parameter: connectionId"), "{error}");
    let error = stdio_error(
        &server,
        "tools/call",
        json!({ "name": "files_scan_digest", "arguments": { "connectionId": "smoke-nope", "path": "/" } }),
    );
    assert!(error.contains("Unknown connectionId 'smoke-nope'"), "{error}");
    // 引导错误带内联凭据出路（fallback 关闭时也保持同一消息形状）。
    assert!(error.contains("inline connection parameters"), "{error}");

    // arguments 非对象：清晰报错（与 DBX 桥 mcp/call 同语义）。
    let error = stdio_error(
        &server,
        "tools/call",
        json!({ "name": "files_scan_digest", "arguments": "not-an-object" }),
    );
    assert!(error.contains("arguments must be a JSON object"), "{error}");

    // quick_paths：limit 数字字符串被接受并 clamp（本地 fs 连接只有 root chip）。
    let quick = unwrap_envelope(&stdio_result(
        &server,
        "tools/call",
        json!({
            "name": "files_ui_quick_paths",
            "arguments": { "connection": connection, "limit": "1" },
        }),
    ));
    assert_eq!(quick["paths"].as_array().unwrap().len(), 1, "{quick}");

    // 空 dataBase64 建空文件（此前被 required_str 误拒，工作台路径允许）。
    unwrap_envelope(&stdio_result(
        &server,
        "tools/call",
        json!({
            "name": "files_write",
            "arguments": { "connection": connection, "path": "/data/empty.txt", "dataBase64": "" },
        }),
    ));
    assert_eq!(
        fsroot.join("data/empty.txt").metadata().unwrap().len(),
        0,
        "empty payload must create an empty file"
    );

    // 尾斜杠文件路径 delete：必须真删而不是静默 no-op（OpenDAL 陷阱）。
    let preview = unwrap_envelope(&stdio_result(
        &server,
        "tools/call",
        json!({
            "name": "files_delete",
            "arguments": { "connection": connection, "path": "/data/b.log/" },
        }),
    ));
    assert_eq!(preview["preview"]["kind"], "missing", "{preview}");
    unwrap_envelope(&stdio_result(
        &server,
        "tools/call",
        json!({
            "name": "files_delete",
            "arguments": {
                "connection": connection,
                "path": "/data/b.log/",
                "confirmToken": preview["confirmToken"],
            },
        }),
    ));
    assert!(
        !fsroot.join("data/b.log").exists(),
        "trailing-slash delete must remove the real file"
    );

    // 两阶段 purge 成功路径：/data/sub 目录递归删除（磁盘校验）。
    let purge_preview = unwrap_envelope(&stdio_result(
        &server,
        "tools/call",
        json!({
            "name": "files_purge",
            "arguments": { "connection": connection, "path": "/data/sub" },
        }),
    ));
    assert_eq!(purge_preview["preview"]["kind"], "dir", "{purge_preview}");
    unwrap_envelope(&stdio_result(
        &server,
        "tools/call",
        json!({
            "name": "files_purge",
            "arguments": {
                "connection": connection,
                "path": "/data/sub",
                "confirmToken": purge_preview["confirmToken"],
            },
        }),
    ));
    assert!(!fsroot.join("data/sub").exists(), "purged on disk");

    // 内联 readOnly 布尔字符串变体：意图是只读，写工具必须被拒（不能
    // 因为 "true" 是字符串就静默降级为可写连接）。
    let ro_root = dir.path().join("ro-root");
    std::fs::create_dir_all(&ro_root).unwrap();
    let ro_connection = json!({
        "protocol": "local",
        "root": ro_root.to_string_lossy(),
        "readOnly": "true",
    });
    let error = stdio_error(
        &server,
        "tools/call",
        json!({
            "name": "files_mkdir",
            "arguments": { "connection": ro_connection, "path": "/x" },
        }),
    );
    assert!(error.contains("read-only"), "{error}");

    // 审计：stdio 写路径与工作台共用 audit 基线，记 source:"mcp"。
    let audits = server.store.read_audit();
    assert!(
        audits
            .iter()
            .any(|record| record.action == "files/delete"
                && record.source.as_deref() == Some("mcp")),
        "{audits:?}"
    );
}

// -- 第二轮：错误消息引导 + 可选字符串参数 fail-fast -----------------------

#[test]
fn unknown_tool_suggests_variants_and_lists_discovery() {
    // 分隔符/大小写变体给 Did-you-mean（ssh unknown_tool_message 同构）。
    let error = unknown_tool_message("FILES-SCANDIGEST");
    assert!(error.contains("Did you mean 'files_scan_digest'?"), "{error}");
    // 每个未知名都列出全部注册名与发现面（桥 mcp/tools / stdio tools/list）。
    let error = unknown_tool_message("files_nonexistent");
    assert!(!error.contains("Did you mean"), "{error}");
    for tool in ALL_TOOL_NAMES {
        assert!(error.contains(tool), "{tool} missing from: {error}");
    }
    assert!(error.contains("mcp/tools"), "{error}");
    assert!(error.contains("tools/list"), "{error}");
    // dispatch 真正走这条消息（run_tool 的 other 分支与 stdio 同源）。
    let (server, _dir) = stdio_server();
    let error = stdio_error(
        &server,
        "tools/call",
        json!({ "name": "files_nonexistent", "arguments": {} }),
    );
    assert!(error.contains("Unknown tool"), "{error}");
    assert!(error.contains("files_scan_digest"), "{error}");
}

#[test]
fn unknown_intent_id_error_guides_caller() {
    let (server, _dir) = stdio_server();
    // files_ui_state 带 intentId 走 run_tool（不经 stdio 的 UI UNAVAILABLE
    // 短路？——走：该工具在 STDIO_UI_TOOLS 内。此处直接打 run_tool 验证
    // 桥路径（mcp/call）的错误文本。）
    let rt = tokio::runtime::Runtime::new().unwrap();
    let error = rt
        .block_on(server.mcp.run_tool(
            "files_ui_state",
            &json!({ "intentId": "i-nope" }),
            &server.engine,
            &server.transfers,
            &server.store,
            None,
        ))
        .expect_err("unknown intentId must error");
    assert!(error.contains("unknown intentId"), "{error}");
    assert!(error.contains("60s"), "{error}");
    assert!(error.contains("files_ui_"), "{error}");
    assert!(error.contains("omit intentId"), "{error}");
}

#[test]
fn digest_rejects_non_string_optional_args_instead_of_silent_defaults() {
    let mcp = mcp();
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new();
    engine
        .connect(
            StoredConnection::from_lifecycle_params(&json!({
                "connection": {
                    "id": "dig",
                    "external_config": { "protocol": "fs", "root": dir.path() },
                }
            }))
            .unwrap(),
        )
        .unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    // path 传了非字符串：绝不静默回落 "/"（那会扫错整棵树）。
    let error = rt
        .block_on(mcp.scan_digest_tool(&engine, &json!({ "connectionId": "dig", "path": 123 })))
        .unwrap_err();
    assert!(error.contains("'path' must be a non-empty string"), "{error}");
    // glob/format 同理 fail-fast；空串也是明确拒绝而非静默忽略。
    for (key, value) in [("glob", json!(true)), ("format", json!(7)), ("path", json!(""))] {
        let error = rt
            .block_on(mcp.scan_digest_tool(
                &engine,
                &json!({ "connectionId": "dig", key: value }),
            ))
            .unwrap_err();
        assert!(
            error.contains(&format!("'{key}' must be a non-empty string")),
            "{key}: {error}"
        );
    }
    // 缺省值路径不受影响：path 缺省扫根、format 缺省 digest。
    let payload = rt
        .block_on(mcp.scan_digest_tool(&engine, &json!({ "connectionId": "dig" })))
        .unwrap();
    assert_eq!(payload["path"], "/", "{payload}");
    assert!(payload.get("stats").is_some(), "digest format by default");
}

#[test]
fn allow_delete_refusal_names_the_way_out() {
    let connection = StoredConnection::from_lifecycle_params(&json!({
        "connection": {
            "id": "c",
            "external_config": { "protocol": "fs", "allow_delete": false },
        }
    }))
    .unwrap();
    let error = ensure_deletable(&connection).unwrap_err();
    assert!(error.contains("allow_delete=false"), "{error}");
    assert!(error.contains("allowDelete"), "{error}");
}

#[test]
fn base64_error_names_the_expected_format() {
    let (server, dir) = stdio_server();
    let fsroot = dir.path().join("fsroot-b64");
    std::fs::create_dir_all(&fsroot).unwrap();
    let connection = json!({ "protocol": "local", "root": fsroot.to_string_lossy() });
    let error = stdio_error(
        &server,
        "tools/call",
        json!({
            "name": "files_write",
            "arguments": { "connection": connection, "path": "/x.txt", "dataBase64": "!!!not-base64!!!" },
        }),
    );
    assert!(error.contains("base64"), "{error}");
    assert!(error.contains("RFC 4648"), "{error}");
}

// -- 第五轮（可靠性纵深）：stdio 传输层健壮性 ------------------------------

/// dispatch 一次原始 JSON-RPC 请求（id/method 形状由用例自定）。
fn dispatch_raw(server: &StdioServer, request: Value) -> Option<Value> {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(server.dispatch(request))
}

#[test]
fn stdio_dispatch_rejects_missing_method_and_non_scalar_ids() {
    let (server, _dir) = stdio_server();
    // 缺 method（含 method 非字符串）：invalid request -32600，不是
    // "Method not found"（空串 method 不是可纠正的未知方法名）。
    for request in [
        json!({ "jsonrpc": "2.0", "id": 1 }),
        json!({ "jsonrpc": "2.0", "id": 1, "method": "" }),
        json!({ "jsonrpc": "2.0", "id": 1, "method": 42 }),
    ] {
        let response = dispatch_raw(&server, request.clone()).expect("answered");
        assert_eq!(response["error"]["code"], -32600, "{request}");
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("missing method"),
            "{response}"
        );
    }
    // id 为 object/array/boolean：无效请求 -32600，id 不回显。
    for bad_id in [json!({ "a": 1 }), json!([1, 2]), json!(true)] {
        let request = json!({ "jsonrpc": "2.0", "id": bad_id, "method": "ping" });
        let response = dispatch_raw(&server, request).expect("answered");
        assert_eq!(response["error"]["code"], -32600, "{response}");
        assert!(response["id"].is_null(), "{response}");
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("id must be a string, number, or null"),
            "{response}"
        );
    }
    // 合法 id 形状照常：string / number / null 都得到正常响应。
    for good_id in [json!("str-1"), json!(7), json!(null)] {
        let request = json!({ "jsonrpc": "2.0", "id": good_id, "method": "ping" });
        let response = dispatch_raw(&server, request).expect("answered");
        assert_eq!(response["result"], json!({}), "{response}");
        assert_eq!(response["id"], good_id, "id echoed verbatim");
    }
    // 无 id 的非 notification 请求：-32600（保留既有形状），连接继续。
    let response = dispatch_raw(&server, json!({ "jsonrpc": "2.0", "method": "ping" }))
        .expect("answered");
    assert_eq!(response["error"]["code"], -32600, "{response}");
    // notifications/* 无 id：不回包（None）。
    assert!(dispatch_raw(
        &server,
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
    )
    .is_none());
}

#[test]
fn stdio_dispatch_tolerates_missing_or_variant_jsonrpc_field() {
    // 设计内宽容（族一致：ssh/ldap/kafka 均不校验 jsonrpc 字段）：真实
    // MCP 客户端可能省略或变体，拒绝只会破坏兼容而没有可防护的行为。
    for version in [json!("1.0"), json!(2), json!(null)] {
        let (server, _dir) = stdio_server();
        let request = json!({ "jsonrpc": version, "id": 1, "method": "ping" });
        let response = dispatch_raw(&server, request).expect("answered");
        assert_eq!(response["result"], json!({}), "{response}");
    }
    let (server, _dir) = stdio_server();
    let response = dispatch_raw(&server, json!({ "id": 1, "method": "ping" })).expect("answered");
    assert_eq!(response["result"], json!({}), "{response}");
}

#[test]
fn parse_request_line_tolerates_blank_and_whitespace_lines() {
    assert!(parse_request_line("").unwrap().is_none());
    assert!(parse_request_line("   ").unwrap().is_none());
    // 外层 reader 已 trim 行尾 CRLF；行内 JSON 正常解析。
    let parsed = parse_request_line(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .unwrap()
        .expect("valid line");
    assert_eq!(parsed["method"], "ping");
    // 非法 JSON：-32700 响应（null id），不是进程错误。
    let error = parse_request_line("not json").unwrap_err();
    assert_eq!(error["error"]["code"], -32700, "{error}");
}

#[test]
fn stdio_max_line_env_is_parsed_with_safe_fallback() {
    // 与桥接用例共享进程级 env 锁：这些用例会动 DBX_APP_* 之外的
    // 进程全局变量。
    let _guard = BRIDGE_ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    std::env::set_var("DBX_FILES_MCP_STDIO_MAX_LINE", "1024");
    assert_eq!(stdio_max_line_bytes(), 1024);
    std::env::set_var("DBX_FILES_MCP_STDIO_MAX_LINE", " 8388608 ");
    assert_eq!(stdio_max_line_bytes(), 8_388_608);
    for junk in ["0", "-5", "big", ""] {
        std::env::set_var("DBX_FILES_MCP_STDIO_MAX_LINE", junk);
        assert_eq!(
            stdio_max_line_bytes(),
            DEFAULT_STDIO_MAX_LINE_BYTES,
            "junk {junk:?} falls back to the default"
        );
    }
    std::env::remove_var("DBX_FILES_MCP_STDIO_MAX_LINE");
    assert_eq!(stdio_max_line_bytes(), DEFAULT_STDIO_MAX_LINE_BYTES);
}

// -- 第五轮（可靠性纵深）：confirm/cursor/intent 存储 churn -----------------

#[test]
fn confirm_table_churns_at_ttl_and_hard_cap() {
    let table = mcp();
    let args = json!({ "connectionId": "c", "path": "/x" });
    // 600+ 次 preview（从不 confirm）：表被 TTL prune + 硬上限钳住，
    // 绝不单调增长（签发量超过硬上限，表停在上限）。
    for _ in 0..(Mcp::MAX_CONFIRM_ENTRIES + 200) {
        let (token, _) = table.confirm_begin(&args);
        assert!(token.starts_with("c-"));
    }
    {
        let confirms = table
            .confirms
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        assert_eq!(confirms.len(), Mcp::MAX_CONFIRM_ENTRIES, "capped");
        // 最新 token 必然在表内（淘汰的是最早过期者）。
        let now = unix_millis_now() as u128;
        assert!(confirms
            .values()
            .all(|entry| entry.expires_at_millis > now));
    }
    // 过期条目在下一次 preview 时被清走（TTL churn）。
    let expired_at = unix_millis_now() as u128 - 1;
    table
        .confirms
        .lock()
        .unwrap()
        .insert("c-expired".to_string(), ConfirmEntry { hash: 0, expires_at_millis: expired_at });
    let _ = table.confirm_begin(&args);
    assert!(
        !table.confirms.lock().unwrap().contains_key("c-expired"),
        "expired token pruned on the next preview"
    );
}

#[test]
fn confirm_cap_evicts_the_earliest_expiring_token() {
    let table = mcp();
    {
        let mut confirms = table.confirms.lock().unwrap();
        for index in 0..Mcp::MAX_CONFIRM_ENTRIES {
            confirms.insert(
                format!("c-fill-{index}"),
                ConfirmEntry {
                    hash: 0,
                    // 远未来的过期时间 + 相对序：TTL prune 不参与，
                    // 只验证硬上限淘汰（最早过期者 = 序号最小者）。
                    expires_at_millis: unix_millis_now() as u128 + 1_000_000 + index as u128,
                },
            );
        }
    }
    let (fresh, _) = table.confirm_begin(&json!({}));
    let confirms = table.confirms.lock().unwrap();
    assert_eq!(confirms.len(), Mcp::MAX_CONFIRM_ENTRIES, "cap holds");
    assert!(confirms.contains_key(&fresh), "the fresh token survives");
    assert!(
        !confirms.contains_key("c-fill-0"),
        "the earliest-expiring token is evicted"
    );
    assert!(confirms.contains_key("c-fill-1"), "newer filler stays");
}

#[test]
fn confirm_ttl_retune_does_not_redate_issued_tokens() {
    let mcp = mcp();
    let (token, expires_at) = mcp.confirm_begin(&json!({ "connectionId": "c", "path": "/a" }));
    assert!(expires_at > unix_millis_now() as u128 + 55_000, "60s default");
    // 调小 TTL：已签发 token 的过期时间不追溯（begin 时固定）。
    mcp.settings_set(&json!({ "confirmTtlSecs": 10 })).unwrap();
    let (_, retuned_expires_at) = mcp.confirm_begin(&json!({ "connectionId": "c", "path": "/b" }));
    let now = unix_millis_now() as u128;
    assert!(
        retuned_expires_at <= now + 11_000,
        "new tokens use the retuned TTL: {retuned_expires_at} vs now {now}"
    );
    // 旧 token 在其原始 600s 窗口内仍然有效（消费即成功，一次性）。
    mcp.confirm_verify(&token, &json!({ "connectionId": "c", "path": "/a" }))
        .expect("pre-retune token keeps its original window");
    // 过期报文携带实际生效（调小后）的 TTL。
    let (token2, _) = mcp.confirm_begin(&json!({ "connectionId": "c", "path": "/c" }));
    mcp.confirms
        .lock()
        .unwrap()
        .get_mut(&token2)
        .unwrap()
        .expires_at_millis = unix_millis_now() as u128 - 1;
    let error = mcp
        .confirm_verify(&token2, &json!({ "connectionId": "c", "path": "/c" }))
        .unwrap_err();
    assert!(error.contains("TTL 10s"), "{error}");
}

#[test]
fn cursor_sessions_churn_with_lru_and_retuned_settings() {
    let mcp = mcp();
    mcp.settings_set(&json!({ "maxCursorSessions": 2 })).unwrap();
    let (a, _) = mcp.cursor_put(vec!["/a".into()]);
    let (b, _) = mcp.cursor_put(vec!["/b".into()]);
    // 活跃会话不被误逐：touch A 后物化 C，被逐的是 B（LRU）。
    let page = mcp.cursor_next(&json!({ "cursorId": a })).unwrap();
    assert_eq!(page["rows"][0]["path"], "/a", "{page}");
    let (c, _) = mcp.cursor_put(vec!["/c".into()]);
    mcp.cursor_next(&json!({ "cursorId": a }))
        .expect("recently-touched session survives the LRU eviction");
    let error = mcp.cursor_next(&json!({ "cursorId": b })).unwrap_err();
    assert!(error.contains("unknown cursorId"), "{error}");
    assert!(
        error.contains("re-run files_scan_digest"),
        "evicted cursorId answers with the fresh-digest guidance: {error}"
    );
    let _ = mcp.cursor_next(&json!({ "cursorId": c })).unwrap();

    // settings 缩容即时生效：8 → 1，下一次物化后表只剩最新一条。
    mcp.settings_set(&json!({ "maxCursorSessions": 8 })).unwrap();
    for index in 0..8 {
        mcp.cursor_put(vec![format!("/n{index}")]);
    }
    {
        let cursors = mcp.lock_cursors();
        assert_eq!(cursors.entries.len(), 8, "cap raised to 8 holds 8");
    }
    mcp.settings_set(&json!({ "maxCursorSessions": 1 })).unwrap();
    let (last, _) = mcp.cursor_put(vec!["/last".into()]);
    {
        let cursors = mcp.lock_cursors();
        assert_eq!(cursors.entries.len(), 1, "shrink evicts overflow at once");
    }
    mcp.cursor_next(&json!({ "cursorId": last }))
        .expect("the newest session survives");
}

#[test]
fn cursor_materialization_cap_churn_loops_stably() {
    let mcp = mcp();
    // 等效压测（真机 M12 用 DBX_FILES_MCP_CLAMP_FILES 调小同型）：物化
    // 上限钳到 sanitize 下限 100、会话上限 1，反复「打满 → 翻到底 →
    // 再物化（旧会话被逐）」50 轮，断言无 panic、行数恒被钳、表恒 1。
    mcp.settings_set(&json!({ "maxCursorRows": 100, "maxCursorSessions": 1 }))
        .unwrap();
    for round in 0..50 {
        let rows: Vec<String> = (0..101).map(|index| format!("/r{index}")).collect();
        let (cursor_id, truncated) = mcp.cursor_put(rows);
        assert!(truncated, "round {round}: 101 rows must report truncation");
        let page = mcp.cursor_next(&json!({ "cursorId": cursor_id, "offset": 99, "n": 20 }))
            .unwrap_or_else(|error| panic!("round {round}: {error}"));
        assert_eq!(page["rows"].as_array().unwrap().len(), 1, "{page}");
        assert_eq!(page["done"], true, "{page}");
        // 显式 offset 越过物化上限：恒空且 done（绝不漏出第 101 行）。
        let over = mcp.cursor_next(&json!({ "cursorId": cursor_id, "offset": 100 }))
            .unwrap();
        assert_eq!(over["rows"], json!([]), "{over}");
        {
            let cursors = mcp.lock_cursors();
            assert_eq!(cursors.entries.len(), 1, "round {round}: session table capped at 1");
            assert_eq!(
                cursors.entries[0].1.rows.len(),
                100,
                "round {round}: materialized rows clamped"
            );
        }
    }
}

#[test]
fn intent_churn_and_snapshot_stay_correct() {
    let mcp = mcp();
    let now = unix_millis_now() as u128;
    // 500 轮登记：LRU(20) + TTL 钳表；早期 intent 逐出、未知查报 Unknown。
    for index in 0..500 {
        mcp.register_intent(&format!("i-{index}"), "focus", json!({}), now);
    }
    assert!(matches!(
        mcp.intent_lookup("i-0", now),
        IntentLookup::Unknown
    ));
    assert!(matches!(
        mcp.intent_lookup("i-499", now),
        IntentLookup::Found(_)
    ));
    // 登记 → 回报 applied → 快照正确；过期 → Expired 且条目移除。
    mcp.register_intent("i-live", "search", json!({"path": "/data"}), now);
    let reported = mcp
        .report(&json!({
            "intentId": "i-live",
            "status": "applied",
            "summary": { "count": 3 },
        }))
        .unwrap();
    assert_eq!(reported["intentId"], "i-live", "{reported}");
    match mcp.intent_lookup("i-live", now) {
        IntentLookup::Found(state) => {
            assert_eq!(state["state"], "applied", "{state}");
            assert_eq!(state["summary"]["count"], 3, "{state}");
        }
        IntentLookup::Expired | IntentLookup::Unknown => {
            panic!("expected Found for i-live after an applied report")
        }
    }
    mcp.register_intent("i-dying", "select", json!({}), now);
    let future = now + 61_000; // TTL 60s 过后
    assert!(
        matches!(mcp.intent_lookup("i-dying", future), IntentLookup::Expired),
        "expired lookup reports Expired"
    );
    assert!(
        matches!(mcp.intent_lookup("i-dying", future), IntentLookup::Unknown),
        "the expired entry is removed on read"
    );
    // 过期条目不阻塞回报路径之外的快照：同一 UI state 反复上报 churn，
    // 快照恒为「最后写入」，形状不漂移。
    for round in 0..100 {
        let summary = json!({ "panel": "browse", "count": round });
        mcp.report(&json!({ "status": "snapshot", "summary": summary })).unwrap();
        let snapshot = mcp
            .snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .unwrap_or(Value::Null);
        assert_eq!(snapshot["count"], round, "snapshot is last-write-wins");
        assert_eq!(snapshot["panel"], "browse", "snapshot shape stable");
    }
}

#[test]
fn digest_is_idempotent_across_repeated_calls() {
    let mcp = mcp();
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::new();
    engine
        .connect(
            StoredConnection::from_lifecycle_params(&json!({
                "connection": {
                    "id": "dig",
                    "external_config": { "protocol": "fs", "root": dir.path() },
                }
            }))
            .unwrap(),
        )
        .unwrap();
    std::fs::create_dir_all(dir.path().join("d")).unwrap();
    for name in ["a.txt", "b.log"] {
        std::fs::write(dir.path().join("d").join(name), format!("content {name}")).unwrap();
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reference = rt
        .block_on(mcp.scan_digest_tool(&engine, &json!({ "connectionId": "dig", "path": "/d" })))
        .unwrap();
    for _ in 0..100 {
        let again = rt
            .block_on(mcp.scan_digest_tool(
                &engine,
                &json!({ "connectionId": "dig", "path": "/d" }),
            ))
            .unwrap();
        assert_eq!(again["matched"], reference["matched"], "{again}");
        assert_eq!(again["scanned"], reference["scanned"], "{again}");
        assert_eq!(again["stats"], reference["stats"], "aggregate drift");
        assert_eq!(again["sample"], reference["sample"], "sample drift");
        assert!(again["cursorId"].as_str().unwrap().starts_with("cur-"));
    }
    assert_eq!(reference["matched"], 2, "a.txt + b.log (no dir marker inside /d)");
}

// -- 第五轮（可靠性纵深）：MCP 路径形状门（对抗输入） -----------------------

#[test]
fn validate_path_shape_rejects_adversarial_segments() {
    // `..` 段（父遍历）与 `.` 段（根的混淆拼写）一律拒绝。
    for path in [
        "/..", "/../x", "/a/../b", "/a/../..", "a/../../etc", "/etc/..", "/.",
        "//.", "/./a", "/a/./b", "/data/./x.txt",
    ] {
        let error = validate_path_shape(path, "path").unwrap_err();
        assert!(
            error.contains("'.' or '..' path segment"),
            "{path}: {error}"
        );
    }
    // 控制字符（JSON \u0000 / 换行等）优雅报错，不 panic、不透传后端。
    for path in ["/a\nb", "/a\u{0}b", "/a\tb", "/a\u{1f}b"] {
        let error = validate_path_shape(path, "path").unwrap_err();
        assert!(error.contains("control characters"), "{path}: {error}");
    }
    // 超长路径（>4096 字节）：主动边界与清晰报文。
    let long = format!("/{}", "a".repeat(4096));
    let error = validate_path_shape(&long, "path").unwrap_err();
    assert!(error.contains("4096-byte path limit"), "{error}");
    // 合法形状放行（空格/unicode 文件名是正常输入）。
    for path in ["/a/b.txt", "/data/报告 v2.txt", "/café.txt"] {
        validate_path_shape(path, "path")
            .unwrap_or_else(|error| panic!("'{path}' should pass: {error}"));
    }
}

#[test]
fn file_target_path_combines_shape_and_root_gates() {
    assert_eq!(
        file_target_path("/a.txt/", "path").unwrap(),
        "/a.txt",
        "trailing slash dropped"
    );
    for (path, marker) in [
        ("/", "not the root itself"),
        ("//", "not the root itself"),
        ("/..", "'.' or '..' path segment"),
        ("/.", "'.' or '..' path segment"),
        ("/a/../..", "'.' or '..' path segment"),
    ] {
        let error = file_target_path(path, "path").unwrap_err();
        assert!(error.contains(marker), "{path}: {error}");
    }
}

#[test]
fn purge_and_delete_root_red_line_survives_bypass_spellings() {
    let (server, dir) = stdio_server();
    let fsroot = dir.path().join("fsroot-redline");
    let data = fsroot.join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("keep.txt"), "keep").unwrap();
    let connection = json!({ "protocol": "local", "root": fsroot.to_string_lossy() });
    // 根红线对抗拼写：`/` `//` `/.` `//.` `/./` `/..` `  `（空白归根）
    // ——全部拒绝（红线门或路径形状门），且磁盘树完好。
    for path in ["/", "//", "/.", "//.", "/./", "/..", "  ", "/../", "a/../../.."] {
        let error = stdio_error(
            &server,
            "tools/call",
            json!({
                "name": "files_purge",
                "arguments": { "connection": connection, "path": path },
            }),
        );
        assert!(
            error.contains("refused")
                || error.contains("'.' or '..' path segment")
                || error.contains("root"),
            "purge({path:?}) must stay refused: {error}"
        );
    }
    let error = stdio_error(
        &server,
        "tools/call",
        json!({
            "name": "files_delete",
            "arguments": { "connection": connection, "path": "a/../../etc" },
        }),
    );
    assert!(error.contains("'.' or '..' path segment"), "{error}");
    assert!(data.join("keep.txt").exists(), "nothing was deleted");
    assert!(fsroot.join("data").exists(), "root survived every spelling");
}

#[test]
fn tilde_and_percent_and_unicode_are_literal_never_expanded() {
    let (server, dir) = stdio_server();
    let fsroot = dir.path().join("fsroot-literal");
    std::fs::create_dir_all(&fsroot).unwrap();
    let connection = json!({ "protocol": "local", "root": fsroot.to_string_lossy() });
    // `~` 不展开：写的是连接根下的字面 `~` 目录（设计内保守行为）。
    unwrap_envelope(&stdio_result(
        &server,
        "tools/call",
        json!({
            "name": "files_write",
            "arguments": {
                "connection": connection,
                "path": "~/x.txt",
                "dataBase64": BASE64_STANDARD.encode("tilde"),
            },
        }),
    ));
    assert!(fsroot.join("~").join("x.txt").exists(), "~ stays literal");
    // URL 编码风格不解码：`%2e%2e` 是字面目录名，绝不逃逸连接根。
    unwrap_envelope(&stdio_result(
        &server,
        "tools/call",
        json!({
            "name": "files_mkdir",
            "arguments": { "connection": connection, "path": "/%2e%2e" },
        }),
    ));
    assert!(fsroot.join("%2e%2e").exists(), "percent stays literal");
    // NFC / NFD 组合：不归一化，两个拼写是两个独立字面条目（fs 语义）。
    let nfc = "/caf\u{e9}.txt";
    let nfd = "/cafe\u{301}.txt";
    for path in [nfc, nfd] {
        unwrap_envelope(&stdio_result(
            &server,
            "tools/call",
            json!({
                "name": "files_write",
                "arguments": {
                    "connection": connection,
                    "path": path,
                    "dataBase64": BASE64_STANDARD.encode("u"),
                },
            }),
        ));
    }
    let digest = unwrap_envelope(&stdio_result(
        &server,
        "tools/call",
        json!({
            "name": "files_scan_digest",
            "arguments": { "connection": connection, "path": "/", "glob": "*.txt" },
        }),
    ));
    // 插件契约是“不自行归一化”：两种拼写都按字面写成功（上方 unwrap）。
    // 落盘后剩几条由文件系统语义决定：APFS（macOS 默认卷）NFC/NFD 查找
    // 不敏感，后写覆盖先写只剩 1 条；ext4/NTFS 逐字节保留则是 2 条。
    // matched 还含 ~/x.txt（glob 对分隔符宽松），所以只数 café 条目。
    let cafe_entries = digest["sample"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter(|r| {
                    r["path"].as_str().is_some_and(|p| p.contains("caf"))
                })
                .count()
        })
        .unwrap_or(0);
    if cfg!(target_os = "macos") {
        assert_eq!(
            cafe_entries, 1,
            "APFS folds NFC/NFD; plugin itself must not: {digest}"
        );
    } else {
        assert_eq!(cafe_entries, 2, "NFC and NFD stay distinct entries: {digest}");
    }
}

// -- 第七轮（并发安全与口径拉齐）-----------------------------------------

#[test]
fn missing_required_enumerates_every_business_gap() {
    // 缺参枚举式（ssh 口径拉齐，MCP_ACCEPTANCE §3.9）：一次点名全部缺失
    // 的业务 required 参数，按 schema required 顺序。
    let arguments = json!({});
    let error = missing_required(&arguments, &["path", "dataBase64"]).unwrap_err();
    assert_eq!(error, "Missing required parameters: path, dataBase64");

    // null 视为缺失（absent 与 null 同罪）。
    let error = missing_required(
        &json!({ "path": null, "newPath": null }),
        &["path", "newPath"],
    )
    .unwrap_err();
    assert_eq!(error, "Missing required parameters: path, newPath");

    // 部分在场：只枚举缺失的那部分，保持 schema 顺序。
    let error = missing_required(&json!({ "path": "/a" }), &["path", "newPath"]).unwrap_err();
    assert_eq!(error, "Missing required parameters: newPath");

    // present-but-类型错误不混入枚举（由 required_str 精确点名）。
    let arguments = json!({ "path": 123, "newPath": "/b" });
    assert!(missing_required(&arguments, &["path", "newPath"]).is_ok());
    let error = required_str(&arguments, "path").unwrap_err();
    assert_eq!(error, "Parameter 'path' must be a non-empty string");

    // 空字符串不算缺失（files_write dataBase64="" 是合法空文件语义）。
    assert!(missing_required(&json!({ "dataBase64": "" }), &["dataBase64"]).is_ok());

    // 全部在场 → Ok。
    assert!(missing_required(&json!({ "path": "/a" }), &["path"]).is_ok());
}

#[test]
fn missing_required_flows_through_tool_dispatch() {
    let (server, dir) = stdio_server();
    // stdio 连接门先于业务参数校验（§3.7，call_tool 拦截）：完全空参先
    // 报连接寻址指引——顺序张力为登记设计，核对器 CONNECTION_GATE_RE
    // 归一为 WARN。
    let error = stdio_error(
        &server,
        "tools/call",
        json!({ "name": "files_write", "arguments": {} }),
    );
    assert!(
        error.contains("Missing required parameter: connectionId"),
        "{error}"
    );
    // 内联连接在场（合法可解析），业务缺参枚举即达：一次点名 path +
    // dataBase64（stdio schema required 去 connectionId 后的集合，
    // 核对器 B1 口径）。
    let inline = json!({ "protocol": "local", "root": dir.path().to_string_lossy() });
    let error = stdio_error(
        &server,
        "tools/call",
        json!({
            "name": "files_write",
            "arguments": { "connection": inline },
        }),
    );
    assert!(
        error.contains("Missing required parameters: path, dataBase64"),
        "{error}"
    );
    // files_rename：path 在场只缺 newPath → 单点。
    let error = stdio_error(
        &server,
        "tools/call",
        json!({
            "name": "files_rename",
            "arguments": {
                "connection": {
                    "protocol": "local",
                    "root": dir.path().to_string_lossy(),
                },
                "path": "/a",
            },
        }),
    );
    assert_eq!(error, "Missing required parameters: newPath", "{error}");
    // present-but-类型错误仍单独精确点名，不混入 Missing required 枚举。
    // 内联连接在场（合法可解析），业务参数类型错误才可达。
    let dir = tempfile::TempDir::new().unwrap();
    let error = stdio_error(
        &server,
        "tools/call",
        json!({
            "name": "files_write",
            "arguments": {
                "connection": {
                    "protocol": "local",
                    "root": dir.path().to_string_lossy(),
                },
                "path": 123,
                "dataBase64": "aGk=",
            },
        }),
    );
    assert!(
        error.contains("Parameter 'path' must be a non-empty string"),
        "{error}"
    );
    assert!(!error.contains("Missing required"), "{error}");
}

#[test]
fn normalized_format_is_case_insensitive_and_lists_legal_values() {
    assert_eq!(normalized_format(None).unwrap(), "digest");
    assert_eq!(normalized_format(Some("rows")).unwrap(), "rows");
    // 大小写归一（§3.3）：真实调用方 `format: "ROWS"` 正常工作。
    assert_eq!(normalized_format(Some("ROWS")).unwrap(), "rows");
    assert_eq!(normalized_format(Some("Digest")).unwrap(), "digest");
    assert_eq!(normalized_format(Some(" Rows ")).unwrap(), "rows");
    // 非法值报错列合法值并带原值点名。
    let error = normalized_format(Some("bogus")).unwrap_err();
    assert!(
        error.contains("format must be 'digest' or 'rows'") && error.contains("bogus"),
        "{error}"
    );
}

#[test]
fn depth_error_spells_the_legal_range_inline() {
    let (server, dir) = stdio_server();
    let fsroot = dir.path().join("fsroot");
    std::fs::create_dir_all(&fsroot).unwrap();
    let connection = json!({ "protocol": "local", "root": fsroot.to_string_lossy() });
    // 非数字 depth：报错点名参数并列出合法范围（§3.3），连接真实可解析。
    let error = stdio_error(
        &server,
        "tools/call",
        json!({
            "name": "files_scan_digest",
            "arguments": { "connection": connection, "path": "/", "depth": "bogus" },
        }),
    );
    assert!(error.contains("depth must be a non-negative integer"), "{error}");
    assert!(error.contains("1..=16"), "{error}");
}

/// 并发 confirm churn（Rust 侧等价于 Go -race 轮）：8 线程混并发
/// preview/consume/hold 2000+ 次签发（超 1024 硬上限，淘汰参与）。
/// 断言：令牌一次性（成功消费后重放必败）、表容量收敛 ≤1024、
/// 无过期残留。
#[test]
fn confirm_table_survives_concurrent_churn_without_double_consumption() {
    let table = Arc::new(mcp());
    let args = json!({ "connectionId": "c", "path": "/x" });
    let rounds = 250u32;
    let threads = 8usize;
    let barrier = Arc::new(std::sync::Barrier::new(threads));
    let mut handles = Vec::new();
    for _ in 0..threads {
        let table = Arc::clone(&table);
        let args = args.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            let mut held: Vec<String> = Vec::new();
            for round in 0..rounds {
                let (token, _) = table.confirm_begin(&args);
                match round % 3 {
                    0 => {
                        // 立即消费：一次成功（或同毫秒淘汰压力下的明确
                        // unknown），随后重放必须失败 —— 一次性。
                        match table.confirm_verify(&token, &args) {
                            Ok(()) => {}
                            Err(error) => assert!(
                                error.contains("unknown or already used"),
                                "fresh token must not fail with hash/expiry: {error}"
                            ),
                        }
                        assert!(
                            table.confirm_verify(&token, &args).is_err(),
                            "replay of a consumed token must fail"
                        );
                    }
                    1 => held.push(token),
                    _ => {} // 搁置：交给 TTL/上限淘汰路径
                }
            }
            for token in held {
                match table.confirm_verify(&token, &args) {
                    Ok(()) => {
                        assert!(
                            table.confirm_verify(&token, &args).is_err(),
                            "held token replay must fail"
                        );
                    }
                    Err(error) => {
                        // 并发淘汰下被逐的 token 明确报 unknown（设计内），
                        // 绝不允许 hash/expiry 之类别的错误。
                        assert!(
                            error.contains("unknown or already used"),
                            "held token failed unexpectedly: {error}"
                        );
                    }
                }
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    let confirms = table.confirms.lock().unwrap();
    assert!(
        confirms.len() <= Mcp::MAX_CONFIRM_ENTRIES,
        "capacity converges at the hard cap, got {}",
        confirms.len()
    );
    let now = unix_millis_now() as u128;
    assert!(
        confirms.values().all(|entry| entry.expires_at_millis > now),
        "no expired residue after concurrent churn"
    );
}

/// 并发 cursor churn：物化/翻页/淘汰混并发。阶段 A 钉「活跃会话不被
/// 误逐」（物化驻留 2 条 < cap 4，4 线程高并发 touch + 读路径 churn，
/// 活跃会话全程必活）；阶段 B 钉「容量收敛 + 翻页 offset 原子无重叠」
/// （8 线程 × 100 唯一会话超压物化——每次 cursor_put 都是新 UUID 会话
/// ——同时 4 线程并发翻同一会话，驱逐压力下成功翻页不重不漏越界）。
#[test]
fn cursor_table_survives_concurrent_materialize_page_evict() {
    let mcp = Arc::new(mcp());
    mcp.settings_set(&json!({ "maxCursorSessions": 4 })).unwrap();

    // 阶段 A：活跃保护。物化只进 2 条新会话（驻留 3 < cap 4，驱逐永不
    // 发生），5 个线程并发 touch 同一活跃会话（LRU get 的 remove+push
    // 与少量物化交错）——活跃会话绝不能在并发 touch 下失踪。
    let (active, _) = mcp.cursor_put((0..30).map(|index| format!("/a/{index}")).collect());
    {
        let seeders: Vec<_> = (0..2)
            .map(|index| {
                let mcp = Arc::clone(&mcp);
                std::thread::spawn(move || {
                    let _ = mcp.cursor_put(vec![format!("/seed/{index}")]);
                })
            })
            .collect();
        for handle in seeders {
            handle.join().unwrap();
        }
        let touches: Vec<_> = (0..5)
            .map(|_| {
                let mcp = Arc::clone(&mcp);
                let active = active.clone();
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        // 显式 offset=0：每次重读全窗（offset 语义见
                        // cursor_next——显式值任意重读）。n 被 clamp 到
                        // MAX_CURSOR_PAGE=20，30 行会话每页恒 20 行。
                        let page = mcp
                            .cursor_next(
                                &json!({ "cursorId": active, "n": 30, "offset": 0 }),
                            )
                            .expect("active session must survive concurrent touches");
                        assert_eq!(page["rows"].as_array().unwrap().len(), 20, "{page}");
                        assert_eq!(page["offset"], 0, "{page}");
                        assert_eq!(page["nextOffset"], 20, "{page}");
                    }
                })
            })
            .collect();
        for handle in touches {
            handle.join().unwrap();
        }
    }

    // 阶段 B：收敛 + 无重叠。8 线程 × 100 个唯一会话超压物化（>> cap），
    // 同时 4 线程并发翻同一 30 行会话（n=10，无 offset）：成功响应的行
    // 集合互不重叠（offset 推进原子，无双重消费）。
    let rows: Vec<String> = (0..30).map(|index| format!("/b/{index}")).collect();
    let (shared, _) = mcp.cursor_put(rows.clone());
    let barrier = Arc::new(std::sync::Barrier::new(8 + 4));
    let mut handles = Vec::new();
    for thread in 0..8 {
        let mcp = Arc::clone(&mcp);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for round in 0..100 {
                let (id, _) = mcp.cursor_put(vec![format!("/c/{thread}-{round}")]);
                assert!(id.starts_with("cur-"));
            }
        }));
    }
    let all_paths: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    for _ in 0..4 {
        let mcp = Arc::clone(&mcp);
        let barrier = Arc::clone(&barrier);
        let shared = shared.clone();
        let all_paths = Arc::clone(&all_paths);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            loop {
                let page = match mcp.cursor_next(&json!({ "cursorId": shared, "n": 10 })) {
                    Ok(page) => page,
                    // 并发淘汰压力下 unknown 是设计内结果（重发 digest）。
                    Err(error) => {
                        assert!(error.contains("unknown cursorId"), "{error}");
                        break;
                    }
                };
                let done = page["done"].as_bool().unwrap();
                for row in page["rows"].as_array().unwrap() {
                    all_paths
                        .lock()
                        .unwrap()
                        .push(row["path"].as_str().unwrap().to_string());
                }
                if done {
                    break;
                }
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    let mut paths = all_paths.lock().unwrap().clone();
    paths.sort();
    paths.dedup();
    assert!(
        paths.iter().all(|path| rows.contains(path)),
        "paged paths must come from the materialized set only"
    );
    // 容量收敛：阶段 B 后表 ≤ cap 且留存会话全未过期。
    let cursors = mcp.cursors.lock().unwrap();
    assert!(
        cursors.entries.len() <= 4,
        "cap converges, got {}",
        cursors.entries.len()
    );
    let now = unix_millis_now() as u128;
    assert!(
        cursors
            .entries
            .iter()
            .all(|(_, session)| session.expires_at_millis > now),
        "no expired cursor residue"
    );
}

/// stdio dispatch 是 spawn 并发（每行一个 tokio task，注释钉于
/// `run_mcp_stdio`）：16 个并发 dispatch 各带唯一 id，断言响应 id
/// 一一对应、互不串扰（id 关联性在并发下成立）。
#[test]
fn stdio_concurrent_dispatch_keeps_id_correlation() {
    let (server, _dir) = stdio_server();
    let server = Arc::new(server);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let mut handles = Vec::new();
        for id in 0..16u64 {
            let server = Arc::clone(&server);
            handles.push(tokio::spawn(async move {
                let request = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "tools/list",
                    "params": {},
                });
                (id, server.dispatch(request).await)
            }));
        }
        for handle in handles {
            let (id, response) = handle.await.unwrap();
            let response = response.expect("tools/list always answers");
            assert_eq!(response["id"], json!(id), "response id must correlate");
            assert!(
                response["result"]["tools"].as_array().unwrap().len() >= 12,
                "{response}"
            );
        }
    });
}
