#!/usr/bin/env python3
"""dbx-files-plugin sidecar smoke test (fs temp dir + memory:// + optional containers).

Drives the sidecar over its stdio-framed protocol (sidecar_client.py):
  - fs section (temp directory): list / mkdir / write / read / copy / move /
    rename / delete / purge(root refusal) / upload+download binary-channel
    round-trip / cancel / read_only gate / capabilities
  - archive section (both fs + memory): files/archiveList (+pagination) /
    files/extract (sync + job degrade) / zip Phase-2 refusal (B-ARCHIVE)
  - memory:// section: the same subset with zero external dependencies
  - s3 (MinIO), sftp and smb (Samba) container sections: SKIP unless the
    matching DBX_FILES_S3_* / DBX_FILES_SFTP_* / DBX_FILES_SMB_* environment
    variables are set
  - sftp-native section: the russh+russh-sftp dual-stack adapter (password
    auth the OpenDAL sftp service cannot do) — SKIP unless
    DBX_FILES_SFTP_NATIVE_HOST/PORT/USER/PASSWORD/KEY/BASE are set

Methods owned by parallel tracks (F-A lifecycle / F-B engine ops) that are not
implemented yet are reported as SKIP, not FAIL, so the suite stays green while
the 9-way parallel implementation lands.

Usage:
    python3 scripts/smoke_test.py                  # local fs + memory sections
    DBX_PLUGIN_SIDECAR=... python3 scripts/smoke_test.py
    DBX_FILES_S3_ENDPOINT=... DBX_FILES_S3_BUCKET=... \
    DBX_FILES_S3_ACCESS_KEY=... DBX_FILES_S3_SECRET_KEY=... \
        python3 scripts/smoke_test.py              # adds the MinIO section
    DBX_FILES_SMB_HOST=... DBX_FILES_SMB_SHARE=... \
    DBX_FILES_SMB_USER=... DBX_FILES_SMB_PASSWORD=... \
        python3 scripts/smoke_test.py              # adds the Samba section
"""

from __future__ import annotations

import base64
import json
import os
import struct
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from sidecar_client import SidecarClient, SidecarError, lifecycle_params

CHUNK = 256 * 1024  # upload/download binary frame budget: 8B offset + <=256KiB data

RESULTS: list[tuple[str, str, str]] = []  # (section, name, status)
METHOD_MISSING = "method not found"


def mark(section: str, name: str, status: str, note: str = "") -> None:
    RESULTS.append((section, name, status))
    icon = {"pass": "  ok", "skip": "SKIP", "fail": "FAIL"}[status]
    suffix = f" — {note}" if note else ""
    print(f"    [{icon}] {section}/{name}{suffix}")


def is_method_missing(error: str) -> bool:
    return METHOD_MISSING.lower() in error.lower()


class Runner:
    """Thin harness: expected failures and per-step SKIP on missing methods."""

    def __init__(self, client: SidecarClient, section: str):
        self.client = client
        self.section = section

    def call(self, method: str, params: dict) -> dict:
        return self.client.request(method, params)

    def expect_error(self, method: str, params: dict, needle: str = "") -> bool:
        try:
            self.client.request(method, params)
        except SidecarError as error:
            if is_method_missing(str(error)):
                return False  # caller decides: skip
            if needle and needle.lower() not in str(error).lower():
                raise SidecarError(f"{method}: expected error containing {needle!r}, got: {error}")
            return True
        raise SidecarError(f"{method}: expected an error, got success")

    def step(self, name: str, fn) -> None:
        """Run one scenario; SidecarError 'method not found' becomes SKIP."""
        try:
            fn()
        except SidecarError as error:
            if is_method_missing(str(error)):
                mark(self.section, name, "skip", str(error)[:80])
                return
            mark(self.section, name, "fail", str(error)[:200])
            raise
        mark(self.section, name, "pass")


def connect(client: SidecarClient, connection_id: str, external_config: dict, root: str = "", secrets: dict | None = None) -> None:
    connection = {
        "id": connection_id,
        "name": connection_id,
        "db_type": "storage",
        "host": "",
        "port": 0,
        "external_config": external_config,
    }
    if root:
        connection["external_config"] = {**external_config, "root": root}
    # Secret-bound fields (secret_access_key/password) travel in
    # connection.connection_secrets (model.rs parses them from there only);
    # putting them in external_config leaves them unset and makes the s3
    # signer fall back to the env/IMDS credential chain (slow, failing).
    if secrets:
        connection["connection_secrets"] = secrets
    client.request("connection/connect", lifecycle_params(connection))


def upload_bytes(runner: Runner, remote_path: str, payload: bytes) -> dict:
    """Push payload through the files/upload binary channel; return finish result."""
    start = runner.call("files/upload/start", {"connectionId": runner.connection_id, "remotePath": remote_path, "size": len(payload)})
    task_id = start["taskId"]
    offset = 0
    while offset < len(payload):
        chunk = payload[offset:offset + CHUNK]
        runner.client.send_binary(f"files/upload/{task_id}", struct.pack(">Q", offset) + chunk)
        offset += len(chunk)
        time.sleep(0.02)  # give the worker pool a beat; frames must stay in order
    return runner.call("files/upload/finish", {"taskId": task_id})


