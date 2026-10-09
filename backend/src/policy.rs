//! Connection-level policy gates for the files plugin.
//!
//! Two orthogonal policy surfaces live here (implementation plan §9):
//!
//! 1. **Path whitelist** — every user-supplied path is sanitized and, when
//!    `lock_to_root` is set, required to stay inside the configured `root`.
//!    Traversal (`..`) that escapes the visible space is always rejected,
//!    independent of `lock_to_root`.
//! 2. **Write gates** — `read_only` rejects every mutating operation;
//!    `allow_delete` additionally rejects `delete`/`rmdir`/`purge` (and the
//!    implicit source deletion of `rename`/`move`).
//!
//! (The former third surface — egress URL validation — was removed: it had
//! zero production call sites, and its `Derived`-origin semantics would have
//! blocked legitimate user-typed intranet/loopback `files/copyurl` sources.
//! See the 0.1.81 adversarial review, verdict #3.)
//! All functions return plain `String` errors; `main.rs` wraps them into
//! plugin error code `-32000`.

// Wired into the crate from `engine/ops.rs` (main.rs is frozen), so several
// public helpers are consumed only by tests / the consolidated call sites.
#![allow(dead_code)]

use serde_json::Value;

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
    /// Operator-relative path handed to the storage backend (`""` denotes
    /// the root).
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
    /// - `"/a/b"` — absolute within the backend-visible space, or the
    ///   listing vocabulary: `files/list` entries carry fs-relative paths
    ///   spelled with a leading `/` (rclone ports the root into the fs
    ///   string, so for rooted connections the visible space IS the root
    ///   and `/sub` addresses `root/sub`). A slash-prefixed spelling that
    ///   is not at/under the configured root is therefore re-resolved
    ///   root-relative — otherwise every path the backend itself returns
    ///   would be an invalid input on locked-root connections (the
    ///   "outside the locked connection root" round-trip bug).
    ///
    /// When `lock_to_root` is set and a root is configured, every resolved
    /// path stays inside `root`; root-prefixed absolute spellings keep their
    /// absolute reading. Rejected: NUL/control characters and any `..` that
    /// escapes the visible space (backslash spellings are probed under the
    /// worst-case `\`-as-separator reading; literal backslash names pass
    /// through — see [`sanitize`]).
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
            // cloned: the listing-vocabulary respell below still reads it.
            sanitized.clone()
        };

        if self.lock_to_root && self.root != "/" {
            let under_root = absolute == self.root
                || absolute.starts_with(&format!("{}/", self.root));
            if !under_root {
                if relative_input {
                    // Unreachable by construction (a relative input joins
                    // onto the root above); kept as the lock's defensive
                    // rejection should the resolution forms ever grow.
                    return Err(format!(
                        "path '{path}' is outside the locked connection root '{}'",
                        self.root
                    ));
                }
                // Listing vocabulary: re-read the slash-prefixed spelling as
                // root-relative (`/sub` → `sub` → `root/sub`). Terminates:
                // the re-spelled input is relative, so the recursive call
                // lands under the root directly.
                let respelled = sanitized.trim_start_matches('/');
                return self.resolve(respelled);
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
///
/// Windows drive-letter roots (`C:\data`, `C:\`) use `\` as separator — the
/// `..`-escape probe reads `\` as a separator, so without the drive-form
/// pre-normalization a root like `C:\data..\x` would read differently than
/// its forward-slash spelling, and `C:\data` vs `C:/data` would behave
/// differently with no warning (review FILES-M3). Drive-form input is
/// backslash-normalized to `/` first, so both spellings canonicalize
/// identically and the lock applies.
fn canonical_root(root: &str) -> String {
    let trimmed = root.trim_end_matches('/');
    let is_drive_form = {
        let bytes = trimmed.as_bytes();
        bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
    };
    let normalized = if is_drive_form {
        trimmed.replace('\\', "/")
    } else {
        trimmed.to_string()
    };
    let canonical = sanitize(&normalized).unwrap_or_else(|_| "/".to_string());
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
///
/// Control characters are always rejected. Backslashes are LEGAL: object
/// store keys (s3 family) and non-Windows file names carry them literally,
/// and listings return such entries verbatim — rejecting them at read time
/// contradicted the listing (issue #80). The traversal red line is instead
/// enforced under the worst-case separator reading: `\` is normalized to `/`
/// for a `..`-escape check (on Windows fs `\` IS a separator), while the
/// literal path is handed to the backend untouched.
fn sanitize(path: &str) -> Result<String, String> {
    if path.chars().any(|ch| ch.is_control()) {
        return Err(format!("path '{path}' contains control characters"));
    }
    if path.contains('\\') && escapes_via_backslash(path) {
        return Err(format!(
            "path '{path}' escapes the connection root via backslash traversal"
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

/// `\`-aware traversal probe: would this path escape the visible space if
/// every backslash were a separator? Pure check — the literal path travels
/// on (object keys keep their backslashes byte-for-byte).
fn escapes_via_backslash(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let mut depth: usize = 0;
    for segment in normalized.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if depth == 0 {
                    return true;
                }
                depth -= 1;
            }
            _ => depth += 1,
        }
    }
    false
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
            // `\` 按最坏分隔符解释（Windows fs）做穿越检测：两级 `..` 逃出
            // 可视空间顶层 → 拒绝（issue #80 修复后的红线）。
            "a\\..\\..\\x",
            "\\..\\x",
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

    /// issue #80 回归（图 2：`txffp\20201214\…jpg`）：对象存储 key 与非
    /// Windows 文件名里的反斜杠是合法字面字符，列表能原样返回这类条目，
    /// 读侧不得自相矛盾地拒绝。字面路径逐字节透传；只有按最坏分隔符解释
    /// 会 `..` 逃逸的拼写才被拒。
    #[test]
    fn resolve_passes_backslash_names_through_literally() {
        let policy = policy("/", false, false, true);
        let cases = [
            "txffp\\20201214\\79e794bf3c8d4647b3f76bf25b4eb13e.jpg",
            "flat\\back.jpg",
            "a,b.csv",
            "balance#alipay#20220822#QRA_1.csv",
            // 单级 `..\` 不逃逸（Windows 语义下等价 `a/../b`，仍在可视空间）。
            "a\\..\\b",
        ];
        for input in cases {
            let resolved = policy
                .resolve(input)
                .unwrap_or_else(|err| panic!("resolve('{input}') should pass: {err}"));
            // 字面透传：relative 与输入逐字节一致（根为 / 时）。
            assert_eq!(resolved.relative, input, "'{input}' must travel literally");
        }
    }

    /// 工具面词汇回环（本仓 bug 报告：锁定的本地 root 连接点目录树报
    /// "outside the locked connection root"）。files/list 返回的 entry.path
    /// 恒为 `/sub` 形态（fs 相对、带前导斜杠，entry_from_item 契约）；该
    /// 路径必须能原样传回任何 files/* 操作——否则后端自己给出的路径是
    /// 非法输入。带前导斜杠且不在 root 下的拼写按「root 相对」重解；
    /// root 前缀绝对拼写与 `..` 逃逸语义不变。
    #[test]
    fn resolve_accepts_listing_vocabulary_round_trip() {
        // 局部不得叫 `policy`：会遮蔽本块的 `policy(..)` 构造帮助函数。
        let locked = policy("/Users/Jinpy/Downloads", true, false, true);
        // 列根得到的子目录路径（tree 节点/文件表格/MCP list 条目都携带它）。
        let resolved = locked
            .resolve("/微信支付账单(20250301-20250510)")
            .unwrap_or_else(|err| panic!("listing entry path must round-trip: {err}"));
        assert_eq!(resolved.absolute, "/Users/Jinpy/Downloads/微信支付账单(20250301-20250510)");
        assert_eq!(resolved.relative, "微信支付账单(20250301-20250510)");
        // 深层条目同语义。
        let resolved = locked.resolve("/sub/deep").expect("deep listing path");
        assert_eq!(resolved.absolute, "/Users/Jinpy/Downloads/sub/deep");
        assert_eq!(resolved.relative, "sub/deep");
        // 多余前导斜杠同样归一。
        let resolved = locked.resolve("//sub").expect("double slash");
        assert_eq!(resolved.absolute, "/Users/Jinpy/Downloads/sub");
        // 未锁定连接上带斜杠拼写保持原语义（绝对拼写字面保留、relative 取
        // 尾段——逐字不变）；词汇重解只在 lock_to_root 下发生。
        let unlocked = policy("/Users/Jinpy/Downloads", false, false, true);
        let unlocked_resolved = unlocked.resolve("/sub/deep").unwrap();
        assert_eq!(unlocked_resolved.absolute, "/sub/deep");
        assert_eq!(unlocked_resolved.relative, "sub/deep");
    }

    /// 锁定连接的越界拒绝面收窄到真实逃逸：`..` 逃逸依旧拒绝；root 前缀
    /// 绝对拼写依旧按绝对处理（等于 root 自身合法）。
    #[test]
    fn resolve_locked_still_rejects_traversal() {
        let policy = policy("/mnt/nas", true, false, true);
        let error = policy.resolve("../escape").unwrap_err();
        assert!(error.contains("escapes the connection root"), "{error}");
        assert!(policy.resolve("/mnt/nas").is_ok());
        assert!(policy.resolve("/mnt/nas/sub").is_ok());
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
            // listing 词汇：带前导斜杠的非 root 前缀拼写 = root 相对。
            ("/sub", "/mnt/nas/sub", "sub"),
        ];
        for (input, absolute, relative) in cases {
            let resolved = policy
                .resolve(input)
                .unwrap_or_else(|err| panic!("resolve('{input}') should pass: {err}"));
            assert_eq!(resolved.absolute, absolute, "input '{input}'");
            assert_eq!(resolved.relative, relative, "input '{input}'");
        }
        for input in ["../escape", "/"] {
            // "/" resolves to the root itself and is fine — only traversal
            // escapes must fail. Slash-prefixed spellings are the listing
            // vocabulary and resolve root-relative (round-trip test above).
            if input == "/" {
                assert!(policy.resolve(input).is_ok());
            } else {
                assert!(
                    policy.resolve(input).is_err(),
                    "resolve('{input}') should be rejected"
                );
            }
        }
    }

    /// Review FILES-M3 regression: a Windows drive-letter root written with
    /// backslashes (`C:\data`) used to be rejected by sanitize and silently
    /// fall back to `/`, making `lock_to_root` a no-op. Drive-form input
    /// must backslash-normalize to the exact same canonical root as its
    /// forward-slash spelling, and the lock must actually lock.
    ///
    /// 词汇语义更新后（listing round-trip）："锁定"的形态是解析保证——
    /// 带斜杠的越界拼写在 rooted 连接上按 root 相对重解（`/C:/elsewhere`
    /// → `C:/elsewhere` → `root/C:/elsewhere`），不再硬拒；相对输入与
    /// root 前缀绝对拼写仍在 root 下解析。canonicalization 回归（两种盘
    /// 符拼写出同一 root，不再静默回落 `/`）保持不变。
    #[test]
    fn windows_drive_root_normalizes_and_lock_to_root_applies() {
        let backslash = PathPolicy::from_parts("C:\\data", true, false, true);
        let forward = PathPolicy::from_parts("C:/data", true, false, true);
        // Both spellings canonicalize identically (`/C:/data`)...
        assert_eq!(backslash.root, forward.root, "{:?} vs {:?}", backslash.root, forward.root);
        assert_eq!(backslash.root, "/C:/data");
        // ...and the lock resolves: relative and root-prefixed inputs land
        // inside the drive-form root (before the FILES-M3 fix the root fell
        // back to "/" and nothing was scoped at all).
        assert!(backslash.resolve("sub/f").is_ok());
        assert!(backslash.resolve("/C:/data/sub/f").is_ok());
        assert_eq!(
            backslash.resolve("/C:/elsewhere").unwrap().relative,
            "C:/elsewhere",
            "slash-prefixed outside spelling is the listing vocabulary → root-relative"
        );
        assert_eq!(backslash.resolve("/C:/datax").unwrap().relative, "C:/datax");
        assert_eq!(forward, backslash);
        // Bare drive root (`C:\` == `C:/`): also normalized, still scoped —
        // the bare drive root scopes the whole drive.
        let bare = PathPolicy::from_parts("C:\\", true, false, true);
        assert_eq!(bare.root, "/C:");
        assert!(bare.resolve("f").is_ok());
        assert!(bare.resolve("/C:/other/f").is_ok());
        assert!(bare.resolve("/D:/elsewhere").is_ok());
    }

    #[test]
    fn resolve_without_lock_allows_absolute_outside_root() {
        let policy = policy("/mnt/nas", false, false, true);
        let resolved = policy.resolve("/etc/x").expect("unlocked absolute pass");
        assert_eq!(resolved.absolute, "/etc/x");
        // Physically confined by the Operator root; the engine hands the
        // operator-relative remainder to the backend.
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
        // 反斜杠拼写按最坏分隔符解释探穿越（issue #80 修复后的红线）。
        for input in [
            "/..",
            "../x",
            "a/../../etc",
            "/a/../..",
            "a\\..\\..\\b",
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
        // 根相对语义，`~` 依旧是字面名）。反斜杠名是合法字面（issue #80：
        // 对象存储 key / 非 Windows 文件名），`\..\` 单级拼写不逃逸同样
        // 字面传递——穿越红线由 escapes_via_backslash 的最坏解释守住。
        for (input, literal) in [
            ("~", "/~"),
            ("/home/u/~x", "/home/u/~x"),
            ("/etc/file.", "/etc/file."),
            ("/dir/%2e%2e/next", "/dir/%2e%2e/next"),
            ("/caf\u{e9}", "/caf\u{e9}"),   // NFC
            ("/cafe\u{301}", "/cafe\u{301}"), // NFD 组合（与 NFC 不同字节 → 不同条目）
            ("/data/报告 v2.txt", "/data/报告 v2.txt"), // 空格与 unicode 文件名合法
            ("/txffp\\20201214\\a.jpg", "/txffp\\20201214\\a.jpg"),
            ("/a\\..\\b", "/a\\..\\b"),
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

}
