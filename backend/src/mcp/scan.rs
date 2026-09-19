// ---------------------------------------------------------------------------
// Digest scan: filter, walk, aggregate
// ---------------------------------------------------------------------------

use crate::engine::ops;
use serde_json::{json, Value};

use std::collections::HashMap;

use super::{GROUP_LIMIT, TOP_N, numeric_arg_u64, optional_str};

/// One matched entry retained for aggregation/cursor materialization.
#[derive(Debug, Clone)]
pub struct PathRow {
    pub path: String,
    pub kind: &'static str,
    pub size: Option<u64>,
    pub modified_at: Option<u64>,
}

/// Local predicates (design §3.1): evaluated inside the sidecar against every
/// scanned entry; nothing but the verdict leaves the loop.
#[derive(Debug, Clone, Default)]
pub struct ScanFilter {
    pub glob: Option<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub modified_since: Option<u64>,
    pub modified_until: Option<u64>,
}

impl ScanFilter {
    pub(crate) fn from_arguments(arguments: &Value) -> Result<Self, String> {
        Ok(Self {
            glob: optional_str(arguments, "glob")?.map(str::to_string),
            min_size: numeric_arg_u64(arguments, "minSizeBytes")?,
            max_size: numeric_arg_u64(arguments, "maxSizeBytes")?,
            modified_since: numeric_arg_u64(arguments, "modifiedSince")?,
            modified_until: numeric_arg_u64(arguments, "modifiedUntil")?,
        })
    }

    /// A row matches when the glob (if any) hits and the size/mtime predicates
    /// pass. Directory entries only need the glob (size/mtime predicates do
    /// not apply to them); they are always walked.
    pub(crate) fn matches(&self, row: &PathRow) -> bool {
        if let Some(glob) = &self.glob {
            if !glob_match(glob, &row.path) {
                return false;
            }
        }
        if row.kind == "dir" {
            return true;
        }
        if let Some(min) = self.min_size {
            if row.size.unwrap_or(0) < min {
                return false;
            }
        }
        if let Some(max) = self.max_size {
            if row.size.unwrap_or(0) > max {
                return false;
            }
        }
        if let Some(since) = self.modified_since {
            match row.modified_at {
                Some(modified) if modified < since => return false,
                // Entries without mtime cannot prove recency; keep the row
                // rather than silently dropping half the tree.
                _ => {}
            }
        }
        if let Some(until) = self.modified_until {
            match row.modified_at {
                Some(modified) if modified <= until => {}
                Some(_) => return false,
                // Backends without mtime cannot prove recency; keep the row
                // rather than silently dropping half the tree.
                None => {}
            }
        }
        true
    }
}

/// Accumulator for the local walk.
pub(crate) struct WalkState {
    filter: ScanFilter,
    pub(crate) matched: Vec<PathRow>,
    pub(crate) scanned: usize,
    pub(crate) truncated: bool,
}

impl WalkState {
    pub(crate) fn new(filter: ScanFilter) -> Self {
        Self {
            filter,
            matched: Vec::new(),
            scanned: 0,
            truncated: false,
        }
    }

    /// Records one visited entry (filter applied, scanned counter advanced).
    /// The rclone walk (`tools.rs::scan_digest_rclone`) feeds pre-enumerated
    /// entries here instead of calling `ops::list` per level.
    pub(crate) fn visit(&mut self, row: PathRow) {
        self.scanned += 1;
        if self.filter.matches(&row) {
            self.matched.push(row);
        }
    }

    /// Whether the absolute scanned budget is used up.
    pub(crate) fn exhausted(&self, max_entries: usize) -> bool {
        self.scanned >= max_entries
    }
}

/// Breadth-first walk with a depth cap (design §6.2). Reuses `ops::list` per
/// level so directory vs. file kinds stay authoritative. `max_entries` is the
/// absolute scanned budget (100k clamp). Everything happens in the sidecar.
pub(crate) async fn walk_subtree(
    operator: &opendal::Operator,
    start: &str,
    depth: u32,
    state: &mut WalkState,
    max_entries: usize,
) -> Result<(), String> {
    let mut queue = std::collections::VecDeque::new();
    queue.push_back((start.to_string(), depth));
    while let Some((dir, remaining_depth)) = queue.pop_front() {
        if state.scanned >= max_entries {
            state.truncated = true;
            break;
        }
        let entries = ops::list(operator, &dir, false).await?;
        for entry in entries {
            state.scanned += 1;
            if state.scanned > max_entries {
                state.truncated = true;
                break;
            }
            let row = PathRow {
                path: entry.path.clone(),
                kind: entry.kind,
                size: entry.size,
                modified_at: entry.modified_at,
            };
            let is_dir = row.kind == "dir";
            if state.filter.matches(&row) {
                state.matched.push(row);
            }
            if is_dir && remaining_depth > 1 {
                queue.push_back((entry.path.clone(), remaining_depth - 1));
            }
        }
    }
    Ok(())
}