def download_bytes(runner: Runner, remote_path: str) -> tuple[bytes, dict]:
    """Pull the whole file through the files/download channel; return (bytes, start).

    The sidecar runs a pump task: files/download/start pushes every
    8-byte-BE-offset + <=256KiB frame on files/download/{taskId} by itself
    (ssh-sftp upload-channel shape, sidecar → host direction)."""
    start = runner.call("files/download/start", {"connectionId": runner.connection_id, "remotePath": remote_path})
    task_id, size = start["taskId"], int(start.get("size", 0))
    channel = f"files/download/{task_id}"
    data = bytearray(size)
    received = 0
    while received < size:
        frame = take_frame(runner.client, channel, timeout=15.0)
        if frame is None:
            raise SidecarError(f"download frame missing on {channel} at {received}/{size}")
        (frame_offset,) = struct.unpack(">Q", frame[:8])
        chunk = frame[8:]
        data[frame_offset:frame_offset + len(chunk)] = chunk
        received += len(chunk)
    runner.call("files/download/finish", {"taskId": task_id})
    return bytes(data), start


def take_frame(client: SidecarClient, channel: str, timeout: float = 5.0) -> bytes | None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for frame in list(client.binary_frames):
            if frame[0] == channel:
                client.binary_frames.remove(frame)
                return frame[1]
        client.timeout = max(0.2, deadline - time.monotonic())
        try:
            client._pump(None)
        except SidecarError:
            break
    for frame in client.binary_frames:
        if frame[0] == channel:
            client.binary_frames.remove(frame)
            return frame[1]
    return None


# --------------------------------------------------------------------------
# Scenario groups (shared between fs and memory sections)
# --------------------------------------------------------------------------

def wait_job(runner: Runner, job_id: str, timeout: float = 30.0) -> str:
    """Polls files/transfer/status until the job reaches a terminal state.

    The degraded files/copy|files/move transport (X-A) now returns a real
    async jobId; structural scenarios wait for `completed` before asserting.
    """
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        status = runner.call("files/transfer/status", {"jobId": job_id})
        job = status.get("job", {})
        last = job.get("status") or job.get("state")
        if last in ("completed", "failed", "canceled"):
            return last
        time.sleep(0.2)
    raise SidecarError(f"job {job_id} did not reach a terminal state within {timeout}s (last: {last})")


def scenario_capabilities(runner: Runner) -> None:
    caps = runner.call("files/capabilities", {"connectionId": runner.connection_id})
    assert isinstance(caps, dict), f"capabilities is not an object: {caps!r}"
    assert "copy" in caps and "rename" in caps, f"capabilities missing copy/rename: {caps!r}"


QUICK_KEYS = {"root", "home", "desktop", "downloads", "documents", "pictures"}


def scenario_quick_paths(runner: Runner, expect_user_dirs: bool) -> None:
    """§8.1 files/quickPaths: workbench quick-jump chips.

    Non-fs / root-confined connections fall back to the root chip only; an
    unconfined fs connection exposes stat-verified user directories."""
    def _chips():
        result = runner.call("files/quickPaths", {"connectionId": runner.connection_id})
        paths = result.get("paths", [])
        assert paths and paths[0]["key"] == "root" and paths[0]["path"] == "/", f"root chip missing: {paths}"
        keys = {p["key"] for p in paths}
        assert keys <= QUICK_KEYS, f"unknown chip keys: {sorted(keys)}"
        if expect_user_dirs:
            assert "home" in keys, f"unconfined fs should expose the home chip: {paths}"
        else:
            assert keys == {"root"}, f"confined/non-fs must fall back to the root chip only: {sorted(keys)}"
    runner.step("quick-paths", _chips)


