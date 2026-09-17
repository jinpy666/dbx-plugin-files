// ---------------------------------------------------------------------------
// Response envelope + token-economy caps
// ---------------------------------------------------------------------------

use serde_json::{json, Value};

/// Locator fields keep their full text across every truncation pass.
const ANCHOR_FIELDS: [&str; 3] = ["path", "intentId", "cursorId"];

/// MCP content envelope (ssh `call_tool` shape).
pub(crate) fn content_envelope(payload: &Value) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(payload).unwrap_or_default(),
        }],
        "isError": false,
    })
}

/// Truncates a string to `width` chars (char-boundary safe).
pub fn truncate_cell(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        text.chars().take(width).collect()
    }
}

/// Token-economy caps on a finished tool payload (design §3): cells to
/// `cell_width` chars (anchor fields excepted), then arrays trimmed until the
/// serialization fits `max_bytes`; a final fallback hard-truncates remaining
/// long strings when no array can shrink further. `truncated: true` is set on
/// the top-level object when anything was cut. Returns the truncated flag.
pub fn cap_response(value: &mut Value, max_bytes: usize, cell_width: usize) -> bool {
    let cell_cut = cap_cells(value, cell_width);
    let mut trimmed = false;
    while serde_json::to_string(value)
        .map(|text| text.len())
        .unwrap_or(0)
        > max_bytes
    {
        if !trim_longest_array(value) {
            break;
        }
        trimmed = true;
    }
    let still_over = serde_json::to_string(value)
        .map(|text| text.len())
        .unwrap_or(0)
        > max_bytes;
    if still_over && hard_truncate_strings(value, cell_width) {
        trimmed = true;
    }
    let truncated = cell_cut || trimmed;
    if truncated {
        if let Some(map) = value.as_object_mut() {
            map.insert("truncated".to_string(), Value::Bool(true));
        }
    }
    truncated
}

/// Pass 1: every string cell longer than `width` is cut, except object
/// members named in [`ANCHOR_FIELDS`] (design §3 "定位字段不截断").
fn cap_cells(value: &mut Value, width: usize) -> bool {
    let mut cut = false;
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if ANCHOR_FIELDS.contains(&key.as_str()) {
                    continue;
                }
                cut |= cap_cells(entry, width);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                cut |= cap_cells(item, width);
            }
        }
        Value::String(text) if text.chars().count() > width => {
            *text = truncate_cell(text, width);
            cut = true;
        }
        _ => {}
    }
    cut
}

/// Pass 2: drop the last element of the largest non-empty array. Returns
/// false when no non-empty array remains.
fn trim_longest_array(value: &mut Value) -> bool {
    fn longest(value: &Value) -> Option<usize> {
        match value {
            Value::Object(map) => map.values().filter_map(longest).max(),
            Value::Array(items) => {
                let own = (!items.is_empty()).then_some(items.len());
                own.into_iter()
                    .chain(items.iter().filter_map(longest))
                    .max()
            }
            _ => None,
        }
    }
    fn pop(value: &mut Value, target_len: usize) -> bool {
        match value {
            Value::Object(map) => map.values_mut().any(|entry| pop(entry, target_len)),
            Value::Array(items) => {
                if items.len() == target_len {
                    items.pop();
                    return true;
                }
                items.iter_mut().any(|item| pop(item, target_len))
            }
            _ => false,
        }
    }
    longest(value).map(|len| pop(value, len)).unwrap_or(false)
}

/// Fallback pass: truncate every remaining long string (anchors included).
fn hard_truncate_strings(value: &mut Value, width: usize) -> bool {
    let mut cut = false;
    match value {
        Value::Object(map) => {
            for entry in map.values_mut() {
                cut |= hard_truncate_strings(entry, width);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                cut |= hard_truncate_strings(item, width);
            }
        }
        Value::String(text) if text.chars().count() > width => {
            *text = truncate_cell(text, width);
            cut = true;
        }
        _ => {}
    }
    cut
}
