#!/usr/bin/env python3
"""Validate standalone Files plugin identity and package-relative paths."""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SEMVER = re.compile(r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")


def fail(message: str) -> None:
    raise SystemExit(f"FAIL: {message}")


def check_backend_method_parity() -> int:
    """backendParity.spec.ts 的 GOLDEN 必须与 main.rs 分发臂严格一致（后端方向）。

    前端 spec 只能拦「mock 实现了 GOLDEN 之外的方法」；后端新增分发臂而
    GOLDEN 未跟的漂移方向（运行时报 method-not-found）由这里断言——浏览器
    工程刻意不带 node:fs，读不了 main.rs，所以放在 Python 校验层。
    提取规则：main.rs 里 `"…/…" | "…/…" =>` 形态的分发臂字面量（事件/通道
    名如 files/list/chunk 不在 `=>` 紧邻位，不会误收）。
    """
    spec_path = ROOT / "frontend/src/lib/backendParity.spec.ts"
    spec = spec_path.read_text(encoding="utf-8")
    section = re.search(
        r"GOLDEN_BACKEND_METHODS: ReadonlySet<string> = new Set\(\[(.*?)\]\);",
        spec, re.S,
    )
    if not section:
        fail("backendParity.spec.ts GOLDEN_BACKEND_METHODS declaration not found")
    golden = set(re.findall(r'"((?:connection|files|mcp)/[^"]+)"', section.group(1)))

    main_rs = (ROOT / "backend/src/main.rs").read_text(encoding="utf-8")
    arms: set[str] = set()
    for line in main_rs.splitlines():
        if "=>" not in line:
            continue
        for literal in re.finditer(
            r'"((?:connection|files|mcp)/[A-Za-z][A-Za-z/]*)"(?=\s*(?:\||=>))', line,
        ):
            arms.add(literal.group(1))

    if not golden:
        fail("GOLDEN_BACKEND_METHODS extracted empty — extraction rule drifted")
    missing_in_golden = sorted(arms - golden)
    stale_in_golden = sorted(golden - arms)
    if missing_in_golden:
        fail(
            "backend dispatch arms missing from GOLDEN_BACKEND_METHODS "
            f"(add them to backendParity.spec.ts): {missing_in_golden}"
        )
    if stale_in_golden:
        fail(
            "GOLDEN_BACKEND_METHODS lists methods with no dispatch arm in "
            f"main.rs (remove or reconcile): {stale_in_golden}"
        )
    return len(golden)


def main() -> int:
    manifest_path = ROOT / "manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if manifest.get("manifest_version") != 1:
        fail("manifest_version must be 1")
    if manifest.get("id") != "io.dbx.files":
        fail(f"manifest id is {manifest.get('id')!r}, expected io.dbx.files")
    version = manifest.get("version")
    if not isinstance(version, str) or not SEMVER.fullmatch(version):
        fail(f"invalid manifest version: {version!r}")
    for field in ("source", "homepage"):
        value = manifest.get(field, "")
        if not isinstance(value, str) or not value.startswith(("http://", "https://")):
            fail(f"{field} must be an HTTP(S) URL")

    entrypoint = manifest.get("entrypoints", {}).get("backend", {})
    if entrypoint.get("executable") != "bin/dbx-plugin-files":
        fail("backend executable must be bin/dbx-plugin-files")
    if manifest.get("entrypoints", {}).get("ui", {}).get("entry") != "ui/index.html":
        fail("UI entry must be ui/index.html")

    toml = (ROOT / "dbx-plugin.toml").read_text(encoding="utf-8")
    if 'directory = "backend"' not in toml or 'binary = "dbx-plugin-files"' not in toml:
        fail("dbx-plugin.toml backend identity/path is stale")

    cargo = (ROOT / "backend/Cargo.toml").read_text(encoding="utf-8")
    if not re.search(r'(?m)^name\s*=\s*"dbx-plugin-files"\s*$', cargo):
        fail("backend Cargo package name does not match manifest")
    if not re.search(rf'(?m)^version\s*=\s*"{re.escape(version)}"\s*$', cargo):
        fail("backend Cargo version does not match manifest")
    main_rs = (ROOT / "backend/src/main.rs").read_text(encoding="utf-8")
    if 'PluginMetadata::new("io.dbx.files", env!("CARGO_PKG_VERSION"))' not in main_rs:
        fail("backend initialization identity is missing or stale")

    required = [
        "assets/plugin.svg", "frontend/package.json", "backend/Cargo.toml",
        "scripts/test.sh", "scripts/connection-forms/verify.mjs",
        "shared/frontend/binaryEvent.ts", "shared/frontend/themeSync.ts",
        "shared/frontend/uiIntent.ts", "shared/frontend/editorTheme.ts",
        "shared/sdk/rust/dbx-plugin-sdk/src/lib.rs",
    ]
    for relative in required:
        if not (ROOT / relative).exists():
            fail(f"missing required path: {relative}")

    method_count = check_backend_method_parity()

    print(f"PASS repository identity: {manifest['id']} {version}; standalone paths and vendored SDK present")
    print(f"PASS backend method parity: {method_count} dispatch arms == GOLDEN_BACKEND_METHODS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