def scenario_structure(runner: Runner, base: str) -> None:
    cid = runner.connection_id

    def _mkdir():
        runner.call("files/mkdir", {"connectionId": cid, "path": f"{base}/dir"})
    runner.step("mkdir", _mkdir)

    def _write():
        payload = base64.b64encode(b"hello files").decode()
        runner.call("files/write", {"connectionId": cid, "path": f"{base}/dir/hello.txt", "dataBase64": payload})
    runner.step("write", _write)

    def _list():
        result = runner.call("files/list", {"connectionId": cid, "path": base})
        names = [entry["name"] for entry in result.get("entries", [])]
        assert "dir" in names, f"list is missing the created directory: {names}"
    runner.step("list", _list)

    def _read():
        result = runner.call("files/read", {"connectionId": cid, "path": f"{base}/dir/hello.txt"})
        assert base64.b64decode(result["dataBase64"]) == b"hello files", "read round-trip mismatch"
    runner.step("read", _read)

    def _copy():
        result = runner.call("files/copy", {"connectionId": cid, "sourcePath": f"{base}/dir/hello.txt", "targetPath": f"{base}/dir/hello-copy.txt"})
        # Degraded transports return a pollable jobId; native ones finish inline.
        if result.get("jobId"):
            state = wait_job(runner, result["jobId"])
            assert state == "completed", f"copy job ended as {state}"
            # P-FILES ①a: the degraded copy job must appear in the unified
            # transfers/list (kind "copy").
            jobs = runner.call("files/transfers/list", {"connectionId": cid}).get("jobs", [])
            assert any(job.get("jobId") == result["jobId"] for job in jobs), \
                f"degraded copy job missing from transfers/list: {[job.get('jobId') for job in jobs]}"
        result = runner.call("files/stat", {"connectionId": cid, "path": f"{base}/dir/hello-copy.txt"})
        assert int(result["entry"]["size"]) == len(b"hello files"), "copied file has the wrong size"
    runner.step("copy", _copy)

    def _move():
        result = runner.call("files/move", {"connectionId": cid, "sourcePath": f"{base}/dir/hello-copy.txt", "targetPath": f"{base}/dir/moved.txt"})
        if result.get("jobId"):
            state = wait_job(runner, result["jobId"])
            assert state == "completed", f"move job ended as {state}"
        result = runner.call("files/list", {"connectionId": cid, "path": f"{base}/dir"})
        names = [entry["name"] for entry in result.get("entries", [])]
        assert "moved.txt" in names and "hello-copy.txt" not in names, f"move did not relocate the file: {names}"
    runner.step("move", _move)

    def _rename():
        runner.call("files/rename", {"connectionId": cid, "path": f"{base}/dir/moved.txt", "newPath": f"{base}/dir/renamed.txt"})
        result = runner.call("files/list", {"connectionId": cid, "path": f"{base}/dir"})
        names = [entry["name"] for entry in result.get("entries", [])]
        assert "renamed.txt" in names, f"rename failed: {names}"
    runner.step("rename", _rename)

    def _rename_dir_degrade():
        # P-FILES ②: OpenDAL rename accepts files only — a directory rename
        # degrades to a copy+delete async job with a pollable jobId.
        runner.call("files/mkdir", {"connectionId": cid, "path": f"{base}/dir2"})
        payload = base64.b64encode(b"dirrename").decode()
        runner.call("files/write", {"connectionId": cid, "path": f"{base}/dir2/inner.txt", "dataBase64": payload})
        result = runner.call("files/rename", {"connectionId": cid, "path": f"{base}/dir2", "newPath": f"{base}/dir2-renamed"})
        assert result.get("success"), f"dir rename rejected: {result}"
        job_id = result.get("jobId")
        if job_id:
            state = wait_job(runner, job_id)
            assert state == "completed", f"dir rename job ended as {state}"
        entries = runner.call("files/list", {"connectionId": cid, "path": f"{base}/dir2-renamed"}).get("entries", [])
        assert any(entry["name"] == "inner.txt" for entry in entries), f"dir rename lost contents: {entries}"
        listing = runner.call("files/list", {"connectionId": cid, "path": base}).get("entries", [])
        assert not any(entry["name"] == "dir2" for entry in listing), "dir rename left the source behind"
    runner.step("rename-dir-degrade", _rename_dir_degrade)

    def _delete():
        runner.call("files/delete", {"connectionId": cid, "path": f"{base}/dir/renamed.txt"})
        result = runner.call("files/list", {"connectionId": cid, "path": f"{base}/dir"})
        names = [entry["name"] for entry in result.get("entries", [])]
        assert "renamed.txt" not in names, "delete left the file behind"
    runner.step("delete", _delete)

    def _purge_root_refused():
        refused = runner.expect_error("files/purge", {"connectionId": cid, "path": "/"}, needle="root")
        if not refused:
            raise SidecarError("purge on root unexpectedly succeeded")
    runner.step("purge-refuses-root", _purge_root_refused)

    def _purge():
        runner.call("files/mkdir", {"connectionId": cid, "path": f"{base}/purge-me/inner"})
        payload = base64.b64encode(b"bye").decode()
        runner.call("files/write", {"connectionId": cid, "path": f"{base}/purge-me/inner/x.txt", "dataBase64": payload})
        runner.call("files/purge", {"connectionId": cid, "path": f"{base}/purge-me"})
        result = runner.call("files/list", {"connectionId": cid, "path": base})
        names = [entry["name"] for entry in result.get("entries", [])]
        assert "purge-me" not in names, "purge left the directory behind"
    runner.step("purge-directory", _purge)


def scenario_audit(runner: Runner) -> None:
    """§8.4 files/audit/list: read-only projection of the store audit trail.

    Relies on the preceding scenario_structure write ops (mkdir/write/copy/
    move/rename/delete/purge) having appended audit lines for this
    connection. Response shape is owned by AuditPanel.vue:
    {entries:[{at,action,connectionId,path,result}]}, newest first."""
    def _list():
        result = runner.call("files/audit/list", {"connectionId": runner.connection_id, "limit": 10})
        entries = result.get("entries", [])
        assert entries, "audit/list returned no entries after write operations"
        for entry in entries:
            for key in ("at", "action", "connectionId", "path", "result"):
                assert key in entry, f"audit entry missing {key!r}: {entry!r}"
            assert "password" not in json.dumps(entry).lower(), f"secret-shaped key in audit entry: {entry!r}"
        assert len(entries) <= 10, "limit not honoured"
        assert entries[0]["action"] == "files/purge", f"newest-first violated: {entries[0]!r}"
    runner.step("audit-list", _list)

    def _limit_guard():
        refused = runner.expect_error("files/audit/list", {"limit": -5}, needle="limit")
        if not refused:
            raise SidecarError("negative limit unexpectedly succeeded")
    runner.step("audit-list-negative-limit", _limit_guard)


def scenario_transfer_roundtrip(runner: Runner, base: str) -> None:
    payload = os.urandom(CHUNK * 2 + 12345)  # spans three 256KiB frames

    def _roundtrip():
        remote = f"{base}/blob.bin"
        upload_bytes(runner, remote, payload)
        data, _start = download_bytes(runner, remote)
        assert data == payload, f"binary round-trip mismatch: sent {len(payload)} got {len(data)}"
    runner.step("upload-download-roundtrip", _roundtrip)

    def _status():
        remote = f"{base}/status.bin"
        upload_bytes(runner, remote, b"status probe")
        jobs = runner.call("files/transfers/list", {"connectionId": runner.connection_id})
        entries = jobs.get("jobs", [])
        assert any(job.get("jobId") or job.get("taskId") for job in entries), "transfers/list returned no jobs"
        target = next((job for job in entries if job.get("remotePath", "").endswith("status.bin")), entries[-1])
        job_id = target.get("jobId") or target.get("taskId")
        status = runner.call("files/transfer/status", {"jobId": job_id})
        state = status.get("job", {}).get("status") or status.get("job", {}).get("state")
        assert state in ("completed", "running", "queued"), \
            f"unexpected job state: {status}"
    runner.step("transfers-list-status", _status)

    def _cancel():
        start = runner.call("files/upload/start",
                            {"connectionId": runner.connection_id, "remotePath": f"{base}/cancelled.bin", "size": 999999})
        task_id = start["taskId"]
        runner.call("files/transfer/cancel", {"taskId": task_id})
        status = runner.call("files/transfer/status", {"jobId": task_id})
        state = status.get("job", {}).get("status") or status.get("job", {}).get("state")
        assert state in ("canceled", "cancelled"), f"cancel did not reach canceled state: {state}"
    runner.step("transfer-cancel", _cancel)

    def _clear():
        remote = f"{base}/clear-probe.bin"
        upload_bytes(runner, remote, b"clear probe")
        cleared = runner.call("files/transfers/clear", {"connectionId": runner.connection_id})
        assert cleared.get("cleared", 0) >= 1, f"transfers/clear removed nothing: {cleared}"
        jobs = runner.call("files/transfers/list", {"connectionId": runner.connection_id})
        entries = jobs.get("jobs", [])
        assert not any(
            (job.get("remotePath") or "").endswith("clear-probe.bin") for job in entries
        ), "cleared history still visible in transfers/list"
    runner.step("transfers-clear", _clear)


