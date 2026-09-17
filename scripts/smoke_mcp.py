#!/usr/bin/env python3
"""MCP smoke test for the dbx-files-plugin sidecar (M2 of the plugin-MCP plan,
scenarios M1-M10 — the same case table as ldap/scripts/smoke_mcp.py).

Covers shared/IMPL_PLAN_PLUGIN_MCP.zh-CN.md (v2) §1/§2/§3/§4/§6.2 over the
stdio-framed protocol (mcp/tools + mcp/call + mcp/settings, the ssh mcp.rs
skeleton, shapes aligned with the ldap Go implementation):

    M1  mcp/settings/get + set           -> defaults, partial update, invalid refused
    M2  mcp/tools lists the 10 files tools -> names + JSON Schema required fields
    M3  read-only connection tools list  -> write tools omitted with reason
    M4  write tool on read-only conn     -> refused
    M5  UI intent without a frontend     -> state=pending + digest fallback hint
    M6  UI intent with a report          -> state=applied + summary roundtrip
    M7  files_ui_state                   -> by intentId + latest snapshot
    M8  digest/cursor gates              -> unknown connection/cursor refused
    M9  unknown tool / method            -> clear error (SKIP semantics)
    M10 digest + cursor + two-phase writes on a temp-dir fs connection
        (no container needed: the tree is built through the MCP write tools;
        two-phase purge success + quick_paths limit included)
    M11 LLM input variants               -> numeric strings for numeric params,
        empty dataBase64 creates an empty file, trailing-slash file delete is
        real (never a silent no-op), unknown params tolerated, unknown cursor
        guides back to a fresh digest
    M12 big-tree clamp (real 100k)       -> scanned clamps at the 100k budget
        with scanTruncated, cursor materialization caps at 10k rows
        (cursorTruncated + paging stops there); DBX_FILES_MCP_CLAMP_FILES
        tunes the tree size (default 100001 files > the budget)
    M13 TTL real-time expiry             -> cursorTtlSecs/confirmTtlSecs tuned
        to the 10s floor, one real wait proves BOTH expiry messages carry the
        actual tuned TTL ("cursor expired (TTL 10s)" /
        "confirmToken expired (TTL 10s)") on the real clock, not injected

Remote-protocol container sections (R1-R7, stdio `--mcp` inline credentials):
each SKIPs unless its env vars are set — scripts/container_smoke.sh provides
them (MinIO / mod_dav / pyftpdlib / OpenSSH / Samba / russh password target):

    R1  s3 (MinIO)    DBX_FILES_S3_ENDPOINT/BUCKET/ACCESS_KEY/SECRET_KEY
    R2  webdav (mod_dav)  DBX_FILES_WEBDAV_ENDPOINT/USER/PASSWORD
    R3  ftp (pyftpdlib)   DBX_FILES_FTP_ENDPOINT/USER/PASSWORD
    R4  sftp (OpenSSH, keyfile auth)  DBX_FILES_SFTP_HOST/PORT/USER/KEY
    R5  smb (Samba, share-scoped)     DBX_FILES_SMB_HOST/PORT/SHARE/USER/PASSWORD
    R6  sftp-native (russh password)  DBX_FILES_SFTP_NATIVE_HOST/PORT/USER/PASSWORD
    R7  oss (env-gated; no OSS-API-compatible test container exists —
        MinIO speaks S3, not the Aliyun OSS API OpenDAL's oss service
        speaks; set DBX_FILES_OSS_ENDPOINT/BUCKET/ACCESS_KEY/SECRET_KEY
        against a real OSS endpoint to enable)

    Every R section runs the full agent loop over inline credentials:
    files_write/mkdir -> files_scan_digest (aggregate) -> files_cursor_next
    (paging) -> files_delete two-phase (tampered args invalidate, confirm
    executes) -> files_purge two-phase; deletions are verified by re-digest
    matched counts (the MCP face never reads file bodies). Each connection
    pins its protocol-specialized auth/path shape: R4 proves keyfile-only
    auth end to end (no password travels at all — OpenDAL 0.57's sftp
    service has none), R5 pins the share-scoped endpoint + secret-bound
    password over the custom adapter's recursive purge, R6 proves password
    auth the OpenDAL sftp service cannot do, R7 (when enabled) proves the
    s3-shaped secret binding lands on the oss `access_key_secret` key. The
    loop builds its tree under a unique writable `base` directory (never a
    connection root): home-restricted SSH servers cannot materialize a root
    at filesystem / (R4/R6 first-run finding, see the R section comment).

Standalone stdio mode (`--mcp`, design §0.2/§5 stdio row; scenarios S1-S4,
no external dependency — always run, never SKIP):

    S1  initialize / ping / tools/list / notifications / unknown method
        (-32601, family-wide) / parse error (-32700)
    S2  UI intent tools in stdio    -> explicit UNAVAILABLE (not pending)
    S3  localFs inline credentials  -> digest + cursor real round-trip
        (tree built through the stdio write tools; pooled connectionId;
        missing connectionId guidance)
    S4  two-phase delete full flow  -> preview -> tampered args invalidate ->
        re-preview -> confirm executes -> file gone on disk -> replay refused;
        directory rename stdio refusal

    B1  stdio app-bridge fallback  -> unpooled connectionId forwards to a
        local mock of the DBX app bridge (snake_case five-field contract,
        MCP envelope verbatim, HTTP 404 surfaced); with the bridge
        unpublished the call fails closed with the "DBX app bridge" reason
        plus the inline-credential ways out (30s wake budget, ssh 场景 8 /
        ldap M14 同款). Always run (mock is in-process), never SKIP.

    T1-T6 stdio transport robustness (可靠性纵深轮; every assertion is
    followed by a legal-request health probe — the process must not crash,
    stall, or lose the pipe):

    T1  garbage lines (invalid JSON, invalid UTF-8 bytes, binary noise)
        -> -32700 with a null id, process alive
    T2  notifications/ stay silent; an id-less non-notification request
        -> -32600; connection usable after both
    T3  missing/non-string method and non-scalar id -> -32600; the jsonrpc
        version field is deliberately tolerated (family-wide, pinned)
    T4  oversized lines: an 8 MiB inline dataBase64 (≈10.7 MiB line, inside
        the 16 MiB reader ceiling) parses and fails at the 4 MiB write cap
        (tool-level isError + upload-channel guidance); a line over the reader ceiling
        (DBX_FILES_MCP_STDIO_MAX_LINE tuned to 1 KiB) is refused at the
        reader with -32700 naming the env — no panic, pipe alive
    T5  pipelining: 4 requests of different ids in flight (with a garbage
        line mixed in) -> every response correlates to its id
    T6  blank/whitespace lines are skipped; CRLF-terminated requests parse

SKIP semantics (M0 §5.2, same as smoke_test.py):
  * method/tool not registered  -> SKIP (never FAIL);
  * no sidecar binary           -> whole suite SKIP (FAIL with DBX_FILES_REQUIRE=1).

Usage:
    python3 scripts/smoke_mcp.py                       # release binary from the repo
    python3 scripts/smoke_mcp.py --binary /path/to/dbx-plugin-files
    DBX_PLUGIN_SIDECAR=/path/to/dbx-plugin-files python3 scripts/smoke_mcp.py
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import pathlib
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import uuid

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from sidecar_client import (  # noqa: E402
    SidecarClient,
    SidecarError,
    default_binary,
    lifecycle_params,
)

REQUIRE = os.environ.get("DBX_FILES_REQUIRE", "") == "1"
METHOD_MISSING = "method not found"

EXPECTED_TOOLS = [
    "files_ui_focus",
    "files_ui_search",
    "files_ui_select",
    "files_ui_state",
    "files_scan_digest",
    "files_cursor_next",
    "files_ui_quick_paths",
    "files_write",
    "files_mkdir",
    "files_rename",
    "files_delete",
    "files_purge",
    "files_sync",
]

WRITE_TOOLS = ["files_write", "files_mkdir", "files_rename", "files_delete", "files_purge"]


class SkipScenario(Exception):
    """Raised by a scenario when a precondition is unavailable."""


class ScenarioResult:
    def __init__(self, no: str, name: str, status: str, detail: str = ""):
        self.no = no
        self.name = name
        self.status = status
        self.detail = detail


RESULTS: list[ScenarioResult] = []


def scenario(no: str, name: str):
    def decorate(fn):
        def run(*args, **kwargs):
            try:
                note = fn(*args, **kwargs)
            except SkipScenario as cause:
                RESULTS.append(ScenarioResult(no, name, "SKIP", str(cause)))
            except (SidecarError, AssertionError) as cause:
                if is_method_missing(cause):
                    RESULTS.append(ScenarioResult(no, name, "SKIP", f"backend not implemented: {cause}"))
                else:
                    RESULTS.append(ScenarioResult(no, name, "FAIL", str(cause)))
            except Exception as cause:  # unexpected crash counts as FAIL
                RESULTS.append(ScenarioResult(no, name, "FAIL", f"unexpected: {cause}"))
            else:
                RESULTS.append(ScenarioResult(no, name, "PASS", note or ""))
        run.no = no  # type: ignore[attr-defined]
        run.name = name  # type: ignore[attr-defined]
        return run
    return decorate


# -- helpers -------------------------------------------------------------------


def is_method_missing(error: Exception) -> bool:
    return METHOD_MISSING.lower() in str(error).lower()


def expect_error(fn, markers: tuple[str, ...] = ()) -> SidecarError:
    try:
        fn()
    except SidecarError as cause:
        lowered = str(cause).lower()
        if markers and not any(marker in lowered for marker in markers):
            raise AssertionError(f"error message mismatch: {cause}") from cause
        return cause
    raise AssertionError("expected a sidecar error but the call succeeded")


def unwrap(result: dict) -> dict:
    """mcp/call returns the MCP content envelope; unwrap the JSON text payload."""
    content = result.get("content") or []
    assert content and content[0].get("type") == "text", result
    return json.loads(content[0]["text"])


def call_tool(client: SidecarClient, tool: str, **arguments) -> dict:
    return unwrap(client.request("mcp/call", {"tool": tool, "arguments": arguments}))


def make_fs_connection(connection_id: str, root: str, **extra_config) -> dict:
    external = {"protocol": "fs", "root": root}
    external.update(extra_config)
    return {
        "id": connection_id,
        "name": f"smoke-mcp-{connection_id}",
        "db_type": "storage",
        "host": "",
        "port": 0,
        "external_config": external,
    }


def connect(client: SidecarClient, connection: dict) -> None:
    client.request("connection/connect", lifecycle_params(connection))


def report_intent_applied(message: dict) -> dict | None:
    """on_event hook: answer a files/ui/intent notification with a report."""
    if message.get("method") == "files/ui/intent":
        intent_id = (message.get("params") or {}).get("intentId", "")
        return {
            "method": "files/ui/state/report",
            "params": {
                "intentId": intent_id,
                "status": "applied",
                "summary": {
                    "count": 1,
                    "anchor": {"path": "/data/report.txt"},
                },
            },
        }
    return None


# -- scenarios (offline: a temp-dir fs connection is enough) --------------------


@scenario("M1", "mcp/settings get/set defaults + validation")
def m1_settings(client: SidecarClient) -> str:
    settings = client.request("mcp/settings/get")
    assert settings["responseLimitBytes"] == 16 * 1024, settings
    assert settings["settings"]["reportWaitMs"] == 5000, settings
    assert settings["settings"]["cellWidth"] == 120, settings
    assert settings["settings"]["digestGroupLimit"] == 20, settings
    assert settings["settings"]["digestTopN"] == 10, settings
    updated = client.request("mcp/settings/set", {"reportWaitMs": 300, "cellWidth": 80})["settings"]
    assert updated["reportWaitMs"] == 300 and updated["cellWidth"] == 80, updated
    expect_error(lambda: client.request("mcp/settings/set", {"reportWaitMs": 0}), ("between 1 and",))
    expect_error(lambda: client.request("mcp/settings/set", {"reportWaitMs": "fast"}), ("positive integer",))
    expect_error(lambda: client.request("mcp/settings/set", {"digestGroupLimit": 21}), ("between 1 and 20",))
    # 白名单外字段容忍（部分更新语义），当前值不被污染。
    tolerated = client.request("mcp/settings/set", {"unknownField": 1})["settings"]
    assert tolerated["reportWaitMs"] == 300, tolerated
    return "defaults 5s/16KiB; partial update; invalid refused"


@scenario("M2", "mcp/tools lists the 13 files tools with schemas")
def m2_tools(client: SidecarClient) -> str:
    tools = client.request("mcp/tools")["tools"]
    names = [tool["name"] for tool in tools]
    for expected in EXPECTED_TOOLS:
        assert expected in names, f"{expected} missing from {names}"
    assert "omittedWriteTools" not in client.request("mcp/tools"), "no omissions without a connection"
    digest = next(tool for tool in tools if tool["name"] == "files_scan_digest")
    schema = digest["inputSchema"]
    assert schema["type"] == "object" and "connectionId" in schema["properties"], schema
    assert "connectionId" in schema["required"], schema
    for two_phase in ("files_delete", "files_purge"):
        write = next(tool for tool in tools if tool["name"] == two_phase)
        assert "confirmToken" in write["inputSchema"]["properties"], write
    return f"{len(tools)} tools with JSON Schema"


@scenario("M3", "read-only connection omits the write tools")
def m3_readonly_tools(client: SidecarClient) -> str:
    ro_id = "smoke-mcp-ro"
    connect(client, make_fs_connection(ro_id, tempfile.mkdtemp(prefix="dbx-files-mcp-ro-"), read_only=True))
    result = client.request("mcp/tools", {"connectionId": ro_id})
    names = [tool["name"] for tool in result["tools"]]
    for write in WRITE_TOOLS:
        assert write not in names, f"{write} must not be listed for a read-only connection"
    omitted = result.get("omittedWriteTools") or []
    assert len(omitted) == len(WRITE_TOOLS), omitted
    assert all("read-only" in entry["reason"] for entry in omitted), omitted
    # 全量清单（不带 connectionId）仍包含写工具。
    all_names = [tool["name"] for tool in client.request("mcp/tools")["tools"]]
    for write in WRITE_TOOLS:
        assert write in all_names
    return "write tools omitted with a reason for read-only"


@scenario("M4", "write tool refused on a read-only connection")
def m4_readonly_write(client: SidecarClient) -> str:
    ro_id = "smoke-mcp-ro"
    error = expect_error(
        lambda: client.request("mcp/call", {
            "tool": "files_mkdir",
            "arguments": {"connectionId": ro_id, "path": "/x"},
        }),
        ("read-only",),
    )
    assert "-32000" in f"{error}" or "read-only" in f"{error}"
    return "mcp/call refused before any write"


@scenario("M5", "UI intent pends without a frontend")
def m5_intent_pending(client: SidecarClient) -> str:
    # M1 把 reportWaitMs 调到 300ms：无前端时快速收敛为 pending。
    result = call_tool(client, "files_ui_focus", panel="browse")
    assert result["state"] == "pending", result
    assert result["intentId"].startswith("i-"), result
    assert "files_scan_digest" in result.get("hint", ""), result
    # intent 状态表补查路径：files_ui_state {intentId} 仍读得到 pending。
    state = call_tool(client, "files_ui_state", intentId=result["intentId"])
    assert state["state"] == "pending", state
    return "pending + digest fallback hint"


@scenario("M6", "UI intent applied via the report callback")
def m6_intent_applied(client: SidecarClient) -> str:
    result = unwrap(client.request(
        "mcp/call",
        {"tool": "files_ui_search", "arguments": {"path": "/data"}},
        on_event=report_intent_applied,
    ))
    assert result["state"] == "applied", result
    summary = result.get("summary") or {}
    assert summary.get("count") == 1 and summary["anchor"]["path"], result
    return "applied + summary roundtrip over files/ui/state/report"


@scenario("M7", "files_ui_state reads intent result and snapshot")
def m7_ui_state(client: SidecarClient) -> str:
    applied = unwrap(client.request(
        "mcp/call",
        {"tool": "files_ui_select", "arguments": {"path": "/data/report.txt"}},
        on_event=report_intent_applied,
    ))
    assert applied["state"] == "applied", applied
    state = call_tool(client, "files_ui_state", intentId=applied["intentId"])
    assert state["state"] == "applied" and state["summary"]["count"] == 1, state
    # 快照型 report 后，无 intentId 的 files_ui_state 返回最新快照。
    client.request("files/ui/state/report", {"status": "snapshot", "summary": {"panel": "browse", "count": 42}})
    snapshot = call_tool(client, "files_ui_state")
    assert snapshot["snapshot"]["panel"] == "browse" and snapshot["snapshot"]["count"] == 42, snapshot
    expect_error(
        lambda: call_tool(client, "files_ui_state", intentId="i-nonexistent"),
        ("unknown intentid",),
    )
    return "by intentId + latest snapshot both work"


@scenario("M8", "digest tools refuse unknown connections")
def m8_digest_gates(client: SidecarClient) -> str:
    expect_error(
        lambda: call_tool(client, "files_scan_digest", connectionId="smoke-mcp-nope", path="/"),
        ("not connected", "connection"),
    )
    expect_error(
        lambda: call_tool(client, "files_cursor_next", cursorId="cur-nope"),
        ("unknown cursorid",),
    )
    expect_error(
        lambda: call_tool(client, "files_ui_quick_paths"),
        ("missing required parameter: connectionid",),
    )
    return "unknown connection / cursor / missing connectionId all refused"


@scenario("M9", "unknown tool and unknown method")
def m9_unknown(client: SidecarClient) -> str:
    expect_error(
        lambda: client.request("mcp/call", {"tool": "files_nonexistent", "arguments": {}}),
        ("unknown tool",),
    )
    try:
        client.request("mcp/nonexistent", {})
        raise AssertionError("expected method-not-found for an unregistered method")
    except SidecarError as cause:
        if not is_method_missing(cause):
            raise AssertionError(f"expected method-not-found, got: {cause}") from cause
    return "unknown tool + unknown method refused clearly"


# -- fs scenario (M10) ------------------------------------------------------------


@scenario("M10", "digest + cursor + two-phase writes (fs temp dir)")
def m10_fs(client: SidecarClient) -> str:
    conn_id = f"smoke-mcp-main-{uuid.uuid4().hex[:6]}"
    root = tempfile.mkdtemp(prefix="dbx-files-mcp-")
    connect(client, make_fs_connection(conn_id, root))
    try:
        # 建树走 MCP 写工具本身：mkdir 单阶段 + write 单阶段。
        call_tool(client, "files_mkdir", connectionId=conn_id, path="/mcp-smoke")
        for name in ("a.txt", "b.txt", "c.log"):
            call_tool(client, "files_write", connectionId=conn_id,
                      path=f"/mcp-smoke/{name}",
                      dataBase64=base64.b64encode(f"content of {name}".encode()).decode())

        # digest：递归扫描 + 本地聚合 + cursor 物化（扫描过程数据不出 sidecar）。
        digest = call_tool(client, "files_scan_digest", connectionId=conn_id,
                           path="/mcp-smoke", glob="*")
        assert digest["matched"] >= 3, digest
        assert digest["scanned"] >= digest["matched"], digest
        assert digest["stats"]["byExtension"], digest
        assert len(digest["sample"]) <= 5, digest
        assert digest["cursorId"], digest

        batch = call_tool(client, "files_cursor_next", cursorId=digest["cursorId"], n=2)
        assert batch["rows"] and len(batch["rows"]) <= 2, batch
        assert batch["offset"] == 0 and batch["nextOffset"] == len(batch["rows"]), batch
        assert batch["done"] is False, batch
        more = call_tool(client, "files_cursor_next", cursorId=digest["cursorId"])
        assert more["done"] is True, more

        # 两阶段 delete：preview + confirmToken（不执行）→ 带 token 执行。
        target = "/mcp-smoke/a.txt"
        preview = call_tool(client, "files_delete", connectionId=conn_id, path=target)
        assert preview["preview"]["path"] == target and preview.get("confirmToken"), preview
        assert "nothing written yet" in preview.get("note", ""), preview

        # 参数 hash 绑定：token 有效但参数被改 → 作废（须重开预览）。
        expect_error(lambda: call_tool(client, "files_delete", connectionId=conn_id,
                                       path="/mcp-smoke/b.txt", confirmToken=preview["confirmToken"]),
                     ("arguments changed",))

        preview2 = call_tool(client, "files_delete", connectionId=conn_id, path=target)
        confirm = call_tool(client, "files_delete", connectionId=conn_id,
                            path=target, confirmToken=preview2["confirmToken"])
        assert confirm["success"] is True, confirm

        # 一次性：同 token 复用 → unknown。
        expect_error(lambda: call_tool(client, "files_delete", connectionId=conn_id,
                                       path=target, confirmToken=preview2["confirmToken"]),
                     ("unknown or already used",))

        # purge 根路径红线（连接 root = "/"）。
        expect_error(
            lambda: call_tool(client, "files_purge", connectionId=conn_id, path="/"),
            ("root",),
        )

        # 审计：MCP 写路径记 source:"mcp"（对齐 files/audit 基线）。
        audit = client.request("files/audit/list", {"connectionId": conn_id, "limit": 100})
        mcp_entries = [entry for entry in audit.get("entries", []) if entry.get("source") == "mcp"]
        actions = {entry["action"] for entry in mcp_entries}
        assert "files/delete" in actions, f"MCP delete not audited: {audit}"

        # quick_paths：纯元发现；fs 限根连接只有 root chip，limit 数字字符串。
        quick = call_tool(client, "files_ui_quick_paths", connectionId=conn_id, limit="1")
        assert quick["paths"] and len(quick["paths"]) <= 1, quick

        # 两阶段 purge 成功路径（子目录递归删除，磁盘校验）。
        purge_preview = call_tool(client, "files_purge", connectionId=conn_id, path="/mcp-smoke")
        assert purge_preview["preview"]["kind"] == "dir", purge_preview
        purge_done = call_tool(client, "files_purge", connectionId=conn_id,
                               path="/mcp-smoke", confirmToken=purge_preview["confirmToken"])
        assert purge_done["success"] is True, purge_done
        assert not os.path.exists(os.path.join(root, "mcp-smoke")), "purged on disk"

        return (f"digest matched={digest['matched']}; cursor paged; two-phase delete executed; "
                f"token single-use; two-phase purge executed; audit source=mcp "
                f"({len(mcp_entries)} entries)")
    finally:
        try:
            client.request("connection/disconnect", {"connection": {"id": conn_id}})
        except Exception:
            pass


# -- LLM input variants (M11) -----------------------------------------------------


@scenario("M11", "LLM input variants: numeric strings, empty writes, trailing slashes")
def m11_llm_variants(client: SidecarClient) -> str:
    conn_id = f"smoke-mcp-var-{uuid.uuid4().hex[:6]}"
    root = tempfile.mkdtemp(prefix="dbx-files-mcp-var-")
    connect(client, make_fs_connection(conn_id, root))
    try:
        call_tool(client, "files_mkdir", connectionId=conn_id, path="/v")
        # 未知参数容忍（部分更新语义；不因陌生键报错）。
        call_tool(client, "files_write", connectionId=conn_id, path="/v/a.txt",
                  dataBase64=base64.b64encode(b"hello").decode(), unknownKwarg="ignored")

        # 数值参数接受数字字符串（depth/size/mtime 谓词同构）；越界 depth clamp 不报错。
        digest = call_tool(client, "files_scan_digest", connectionId=conn_id, path="/v/",
                           glob="*.txt", depth="2", minSizeBytes="0", modifiedSince="0")
        assert digest["matched"] == 1, digest
        digest_clamped = call_tool(client, "files_scan_digest", connectionId=conn_id,
                                   path="/v", depth="99")
        assert digest_clamped["matched"] >= 1, digest_clamped
        # 非数字字符串仍清晰报错并点名参数，且列出合法范围（§3.3）。
        error = expect_error(
            lambda: call_tool(client, "files_scan_digest", connectionId=conn_id,
                              path="/v", depth="deep"),
            ("non-negative integer",),
        )
        assert "1..=16" in f"{error}", error

        # 缺参枚举式（§3.9，第七轮拉齐）：一次点名全部缺失的业务 required
        # 参数（connectionId 在场，业务参数 path/dataBase64 一起枚举）。
        error = expect_error(
            lambda: call_tool(client, "files_write", connectionId=conn_id),
            ("missing required parameters",),
        )
        assert "path" in f"{error}" and "dataBase64" in f"{error}", error
        # 部分缺失只枚举缺失项。
        error = expect_error(
            lambda: call_tool(client, "files_rename", connectionId=conn_id, path="/v/a.txt"),
            ("missing required parameters: newpath",),
        )

        # 空 dataBase64 建空文件（与工作台写路径同语义）。
        call_tool(client, "files_write", connectionId=conn_id, path="/v/empty.bin", dataBase64="")
        assert os.path.exists(os.path.join(root, "v/empty.bin")), "empty file created"
        assert os.path.getsize(os.path.join(root, "v/empty.bin")) == 0

        # 尾斜杠文件路径 delete：必须真删，不能静默 no-op。
        preview = call_tool(client, "files_delete", connectionId=conn_id, path="/v/a.txt/")
        assert preview["preview"]["kind"] in ("file", "missing"), preview
        confirm = call_tool(client, "files_delete", connectionId=conn_id,
                            path="/v/a.txt/", confirmToken=preview["confirmToken"])
        assert confirm["success"] is True, confirm
        assert not os.path.exists(os.path.join(root, "v/a.txt")), \
            "trailing-slash delete must remove the real file"

        # cursor 未知错误给出可行动引导（重发 digest）。
        error = expect_error(
            lambda: call_tool(client, "files_cursor_next", cursorId="cur-nope"),
            ("unknown cursorid",),
        )
        assert "files_scan_digest" in f"{error}", error
        return "numeric strings/empty payload/trailing slash/unknown params all tolerated"
    finally:
        try:
            client.request("connection/disconnect", {"connection": {"id": conn_id}})
        except Exception:
            pass


# -- big-tree clamp (M12) + TTL real-time expiry (M13) ---------------------------


@scenario("M12", "big-tree clamp: real 100k scan budget + 10k cursor cap")
def m12_clamp(client: SidecarClient) -> str:
    count = int(os.environ.get("DBX_FILES_MCP_CLAMP_FILES", "100001"))
    assert count >= 2, "need at least 2 files to page a cursor"
    root = tempfile.mkdtemp(prefix="dbx-files-mcp-clamp-")
    flat = os.path.join(root, "flat")
    os.makedirs(flat)
    conn_id = f"smoke-mcp-clamp-{uuid.uuid4().hex[:6]}"
    connect(client, make_fs_connection(conn_id, root))
    try:
        # Real 100k-scale tree (flat, one level): os.open/close keeps the
        # setup a few seconds; DBX_FILES_MCP_CLAMP_FILES can shrink it for a
        # parametrized equivalent run (assertions below use min(count, budget)).
        for index in range(count):
            fd = os.open(os.path.join(flat, f"f{index:06d}.txt"),
                         os.O_CREAT | os.O_WRONLY, 0o600)
            os.close(fd)

        budget = 100_000
        # Clamp semantics (verified here on a real tree): the entry that
        # oversteps the budget is counted in `scanned` (that is how the walk
        # knows the tree is bigger) but never kept, so scanned == min(count,
        # budget + 1) while matched == min(count, budget).
        digest = call_tool(client, "files_scan_digest", connectionId=conn_id, path="/flat")
        assert digest["scanned"] == min(count, budget + 1), digest
        assert digest["matched"] == min(count, budget), digest
        assert digest["scanTruncated"] is (count > budget), digest
        if count > 10_000:
            # Cursor materialization caps at 10k rows: paging must stop there,
            # never leak row 10001 even via an explicit offset.
            assert digest.get("cursorTruncated") is True, digest
            tail = call_tool(client, "files_cursor_next",
                             cursorId=digest["cursorId"], offset=9_999, n=20)
            assert len(tail["rows"]) == 1 and tail["done"] is True, tail
            over = call_tool(client, "files_cursor_next",
                             cursorId=digest["cursorId"], offset=10_000, n=20)
            assert over["rows"] == [] and over["done"] is True, over
            return (f"scanned={digest['scanned']} (clamped at {budget}, "
                    f"scanTruncated=true); cursor capped at 10000 rows "
                    f"(cursorTruncated=true)")
        digest_small = call_tool(client, "files_scan_digest",
                                 connectionId=conn_id, path="/flat")
        assert digest_small["scanTruncated"] is False, digest_small
        return f"scanned={digest_small['scanned']} (< budget, no truncation)"
    finally:
        shutil.rmtree(root, ignore_errors=True)


@scenario("M13", "TTL real-time expiry: 10s tuned TTLs on the real clock")
def m13_ttl_realtime(client: SidecarClient) -> str:
    # settings floor for both TTLs is 10s (sanitized clamp): a single real
    # wait covers the cursor and confirmToken timelines at once.
    client.request("mcp/settings/set", {"cursorTtlSecs": 10, "confirmTtlSecs": 10})
    conn_id = f"smoke-mcp-ttl-{uuid.uuid4().hex[:6]}"
    root = tempfile.mkdtemp(prefix="dbx-files-mcp-ttl-")
    connect(client, make_fs_connection(conn_id, root))
    try:
        call_tool(client, "files_write", connectionId=conn_id, path="/victim.txt",
                  dataBase64=base64.b64encode(b"ttl").decode())
        preview = call_tool(client, "files_delete", connectionId=conn_id, path="/victim.txt")
        assert preview.get("confirmToken") and preview.get("expiresAt"), preview
        digest = call_tool(client, "files_scan_digest", connectionId=conn_id, path="/")
        cursor_id = digest["cursorId"]

        time.sleep(10.6)  # real wall-clock expiry, no injected clock
        error = expect_error(
            lambda: call_tool(client, "files_cursor_next", cursorId=cursor_id),
            ("cursor expired (ttl 10s)",),
        )
        assert "files_scan_digest" in f"{error}", error
        error = expect_error(
            lambda: call_tool(client, "files_delete", connectionId=conn_id,
                              path="/victim.txt", confirmToken=preview["confirmToken"]),
            ("confirmtoken expired (ttl 10s)",),
        )
        assert "preview" in f"{error}", error
        # 过期的 token 从未执行过删除：文件仍在（fail-safe 语义）。
        still_there = call_tool(client, "files_scan_digest",
                                connectionId=conn_id, path="/", glob="victim.txt")
        assert still_there["matched"] == 1, still_there
        return ("real 10s wait: 'cursor expired (TTL 10s)' + "
                "'confirmToken expired (TTL 10s)'; nothing executed on expiry")
    finally:
        # 恢复默认，避免污染后续场景（settings 持久化在隔离 data_dir）。
        client.request("mcp/settings/set", {"cursorTtlSecs": 600, "confirmTtlSecs": 60})
        try:
            client.request("connection/disconnect", {"connection": {"id": conn_id}})
        except Exception:
            pass
        shutil.rmtree(root, ignore_errors=True)


# -- remote-protocol stdio sections (R1-R7; containers, env-gated SKIP) ----------
#
# Shared loop (below). The loop builds every tree under a unique `base`
# directory through the MCP write tools themselves (files_mkdir is mkdir -p
# on every backend) instead of scoping a connection `root`: home-restricted
# SSH servers cannot materialize a root at filesystem / — R4 found the
# OpenDAL 0.57 sftp service silently swallowing PermissionDenied while
# auto-creating a missing root (is_sftp_protocol_error treats ANY protocol
# error as "already exists"), surfacing later as a confusing NoSuchFile on
# write; the sftp-native adapter reports the same PermissionDenied honestly.
# Base-under-a-writable-area is the shape the framed smoke proves per
# protocol, and the unique fresh base keeps the exact matched counts
# deterministic.


def run_remote_stdio_roundtrip(connection: dict, tag: str, base_dir: str | None = None) -> str:
    """Full agent loop on a real remote protocol via stdio inline credentials:
    write/mkdir -> digest aggregate -> cursor paging -> two-phase delete ->
    two-phase purge. Deletions are verified by re-digest matched counts (the
    MCP face never reads file bodies, so matched IS the source of truth)."""
    session = StdioSession(default_binary())
    try:
        connection = {**connection, "name": f"smoke-stdio-{tag}"}
        base = base_dir or f"/mcp-smoke-{uuid.uuid4().hex[:8]}"
        # 建树走 stdio MCP 写工具本身：mkdir 单阶段（mkdir -p 语义）建
        # base/sub 两级，随后单阶段 write。不设连接 root（base 在各协议
        # 默认根下的可写区，fresh 唯一 base 保证 matched 计数确定）。
        session.call_tool("files_mkdir", connection=connection, path=f"{base}/sub")
        for name in ("a.txt", "b.log"):
            session.call_tool("files_write", connection=connection, path=f"{base}/{name}",
                              dataBase64=base64.b64encode(f"content of {name}".encode()).decode())
        session.call_tool("files_write", connection=connection, path=f"{base}/sub/c.txt",
                          dataBase64=base64.b64encode(b"nested").decode())

        digest = session.call_tool("files_scan_digest", connection=connection,
                                   path=base, glob="*")
        # a.txt + b.log + sub (dir entry) + sub/c.txt
        assert digest["matched"] == 4, digest
        assert digest["scanned"] >= digest["matched"], digest
        assert digest["stats"]["byExtension"], digest
        assert digest["cursorId"], digest
        batch = session.call_tool("files_cursor_next", cursorId=digest["cursorId"], n=2)
        assert len(batch["rows"]) == 2 and batch["done"] is False, batch
        rest = session.call_tool("files_cursor_next", cursorId=digest["cursorId"])
        assert rest["done"] is True, rest

        # enum 变体在线断言（契约 §3.3，第七轮钉进容器段）：真实连接上报错
        # 列合法值 + 大小写归一正常工作。离线段被连接门拦截探不到，只有
        # 在线（R1-R6 真实协议）才能钉死这组行为。
        error = session.call_tool_error("files_scan_digest", connection=connection,
                                        path=base, format="bogus")
        assert "format must be 'digest' or 'rows'" in error and "bogus" in error, error
        error = session.call_tool_error("files_scan_digest", connection=connection,
                                        path=base, depth="bogus")
        assert "depth" in error and "1..=16" in error, error
        upper = session.call_tool("files_scan_digest", connection=connection,
                                  path=base, glob="*", format="ROWS")
        assert upper["matched"] == digest["matched"], (digest, upper)
        assert "rows" in upper and len(upper["rows"]) >= 1, upper

        # Two-phase delete: tampered args invalidate, re-preview, confirm runs,
        # re-digest proves exactly one row disappeared.
        preview = session.call_tool("files_delete", connection=connection, path=f"{base}/a.txt")
        assert preview.get("confirmToken"), preview
        error = session.call_tool_error("files_delete", connection=connection,
                                        path=f"{base}/b.log", confirmToken=preview["confirmToken"])
        assert "arguments changed" in error, error
        preview2 = session.call_tool("files_delete", connection=connection, path=f"{base}/a.txt")
        confirm = session.call_tool("files_delete", connection=connection, path=f"{base}/a.txt",
                                    confirmToken=preview2["confirmToken"])
        assert confirm["success"] is True, confirm
        after_delete = session.call_tool("files_scan_digest", connection=connection,
                                         path=base, glob="*")
        assert after_delete["matched"] == digest["matched"] - 1, (digest, after_delete)

        # Two-phase purge of the subtree: verified down to the single remaining
        # root file (object stores may collapse dir markers — kind is loose).
        purge_preview = session.call_tool("files_purge", connection=connection, path=f"{base}/sub")
        assert purge_preview["preview"]["kind"] in ("dir", "missing"), purge_preview
        purge_done = session.call_tool("files_purge", connection=connection, path=f"{base}/sub",
                                       confirmToken=purge_preview["confirmToken"])
        assert purge_done["success"] is True, purge_done
        after_purge = session.call_tool("files_scan_digest", connection=connection,
                                        path=base, glob="*")
        assert after_purge["matched"] == 1, (after_delete, after_purge)
        return (f"matched={digest['matched']}; cursor paged; enum probes ok "
                f"(bogus depth/format refused with legal values, ROWS normalized); "
                f"two-phase delete executed (re-digest verified); "
                f"two-phase purge executed (re-digest verified)")
    finally:
        session.close()


@scenario("R1", "stdio remote s3 (MinIO): full MCP loop on inline credentials")
def r1_remote_s3(_client: SidecarClient) -> str:
    endpoint = os.environ.get("DBX_FILES_S3_ENDPOINT")
    bucket = os.environ.get("DBX_FILES_S3_BUCKET")
    access = os.environ.get("DBX_FILES_S3_ACCESS_KEY")
    secret = os.environ.get("DBX_FILES_S3_SECRET_KEY")
    if not (endpoint and bucket and access and secret):
        raise SkipScenario(
            "set DBX_FILES_S3_ENDPOINT/BUCKET/ACCESS_KEY/SECRET_KEY to enable "
            "(scripts/container_smoke.sh provides them)")
    connection = {"protocol": "s3", "endpoint": endpoint, "bucket": bucket,
                  "region": os.environ.get("DBX_FILES_S3_REGION", "us-east-1"),
                  "accessKeyId": access, "secretAccessKey": secret}
    return run_remote_stdio_roundtrip(connection, "s3")


@scenario("R2", "stdio remote webdav (mod_dav): full MCP loop on inline credentials")
def r2_remote_webdav(_client: SidecarClient) -> str:
    endpoint = os.environ.get("DBX_FILES_WEBDAV_ENDPOINT")
    if not endpoint:
        raise SkipScenario(
            "set DBX_FILES_WEBDAV_ENDPOINT/USER/PASSWORD to enable "
            "(scripts/container_smoke.sh provides them)")
    connection = {"protocol": "webdav", "endpoint": endpoint,
                  "username": os.environ.get("DBX_FILES_WEBDAV_USER", ""),
                  "password": os.environ.get("DBX_FILES_WEBDAV_PASSWORD", "")}
    return run_remote_stdio_roundtrip(connection, "webdav")


@scenario("R3", "stdio remote ftp (pyftpdlib): full MCP loop on inline credentials")
def r3_remote_ftp(_client: SidecarClient) -> str:
    endpoint = os.environ.get("DBX_FILES_FTP_ENDPOINT")
    if not endpoint:
        raise SkipScenario(
            "set DBX_FILES_FTP_ENDPOINT/USER/PASSWORD to enable "
            "(scripts/container_smoke.sh provides them)")
    connection = {"protocol": "ftp", "endpoint": endpoint,
                  "user": os.environ.get("DBX_FILES_FTP_USER", ""),
                  "password": os.environ.get("DBX_FILES_FTP_PASSWORD", "")}
    return run_remote_stdio_roundtrip(connection, "ftp")


@scenario("R4", "stdio remote sftp (OpenSSH): keyfile-only auth, full MCP loop")
def r4_remote_sftp(_client: SidecarClient) -> str:
    host = os.environ.get("DBX_FILES_SFTP_HOST")
    key = os.environ.get("DBX_FILES_SFTP_KEY")
    if not (host and key):
        raise SkipScenario(
            "set DBX_FILES_SFTP_HOST/PORT/USER/KEY to enable "
            "(scripts/container_smoke.sh provides them)")
    user = os.environ.get("DBX_FILES_SFTP_USER", "tester")
    port = os.environ.get("DBX_FILES_SFTP_PORT", "22")
    # Protocol specialization (P-FILES ③): OpenDAL 0.57's sftp service is
    # keyfile-only — the engine deliberately never forwards a password, so
    # this inline payload carries NO password at all. The endpoint must be
    # the ssh:// URI form (the openssh crate extracts user/port from the URI
    # only) and known_hosts "accept" tolerates the container's ephemeral
    # host key.
    connection = {"protocol": "sftp", "endpoint": f"ssh://{user}@{host}:{port}",
                  "user": user, "key": key, "knownHostsStrategy": "accept"}
    # linuxserver/openssh-server: USER_NAME's home is /config (writable). The
    # base lives there — R4's first container run proved a root/base at
    # filesystem / cannot work for a home-restricted user (see the shared
    # loop's comment for the swallowed-PermissionDenied root cause).
    return run_remote_stdio_roundtrip(
        connection, "sftp", base_dir=f"/config/mcp-smoke-{uuid.uuid4().hex[:8]}")


@scenario("R5", "stdio remote smb (Samba): share-scoped loop over the custom adapter")
def r5_remote_smb(_client: SidecarClient) -> str:
    host = os.environ.get("DBX_FILES_SMB_HOST")
    share = os.environ.get("DBX_FILES_SMB_SHARE")
    user = os.environ.get("DBX_FILES_SMB_USER")
    password = os.environ.get("DBX_FILES_SMB_PASSWORD")
    if not (host and share and user and password):
        raise SkipScenario(
            "set DBX_FILES_SMB_HOST/PORT/SHARE/USER/PASSWORD to enable "
            "(scripts/container_smoke.sh provides them)")
    port = os.environ.get("DBX_FILES_SMB_PORT", "445")
    # Protocol specialization (F5-SMB): bare host:port endpoint (smb:// URI
    # tolerated, bare form pinned here), a selected share scopes the whole
    # loop to its tree, and the password is the secret-bound field the
    # custom adapter negotiates over NTLM. The shared loop's two-phase purge
    # exercises the adapter-implemented delete_with_recursive (the smb2
    # crate has no server-side recursive delete).
    connection = {"protocol": "smb", "endpoint": f"{host}:{port}", "share": share,
                  "username": user, "password": password}
    return run_remote_stdio_roundtrip(connection, "smb")


@scenario("R6", "stdio remote sftp-native (russh): password auth full MCP loop")
def r6_remote_sftp_native(_client: SidecarClient) -> str:
    host = os.environ.get("DBX_FILES_SFTP_NATIVE_HOST")
    user = os.environ.get("DBX_FILES_SFTP_NATIVE_USER", "")
    password = os.environ.get("DBX_FILES_SFTP_NATIVE_PASSWORD", "")
    key = os.environ.get("DBX_FILES_SFTP_NATIVE_KEY", "")
    if not (host and user and (password or key)):
        raise SkipScenario(
            "set DBX_FILES_SFTP_NATIVE_HOST/PORT/USER/PASSWORD (or KEY) to "
            "enable (scripts/container_smoke.sh provides them)")
    port = os.environ.get("DBX_FILES_SFTP_NATIVE_PORT", "22")
    # Protocol specialization (dual-stack decision 2026-08-31): sftp-native
    # exists because OpenDAL's sftp service cannot do password auth — so the
    # password form is THE shape under test here. Bare `user@host:port`
    # endpoint (ssh:// also accepted), default host-key strategy Tolerate
    # (accept-new) fits the container's ephemeral key.
    connection = {"protocol": "sftp-native", "endpoint": f"{user}@{host}:{port}",
                  "user": user}
    if password:
        connection["password"] = password
    else:
        connection["key"] = key
    # Same writable-home base as R4: the adapter's stat-first mkdir -p reports
    # PermissionDenied honestly when the base would live at filesystem /
    # (R6's first container run pinned exactly that refusal).
    return run_remote_stdio_roundtrip(
        connection, "sftp-native", base_dir=f"/config/mcp-smoke-{uuid.uuid4().hex[:8]}")


@scenario("R7", "stdio remote oss: env-gated (no OSS-API-compatible test container)")
def r7_remote_oss(_client: SidecarClient) -> str:
    endpoint = os.environ.get("DBX_FILES_OSS_ENDPOINT")
    bucket = os.environ.get("DBX_FILES_OSS_BUCKET")
    access = os.environ.get("DBX_FILES_OSS_ACCESS_KEY")
    secret = os.environ.get("DBX_FILES_OSS_SECRET_KEY")
    if not (endpoint and bucket and access and secret):
        raise SkipScenario(
            "no OSS-API-compatible test container exists (MinIO speaks the S3 "
            "API only, not the Aliyun OSS API OpenDAL's oss service speaks); "
            "set DBX_FILES_OSS_ENDPOINT/BUCKET/ACCESS_KEY/SECRET_KEY against "
            "a real OSS endpoint to enable")
    # Protocol specialization: oss reuses the s3-shaped inline fields, but
    # the secret lands on OpenDAL's `access_key_secret` key (never
    # `secret_access_key`) and no region is forwarded.
    connection = {"protocol": "oss", "endpoint": endpoint, "bucket": bucket,
                  "accessKeyId": access, "secretAccessKey": secret}
    return run_remote_stdio_roundtrip(connection, "oss")


# -- stdio scenarios (S1-S4: standalone `--mcp` server mode, no deps) ------------


class StdioSession:
    """One `--mcp` sidecar process speaking newline-delimited JSON-RPC 2.0."""

    def __init__(self, binary: str | None = None, extra_env: dict | None = None):
        binary = binary or default_binary()
        if not os.path.exists(binary):
            raise SidecarError(
                f"sidecar binary not found: {binary} (use --binary or set DBX_PLUGIN_SIDECAR)")
        self.data_dir = tempfile.mkdtemp(prefix="dbx-files-mcp-stdio-")
        env = dict(os.environ)
        env["DBX_PLUGIN_DATA_DIR"] = self.data_dir
        if extra_env:
            env.update(extra_env)
        self.proc = subprocess.Popen(
            [binary, "--mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            env=env,
        )
        self.next_id = 0
        # Out-of-order response frames buffered by wait_for (never discarded);
        # _rawbuf is the shared line buffer fed by the reader thread.
        self.pending: list[dict] = []
        self._rawbuf = b""
        self._chunks: "queue.Queue[bytes]" = queue.Queue()
        # Single read path via a background os.read pump. select() is not an
        # option: Windows select only accepts sockets (WinError 10093 on pipe
        # fds, 2026-09-15 candidate run), and select-on-BufferedReader reports
        # "not ready" when a previous read already pulled later frames into
        # the userspace buffer (ldap M18 2/6 flake root cause).
        self._reader = threading.Thread(target=self._pump_stdout, daemon=True)
        self._reader.start()

    def _pump_stdout(self) -> None:
        fd = self.proc.stdout.fileno()
        while True:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                return
            if not chunk:
                return
            self._chunks.put(chunk)

    def _next_line(self, timeout: float) -> bytes:
        deadline = time.monotonic() + timeout
        while True:
            index = self._rawbuf.find(b"\n")
            if index >= 0:
                line = self._rawbuf[:index + 1]
                self._rawbuf = self._rawbuf[index + 1:]
                return line
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError("timed out waiting for a stdio line")
            try:
                self._rawbuf += self._chunks.get(timeout=remaining)
            except queue.Empty:
                raise AssertionError("timed out waiting for a stdio line") from None

    def send(self, payload: dict) -> None:
        self.proc.stdin.write((json.dumps(payload) + "\n").encode())
        self.proc.stdin.flush()

    def send_raw(self, line: str) -> None:
        self.proc.stdin.write((line + "\n").encode())
        self.proc.stdin.flush()

    def send_bytes(self, raw: bytes) -> None:
        """Arbitrary bytes as one line (invalid UTF-8 / CRLF spellings)."""
        self.proc.stdin.write(raw)
        self.proc.stdin.flush()

    def recv(self) -> dict:
        line = self._next_line(60)
        if not line.strip():
            raise AssertionError("stdio sidecar closed the stream before replying")
        return json.loads(line)

    def send_request(self, method: str, params: dict, request_id: int) -> int:
        """Sends without waiting for the answer (pipelining primitive)."""
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        return request_id

    def wait_for(self, want_id: int) -> dict:
        """Reads lines until the response with this id arrives. Out-of-order
        answers are buffered (never discarded): discarding a frame that a
        later wait_for awaits hangs the scenario forever (files T5 root
        cause). Notifications (no id key) never answer anything. Reads via
        the background os.read pump + our own line buffer — the deadline
        stays honest so a missing frame fails instead of hanging."""
        deadline = time.monotonic() + 60
        while True:
            for index, message in enumerate(self.pending):
                if message.get("id") == want_id:
                    return self.pending.pop(index)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError(f"timed out waiting for response id {want_id}")
            raw = self._next_line(remaining)
            text = raw.decode(errors="replace").strip()
            if not text:
                continue
            message = json.loads(text)
            if "id" not in message:
                continue  # notification: silent by protocol
            self.pending.append(message)

    def request(self, method: str, params: dict) -> dict:
        self.next_id += 1
        want = self.send_request(method, params, self.next_id)
        return self.wait_for(want)

    def call_tool(self, name: str, **arguments) -> dict:
        message = self.request("tools/call", {"name": name, "arguments": arguments})
        if "error" in message:
            raise AssertionError(f"{name} errored: {message['error'].get('message')}")
        result = message["result"]
        if result.get("isError", False):
            raise AssertionError(f"{name} failed: {result}")
        content = result.get("content") or []
        assert content and content[0].get("type") == "text", result
        return json.loads(content[0]["text"])

    def call_tool_error(self, name: str, **arguments) -> str:
        """Tool-level errors arrive as MCP isError results (ldap/kafka stdio
        parity); structural -32602 mistakes still arrive as JSON-RPC errors —
        both shapes carry an actionable message."""
        message = self.request("tools/call", {"name": name, "arguments": arguments})
        if "error" in message:
            return message["error"]["message"]
        result = message.get("result") or {}
        assert result.get("isError") is True, f"{name} should have errored: {message}"
        return result["content"][0]["text"]

    def close(self) -> None:
        try:
            if self.proc.stdin:
                self.proc.stdin.close()
            self.proc.wait(timeout=10)
        except Exception:
            self.proc.kill()


@scenario("S1", "stdio handshake: initialize/tools/list/ping/notifications")
def s1_stdio_handshake(_client: SidecarClient) -> str:
    session = StdioSession(default_binary())
    try:
        result = session.request("initialize", {})["result"]
        assert result["protocolVersion"] == "2024-11-05", result
        assert result["serverInfo"]["name"] == "io.dbx.files", result
        assert result["serverInfo"]["version"], result
        assert result["capabilities"]["tools"]["listChanged"] is False, result

        tools = session.request("tools/list", {})["result"]["tools"]
        names = [tool["name"] for tool in tools]
        for expected in EXPECTED_TOOLS:
            assert expected in names, f"{expected} missing from stdio tools/list: {names}"
        digest = next(tool for tool in tools if tool["name"] == "files_scan_digest")
        # 连接类工具声明内联 connection 参数并把 required 的 connectionId
        # 放宽为 anyOf（严格校验宿主不丢参；ldap/kafka stdio 同形）。
        digest_schema = digest["inputSchema"]
        assert "connection" in digest_schema["properties"], digest
        assert digest_schema["anyOf"] == [
            {"required": ["connectionId"]},
            {"required": ["connection"]},
        ], digest_schema
        assert "connectionId" not in digest_schema["required"], digest_schema
        cursor = next(tool for tool in tools if tool["name"] == "files_cursor_next")
        assert "connection" not in cursor["inputSchema"]["properties"], cursor

        # notifications/initialized is never answered: the next line on the
        # pipe must be the ping response that follows it.
        session.next_id += 1
        want = session.next_id
        session.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        session.send({"jsonrpc": "2.0", "id": want, "method": "ping", "params": {}})
        message = session.recv()
        assert message.get("id") == want and message.get("result") == {}, message

        # Unknown method → standard JSON-RPC -32601（族内统一：ssh/ldap/kafka
        # 同码；SKIP 判定认 "Method not found" 文本，与数字码解耦）。
        message = session.request("mcp/nonexistent", {})
        assert message["error"]["code"] == -32601, message
        assert "Method not found" in message["error"]["message"], message

        # 结构性参数错误 -32602；工具级错误为 MCP isError 结果（非协议错误，
        # ldap/kafka stdio 同形）。
        message = session.request("tools/call", {"name": "files_scan_digest",
                                                 "arguments": "not-an-object"})
        assert message["error"]["code"] == -32602, message
        message = session.request("tools/call", {"name": "files_nonexistent", "arguments": {}})
        assert message["result"]["isError"] is True, message
        assert "Unknown tool" in message["result"]["content"][0]["text"], message

        session.send_raw("not json")
        message = session.recv()
        assert message["error"]["code"] == -32700, message
        assert "Parse error" in message["error"]["message"], message
    finally:
        session.close()
    return ("initialize 2024-11-05 + io.dbx.files; 13 tools + inline connection schema; "
            "unknown method -32601; invalid params -32602; tool error isError; parse error -32700")


@scenario("S2", "stdio UI intent tools answer explicit UNAVAILABLE")
def s2_stdio_ui_unavailable(_client: SidecarClient) -> str:
    session = StdioSession(default_binary())
    try:
        for name, arguments in (
            ("files_ui_focus", {"panel": "browse"}),
            ("files_ui_search", {"path": "/data"}),
            ("files_ui_select", {"path": "/data/a.txt"}),
            ("files_ui_state", {}),
        ):
            error = session.call_tool_error(name, **arguments)
            assert "UNAVAILABLE" in error, f"{name}: {error}"
            assert name in error, f"{name}: {error}"
            assert "files_scan_digest" in error, f"{name}: {error}"
        # 非 UI 工具不受门影响：报的是真实参数错误而不是 UNAVAILABLE。
        error = session.call_tool_error("files_scan_digest", path="/")
        assert "UNAVAILABLE" not in error, error
        assert "connectionId" in error, error
    finally:
        session.close()
    return "4 UI tools UNAVAILABLE (digest fallback + bridge hint); digest path unaffected"


@scenario("S3", "stdio localFs inline credentials: digest + cursor round-trip")
def s3_stdio_localfs_digest(_client: SidecarClient) -> str:
    root = tempfile.mkdtemp(prefix="dbx-files-mcp-stdio-fs-")
    connection = {"protocol": "local", "root": root, "name": "smoke-stdio"}
    session = StdioSession(default_binary())
    try:
        # 建树走 stdio MCP 写工具本身（mkdir/write 单阶段，内联凭据）。
        session.call_tool("files_mkdir", connection=connection, path="/data/sub")
        for name in ("a.txt", "b.log"):
            session.call_tool(
                "files_write",
                connection=connection,
                path=f"/data/{name}",
                dataBase64=base64.b64encode(f"content of {name}".encode()).decode(),
            )

        digest = session.call_tool(
            "files_scan_digest", connection=connection, path="/data", glob="*"
        )
        assert digest["matched"] >= 3, digest
        assert digest["scanned"] >= digest["matched"], digest
        assert digest["cursorId"], digest
        # 池化键稳定：同一 connection 参数的重复调用复用同一连接条目。
        again = session.call_tool(
            "files_scan_digest", connection=connection, path="/data", format="rows"
        )
        assert again["connectionId"] == digest["connectionId"], (digest, again)
        assert again["connectionId"].startswith("mcp-inline-"), again

        batch = session.call_tool("files_cursor_next", cursorId=digest["cursorId"], n=2)
        assert batch["rows"] and len(batch["rows"]) <= 2, batch
        assert batch["done"] is False, batch

        # 连接寻址引导：缺 connectionId 给出可执行出路（未池化 id 的桥转发
        # 兜底与 fail-closed 引导在 B1 场景覆盖，此处不触发 30s 唤醒预算）。
        error = session.call_tool_error("files_scan_digest", path="/")
        assert "Missing required parameter: connectionId" in error, error

        # quick_paths 纯元发现不受 stdio 降级影响；limit 数字字符串被接受。
        quick = session.call_tool("files_ui_quick_paths", connection=connection, limit="1")
        assert quick["paths"] and len(quick["paths"]) <= 1, quick
        return (f"digest matched={digest['matched']}; pooled id stable; "
                "cursor paged; quick_paths limit ok; addressing guidance ok")
    finally:
        session.close()


@scenario("S4", "stdio localFs two-phase delete full flow")
def s4_stdio_two_phase_delete(_client: SidecarClient) -> str:
    root = tempfile.mkdtemp(prefix="dbx-files-mcp-stdio-del-")
    connection = {"protocol": "local", "root": root, "name": "smoke-stdio-del"}
    session = StdioSession(default_binary())
    try:
        session.call_tool(
            "files_write", connection=connection, path="/victim.txt",
            dataBase64=base64.b64encode(b"delete me").decode(),
        )
        target = "/victim.txt"

        # stdio 缺参枚举（§3.9）：内联连接在场（连接门已过），业务参数
        # path/dataBase64 一次枚举；stdio schema required（去 connectionId
        # 后）与报错枚举集合一致（核对器 B1 口径）。
        error = session.call_tool_error(
            "files_write", connection=connection,
            dataBase64=base64.b64encode(b"x").decode(),
        )
        assert "Missing required parameters: path" in error, error

        preview = session.call_tool("files_delete", connection=connection, path=target)
        assert preview["preview"]["path"] == target and preview.get("confirmToken"), preview
        token = preview["confirmToken"]

        # 参数 hash 绑定：token 有效但参数被改 → 作废（须重开预览）。
        error = session.call_tool_error(
            "files_delete", connection=connection, path="/other.txt", confirmToken=token
        )
        assert "arguments changed" in error, error

        preview2 = session.call_tool("files_delete", connection=connection, path=target)
        confirm = session.call_tool(
            "files_delete", connection=connection, path=target,
            confirmToken=preview2["confirmToken"],
        )
        assert confirm["success"] is True, confirm
        assert not os.path.exists(os.path.join(root, "victim.txt")), "file must be gone on disk"

        # 一次性：同 token 重放 → unknown。
        error = session.call_tool_error(
            "files_delete", connection=connection, path=target,
            confirmToken=preview2["confirmToken"],
        )
        assert "unknown or already used" in error, error

        # 目录 rename 降级 job 需要工作台事件通道：stdio 明确报错不假死。
        session.call_tool("files_mkdir", connection=connection, path="/adir")
        error = session.call_tool_error(
            "files_rename", connection=connection, path="/adir", newPath="/adir2"
        )
        assert "standalone stdio" in error, error
        return "preview -> tamper invalidates -> confirm executes -> disk verified -> replay refused"
    finally:
        session.close()


# -- stdio app-bridge fallback (B1: ssh smoke 场景 8/12 + ldap M14 同构) ---------


class MockBridge:
    """Local mock of the DBX app's `/call-plugin-tool` TCP bridge.

    Minimal host-contract implementation: publishes its port in
    `<app_data_dir>/mcp-bridge-port`, records every request (path + snake_case
    body), and replies with a configurable status/body. Lets the L1 forward
    path be exercised without a real DBX.app.
    """

    def __init__(self, app_data: str, status: int = 200, body: dict | None = None):
        import http.server
        import threading

        self.requests: list[tuple[str, dict]] = []
        self.status = status
        self.body = body or {
            "content": [{"type": "text", "text": json.dumps({"matched": 7})}],
            "isError": False,
        }
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                outer.requests.append(
                    (self.path, json.loads(self.rfile.read(length) or b"{}"))
                )
                payload = json.dumps(outer.body).encode()
                self.send_response(outer.status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def log_message(self, *args):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        port = self.server.server_address[1]
        port_file = pathlib.Path(app_data) / "mcp-bridge-port"
        with port_file.open("w", encoding="utf-8") as handle:
            handle.write(str(port))
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def stop(self) -> None:
        self.server.shutdown()
        self.server.server_close()


@scenario("B1", "stdio app-bridge fallback: fail-closed + mock-bridge forward")
def b1_bridge_fallback(_client: SidecarClient) -> str:
    # 1) 桥未发布：空 app-data + no-op launch（`:`，无 UI 弹出）→ ensure 跑满
    #    app-start 唤醒预算（30s，ssh 场景 8 / ldap M14 同款：这正是被测行为）
    #    → fail-closed 可行动错误（桥失败原因 + 内联凭据出路），不假死。
    app_data = tempfile.mkdtemp(prefix="dbx-files-mcp-appdata-")
    session = StdioSession(default_binary(), extra_env={
        "DBX_APP_DATA_DIR": app_data, "DBX_APP_LAUNCH_CMD": ":"})
    try:
        error = session.call_tool_error(
            "files_scan_digest", connectionId="stdio-nope", path="/")
        for fragment in (
            "Unknown connectionId 'stdio-nope'",
            "DBX app bridge",
            "inline connection parameters",
            "secretAccessKey",
        ):
            assert fragment in error, f"missing {fragment!r}: {error}"
    finally:
        session.close()

    # 2) 桥存在（本地 mock 桥）：未池化 connectionId 转发宿主契约五字段
    #    （snake_case，plugin_id=io.dbx.files），应用侧 MCP envelope 逐字
    #    透传为 stdio 响应。
    app_data = tempfile.mkdtemp(prefix="dbx-files-mcp-stubapp-")
    mock = MockBridge(app_data)
    session = StdioSession(default_binary(), extra_env={
        "DBX_APP_DATA_DIR": app_data, "DBX_APP_LAUNCH_CMD": ":"})
    try:
        forwarded = session.call_tool(
            "files_scan_digest", connectionId="saved-jane", path="/data", glob="*")
        assert forwarded == {"matched": 7}, forwarded
        assert len(mock.requests) == 1, mock.requests
        path, body = mock.requests[0]
        assert path == "/call-plugin-tool", path
        assert body["plugin_id"] == "io.dbx.files", body
        assert body["connection_id"] == "saved-jane", body
        assert body["tool"] == "files_scan_digest", body
        assert body["arguments"]["path"] == "/data", body
    finally:
        session.close()
        mock.stop()

    # 3) 桥 404（连接不存在/旧版应用）：错误带 "DBX app bridge returned HTTP"。
    app_data = tempfile.mkdtemp(prefix="dbx-files-mcp-deadapp-")
    mock = MockBridge(app_data, status=404,
                      body={"error": "Connection with id 'ghost' not found"})
    session = StdioSession(default_binary(), extra_env={
        "DBX_APP_DATA_DIR": app_data, "DBX_APP_LAUNCH_CMD": ":"})
    try:
        error = session.call_tool_error(
            "files_scan_digest", connectionId="ghost", path="/")
        assert "DBX app bridge returned HTTP 404" in error, error
    finally:
        session.close()
        mock.stop()
    return ("fail-closed without the app; mock-bridge forward contract + "
            "envelope verbatim; 404 surfaced")


# -- stdio transport robustness (T1-T6: 可靠性纵深轮; no deps, never SKIP) --------


def _alive(session: StdioSession) -> dict:
    """Health probe after adversarial input: a legal ping still answers and
    the process is still running."""
    message = session.request("ping", {})
    assert message.get("result") == {}, message
    assert session.proc.poll() is None, "sidecar process must stay alive"
    return message


@scenario("T1", "stdio transport: garbage lines, invalid UTF-8, process survives")
def t1_transport_garbage(_client: SidecarClient) -> str:
    session = StdioSession(default_binary())
    try:
        # 非法 JSON 行 → -32700（null id），不是进程错误。
        session.send_bytes(b"not json\n")
        message = session.recv()
        assert message["error"]["code"] == -32700, message
        assert message["id"] is None, message
        assert "Parse error" in message["error"]["message"], message

        # 非法 UTF-8 字节流 → lossy 解码后解析失败 → -32700；进程绝不因
        # 传输层字节退出（read_until + from_utf8_lossy 契约）。
        session.send_bytes(b'\xff\xfe{"jsonrpc": "2.0"\n')
        message = session.recv()
        assert message["error"]["code"] == -32700, message

        # 空洞 JSON 片段、二进制噪声同罪（字节集刻意排除 \n/\r：发送的
        # 必须是恰好一行，响应数才可预期——混入 \n 会拆成多行、多份
        # -32700，残留响应会污染后续场景的行对齐）。
        session.send_bytes(b"{\n")
        assert session.recv()["error"]["code"] == -32700
        noise = bytes(b for b in range(1, 32) if b not in (10, 13)) + b"\n"
        session.send_bytes(noise)
        assert session.recv()["error"]["code"] == -32700

        # 每条对抗输入之后：合法请求仍然成功，进程存活。
        for _ in range(4):
            _alive(session)
    finally:
        session.close()
    return "garbage/invalid-UTF-8 lines → -32700 (null id); connection healthy after each"


@scenario("T2", "stdio notifications and id-less requests keep the connection usable")
def t2_notifications_and_idless(_client: SidecarClient) -> str:
    session = StdioSession(default_binary())
    try:
        # notifications/*（无 id）从不回包：其后合法请求的响应是管线上
        # 下一行（S1 已钉一行形状，这里连续多条 + 混入通知再验证）。
        session.next_id += 1
        want = session.next_id
        session.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        session.send({"jsonrpc": "2.0", "method": "notifications/other"})
        session.send({"jsonrpc": "2.0", "id": want, "method": "ping", "params": {}})
        message = session.recv()
        assert message.get("id") == want and message.get("result") == {}, message

        # 无 id 的非 notification 请求：invalid request -32600（id 回 null），
        # 连接继续可用。
        session.send({"jsonrpc": "2.0", "method": "ping"})
        message = session.recv()
        assert message["error"]["code"] == -32600, message
        assert message["id"] is None, message
        assert "Invalid request" in message["error"]["message"], message
        _alive(session)
    finally:
        session.close()
    return "notifications silent; id-less request → -32600; connection usable after both"


@scenario("T3", "stdio structural validation: missing method, object id, jsonrpc tolerance")
def t3_structural(_client: SidecarClient) -> str:
    session = StdioSession(default_binary())
    try:
        # 缺 method / method 非字符串 / 空串 → -32600（invalid request，
        # 不是可纠正的未知方法）。
        for request in (
            {"jsonrpc": "2.0", "id": 7},
            {"jsonrpc": "2.0", "id": 8, "method": ""},
            {"jsonrpc": "2.0", "id": 9, "method": 42},
        ):
            session.send(request)
            message = session.recv()
            assert message["error"]["code"] == -32600, (request, message)
            assert "missing method" in message["error"]["message"], message

        # id 为 object/array/boolean：-32600，id 不回显（JSON-RPC id ∈
        # string/number/null）。
        for bad_id in ({"a": 1}, [1], True):
            session.send({"jsonrpc": "2.0", "id": bad_id, "method": "ping"})
            message = session.recv()
            assert message["error"]["code"] == -32600, (bad_id, message)
            assert message["id"] is None, message
            assert "id must be a string, number, or null" in message["error"]["message"], message

        # jsonrpc 版本字段宽容（族一致：ssh/ldap/kafka 均不校验；拒绝只会
        # 破坏真实客户端兼容）——缺失 / "1.0" / 数字 2 都正常应答。
        for request in (
            {"id": 21, "method": "ping"},
            {"jsonrpc": "1.0", "id": 22, "method": "ping"},
            {"jsonrpc": 2, "id": 23, "method": "ping"},
        ):
            session.send(request)
            message = session.recv()
            assert message.get("result") == {} and message["id"] == request["id"], message

        _alive(session)
    finally:
        session.close()
    return ("-32600 missing method / non-scalar id; jsonrpc field tolerated "
            "(pinned family behavior); healthy after each")


@scenario("T4", "stdio oversized lines: 8MiB payload errors cleanly, over-limit refused")
def t4_oversized_lines(_client: SidecarClient) -> str:
    # a) 默认 16 MiB 行上限下，8 MiB 参数（base64 后 ≈10.7 MiB 行）必须
    #    完整解析并走工具级错误路径（4 MiB 写上限 → isError + 上传通道
    #    指引），绝不 panic、绝不断流。
    root = tempfile.mkdtemp(prefix="dbx-files-mcp-stdio-big-")
    connection = {"protocol": "local", "root": root, "name": "smoke-big"}
    session = StdioSession(default_binary())
    try:
        session.next_id += 1
        want = session.next_id
        session.send_request("tools/call", {
            "name": "files_write",
            "arguments": {
                "connection": connection,
                "path": "/big.bin",
                "dataBase64": base64.b64encode(b"x" * (8 * 1024 * 1024)).decode(),
            },
        }, want)
        message = session.wait_for(want)
        assert message["result"]["isError"] is True, str(message)[:200]
        text = message["result"]["content"][0]["text"]
        assert "exceeds" in text, message
        assert "upload" in text, message
        _alive(session)
    finally:
        session.close()

    # b) DBX_FILES_MCP_STDIO_MAX_LINE 调小做等效压测：超限行在读取层即被
    #    拒（-32700 + 点名上限与 env），连接继续可用。
    session = StdioSession(default_binary(), extra_env={
        "DBX_FILES_MCP_STDIO_MAX_LINE": "1024"})
    try:
        session.send_bytes(b'{"jsonrpc":"2.0","id":1,"method":"ping","params":{"pad":"' +
                           b"p" * 4096 + b'"}}\n')
        message = session.recv()
        assert message["error"]["code"] == -32700, message
        assert "exceeds the 1024-byte limit" in message["error"]["message"], message
        # 行内的 id=1 不产生响应：下一行是健康探针的应答。
        _alive(session)
    finally:
        session.close()
    return ("8MiB arguments → clean isError write-cap error; over-limit line "
            "(tuned 1KiB cap) → -32700 naming the env; no panic, pipe alive")


@scenario("T5", "stdio pipelining: 3+ requests in flight, answers correlate by id")
def t5_pipelining(_client: SidecarClient) -> str:
    root = tempfile.mkdtemp(prefix="dbx-files-mcp-stdio-pipe-")
    connection = {"protocol": "local", "root": root, "name": "smoke-pipe"}
    session = StdioSession(default_binary())
    try:
        # 不等待响应连发 4 个不同 id 请求（其中夹一条会产生 -32700 的垃圾
        # 行）：4 个响应与 id 一一对应，乱序写出也不串线。
        ids = {}
        session.next_id += 1
        ids["ping"] = session.send_request("ping", {}, session.next_id)
        session.send_bytes(b"garbage line\n")  # id=null 的 -32700 混在中间
        session.next_id += 1
        ids["tools"] = session.send_request("tools/list", {}, session.next_id)
        session.next_id += 1
        ids["quick"] = session.send_request("tools/call", {
            "name": "files_ui_quick_paths",
            "arguments": {"connection": connection},
        }, session.next_id)
        session.next_id += 1
        ids["write"] = session.send_request("tools/call", {
            "name": "files_write",
            "arguments": {"connection": connection, "path": "/p.txt",
                          "dataBase64": base64.b64encode(b"p").decode()},
        }, session.next_id)

        # 混入的垃圾行给一条 id=null 的 -32700（先于/穿插于各响应皆可——
        # spawned handler 与主循环写响应存在调度竞态，只断言全部到达、
        # parse error 恰一条、四个 id 一一对应）。
        parse_errors = []
        answers = {}
        expected = {ids["ping"], ids["tools"], ids["quick"], ids["write"]}
        while len(answers) < len(expected):
            message = session.recv()
            mid = message.get("id")
            if mid is None and "error" in message:
                assert message["error"]["code"] == -32700, message
                parse_errors.append(message)
                continue
            assert mid in expected and mid not in answers, message
            answers[mid] = message
        assert len(parse_errors) == 1, parse_errors

        assert answers[ids["ping"]].get("result") == {}, answers[ids["ping"]]
        tools = answers[ids["tools"]]
        assert len(tools["result"]["tools"]) == 13, str(tools)[:200]
        quick = answers[ids["quick"]]
        assert "paths" in json.loads(quick["result"]["content"][0]["text"]), quick
        write = answers[ids["write"]]
        payload = json.loads(write["result"]["content"][0]["text"])
        assert payload["success"] is True and payload["path"] == "/p.txt", payload
        assert os.path.exists(os.path.join(root, "p.txt")), "write landed on disk"
    finally:
        session.close()
    return "4 in-flight requests (ping/tools/list/quick_paths/write) + garbage line: ids correlate exactly"


@scenario("T6", "stdio blank lines and CRLF tolerance")
def t6_blank_and_crlf(_client: SidecarClient) -> str:
    session = StdioSession(default_binary())
    try:
        # 空行 / 纯空白行：静默跳过（不产生任何响应行）。
        session.send_bytes(b"\n")
        session.send_bytes(b"   \n")
        session.send_bytes(b"\r\n")
        # CRLF 结尾的合法请求：正常解析应答（\r 不进 JSON）。
        session.send_bytes(b'{"jsonrpc":"2.0","id":31,"method":"ping","params":{}}\r\n')
        message = session.recv()
        assert message.get("id") == 31 and message.get("result") == {}, message
        _alive(session)
    finally:
        session.close()
    return "blank/whitespace lines skipped; CRLF-terminated request answered"


# -- runner ------------------------------------------------------------------------


def report() -> None:
    widths = (4, 52, 6)
    print(f"{'No.':<{widths[0]}} {'Scenario':<{widths[1]}} {'Status':<{widths[2]}} Detail")
    for result in RESULTS:
        print(f"{result.no:<{widths[0]}} {result.name:<{widths[1]}} {result.status:<{widths[2]}} {result.detail}")
    counts = {status: sum(1 for r in RESULTS if r.status == status) for status in ("PASS", "FAIL", "SKIP")}
    print(f"\ntotal={len(RESULTS)} PASS={counts['PASS']} FAIL={counts['FAIL']} SKIP={counts['SKIP']}")


def _all_scenarios():
    return [m1_settings, m2_tools, m3_readonly_tools, m4_readonly_write, m5_intent_pending,
            m6_intent_applied, m7_ui_state, m8_digest_gates, m9_unknown, m10_fs, m11_llm_variants,
            m12_clamp, m13_ttl_realtime,
            r1_remote_s3, r2_remote_webdav, r3_remote_ftp,
            r4_remote_sftp, r5_remote_smb, r6_remote_sftp_native, r7_remote_oss,
            s1_stdio_handshake, s2_stdio_ui_unavailable, s3_stdio_localfs_digest,
            s4_stdio_two_phase_delete, b1_bridge_fallback,
            t1_transport_garbage, t2_notifications_and_idless, t3_structural,
            t4_oversized_lines, t5_pipelining, t6_blank_and_crlf]


def main() -> int:
    parser = argparse.ArgumentParser(description="Offline MCP stdio smoke for the dbx-files sidecar")
    parser.add_argument("--binary", default=None,
                        help="sidecar binary path (default: DBX_PLUGIN_SIDECAR or backend/target/release/dbx-plugin-files)")
    args = parser.parse_args()
    binary = args.binary or default_binary()
    if not os.path.exists(binary):
        message = f"sidecar binary not found: {binary} (use --binary or set DBX_PLUGIN_SIDECAR)"
        for fn in _all_scenarios():
            RESULTS.append(ScenarioResult(fn.no, fn.name, "FAIL" if REQUIRE else "SKIP", message))
        report()
        return 0
    # Pin the resolved binary for every default_binary() call site: the stdio
    # scenarios (StdioSession) read the env independently, and on Windows the
    # repo-relative fallback lacks the .exe suffix — the 2026-09-15 Windows
    # candidate run failed exactly there (WinError 2 on process spawn).
    os.environ["DBX_PLUGIN_SIDECAR"] = binary

    # 隔离数据目录：mcp-settings.json 与 audit.jsonl 不污染真实插件数据。
    data_dir = tempfile.mkdtemp(prefix="dbx-files-mcp-smoke-")
    client = SidecarClient.start(binary, data_dir=data_dir)
    try:
        client.initialize()
        for fn in _all_scenarios():
            fn(client)
    finally:
        try:
            client.request("connection/disconnect", {"connection": {"id": "smoke-mcp-ro"}})
        except Exception:
            pass
        try:
            client.close()
        except Exception:
            pass

    report()
    return 1 if any(result.status == "FAIL" for result in RESULTS) else 0


if __name__ == "__main__":
    raise SystemExit(main())