/// Digest shape limits (settings-driven; defaults are the design hard caps).
#[derive(Debug, Clone, Copy)]
pub struct DigestLimits {
    pub group_limit: usize,
    pub top_n: usize,
}

impl Default for DigestLimits {
    fn default() -> Self {
        Self {
            group_limit: GROUP_LIMIT,
            top_n: TOP_N,
        }
    }
}

/// Pure aggregation over the matched rows (unit-tested): total bytes,
/// extension group-by (≤group_limit, count desc), top-N largest/newest
/// (≤top_n). Directory entries never contribute to byte totals or file top-Ns.
pub fn aggregate_rows(rows: &[PathRow], limits: &DigestLimits) -> DigestStats {
    let mut total_bytes: u64 = 0;
    let mut groups: HashMap<String, (u64, u64)> = HashMap::new();
    let mut files: Vec<&PathRow> = Vec::new();
    for row in rows {
        if row.kind == "dir" {
            continue;
        }
        total_bytes += row.size.unwrap_or(0);
        let entry = groups
            .entry(extension_of(&row.path))
            .or_insert((0u64, 0u64));
        entry.0 += 1;
        entry.1 += row.size.unwrap_or(0);
        files.push(row);
    }
    let mut by_extension: Vec<Value> = groups
        .into_iter()
        .map(|(extension, (count, bytes))| {
            json!({
                "extension": extension,
                "count": count,
                "bytes": bytes,
            })
        })
        .collect();
    by_extension.sort_by(|a, b| {
        b["count"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["count"].as_u64().unwrap_or(0))
            .then_with(|| {
                a["extension"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["extension"].as_str().unwrap_or(""))
            })
    });
    by_extension.truncate(limits.group_limit);
    files.sort_by(|a, b| b.size.unwrap_or(0).cmp(&a.size.unwrap_or(0)));
    let largest: Vec<Value> = files.iter().take(limits.top_n).map(|row| row_json(row)).collect();
    files.sort_by(|a, b| {
        b.modified_at
            .unwrap_or(0)
            .cmp(&a.modified_at.unwrap_or(0))
    });
    let newest: Vec<Value> = files.iter().take(limits.top_n).map(|row| row_json(row)).collect();
    DigestStats {
        total_bytes,
        by_extension,
        largest,
        newest,
    }
}

pub struct DigestStats {
    pub total_bytes: u64,
    pub by_extension: Vec<Value>,
    pub largest: Vec<Value>,
    pub newest: Vec<Value>,
}

fn row_json(row: &PathRow) -> Value {
    let mut value = json!({
        "path": row.path,
        "kind": row.kind,
    });
    if let Some(size) = row.size {
        value["size"] = json!(size);
    }
    if let Some(modified) = row.modified_at {
        value["modifiedAt"] = json!(modified);
    }
    value
}

pub(crate) fn take_rows(rows: &[PathRow], limit: usize) -> Vec<Value> {
    rows.iter().take(limit).map(row_json).collect()
}

/// Lowercased extension without the dot, or "(none)" for extensionless names
/// (dotfiles count as extensionless).
pub fn extension_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            extension.to_ascii_lowercase()
        }
        _ => "(none)".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Glob matching (pure, unit-tested): ** crosses segments, * stays within one,
// ? matches a single char.
// ---------------------------------------------------------------------------

pub fn glob_match(pattern: &str, text: &str) -> bool {
    // Slash-free patterns match the basename (common glob semantics);
    // path-shaped patterns match against the full path. The leading slash of
    // both sides is trimmed so "/a/*.txt" and "a/*.txt" behave identically
    // (otherwise split('/') would inject an empty leading pattern segment).
    if !pattern.contains('/') {
        let basename = text.rsplit('/').next().unwrap_or(text);
        return segment_match(pattern, basename);
    }
    let text = text.trim_start_matches('/');
    let pattern = pattern.trim_start_matches('/');
    match_segments(
        &pattern.split('/').collect::<Vec<_>>(),
        &text.split('/').collect::<Vec<_>>(),
    )
}

fn match_segments(pattern: &[&str], text: &[&str]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((&"**", rest)) => {
            // `**` consumes zero or more segments.
            if match_segments(rest, text) {
                return true;
            }
            (1..=text.len()).any(|index| match_segments(rest, &text[index..]))
        }
        Some((&first, rest)) => {
            let Some((head, tail)) = text.split_first() else {
                return false;
            };
            segment_match(first, head) && match_segments(rest, tail)
        }
    }
}

/// Single-segment wildcard match (`*` and `?` never cross '/').
fn segment_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    fn inner(p: &[char], t: &[char]) -> bool {
        match (p.split_first(), t.split_first()) {
            (None, None) => true,
            (None, Some(_)) => false,
            (Some((&'*', rest)), _) => (0..=t.len()).any(|skip| inner(rest, &t[skip..])),
            (Some((&'?', rest)), Some((_, t_rest))) => inner(rest, t_rest),
            (Some((&pc, rest)), Some((&tc, t_rest))) => pc == tc && inner(rest, t_rest),
            (Some(_), None) => false,
        }
    }
    inner(&pattern, &text)
}