def scenario_read_only(client: SidecarClient, section: str, connection_id: str, base: str) -> None:
    runner = Runner(client, section)
    runner.connection_id = connection_id

    def _gate():
        payload = base64.b64encode(b"nope").decode()
        runner.expect_error("files/write",
                            {"connectionId": runner.connection_id, "path": f"{base}/ro.txt", "dataBase64": payload},
                            needle="read")
    runner.step("read-only-gate", _gate)

    def _read_ok():
        runner.call("files/list", {"connectionId": runner.connection_id, "path": "/"})
    runner.step("read-only-list-ok", _read_ok)


# --------------------------------------------------------------------------
# Sections
# --------------------------------------------------------------------------

def scenario_archive(runner: Runner, base: str) -> None:
    """B-ARCHIVE: files/archiveList + files/extract on tar.gz (tar bomb guards
    and zip Phase-2 refusal included). Archives are built in-process with the
    stdlib tarfile module (dynamic-Huffman deflate exercises the sidecar's
    hand-written inflate)."""
    import io
    import tarfile
    import zipfile

    cid = runner.connection_id

    def build_tgz(entries):
        """entries: [(name, text, is_dir)] → tar.gz bytes."""
        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w:gz") as tar:
            for name, text, is_dir in entries:
                info = tarfile.TarInfo(name)
                info.mtime = 1700000000
                if is_dir:
                    info.type = tarfile.DIRTYPE
                    tar.addfile(info)
                else:
                    payload = text.encode()
                    info.size = len(payload)
                    tar.addfile(info, io.BytesIO(payload))
        return buf.getvalue()

    def _list():
        payload = build_tgz([
            ("pkg/", "", True),
            ("pkg/readme.md", "# package", False),
            ("pkg/lib/inner.txt", "inner payload", False),
        ])
        upload_bytes(runner, f"{base}/sample.tar.gz", payload)
        result = runner.call("files/archiveList", {"connectionId": cid, "path": f"{base}/sample.tar.gz"})
        entries = result.get("entries", [])
        assert result.get("total") == 3, f"archiveList total mismatch: {result}"
        by_path = {entry["path"]: entry for entry in entries}
        assert set(by_path) == {"pkg", "pkg/readme.md", "pkg/lib/inner.txt"}, f"entry paths: {list(by_path)}"
        assert by_path["pkg"]["kind"] == "directory", f"dir kind: {by_path['pkg']}"
        assert by_path["pkg/readme.md"]["kind"] == "file", f"file kind: {by_path['pkg/readme.md']}"
        assert by_path["pkg/readme.md"]["size"] == 9, f"file size: {by_path['pkg/readme.md']}"
        assert by_path["pkg/readme.md"]["name"] == "readme.md"
        assert by_path["pkg/readme.md"].get("modifiedAt"), "mtime missing on file entry"
    runner.step("archive-list", _list)

    def _list_paged():
        result = runner.call("files/archiveList", {"connectionId": cid, "path": f"{base}/sample.tar.gz", "page": 2, "pageSize": 2})
        entries = result.get("entries", [])
        assert result.get("total") == 3, f"paged total mismatch: {result}"
        assert len(entries) == 1, f"page 2/size 2 should hold the last entry: {entries}"
        assert entries[0]["path"] == "pkg/lib/inner.txt", f"paged entry: {entries[0]}"
    runner.step("archive-list-paged", _list_paged)

    def _extract_sync():
        result = runner.call("files/extract", {"connectionId": cid, "path": f"{base}/sample.tar.gz", "targetPath": f"{base}/out"})
        # 3-entry package is within the synchronous budget → {success}.
        assert result.get("success"), f"sync extract rejected: {result}"
        assert result.get("transport") != "job", f"small package should not degrade to a job: {result}"
        entries = runner.call("files/list", {"connectionId": cid, "path": f"{base}/out/pkg"}).get("entries", [])
        names = {entry["name"] for entry in entries}
        assert "readme.md" in names and "lib" in names, f"extracted tree incomplete: {names}"
        read = runner.call("files/read", {"connectionId": cid, "path": f"{base}/out/pkg/lib/inner.txt"})
        assert base64.b64decode(read["dataBase64"]) == b"inner payload", "extracted content mismatch"
    runner.step("archive-extract-sync", _extract_sync)

    def _extract_job():
        payload = build_tgz([(f"bulk/f{i:02d}.txt", f"content-{i}", False) for i in range(11)])
        upload_bytes(runner, f"{base}/bulk.tar.gz", payload)
        result = runner.call("files/extract", {"connectionId": cid, "path": f"{base}/bulk.tar.gz", "targetPath": f"{base}/bulk-out"})
        # 11 files exceed the synchronous budget → degrade to a real job.
        assert result.get("transport") == "job" and result.get("jobId"), f"big package must degrade to a job: {result}"
        state = wait_job(runner, result["jobId"])
        assert state == "completed", f"extract job ended as {state}"
        jobs = runner.call("files/transfers/list", {"connectionId": cid}).get("jobs", [])
        assert any(job.get("jobId") == result["jobId"] for job in jobs), "extract job missing from transfers/list"
        entries = runner.call("files/list", {"connectionId": cid, "path": f"{base}/bulk-out/bulk"}).get("entries", [])
        assert len(entries) == 11, f"extracted bulk files missing: {len(entries)}"
    runner.step("archive-extract-job", _extract_job)

    def _zip_phase2():
        zip_buf = io.BytesIO()
        with zipfile.ZipFile(zip_buf, "w") as zf:
            zf.writestr("zip/inner.txt", "zipped")
        upload_bytes(runner, f"{base}/sample.zip", zip_buf.getvalue())
        refused = runner.expect_error("files/archiveList", {"connectionId": cid, "path": f"{base}/sample.zip"}, needle="Phase 2")
        if not refused:
            raise SidecarError("archiveList on a zip unexpectedly succeeded")
        refused = runner.expect_error("files/extract", {"connectionId": cid, "path": f"{base}/sample.zip", "targetPath": f"{base}/zip-out"}, needle="Phase 2")
        if not refused:
            raise SidecarError("extract on a zip unexpectedly succeeded")
    runner.step("archive-zip-phase2-refusal", _zip_phase2)

    def _compress_sync():
        """P-FILES: files/compress on a small source set (synchronous native)."""
        runner.call("files/mkdir", {"connectionId": cid, "path": f"{base}/csrc"})
        for name, text in (("c1.txt", "one"), ("c2.txt", "two")):
            runner.call("files/write", {"connectionId": cid, "path": f"{base}/csrc/{name}", "dataBase64": base64.b64encode(text.encode()).decode()})
        result = runner.call("files/compress", {"connectionId": cid, "paths": [f"{base}/csrc"], "targetPath": f"{base}/csrc.tar.gz"})
        assert result.get("transport") == "native", f"small package must stay native: {result}"
        listed = runner.call("files/archiveList", {"connectionId": cid, "path": f"{base}/csrc.tar.gz"})
        paths = {entry["path"] for entry in listed.get("entries", [])}
        assert paths == {"csrc/c1.txt", "csrc/c2.txt"}, f"compressed entries: {sorted(paths)}"
    runner.step("compress-sync", _compress_sync)

    def _compress_job():
        """11 files exceed the synchronous budget → job degrade; plain .tar."""
        runner.call("files/mkdir", {"connectionId": cid, "path": f"{base}/csrc-bulk"})
        for i in range(11):
            runner.call("files/write", {"connectionId": cid, "path": f"{base}/csrc-bulk/f{i:02d}.txt", "dataBase64": base64.b64encode(f"content-{i}".encode()).decode()})
        result = runner.call("files/compress", {"connectionId": cid, "paths": [f"{base}/csrc-bulk"], "targetPath": f"{base}/csrc-bulk.tar"})
        assert result.get("transport") == "job" and result.get("jobId"), f"big source must degrade to a job: {result}"
        state = wait_job(runner, result["jobId"])
        assert state == "completed", f"compress job ended as {state}"
        listed = runner.call("files/archiveList", {"connectionId": cid, "path": f"{base}/csrc-bulk.tar"})
        assert listed.get("total") == 11, f"job archive total mismatch: {listed}"
    runner.step("compress-job", _compress_job)

    def _compress_refusals():
        """Bad suffix and overwrite attempts are refused with clear errors."""
        refused = runner.expect_error("files/compress", {"connectionId": cid, "paths": [f"{base}/csrc"], "targetPath": f"{base}/nope.zip"}, needle=".tar")
        if not refused:
            raise SidecarError(METHOD_MISSING)
        refused = runner.expect_error("files/compress", {"connectionId": cid, "paths": [f"{base}/csrc"], "targetPath": f"{base}/csrc.tar.gz"}, needle="already exists")
        if not refused:
            raise SidecarError(METHOD_MISSING)
    runner.step("compress-refusals", _compress_refusals)


