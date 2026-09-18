#!/usr/bin/env python3
"""F-RCLONE smoke driver: speaks the sidecar's stdio-framed protocol directly.

Exercises the same wire path the DBX host uses (5-byte frame header: 1 kind
byte + 4-byte BE length, JSON payloads) against a locally built sidecar
binary. Runs the full Phase A surface — plugin/initialize, connection
test/connect/disconnect, files list/listPaged/stat/capabilities/size/
quickPaths, one negative case, and one cross-engine fallback case — plus the
Phase B file surface: read/write with truncation, mkdir/copy/move/rename,
delete/purge (root refusal), publicLink (local refuses), and the binary
upload/download channels (§7 frames: kind 1 payload = 2B channel length +
channel + 8B BE offset + ≤256KiB data). The rclone pass adds the Phase C
dir-sync surface: files/copyDir (real copy), files/syncDir dryRun (no
writes/deletes) and a real syncDir mirror, each visible with a terminal
state in files/transfers/list. The rclone pass closes the method surface
with the Phase D archive round — zip archiveList (rc-serve Range reads),
extract, compress (zip + tar.gz) and byte-level read-back — plus a
standalone `bin --mcp` check (rclone mode): initialize, tools/list,
files_scan_digest over an inline local connection and files_cursor_next
paging.

Usage:
  python3 scripts/rclone_engine_smoke.py <path-to-dbx-plugin-files-bin> [--engine rclone|opendal]

Exit code 0 = all steps passed.
"""
import base64
import io
import json
import os
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import zipfile

PASS = []
FAIL = []

FRAME_KIND_JSON = 0
FRAME_KIND_BINARY = 1
TRANSFER_CHUNK_SIZE = 256 * 1024


def check(name, condition, detail=""):
    if condition:
        PASS.append(name)
        print(f"  PASS  {name}")
    else:
        FAIL.append((name, detail))
        print(f"  FAIL  {name}  {detail}")


def encode_frame(obj: dict) -> bytes:
    payload = json.dumps(obj).encode()
    return struct.pack(">BI", FRAME_KIND_JSON, len(payload)) + payload


def encode_binary(channel: str, data: bytes) -> bytes:
    encoded = channel.encode()
    payload = struct.pack(">H", len(encoded)) + encoded + data
    return struct.pack(">BI", FRAME_KIND_BINARY, len(payload)) + payload


