#!/usr/bin/env python3
"""Live instance smoke for the filesystem/* facade (provider io.dbx.files.fs).

Drives the real sidecar binary over the stdio-framed protocol against a
throwaway local directory (protocol fs, no real credentials): connection
lifecycle + all nine filesystem/* methods + the policy gates (read_only /
allow_delete) + URI escape rejection. Verifies side effects on the real
filesystem, not just RPC responses.

Usage:
    python3 scripts/smoke_fs_facade.py          # backend/target/release binary
    DBX_PLUGIN_SIDECAR=<bin> python3 scripts/smoke_fs_facade.py
"""

from __future__ import annotations

import base64
import hashlib
import os
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from sidecar_client import SidecarClient, SidecarError, default_binary, lifecycle_params  # noqa: E402

PROVIDER = "io.dbx.files.fs"
CHUNK = 1_048_576
results: list[tuple[str, str, str]] = []


def record(name: str, ok: bool = True, note: str = "") -> None:
    results.append((name, "pass" if ok else "FAIL", note))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" — {note}" if note else ""))
    if not ok:
        raise SidecarError(f"smoke step failed: {name} {note}")


def expect_error(client: SidecarClient, method: str, params: dict, needle: str = "") -> str:
    try:
        client.request(method, params)
    except SidecarError as error:
        text = str(error)
        if needle and needle.lower() not in text.lower():
            raise SidecarError(f"{method}: expected error containing {needle!r}, got: {text}")
        return text
    raise SidecarError(f"{method}: expected an error, got success")


def uri(conn: str, *segments: str) -> str:
    from urllib.parse import quote

    tail = "/".join(quote(segment, safe="") for segment in segments)
    return f"rclone://{conn}/{tail}" if segments else f"rclone://{conn}/"