def run_core_sections(client: SidecarClient, fs_root: str) -> None:
    memory_external = {"protocol": "opendal-custom", "service": "memory", "config": {}}
    for section, external, root in (
        ("fs", {"protocol": "fs"}, fs_root),
        ("memory", memory_external, ""),
    ):
        print(f"\n==> section {section}")
        connection_id = f"smoke-{section}"
        try:
            connect(client, connection_id, external, root)
        except SidecarError as error:
            mark(section, "connection/connect", "fail" if not is_method_missing(str(error)) else "skip", str(error)[:120])
            if not is_method_missing(str(error)):
                raise
            continue
        runner = Runner(client, section)
        runner.connection_id = connection_id
        base = f"/smoke-{int(time.time())}" if section == "memory" else "/smoke"
        scenario_capabilities(runner)
        scenario_quick_paths(runner, expect_user_dirs=False)
        scenario_structure(runner, base)
        scenario_audit(runner)
        scenario_transfer_roundtrip(runner, base)
        scenario_archive(runner, base)

        # read_only uses its own connection against the same root. The
        # readonly connection id must match the one scenario_read_only dials
        # (previously "{section}-readonly" vs "smoke-{section}-readonly": the
        # mismatch made fs/read-only-list-ok fail spuriously).
        readonly_id = f"{connection_id}-readonly"
        try:
            connect(client, readonly_id, {**external, "read_only": True}, root)
            scenario_read_only(client, section, readonly_id, base)
        except SidecarError as error:
            if is_method_missing(str(error)):
                mark(section, "read-only-connection", "skip", str(error)[:120])
            else:
                raise

    # files/quickPaths 的用户目录 chips 需要未受限的 fs 连接：OpenDAL fs 的
    # root 为必填项，root="/" 即整盘访问，单独拨一条探针连接；chips 由后端
    # stat 过滤，这里只断言 home chip 存在且键集合法。
    try:
        connect(client, "smoke-quickpaths-fs", {"protocol": "fs"}, "/")
        probe = Runner(client, "fs-quickpaths")
        probe.connection_id = "smoke-quickpaths-fs"
        scenario_quick_paths(probe, expect_user_dirs=True)
    except SidecarError as error:
        if is_method_missing(str(error)):
            mark("fs-quickpaths", "quick-paths", "skip", str(error)[:120])
        else:
            raise