class Sidecar:
    def __init__(self, binary: str, engine: str | None):
        env = dict(os.environ)
        if engine:
            env["DBX_FILES_ENGINE"] = engine
        else:
            env.pop("DBX_FILES_ENGINE", None)
        self.proc = subprocess.Popen(
            [binary],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=env,
        )
        self.buf = b""
        self.next_id = 1
        self.binaries: list[tuple[str, bytes]] = []

    def _read_exact(self, n: int, deadline: float | None = None) -> bytes:
        while len(self.buf) < n:
            remaining = None if deadline is None else deadline - time.time()
            if remaining is not None and remaining <= 0:
                raise RuntimeError("timed out waiting for sidecar frames")
            fd = self.proc.stdout.fileno()
            if deadline is None:
                chunk = os.read(fd, 65536)
            else:
                ready, _, _ = select.select([fd], [], [], remaining)
                if not ready:
                    raise RuntimeError("timed out waiting for sidecar frames")
                chunk = os.read(fd, 65536)
            if not chunk:
                raise RuntimeError("sidecar closed stdout")
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def read_frame_raw(self, deadline: float | None = None):
        kind, length = struct.unpack(">BI", self._read_exact(5, deadline))
        return kind, self._read_exact(length, deadline)

    def read_frame(self):
        """Next JSON frame; binary frames are buffered for read_binary()."""
        while True:
            kind, payload = self.read_frame_raw()
            if kind == FRAME_KIND_JSON:
                return json.loads(payload)
            if kind == FRAME_KIND_BINARY:
                self.binaries.append(self._split_binary(payload))
                continue
            raise RuntimeError(f"unexpected frame kind {kind}")

    @staticmethod
    def _split_binary(payload: bytes) -> tuple[str, bytes]:
        (channel_len,) = struct.unpack(">H", payload[:2])
        channel = payload[2 : 2 + channel_len].decode()
        return channel, payload[2 + channel_len :]

    def send_binary(self, channel: str, data: bytes):
        self.proc.stdin.write(encode_binary(channel, data))
        self.proc.stdin.flush()

    def read_binary(self, channel: str, timeout: float = 60.0) -> tuple[str, bytes]:
        """Next binary frame for `channel` (buffered or freshly read);
        progress-event JSON frames in between are skipped."""
        deadline = time.time() + timeout
        while True:
            for index, (frame_channel, data) in enumerate(self.binaries):
                if frame_channel == channel:
                    return self.binaries.pop(index)
            kind, payload = self.read_frame_raw(deadline)
            if kind == FRAME_KIND_BINARY:
                frame_channel, data = self._split_binary(payload)
                if frame_channel == channel:
                    return frame_channel, data
                self.binaries.append((frame_channel, data))
                continue
            if kind == FRAME_KIND_JSON:
                continue  # files/transfer/progress event
            raise RuntimeError(f"unexpected frame kind {kind}")

    def call(self, method: str, params: dict):
        rid = self.next_id
        self.next_id += 1
        self.proc.stdin.write(
            encode_frame({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        )
        self.proc.stdin.flush()
        while True:
            msg = self.read_frame()
            if msg.get("id") == rid:
                if "error" in msg:
                    return None, msg["error"]
                return msg.get("result"), None

    def close(self):
        self.proc.stdin.close()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def lifecycle_params(conn_id: str, external: dict, secrets: dict | None = None) -> dict:
    connection = {"id": conn_id, "name": conn_id, "external_config": external}
    if secrets:
        connection["connection_secrets"] = secrets
    return {"provider": "io.dbx.files.connection", "connection": connection}


class McpStdio:
    """One `bin --mcp` session over newline-delimited JSON-RPC (Phase D MCP
    check): rclone-engine mode, isolated plugin data dir, inline local
    connection."""

    def __init__(self, binary: str, data_dir: str):
        env = dict(os.environ)
        env["DBX_FILES_ENGINE"] = "rclone"
        env["DBX_PLUGIN_DATA_DIR"] = data_dir
        self.proc = subprocess.Popen(
            [binary, "--mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=env,
        )
        self.buf = b""
        self.next_id = 1

    def _read_line(self, deadline: float) -> bytes:
        while b"\n" not in self.buf:
            remaining = deadline - time.time()
            if remaining <= 0:
                raise RuntimeError("timed out waiting for the MCP stdio reply")
            ready, _, _ = select.select([self.proc.stdout], [], [], remaining)
            if not ready:
                continue
            chunk = os.read(self.proc.stdout.fileno(), 65536)
            if not chunk:
                raise RuntimeError("MCP stdio server closed stdout")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return line

    def call(self, method: str, params: dict, timeout: float = 90.0):
        rid = self.next_id
        self.next_id += 1
        request = json.dumps({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        self.proc.stdin.write(request.encode() + b"\n")
        self.proc.stdin.flush()
        deadline = time.time() + timeout
        while True:
            line = self._read_line(deadline)
            msg = json.loads(line)
            if msg.get("id") == rid:
                if "error" in msg:
                    return None, msg["error"]
                return msg.get("result"), None

    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def unwrap_envelope(result: dict | None) -> dict:
    """MCP content envelope → tool payload (content[0].text is JSON)."""
    content = (result or {}).get("content", [])
    if (result or {}).get("isError"):
        return {"__isError__": True, "text": content[0].get("text", "") if content else ""}
    return json.loads(content[0]["text"]) if content else {}


def run(binary: str, engine: str | None) -> bool:
    label = engine or "default(opendal)"
    print(f"== smoke: engine={label} ==")
    sidecar = Sidecar(binary, engine)
    tmp = tempfile.mkdtemp(prefix="dbx-rclone-smoke-")
    try:
        # Fixtures: one file, one subdir with a file, one empty dir.
        os.makedirs(os.path.join(tmp, "sub"))
        os.makedirs(os.path.join(tmp, "empty"))
        with open(os.path.join(tmp, "hello.txt"), "w") as f:
            f.write("hello rclone")
        with open(os.path.join(tmp, "sub", "nested.txt"), "w") as f:
            f.write("nested")

        result, error = sidecar.call(
            "plugin/initialize", {"host": {"protocolVersions": [1]}}
        )
        check("initialize", result is not None and result.get("protocolVersion") == 1, str(error))
        check(
            "initialize plugin identity",
            result and result.get("plugin", {}).get("id") == "io.dbx.files",
            str(result),
        )

        conn = lifecycle_params(
            "smokefs1", {"protocol": "fs", "root": tmp, "read_only": False}
        )

        result, error = sidecar.call("connection/test", conn)
        check("connection/test", result is not None, str(error))

        result, error = sidecar.call("connection/connect", conn)
        check("connection/connect", result == {"success": True}, str(error))

        result, error = sidecar.call(
            "files/list", {"connectionId": "smokefs1", "path": "/"}
        )
        names = {e["name"] for e in (result or {}).get("entries", [])}
        check(
            "files/list",
            error is None and {"hello.txt", "sub", "empty"} <= names,
            f"error={error} names={sorted(names)}",
        )
        kinds = {e["name"]: e["kind"] for e in (result or {}).get("entries", [])}
        check(
            "files/list kinds",
            kinds.get("sub") == "dir" and kinds.get("hello.txt") == "file",
            str(kinds),
        )

        result, error = sidecar.call(
            "files/list", {"connectionId": "smokefs1", "path": "/", "recurse": True}
        )
        paths = {e["path"] for e in (result or {}).get("entries", [])}
        check(
            "files/list recurse",
            any("nested.txt" in p for p in paths),
            f"error={error} paths={sorted(paths)}",
        )

        result, error = sidecar.call(
            "files/listPaged",
            {"connectionId": "smokefs1", "path": "/", "page": 1, "pageSize": 2},
        )
        total = (result or {}).get("total")
        paged = (result or {}).get("entries", [])
        check(
            "files/listPaged",
            error is None and total == 3 and len(paged) == 2,
            f"error={error} total={total} len={len(paged)}",
        )

        result, error = sidecar.call(
            "files/stat", {"connectionId": "smokefs1", "path": "/hello.txt"}
        )
        entry = (result or {}).get("entry", {})
        check(
            "files/stat",
            error is None and entry.get("kind") == "file" and entry.get("size") == 12,
            f"error={error} entry={entry}",
        )

        result, error = sidecar.call(
            "files/stat", {"connectionId": "smokefs1", "path": "/missing.txt"}
        )
        check("files/stat missing → error", result is None and error, f"result={result}")

        result, error = sidecar.call(
            "files/capabilities", {"connectionId": "smokefs1"}
        )
        caps = result or {}
        check(
            "files/capabilities",
            error is None
            and all(caps.get(k) is True for k in ("list", "read", "write", "stat", "delete", "createDir"))
            and caps.get("readOnly") is False,
            f"error={error} caps={caps}",
        )

        result, error = sidecar.call(
            "files/size", {"connectionId": "smokefs1", "path": "/"}
        )
        # rclone operations/size counts files only (2 files / 18 bytes here);
        # the OpenDAL engine may include directories — frontend-visible
        # semantic difference, tracked in IMPL_PLAN_RCLONE.zh-CN.md §5.
        check(
            "files/size",
            error is None and (result or {}).get("count") == 2 and (result or {}).get("bytes") == 18,
            f"error={error} result={result}",
        )

        result, error = sidecar.call("files/quickPaths", {"connectionId": "smokefs1"})
        check(
            "files/quickPaths",
            error is None and isinstance((result or {}).get("paths"), list) and result["paths"],
            f"error={error} result={result}",
        )

        # Cross-engine fallback reality (rclone pass only): the two engines
        # keep SEPARATE connection tables, so methods not yet ported to the
        # rclone engine cannot fall through for an rclone-registered
        # connection — a documented transition limitation
        # (IMPL_PLAN_RCLONE.zh-CN.md §10), guarded here as a regression check.
        # files/syncDir|copyDir are the Phase C surface and are wired to rclone
        # sync jobs below (the Phase B "unported method" guard is superseded);
        # file methods (mkdir/read/write/...) are ported and exercised below.
        if engine == "rclone":
            # ------------------------------------------------------------------
            # Phase C dir-sync surface (IMPL_PLAN_RCLONE.zh-CN.md §5/§6):
            # copyDir copies for real, dryRun syncDir proves nothing is
            # written/deleted, a real syncDir mirrors and deletes stale target
            # extras, and the unified transfers panel lists the jobs with
            # terminal states. Cancellation is covered by F1's live-process
            # unit tests per the Phase C contract, not here.
            # ------------------------------------------------------------------
            os.makedirs(os.path.join(tmp, "sync-src", "sub"), exist_ok=True)
            with open(os.path.join(tmp, "sync-src", "a.txt"), "w") as f:
                f.write("alpha")
            with open(os.path.join(tmp, "sync-src", "sub", "b.txt"), "w") as f:
                f.write("bravo")

            def wait_dir_job(job_id: str, timeout: float = 30.0):
                """Polls files/transfers/list until the dir job is terminal."""
                deadline = time.time() + timeout
                entry = None
                while time.time() < deadline:
                    result, _ = sidecar.call("files/transfers/list", {})
                    jobs = (result or {}).get("jobs", [])
                    entry = next(
                        (j for j in jobs if j.get("jobId") == job_id), None
                    )
                    if entry and entry.get("status") in (
                        "completed",
                        "failed",
                        "canceled",
                    ):
                        return entry, None
                    time.sleep(0.2)
                return entry, "timed out waiting for a terminal state"

            # 1) files/copyDir: real recursive copy, pollable jobId, and the
            # unified transfers/list view carrying the job to its terminal.
            result, error = sidecar.call(
                "files/copyDir",
                {
                    "sourceConnectionId": "smokefs1",
                    "sourcePath": "/sync-src",
                    "targetConnectionId": "smokefs1",
                    "targetPath": "/copy-target",
                },
            )
            copy_job = (result or {}).get("jobId")
            check(
                "files/copyDir returns jobId",
                error is None and bool(copy_job),
                f"error={error} result={result}",
            )

            entry, wait_error = wait_dir_job(copy_job)
            check(
                "copyDir reaches completed in transfers/list",
                entry is not None
                and entry.get("status") == "completed"
                and entry.get("kind") == "copyDir",
                f"entry={entry} wait_error={wait_error}",
            )
            result, error = sidecar.call(
                "files/read",
                {
                    "connectionId": "smokefs1",
                    "path": "/copy-target/sub/b.txt",
                    "maxBytes": 16,
                },
            )
            data = base64.b64decode((result or {}).get("dataBase64", ""))
            check(
                "copyDir target content",
                data == b"bravo",
                f"error={error} data={data!r}",
            )

            # files/transfer/status answers the dir job with the dirJob shape.
            result, error = sidecar.call(
                "files/transfer/status", {"jobId": copy_job}
            )
            check(
                "transfer/status dirJob shape",
                (result or {}).get("kind") == "dirJob"
                and (result or {}).get("job", {}).get("jobId") == copy_job,
                f"result={result} error={error}",
            )

            # 2a) files/syncDir dryRun against a fresh target: nothing is
            # created on disk (no mkdir, no writes).
            result, error = sidecar.call(
                "files/syncDir",
                {
                    "sourceConnectionId": "smokefs1",
                    "sourcePath": "/sync-src",
                    "targetConnectionId": "smokefs1",
                    "targetPath": "/sync-dry-target",
                    "dryRun": True,
                },
            )
            dry_job = (result or {}).get("jobId")
            check(
                "files/syncDir dryRun returns jobId",
                error is None and bool(dry_job),
                f"error={error} result={result}",
            )
            entry, wait_error = wait_dir_job(dry_job)
            check(
                "syncDir dryRun completes",
                entry is not None
                and entry.get("status") == "completed"
                and entry.get("kind") == "syncDir"
                and entry.get("dryRun") is True,
                f"entry={entry} wait_error={wait_error}",
            )
            result, error = sidecar.call(
                "files/stat",
                {"connectionId": "smokefs1", "path": "/sync-dry-target"},
            )
            check(
                "syncDir dryRun writes nothing",
                result is None and error,
                f"result={result}",
            )

            # 2b) dryRun against an existing target with a stale extra: the
            # extra survives (no deletes in dry-run).
            with open(
                os.path.join(tmp, "copy-target", "extra.txt"), "w"
            ) as f:
                f.write("stale")
            result, error = sidecar.call(
                "files/syncDir",
                {
                    "sourceConnectionId": "smokefs1",
                    "sourcePath": "/sync-src",
                    "targetConnectionId": "smokefs1",
                    "targetPath": "/copy-target",
                    "dryRun": True,
                },
            )
            dry_job = (result or {}).get("jobId")
            check(
                "files/syncDir dryRun (existing target) returns jobId",
                error is None and bool(dry_job),
                f"error={error} result={result}",
            )
            entry, wait_error = wait_dir_job(dry_job)
            check(
                "syncDir dryRun (existing target) completes",
                entry is not None and entry.get("status") == "completed",
                f"entry={entry} wait_error={wait_error}",
            )
            result, error = sidecar.call(
                "files/stat",
                {"connectionId": "smokefs1", "path": "/copy-target/extra.txt"},
            )
            check(
                "syncDir dryRun deletes nothing",
                (result or {}).get("entry", {}).get("kind") == "file",
                f"error={error} result={result}",
            )

            # 3) real files/syncDir: mirrors the source and deletes the stale
            # target extra (rclone sync deleteMode semantics).
            result, error = sidecar.call(
                "files/syncDir",
                {
                    "sourceConnectionId": "smokefs1",
                    "sourcePath": "/sync-src",
                    "targetConnectionId": "smokefs1",
                    "targetPath": "/copy-target",
                },
            )
            sync_job = (result or {}).get("jobId")
            check(
                "files/syncDir returns jobId",
                error is None and bool(sync_job),
                f"error={error} result={result}",
            )
            entry, wait_error = wait_dir_job(sync_job)
            check(
                "syncDir reaches completed",
                entry is not None and entry.get("status") == "completed",
                f"entry={entry} wait_error={wait_error}",
            )
            result, error = sidecar.call(
                "files/stat",
                {"connectionId": "smokefs1", "path": "/copy-target/extra.txt"},
            )
            check(
                "syncDir deletes stale target extra",
                result is None and error,
                f"result={result}",
            )
            result, error = sidecar.call(
                "files/read",
                {
                    "connectionId": "smokefs1",
                    "path": "/copy-target/sub/b.txt",
                    "maxBytes": 16,
                },
            )
            data = base64.b64decode((result or {}).get("dataBase64", ""))
            check(
                "syncDir target mirrored",
                data == b"bravo",
                f"error={error} data={data!r}",
            )

            # Unsupported protocol surfaces an actionable message.
            result, error = sidecar.call(
                "connection/test",
                lifecycle_params("smokebad", {"protocol": "aliyun-drive"}),
            )
            check(
                "aliyun-drive → explicit error",
                result is None and error and "not supported by rclone upstream" in json.dumps(error),
                f"result={result} error={error}",
            )

            # ------------------------------------------------------------------
            # Phase D archive surface (rclone round): zip archiveList via
            # rc-serve Range reads (central directory at the file tail),
            # extract, compress (zip + tar.gz) and byte-level read-back. The
            # OpenDAL engine serves tar/tar.gz only (zip is deferred there),
            # so this section is rclone-round-only by design.
            # ------------------------------------------------------------------
            archive_bin = bytes(range(256)) * 64  # 16 KiB, binary
            os.makedirs(os.path.join(tmp, "arch-src", "sub"), exist_ok=True)
            with open(os.path.join(tmp, "arch-src", "a.txt"), "w") as f:
                f.write("alpha")
            with open(os.path.join(tmp, "arch-src", "sub", "b.bin"), "wb") as f:
                f.write(archive_bin)
            zip_buffer = io.BytesIO()
            with zipfile.ZipFile(zip_buffer, "w", zipfile.ZIP_DEFLATED) as zf:
                zf.writestr("pkg/a.txt", "alpha")
                zf.writestr("pkg/sub/b.bin", archive_bin)
            zip_bytes = zip_buffer.getvalue()

            result, error = sidecar.call(
                "files/write",
                {
                    "connectionId": "smokefs1",
                    "path": "/made.zip",
                    "dataBase64": base64.b64encode(zip_bytes).decode(),
                },
            )
            check("archive zip upload", result == {"success": True}, f"error={error}")

            result, error = sidecar.call(
                "files/archiveList",
                {
                    "connectionId": "smokefs1",
                    "path": "/made.zip",
                    "page": 1,
                    "pageSize": 10,
                },
            )
            entries = (result or {}).get("entries", [])
            paths = [e.get("path") for e in entries]
            check(
                "files/archiveList zip (Range central directory)",
                error is None
                and (result or {}).get("total") == 2
                and paths == ["pkg/a.txt", "pkg/sub/b.bin"]
                and entries[0].get("kind") == "file"
                and entries[0].get("size") == 5,
                f"error={error} entries={entries}",
            )

            result, error = sidecar.call(
                "files/extract",
                {"connectionId": "smokefs1", "path": "/made.zip", "targetPath": "/arch-out"},
            )
            check(
                "files/extract zip",
                (result or {}).get("success") is True
                and (result or {}).get("transport") == "native",
                f"error={error} result={result}",
            )
            result, error = sidecar.call(
                "files/read",
                {"connectionId": "smokefs1", "path": "/arch-out/pkg/a.txt", "maxBytes": 16},
            )
            data = base64.b64decode((result or {}).get("dataBase64", ""))
            check("extract content (text)", data == b"alpha", f"error={error} data={data!r}")
            result, error = sidecar.call(
                "files/read",
                {
                    "connectionId": "smokefs1",
                    "path": "/arch-out/pkg/sub/b.bin",
                    "maxBytes": len(archive_bin),
                },
            )
            data = base64.b64decode((result or {}).get("dataBase64", ""))
            check(
                "extract content (binary)",
                data == archive_bin and (result or {}).get("truncated") is False,
                f"error={error} len={len(data)}",
            )

            # compress: sources → .zip (stored entries) and → .tar.gz (the
            # shared pure tar parsers); then read the zip back through
            # archiveList + extract and compare bytes.
            result, error = sidecar.call(
                "files/compress",
                {
                    "connectionId": "smokefs1",
                    "paths": ["/arch-out"],
                    "targetPath": "/packed.zip",
                },
            )
            check(
                "files/compress → zip",
                (result or {}).get("success") is True
                and (result or {}).get("transport") == "native",
                f"error={error} result={result}",
            )
            result, error = sidecar.call(
                "files/archiveList",
                {"connectionId": "smokefs1", "path": "/packed.zip"},
            )
            paths = [e.get("path") for e in (result or {}).get("entries", [])]
            check(
                "compress zip archiveList",
                paths == ["arch-out/pkg/a.txt", "arch-out/pkg/sub/b.bin"],
                f"error={error} paths={paths}",
            )
            result, error = sidecar.call(
                "files/extract",
                {"connectionId": "smokefs1", "path": "/packed.zip", "targetPath": "/packed-out"},
            )
            check("compress zip extract", (result or {}).get("success") is True, f"error={error}")
            result, error = sidecar.call(
                "files/read",
                {
                    "connectionId": "smokefs1",
                    "path": "/packed-out/arch-out/pkg/sub/b.bin",
                    "maxBytes": len(archive_bin),
                },
            )
            data = base64.b64decode((result or {}).get("dataBase64", ""))
            check("compress zip round-trip bytes", data == archive_bin, f"len={len(data)}")

            result, error = sidecar.call(
                "files/compress",
                {
                    "connectionId": "smokefs1",
                    "paths": ["/arch-out"],
                    "targetPath": "/packed.tar.gz",
                },
            )
            check(
                "files/compress → tar.gz",
                (result or {}).get("success") is True,
                f"error={error} result={result}",
            )
            result, error = sidecar.call(
                "files/archiveList",
                {"connectionId": "smokefs1", "path": "/packed.tar.gz"},
            )
            paths = [e.get("path") for e in (result or {}).get("entries", [])]
            check(
                "compress tar.gz archiveList",
                "arch-out/pkg/a.txt" in paths,
                f"error={error} paths={paths}",
            )

            # Guards: overwrite refusal and unknown suffix.
            result, error = sidecar.call(
                "files/compress",
                {
                    "connectionId": "smokefs1",
                    "paths": ["/arch-out"],
                    "targetPath": "/packed.zip",
                },
            )
            check(
                "compress overwrite refused",
                result is None and error and "already exists" in json.dumps(error),
                f"result={result} error={error}",
            )
            result, error = sidecar.call(
                "files/compress",
                {
                    "connectionId": "smokefs1",
                    "paths": ["/arch-out"],
                    "targetPath": "/packed.rar",
                },
            )
            check(
                "compress bad suffix refused",
                result is None and error and "must end with" in json.dumps(error),
                f"result={result} error={error}",
            )

            # ------------------------------------------------------------------
            # Phase D MCP check: `bin --mcp` in rclone mode over newline
            # JSON-RPC — initialize, tools/list, files_scan_digest (inline
            # local connection) and files_cursor_next paging.
            # ------------------------------------------------------------------
            mcp_root = tempfile.mkdtemp(prefix="dbx-rclone-mcp-")
            mcp_data = tempfile.mkdtemp(prefix="dbx-rclone-mcp-data-")
            with open(os.path.join(mcp_root, "a.txt"), "w") as f:
                f.write("12345")
            os.makedirs(os.path.join(mcp_root, "sub"), exist_ok=True)
            with open(os.path.join(mcp_root, "sub", "b.txt"), "w") as f:
                f.write("1234567")
            mcp = None
            try:
                mcp = McpStdio(binary, mcp_data)
                result, error = mcp.call(
                    "initialize", {"protocolVersion": "2024-11-05", "capabilities": {}}
                )
                check(
                    "mcp initialize",
                    (result or {}).get("protocolVersion") == "2024-11-05",
                    f"error={error} result={result}",
                )
                result, error = mcp.call("tools/list", {})
                names = {
                    t.get("name") for t in (result or {}).get("tools", [])
                }
                check(
                    "mcp tools/list",
                    {"files_scan_digest", "files_cursor_next"} <= names,
                    f"error={error} names={sorted(names)}",
                )
                result, error = mcp.call(
                    "tools/call",
                    {
                        "name": "files_scan_digest",
                        "arguments": {
                            "connection": {"protocol": "local", "root": mcp_root},
                            "path": "/",
                        },
                    },
                )
                payload = unwrap_envelope(result)
                check(
                    "mcp files_scan_digest (rclone)",
                    error is None
                    and payload.get("matched") == 3
                    and payload.get("scanned") == 3
                    and bool(payload.get("cursorId"))
                    and payload.get("stats", {}).get("totalBytes") == 12
                    and len(payload.get("sample", [])) == 3,
                    f"error={error} payload={payload}",
                )
                cursor_id = payload.get("cursorId", "")
                result, error = mcp.call(
                    "tools/call",
                    {
                        "name": "files_cursor_next",
                        "arguments": {"cursorId": cursor_id, "n": 2},
                    },
                )
                payload = unwrap_envelope(result)
                check(
                    "mcp files_cursor_next page 1",
                    len(payload.get("rows", [])) == 2 and payload.get("done") is False,
                    f"error={error} payload={payload}",
                )
                result, error = mcp.call(
                    "tools/call",
                    {
                        "name": "files_cursor_next",
                        "arguments": {"cursorId": cursor_id},
                    },
                )
                payload = unwrap_envelope(result)
                check(
                    "mcp files_cursor_next page 2 (done)",
                    len(payload.get("rows", [])) == 1 and payload.get("done") is True,
                    f"error={error} payload={payload}",
                )
            except Exception as exc:  # noqa: BLE001 — smoke reports via check()
                check("mcp stdio session", False, f"exception: {exc}")
            finally:
                if mcp is not None:
                    mcp.close()
                shutil.rmtree(mcp_root, ignore_errors=True)
                shutil.rmtree(mcp_data, ignore_errors=True)

        # ------------------------------------------------------------------
        # Phase B file surface (IMPL_PLAN_RCLONE.zh-CN.md §5): both engines
        # must answer with the same shapes, so this section is engine-blind.
        # ------------------------------------------------------------------

        # files/write (inline ≤4MiB) + files/read (default 256KiB truncation).
        payload = bytes((i * 31 + 7) % 256 for i in range(300_000))
        result, error = sidecar.call(
            "files/write",
            {
                "connectionId": "smokefs1",
                "path": "/big.bin",
                "dataBase64": base64.b64encode(payload).decode(),
            },
        )
        check("files/write inline", result == {"success": True}, f"error={error}")

        result, error = sidecar.call(
            "files/read", {"connectionId": "smokefs1", "path": "/big.bin"}
        )
        data = base64.b64decode((result or {}).get("dataBase64", ""))
        check(
            "files/read default truncates at 256KiB",
            data == payload[:TRANSFER_CHUNK_SIZE]
            and (result or {}).get("truncated") is True,
            f"error={error} len={len(data)} truncated={(result or {}).get('truncated')}",
        )

        result, error = sidecar.call(
            "files/read",
            {"connectionId": "smokefs1", "path": "/big.bin", "maxBytes": 1024},
        )
        data = base64.b64decode((result or {}).get("dataBase64", ""))
        check(
            "files/read maxBytes window",
            data == payload[:1024] and (result or {}).get("truncated") is True,
            f"error={error} len={len(data)}",
        )

        result, error = sidecar.call(
            "files/read",
            {"connectionId": "smokefs1", "path": "/big.bin", "maxBytes": 400_000},
        )
        data = base64.b64decode((result or {}).get("dataBase64", ""))
        check(
            "files/read full file not truncated",
            data == payload and (result or {}).get("truncated") is False,
            f"error={error} len={len(data)}",
        )

        # files/mkdir (then a file inside for the later purge).
        result, error = sidecar.call(
            "files/mkdir", {"connectionId": "smokefs1", "path": "/dir1"}
        )
        check("files/mkdir", result == {"success": True}, f"error={error}")
        result, error = sidecar.call(
            "files/stat", {"connectionId": "smokefs1", "path": "/dir1"}
        )
        check(
            "files/mkdir stat readback",
            (result or {}).get("entry", {}).get("kind") == "dir",
            f"error={error} result={result}",
        )
        result, error = sidecar.call(
            "files/write",
            {
                "connectionId": "smokefs1",
                "path": "/dir1/inner.txt",
                "dataBase64": base64.b64encode(b"inner").decode(),
            },
        )
        check("files/write under dir", result == {"success": True}, f"error={error}")

        # files/copy / files/move / files/rename (response carries
        # success/transport/jobId on both engines).
        result, error = sidecar.call(
            "files/copy",
            {
                "connectionId": "smokefs1",
                "sourcePath": "/hello.txt",
                "targetPath": "/copy-dst.txt",
            },
        )
        check(
            "files/copy",
            (result or {}).get("success") is True,
            f"error={error} result={result}",
        )
        result, error = sidecar.call(
            "files/stat", {"connectionId": "smokefs1", "path": "/copy-dst.txt"}
        )
        check(
            "files/copy stat readback",
            (result or {}).get("entry", {}).get("size") == 12,
            f"error={error} result={result}",
        )

        result, error = sidecar.call(
            "files/move",
            {
                "connectionId": "smokefs1",
                "sourcePath": "/copy-dst.txt",
                "targetPath": "/copy-moved.txt",
            },
        )
        check(
            "files/move",
            (result or {}).get("success") is True,
            f"error={error} result={result}",
        )
        result, error = sidecar.call(
            "files/stat", {"connectionId": "smokefs1", "path": "/copy-dst.txt"}
        )
        check("files/move source gone", result is None and error, f"result={result}")

        result, error = sidecar.call(
            "files/rename",
            {
                "connectionId": "smokefs1",
                "path": "/copy-moved.txt",
                "newPath": "/copy-renamed.txt",
            },
        )
        check(
            "files/rename",
            (result or {}).get("success") is True,
            f"error={error} result={result}",
        )
        result, error = sidecar.call(
            "files/stat", {"connectionId": "smokefs1", "path": "/copy-renamed.txt"}
        )
        check(
            "files/rename stat readback",
            (result or {}).get("entry", {}).get("kind") == "file",
            f"error={error} result={result}",
        )

        # files/delete + files/purge (with the root red line).
        result, error = sidecar.call(
            "files/delete", {"connectionId": "smokefs1", "path": "/copy-renamed.txt"}
        )
        check("files/delete", result == {"success": True}, f"error={error}")
        result, error = sidecar.call(
            "files/stat", {"connectionId": "smokefs1", "path": "/copy-renamed.txt"}
        )
        check("files/delete stat gone", result is None and error, f"result={result}")

        result, error = sidecar.call(
            "files/purge", {"connectionId": "smokefs1", "path": "/dir1"}
        )
        check("files/purge", result == {"success": True}, f"error={error}")
        result, error = sidecar.call(
            "files/stat", {"connectionId": "smokefs1", "path": "/dir1"}
        )
        check("files/purge dir gone", result is None and error, f"result={result}")

        result, error = sidecar.call(
            "files/purge", {"connectionId": "smokefs1", "path": "/"}
        )
        check(
            "files/purge root refused",
            result is None and error and "refused" in json.dumps(error),
            f"result={result} error={error}",
        )

        # files/publicLink: local filesystem has no presign on either engine.
        result, error = sidecar.call(
            "files/publicLink", {"connectionId": "smokefs1", "path": "/hello.txt"}
        )
        check(
            "files/publicLink local → error",
            result is None and error,
            f"result={result} error={error}",
        )

        # Binary upload channel (§7): start → kind-1 frames (2B channel len +
        # channel + 8B BE offset + data) → finish → size/content readback.
        up_payload = bytes((i * 7) % 256 for i in range(700_000))
        result, error = sidecar.call(
            "files/upload/start",
            {
                "connectionId": "smokefs1",
                "remotePath": "/uploaded.bin",
                "size": len(up_payload),
            },
        )
        task_id = (result or {}).get("taskId")
        check("files/upload/start", bool(task_id), f"error={error} result={result}")

        offset = 0
        for slice_ in (
            up_payload[o : o + TRANSFER_CHUNK_SIZE]
            for o in range(0, len(up_payload), TRANSFER_CHUNK_SIZE)
        ):
            sidecar.send_binary(
                f"files/upload/{task_id}", struct.pack(">Q", offset) + slice_
            )
            offset += len(slice_)
            # Binary dispatch runs on the sidecar's worker pool; a short gap
            # keeps frame order deterministic for the strict offset check.
            time.sleep(0.05)
        result, error = sidecar.call(
            "files/upload/finish", {"taskId": task_id}
        )
        check("files/upload/finish", result == {"success": True}, f"error={error}")

        result, error = sidecar.call(
            "files/stat", {"connectionId": "smokefs1", "path": "/uploaded.bin"}
        )
        check(
            "upload lands at exact remote path and size",
            (result or {}).get("entry", {}).get("size") == len(up_payload),
            f"error={error} result={result}",
        )
        result, error = sidecar.call(
            "files/read",
            {
                "connectionId": "smokefs1",
                "path": "/uploaded.bin",
                "maxBytes": len(up_payload),
            },
        )
        data = base64.b64decode((result or {}).get("dataBase64", ""))
        check(
            "upload content round-trip",
            data == up_payload and (result or {}).get("truncated") is False,
            f"error={error} len={len(data)}",
        )

        # Binary download channel: start → collect kind-1 frames → finish,
        # then verify the reassembled bytes match the upload byte for byte.
        result, error = sidecar.call(
            "files/download/start",
            {"connectionId": "smokefs1", "remotePath": "/uploaded.bin"},
        )
        dl_task = (result or {}).get("taskId")
        dl_size = (result or {}).get("size")
        check(
            "files/download/start",
            bool(dl_task) and dl_size == len(up_payload),
            f"error={error} result={result}",
        )
        try:
            chunks: dict[int, bytes] = {}
            received = 0
            max_slice = 0
            while received < dl_size:
                channel, frame = sidecar.read_binary(
                    f"files/download/{dl_task}", timeout=30
                )
                (frame_offset,) = struct.unpack(">Q", frame[:8])
                body = frame[8:]
                max_slice = max(max_slice, len(body))
                chunks[frame_offset] = body
                received += len(body)
            assembled = b"".join(chunks[o] for o in sorted(chunks))
            check(
                "download frames contiguous ≤256KiB",
                received == dl_size
                and max_slice <= TRANSFER_CHUNK_SIZE
                and assembled == up_payload,
                f"received={received} max_slice={max_slice}",
            )
        except RuntimeError as exc:
            check("download frames contiguous ≤256KiB", False, str(exc))
        result, error = sidecar.call(
            "files/download/finish", {"taskId": dl_task}
        )
        check(
            "files/download/finish",
            (result or {}).get("success") is True
            and (result or {}).get("taskId") == dl_task,
            f"error={error} result={result}",
        )

        result, error = sidecar.call(
            "connection/disconnect", {"connection": {"id": "smokefs1"}}
        )
        check("connection/disconnect", result == {"success": True}, str(error))

        # Unconnected id must be rejected (both engines answer this one from
        # their own tables).
        result, error = sidecar.call(
            "files/list", {"connectionId": "ghost", "path": "/"}
        )
        check("list on unconnected id → error", result is None and error, f"result={result}")
    finally:
        sidecar.close()
        shutil.rmtree(tmp, ignore_errors=True)

    print(f"== {label}: {len(PASS)} passed, {len(FAIL)} failed ==")
    return not FAIL


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    binary = sys.argv[1]
    engine = None
    if "--engine" in sys.argv:
        engine = sys.argv[sys.argv.index("--engine") + 1]
    ok = run(binary, engine)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