def main() -> int:
    sidecar = os.environ.get("DBX_PLUGIN_SIDECAR") or default_binary()
    if not Path(sidecar).exists():
        print(f"SKIP: sidecar binary not found at {sidecar} (build first or set DBX_PLUGIN_SIDECAR)")
        return 2
    print(f"==> sidecar: {sidecar}")

    # 数据目录与存储 root 分离：store(SQLite) 不落在被列目录里，污染分页断言。
    with tempfile.TemporaryDirectory(prefix="dbx-files-fs-facade-") as raw_root, \
            tempfile.TemporaryDirectory(prefix="dbx-files-fs-facade-data-") as data_root:
        root = Path(raw_root)
        # Fixture on the real filesystem: subdir + text file + 2.5 MiB binary
        # (three download chunks) + a short file for the truncation case.
        docs = root / "docs"
        docs.mkdir()
        (docs / "hello.txt").write_text("hello facade\n", encoding="utf-8")
        big = bytes((i * 31 + 7) % 256 for i in range(2_500_000))
        (root / "big.bin").write_bytes(big)
        (root / "note.txt").write_bytes(b"0123456789ABCDEF")
        big_sha = hashlib.sha256(big).hexdigest()
        conn = "fs-facade-smoke"

        client = SidecarClient.start(timeout=30, data_dir=data_root)
        try:
            print("==> plugin/initialize")
            info = client.initialize()
            record("initialize handshake", info.get("plugin", {}).get("id") == "io.dbx.files")

            print("==> connection lifecycle (protocol fs)")
            client.request("connection/test", lifecycle_params(
                {"id": f"{conn}::test", "name": conn, "db_type": "storage", "host": "", "port": 0,
                 "external_config": {"protocol": "fs", "root": str(root)}}))
            record("connection/test")
            client.request("connection/connect", lifecycle_params(
                {"id": conn, "name": conn, "db_type": "storage", "host": "", "port": 0,
                 "external_config": {"protocol": "fs", "root": str(root)}}))
            record("connection/connect")

            print("==> filesystem/list (root + pagination)")
            listing = client.request("filesystem/list", {
                "providerId": PROVIDER, "connectionId": conn, "uri": uri(conn), "limit": 100})
            names = {entry["name"] for entry in listing["entries"]}
            record("list root entries", {"docs", "big.bin", "note.txt"} <= names
                   and all("/" not in n and n not in (".", "..") for n in names),
                   f"names={sorted(names)}")
            record("list root has no nextCursor", "nextCursor" not in listing)

            page_size, cursor, seen, pages = 2, None, [], 0
            while True:
                params = {"providerId": PROVIDER, "connectionId": conn, "uri": uri(conn), "limit": page_size}
                if cursor:
                    params["cursor"] = cursor
                page = client.request("filesystem/list", params)
                seen += [entry["uri"] for entry in page["entries"]]
                pages += 1
                cursor = page.get("nextCursor")
                if not cursor:
                    break
            record("pagination walk", pages == 2 and len(seen) == 3 and len(set(seen)) == 3,
                   f"pages={pages} entries={len(seen)}")

            print("==> filesystem/write (create / conflict / overwrite)")
            target = uri(conn, "docs", "new.md")
            client.request("filesystem/write", {
                "providerId": PROVIDER, "connectionId": conn, "uri": target,
                "dataBase64": base64.b64encode("# facade".encode()).decode(),
                "create": True, "overwrite": False})
            record("write create", (docs / "new.md").read_text(encoding="utf-8") == "# facade")
            expect_error(client, "filesystem/write", {
                "providerId": PROVIDER, "connectionId": conn, "uri": target,
                "dataBase64": "", "create": True, "overwrite": False}, needle="exist")
            record("write refuses existing without overwrite")
            client.request("filesystem/write", {
                "providerId": PROVIDER, "connectionId": conn, "uri": target,
                "dataBase64": base64.b64encode("# facade v2".encode()).decode(),
                "create": True, "overwrite": True})
            record("write overwrite", (docs / "new.md").read_text(encoding="utf-8") == "# facade v2")

            print("==> filesystem/read (exact / truncated / directory)")
            read = client.request("filesystem/read", {
                "providerId": PROVIDER, "connectionId": conn, "uri": target, "maxBytes": 1 << 20})
            record("read exact", base64.b64decode(read["dataBase64"]) == b"# facade v2"
                   and read.get("truncated") is False)
            read = client.request("filesystem/read", {
                "providerId": PROVIDER, "connectionId": conn, "uri": uri(conn, "note.txt"),
                "maxBytes": 10})
            record("read truncated flag", base64.b64decode(read["dataBase64"]) == b"0123456789"
                   and read.get("truncated") is True)
            expect_error(client, "filesystem/read", {
                "providerId": PROVIDER, "connectionId": conn, "uri": uri(conn, "docs"),
                "maxBytes": 1024})
            record("read directory rejects")

            print("==> filesystem/createDirectory")
            client.request("filesystem/createDirectory", {
                "providerId": PROVIDER, "connectionId": conn, "uri": uri(conn, "empty-dir")})
            record("mkdir on real fs", (root / "empty-dir").is_dir())
            client.request("filesystem/createDirectory", {
                "providerId": PROVIDER, "connectionId": conn, "uri": uri(conn, "empty-dir")})
            record("mkdir idempotent")

            print("==> filesystem/rename")
            client.request("filesystem/rename", {
                "providerId": PROVIDER, "connectionId": conn,
                "sourceUri": uri(conn, "docs", "new.md"),
                "targetUri": uri(conn, "docs", "renamed.md"), "overwrite": False})
            record("rename file on real fs", (docs / "renamed.md").exists()
                   and not (docs / "new.md").exists())

            print("==> filesystem/download/* (chunked pull of 2.5 MiB)")
            start = client.request("filesystem/download/open", {
                "downloadId": "dl-smoke-1", "providerId": PROVIDER, "connectionId": conn,
                "uri": uri(conn, "big.bin")})
            record("download/open size", int(start.get("size", -1)) == len(big))
            chunks, done, read_count = [], False, 0
            while not done:
                part = client.request("filesystem/download/read", {
                    "downloadId": "dl-smoke-1", "providerId": PROVIDER, "connectionId": conn})
                data = base64.b64decode(part["dataBase64"])
                done = bool(part.get("done"))
                # 契约：未完成时不得返回空分块；done 允许携带 ≤1MiB 的尾块数据。
                record("download chunk contract", len(data) <= CHUNK and (data or done),
                       f"len={len(data)} done={done}")
                chunks.append(data)
                read_count += 1
            record("download read count sane", read_count == 3, f"reads={read_count}")
            pulled = b"".join(chunks)
            record("download roundtrip sha256", hashlib.sha256(pulled).hexdigest() == big_sha,
                   f"bytes={len(pulled)}")
            client.request("filesystem/download/close", {
                "downloadId": "dl-smoke-1", "providerId": PROVIDER, "connectionId": conn})
            client.request("filesystem/download/close", {
                "downloadId": "dl-smoke-1", "providerId": PROVIDER, "connectionId": conn})
            record("download/close idempotent")

            print("==> filesystem/delete (file / non-empty dir / recursive)")
            client.request("filesystem/delete", {
                "providerId": PROVIDER, "connectionId": conn,
                "uri": uri(conn, "docs", "renamed.md"), "recursive": False})
            record("delete file on real fs", not (docs / "renamed.md").exists())
            expect_error(client, "filesystem/delete", {
                "providerId": PROVIDER, "connectionId": conn, "uri": uri(conn, "docs"),
                "recursive": False})
            record("delete non-empty dir rejects")
            client.request("filesystem/delete", {
                "providerId": PROVIDER, "connectionId": conn, "uri": uri(conn, "docs"),
                "recursive": True})
            record("delete recursive on real fs", not docs.exists())

            print("==> guard rails (provider id / path escape)")
            expect_error(client, "filesystem/list", {
                "providerId": "io.dbx.files.wrong", "connectionId": conn, "uri": uri(conn)})
            record("wrong providerId rejects")
            expect_error(client, "filesystem/write", {
                "providerId": PROVIDER, "connectionId": conn,
                "uri": uri(conn, "..", "escape.txt"),
                "dataBase64": base64.b64encode(b"x").decode(), "create": True, "overwrite": True})
            record("traversal uri rejects", not (root.parent / "escape.txt").exists())

            print("==> policy gates (read_only connection)")
            ro_conn = "fs-facade-smoke-ro"
            client.request("connection/connect", lifecycle_params(
                {"id": ro_conn, "name": ro_conn, "db_type": "storage", "host": "", "port": 0,
                 "external_config": {"protocol": "fs", "root": str(root), "read_only": True}}))
            expect_error(client, "filesystem/write", {
                "providerId": PROVIDER, "connectionId": ro_conn, "uri": uri(ro_conn, "nope.txt"),
                "dataBase64": "", "create": True, "overwrite": True})
            expect_error(client, "filesystem/delete", {
                "providerId": PROVIDER, "connectionId": ro_conn,
                "uri": uri(ro_conn, "note.txt"), "recursive": False})
            read = client.request("filesystem/read", {
                "providerId": PROVIDER, "connectionId": ro_conn,
                "uri": uri(ro_conn, "note.txt"), "maxBytes": 1024})
            record("read_only: write+delete reject, read ok",
                   base64.b64decode(read["dataBase64"]) == b"0123456789ABCDEF")
            expect_error(client, "filesystem/write", {
                "providerId": PROVIDER, "connectionId": conn,
                "uri": "rclone://ghost-conn/x.txt",
                "dataBase64": "", "create": True, "overwrite": True})
            record("unknown connection rejects")

            print("==> disconnect")
            client.request("connection/disconnect", lifecycle_params(
                {"id": conn, "name": conn, "db_type": "storage", "host": "", "port": 0}))
            client.request("connection/disconnect", lifecycle_params(
                {"id": ro_conn, "name": ro_conn, "db_type": "storage", "host": "", "port": 0}))
            record("connection/disconnect both")
        finally:
            client.close()

    passed = sum(1 for _, status, _ in results if status == "pass")
    print(f"\n==== fs facade smoke: {passed}/{len(results)} steps passed ====")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    started = time.monotonic()
    try:
        sys.exit(main())
    except SidecarError as error:
        print(f"\nFAIL: {error}", file=sys.stderr)
        failed = sum(1 for _, status, _ in results if status == "FAIL")
        print(f"==== fs facade smoke: {len(results) - failed}/{len(results)} steps passed (aborted) ====")
        sys.exit(1)
    finally:
        print(f"elapsed: {time.monotonic() - started:.1f}s")