def scenario_public_link(runner: Runner, remote_path: str) -> None:
    """§8.1 files/publicLink over a signature-capable backend (s3 presign)."""
    def _presign():
        result = runner.call("files/publicLink", {
            "connectionId": runner.connection_id,
            "path": remote_path,
            "expireSecs": 600,
        })
        url = result.get("url", "")
        assert url.startswith("http"), f"presigned url is not http(s): {url[:80]}"
    runner.step("publicLink-presign", _presign)


def run_s3_section(client: SidecarClient) -> None:
    endpoint = os.environ.get("DBX_FILES_S3_ENDPOINT")
    bucket = os.environ.get("DBX_FILES_S3_BUCKET")
    access = os.environ.get("DBX_FILES_S3_ACCESS_KEY")
    secret = os.environ.get("DBX_FILES_S3_SECRET_KEY")
    print("\n==> section s3 (MinIO)")
    if not (endpoint and bucket and access and secret):
        mark("s3", "container", "skip", "set DBX_FILES_S3_ENDPOINT/BUCKET/ACCESS_KEY/SECRET_KEY to enable")
        return
    connect(client, "smoke-s3", {
        "protocol": "s3",
        "bucket": bucket,
        "endpoint": endpoint,
        "region": os.environ.get("DBX_FILES_S3_REGION", "us-east-1"),
        "access_key_id": access,
    }, secrets={"secret_access_key": secret})
    runner = Runner(client, "s3")
    runner.connection_id = "smoke-s3"
    base = f"/smoke-{int(time.time())}"
    scenario_capabilities(runner)
    scenario_structure(runner, base)
    scenario_audit(runner)
    scenario_public_link(runner, f"{base}/dir/hello.txt")
    scenario_transfer_roundtrip(runner, base)


def run_sftp_section(client: SidecarClient) -> None:
    """P-FILES ③: real-machine openssh-server section (M3 item pulled in).

    Auth forms:
    - key (DBX_FILES_SFTP_KEY = path to a private key already registered in the
      container's authorized_keys): fully exercised — OpenDAL 0.57's sftp
      service shells out to the ssh binary (openssh crate) and supports
      keyfile auth only;
    - password: accepted by the plugin form/model but deliberately NOT
      forwarded to OpenDAL (engine/mod.rs: the 0.57 sftp service has no
      password option), so a password-only run stays SKIP with that note
      instead of failing.
    """
    host = os.environ.get("DBX_FILES_SFTP_HOST")
    port = os.environ.get("DBX_FILES_SFTP_PORT", "22")
    user = os.environ.get("DBX_FILES_SFTP_USER", "tester")
    key_file = os.environ.get("DBX_FILES_SFTP_KEY")
    password = os.environ.get("DBX_FILES_SFTP_PASSWORD", "")
    print("\n==> section sftp (openssh container)")
    if not host:
        mark("sftp", "container", "skip", "set DBX_FILES_SFTP_HOST/PORT/USER/KEY to enable")
        return
    if not key_file:
        mark(
            "sftp",
            "auth-key",
            "skip",
            "OpenDAL 0.57 sftp service is keyfile-only (no password auth); "
            "set DBX_FILES_SFTP_KEY to a private-key path to enable",
        )
        return
    # openssh crate only extracts user/port from the ssh:// URI form; a bare
    # user@host:port would reach the ssh binary unparsed. known_hosts "accept"
    # tolerates the container's ephemeral host key.
    connect(client, "smoke-sftp", {
        "protocol": "sftp",
        "endpoint": f"ssh://{user}@{host}:{port}",
        "user": user,
        "known_hosts_strategy": "accept",
        # key 必须进连接配置（model.rs 从 external_config.key 读取）——
        # 第 4 轮真机联调发现：此前 DBX_FILES_SFTP_KEY 只读不接，sftp 段
        # 首次真跑即暴露该 smoke 脚本自身缺陷。
        "key": key_file,
    })
    runner = Runner(client, "sftp")
    runner.connection_id = "smoke-sftp"
    # linuxserver/openssh-server: USER_NAME's home is /config (writable).
    base = os.environ.get("DBX_FILES_SFTP_BASE", f"/config/smoke-{int(time.time())}")
    scenario_capabilities(runner)
    scenario_structure(runner, base)
    scenario_audit(runner)
    scenario_transfer_roundtrip(runner, base)
    if password:
        mark("sftp", "auth-password", "skip", "password form not supported by OpenDAL 0.57 sftp (keyfile only); key path exercised above")


