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
        (no container needed: the tree is built through the MCP write tools)

SKIP semantics (M0 §5.2, same as smoke_test.py):
  * method/tool not registered  -> SKIP (never FAIL);
  * no sidecar binary           -> whole suite SKIP (FAIL with DBX_FILES_REQUIRE=1).

Usage:
    python3 scripts/smoke_mcp.py                       # release binary from the repo
    DBX_PLUGIN_SIDECAR=/path/to/dbx-plugin-files python3 scripts/smoke_mcp.py
"""

from __future__ import annotations

import base64
import json
import os
import sys
import tempfile
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


@scenario("M2", "mcp/tools lists the 12 files tools with schemas")
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
        return (f"digest matched={digest['matched']}; cursor paged; two-phase delete executed; "
                f"token single-use; audit source=mcp ({len(mcp_entries)} entries)")
    finally:
        try:
            client.request("connection/disconnect", {"connection": {"id": conn_id}})
        except Exception:
            pass


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
            m6_intent_applied, m7_ui_state, m8_digest_gates, m9_unknown, m10_fs]


def main() -> int:
    binary = default_binary()
    if not os.path.exists(binary):
        message = f"sidecar binary not found: {binary} (set DBX_PLUGIN_SIDECAR)"
        for fn in _all_scenarios():
            RESULTS.append(ScenarioResult(fn.no, fn.name, "FAIL" if REQUIRE else "SKIP", message))
        report()
        return 0

    # 隔离数据目录：mcp-settings.json 与 audit.jsonl 不污染真实插件数据。
    data_dir = tempfile.mkdtemp(prefix="dbx-files-mcp-smoke-")
    client = SidecarClient.start(data_dir=data_dir)
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