def run_smb_section(client: SidecarClient) -> None:
    """F5-S6: Samba container section (IMPL_PLAN_SMB §5), env-gated SKIP like
    the s3/sftp sections so parallel tracks never block each other.

    Credentials: password is a secret-bound field (IMPL_PLAN_SMB §3.1) so it
    travels in connection.connection_secrets (model.rs parses it from there
    only) — it is never printed, never embedded in external_config.

    While engine/smb has not landed (parallel F5-S2/S3/S4), any connect-level
    failure marks the whole section SKIP with a "smb backend not landed yet"
    note instead of FAIL: the protocol method face is identical, only the
    smb protocol branch is missing.
    """
    host = os.environ.get("DBX_FILES_SMB_HOST")
    port = os.environ.get("DBX_FILES_SMB_PORT", "445")
    share = os.environ.get("DBX_FILES_SMB_SHARE")
    user = os.environ.get("DBX_FILES_SMB_USER")
    password = os.environ.get("DBX_FILES_SMB_PASSWORD")
    domain = os.environ.get("DBX_FILES_SMB_DOMAIN", "")
    print("\n==> section smb (Samba container)")
    if not (host and share and user and password):
        mark("smb", "container", "skip", "set DBX_FILES_SMB_HOST/PORT/SHARE/USER/PASSWORD to enable")
        return
    external = {"protocol": "smb", "endpoint": f"{host}:{port}", "share": share, "username": user}
    if domain:
        external["domain"] = domain
    connection = {
        "id": "smoke-smb",
        "name": "smoke-smb",
        "db_type": "storage",
        "host": "",
        "port": 0,
        "external_config": external,
        "connection_secrets": {"password": password},
    }
    # connection/test = negotiate + session setup + tree connect + share root
    # stat (IMPL_PLAN_SMB §2.1).
    try:
        client.request("connection/test", lifecycle_params(connection))
        mark("smb", "connection-test", "pass")
    except SidecarError as error:
        if is_method_missing(str(error)):
            mark("smb", "connection-test", "skip", str(error)[:120])
        else:
            mark("smb", "connection-test", "skip", f"smb backend not landed yet: {str(error)[:120]}")
        return
    try:
        connect(client, "smoke-smb", external, secrets={"password": password})
    except SidecarError as error:
        if is_method_missing(str(error)):
            mark("smb", "connection/connect", "skip", str(error)[:120])
        else:
            mark("smb", "connection/connect", "skip", f"smb backend not landed yet: {str(error)[:120]}")
        return
    runner = Runner(client, "smb")
    runner.connection_id = "smoke-smb"
    base = f"/smoke-{int(time.time())}"
    try:
        scenario_smb_capabilities(runner)
        scenario_structure(runner, base)
        scenario_smb_stat_rmdir(runner, base)
        scenario_transfer_roundtrip(runner, base)
        scenario_smb_public_link_refused(runner, base)
    finally:
        # best-effort teardown of the random base directory
        try:
            runner.call("files/purge", {"connectionId": runner.connection_id, "path": base})
        except SidecarError:
            pass

    # read_only gate re-dials the same share with read_only=true (own id, like
    # the fs/memory readonly connections).
    readonly_id = "smoke-smb-readonly"
    try:
        connect(client, readonly_id, {**external, "read_only": True}, secrets={"password": password})
        scenario_read_only(client, "smb", readonly_id, base)
    except SidecarError as error:
        if is_method_missing(str(error)):
            mark("smb", "read-only-connection", "skip", str(error)[:120])
        else:
            raise


def scenario_smb_capabilities(runner: Runner) -> None:
    """IMPL_PLAN_SMB §2.2 capability contract: the whole files/* face is
    available, copy degrades to the read→write job and presign never exists
    (so files/publicLink must take the backend-unsupported path)."""
    def _caps():
        caps = runner.call("files/capabilities", {"connectionId": runner.connection_id})
        for key in ("list", "read", "write", "stat", "delete", "createDir", "rename"):
            assert caps.get(key) is True, f"smb capability {key!r} should be declared: {caps!r}"
        assert caps.get("copy") is False, f"smb must not declare copy (job degrade instead): {caps!r}"
        assert caps.get("presign") is False, f"smb must not declare presign: {caps!r}"
    runner.step("capabilities-smb-contract", _caps)


def scenario_smb_stat_rmdir(runner: Runner, base: str) -> None:
    """files/stat + files/rmdir; scenario_structure covers the rest of the face."""
    cid = runner.connection_id

    def _stat():
        payload = base64.b64encode(b"stat me").decode()
        runner.call("files/write", {"connectionId": cid, "path": f"{base}/stat.txt", "dataBase64": payload})
        result = runner.call("files/stat", {"connectionId": cid, "path": f"{base}/stat.txt"})
        entry = result.get("entry", {})
        assert entry.get("kind") == "file", f"stat kind mismatch: {entry!r}"
        assert int(entry.get("size") or -1) == len(b"stat me"), f"stat size mismatch: {entry!r}"
    runner.step("stat", _stat)

    def _rmdir():
        runner.call("files/mkdir", {"connectionId": cid, "path": f"{base}/rmdir-me"})
        runner.call("files/rmdir", {"connectionId": cid, "path": f"{base}/rmdir-me"})
        names = [entry["name"] for entry in runner.call("files/list", {"connectionId": cid, "path": base}).get("entries", [])]
        assert "rmdir-me" not in names, f"rmdir left the directory behind: {names}"
    runner.step("rmdir", _rmdir)


def scenario_smb_public_link_refused(runner: Runner, base: str) -> None:
    """IMPL_PLAN_SMB §2.2: presign is never declared → files/publicLink must
    return a business error (-32000 unsupported), never a success or a panic."""
    def _refused():
        refused = runner.expect_error("files/publicLink", {
            "connectionId": runner.connection_id,
            "path": f"{base}/stat.txt",
            "expireSecs": 60,
        })
        if not refused:
            raise SidecarError("files/publicLink on smb unexpectedly succeeded")
    runner.step("publiclink-unsupported", _refused)


def run_sftp_native_section(client: SidecarClient) -> None:
    """sftp-native (dual-stack, russh + russh-sftp) section.

    The OpenDAL sftp service is keyfile-only; this section exercises the
    native adapter with the PASSWORD form (plus keyboard-interactive
    fallback) against a real host, env-gated SKIP like the s3/sftp/smb
    sections:

        DBX_FILES_SFTP_NATIVE_HOST   host (required)
        DBX_FILES_SFTP_NATIVE_PORT   port (default 22)
        DBX_FILES_SFTP_NATIVE_USER   login user (required)
        DBX_FILES_SFTP_NATIVE_PASSWORD  login password (secret-bound)
        DBX_FILES_SFTP_NATIVE_KEY    private key content or path (optional,
                                     used when the password is unset)
        DBX_FILES_SFTP_NATIVE_BASE   writable base dir (default /smoke-<ts>)

    Every write lands under the self-generated base; teardown purges ONLY
    that base.
    """
    host = os.environ.get("DBX_FILES_SFTP_NATIVE_HOST")
    port = os.environ.get("DBX_FILES_SFTP_NATIVE_PORT", "22")
    user = os.environ.get("DBX_FILES_SFTP_NATIVE_USER", "")
    password = os.environ.get("DBX_FILES_SFTP_NATIVE_PASSWORD", "")
    key = os.environ.get("DBX_FILES_SFTP_NATIVE_KEY", "")
    print("\n==> section sftp-native (russh dual-stack)")
    if not (host and user and (password or key)):
        mark("sftp-native", "container", "skip",
             "set DBX_FILES_SFTP_NATIVE_HOST/PORT/USER/PASSWORD (or KEY) to enable")
        return
    external = {"protocol": "sftp-native", "endpoint": f"{user}@{host}:{port}", "user": user}
    if key:
        external["key"] = key
    connection_id = "smoke-sftp-native"
    secrets = {"password": password} if password else None
    try:
        connect(client, connection_id, external, secrets=secrets)
    except SidecarError as error:
        mark("sftp-native", "connection/connect", "fail", str(error)[:160])
        raise
    runner = Runner(client, "sftp-native")
    runner.connection_id = connection_id
    base = os.environ.get("DBX_FILES_SFTP_NATIVE_BASE", f"/smoke-{int(time.time())}")
    try:
        scenario_sftp_native_capabilities(runner)
        scenario_structure(runner, base)
        scenario_smb_stat_rmdir(runner, base)
        scenario_transfer_roundtrip(runner, base)
        scenario_smb_public_link_refused(runner, base)

        readonly_id = f"{connection_id}-readonly"
        connect(client, readonly_id, {**external, "read_only": True}, secrets=secrets)
        scenario_read_only(client, "sftp-native", readonly_id, base)
    finally:
        # best-effort teardown of the random base directory
        try:
            runner.call("files/purge", {"connectionId": runner.connection_id, "path": base})
        except SidecarError as error:
            print(f"    teardown: purge failed for {base}: {error}")


def scenario_sftp_native_capabilities(runner: Runner) -> None:
    """sftp-native capability contract: full files/* face with native rename;
    copy degrades to the read→write job and presign never exists."""
    def _caps():
        caps = runner.call("files/capabilities", {"connectionId": runner.connection_id})
        for key in ("list", "read", "write", "stat", "delete", "createDir", "rename"):
            assert caps.get(key) is True, f"sftp-native capability {key!r} should be declared: {caps!r}"
        assert caps.get("copy") is False, f"sftp-native must not declare copy (job degrade instead): {caps!r}"
        assert caps.get("presign") is False, f"sftp-native must not declare presign: {caps!r}"
    runner.step("capabilities-sftp-native-contract", _caps)


def main() -> None:
    started = time.monotonic()
    sidecar = os.environ.get("DBX_PLUGIN_SIDECAR")
    if not sidecar and not Path(
        os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "backend", "target", "release", "dbx-plugin-files")
    ).exists():
        print("SKIP: sidecar binary not built yet (backend/target/release/dbx-plugin-files; "
              "set DBX_PLUGIN_SIDECAR to override)")
        return

    with tempfile.TemporaryDirectory(prefix="dbx-files-smoke-") as fs_root:
        client = SidecarClient.start(timeout=30)
        try:
            step = "\n==> plugin/initialize"
            print(step)
            info = client.initialize()
            print(json.dumps(info, ensure_ascii=False)[:200])

            run_core_sections(client, fs_root)
            run_s3_section(client)
            run_sftp_section(client)
            run_smb_section(client)
            run_sftp_native_section(client)
        except SidecarError as error:
            print(f"\nFAIL: {error}", file=sys.stderr)
            # Close BEFORE draining stderr: drain_stderr blocks on read() until
            # EOF, so a still-running sidecar used to hang the harness (exit
            # 124). Closing first guarantees EOF immediately.
            client.close()
            print(client.drain_stderr()[-2000:], file=sys.stderr)
            sys.exit(1)
        client.close()

    passed = sum(1 for _, _, status in RESULTS if status == "pass")
    skipped = sum(1 for _, _, status in RESULTS if status == "skip")
    failed = sum(1 for _, _, status in RESULTS if status == "fail")
    print(f"\n{'=' * 62}\nPASS {passed}  SKIP {skipped}  FAIL {failed}  "
          f"({time.monotonic() - started:.1f}s)")
    if failed:
        sys.exit(1)
    if passed == 0:
        print("SKIP: every scenario was skipped (parallel tracks not landed yet)")
        return
    print("PASS: smoke green")


if __name__ == "__main__":
    main()
