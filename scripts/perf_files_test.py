#!/usr/bin/env python3
"""Performance baseline for dbx-files (PROGRESS-P-FILES §4).

Measures, against the local release/debug sidecar binary:
  1. bulk ingest + listPaged pagination latency over 10k entries (memory://)
  2. 50 MiB upload / download throughput via the binary transfer channel
     (memory:// and fs backends)
  3. progress-event throttling effectiveness (events vs bytes ratio)

Requires DBX_PLUGIN_SIDECAR (falls back to target/release binary).
Exit code 0 unless a hard failure occurs; results are printed as a table.
"""

from __future__ import annotations

import os
import struct
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from sidecar_client import SidecarClient, SidecarError, lifecycle_params  # noqa: E402
from smoke_test import CHUNK, Runner, connect, download_bytes, take_frame  # noqa: E402

THROTTLE_MS = 200  # sidecar progress-event floor (main.rs/model.rs)


def progress_events(client: SidecarClient) -> int:
    return sum(1 for e in client.events if e.get("method") == "files/transfer/progress")


def push_upload(client: SidecarClient, task_id: str, payload: bytes, delay: float = 0.0) -> None:
    offset = 0
    while offset < len(payload):
        chunk = payload[offset:offset + CHUNK]
        client.send_binary(f"files/upload/{task_id}", struct.pack(">Q", offset) + chunk)
        offset += len(chunk)
        if delay:
            time.sleep(delay)


def ingest_entries(client: SidecarClient, connection_id: str, count: int, batch_note: str) -> float:
    """Write `count` small files under /perf-bulk/, return elapsed seconds."""
    started = time.monotonic()
    for i in range(count):
        client.request("files/write", {
            "connectionId": connection_id,
            "path": f"/perf-bulk/file-{i:05d}.txt",
            "dataBase64": "aGVsbG8tcGVyZi1maWxl",  # "hello-perf-file"
        })
    return time.monotonic() - started


def measure_pagination(client: SidecarClient, connection_id: str, page_size: int = 500) -> tuple[float, int]:
    started = time.monotonic()
    pages, total = 0, None
    page = 1
    while True:
        result = client.request("files/listPaged", {
            "connectionId": connection_id, "path": "/perf-bulk",
            "page": page, "pageSize": page_size,
        })
        total = int(result.get("total", 0))
        pages = page
        if page * page_size >= total or not result.get("entries"):
            break
        page += 1
    return time.monotonic() - started, total


def throughput(client: SidecarClient, runner: Runner, remote: str, payload: bytes) -> dict:
    """Upload then download payload via binary channel; return MB/s + throttle stats."""
    connection_id = runner.connection_id
    client.events.clear()
    started = time.monotonic()
    upload = runner.call("files/upload/start", {
        "connectionId": connection_id, "remotePath": remote, "size": len(payload)})
    push_upload(client, upload["taskId"], payload)
    runner.call("files/upload/finish", {"taskId": upload["taskId"]})
    upload_secs = time.monotonic() - started

    upload_events = progress_events(client)
    client.events.clear()

    started = time.monotonic()
    download_bytes(runner, remote)
    download_secs = time.monotonic() - started
    download_events = progress_events(client)

    mb = len(payload) / (1024 * 1024)
    return {
        "upload_mbps": mb / upload_secs if upload_secs else 0,
        "download_mbps": mb / download_secs if download_secs else 0,
        "upload_events": upload_events,
        "download_events": download_events,
        "throttle_ratio": (upload_events + download_events) / max(1, len(payload) // CHUNK),
    }


def main() -> int:
    binary = os.environ.get("DBX_PLUGIN_SIDECAR") or str(
        Path(__file__).resolve().parent.parent / "backend/target/release/dbx-plugin-files")
    if not Path(binary).exists():
        print(f"sidecar binary not found: {binary} (set DBX_PLUGIN_SIDECAR)")
        return 2

    rows: list[tuple[str, str]] = []
    client = SidecarClient.start(binary, timeout=60)
    try:
        client.initialize()

        # --- memory:// backend: 10k entries + pagination ---
        connect(client, "perf-memory", {"protocol": "opendal-custom", "service": "memory", "config": {}})
        bulk = int(os.environ.get("PERF_BULK_COUNT", "10000"))
        if os.environ.get("PERF_SKIP_BULK") != "1":
            secs = ingest_entries(client, "perf-memory", bulk, "memory")
            rows.append((f"ingest {bulk} entries (memory)", f"{secs:.2f}s ({bulk / max(secs, 1e-9):.0f} ops/s)"))
            page_secs, total = measure_pagination(client, "perf-memory")
            rows.append((f"listPaged full scan ({total} entries, 500/page)", f"{page_secs * 1000:.0f}ms"))
            client.request("files/purge", {"connectionId": "perf-memory", "path": "/perf-bulk"})

        # --- 50 MiB throughput: memory then fs ---
        payload = os.urandom(50 * 1024 * 1024)
        for conn_id, external, root in [
            ("perf-mem2", {"protocol": "opendal-custom", "service": "memory", "config": "{}"}, ""),
            ("perf-fs", {"protocol": "fs"}, None),
        ]:
            if root is None:
                tmp = tempfile.mkdtemp(prefix="dbx-files-perf-")
                root = tmp
            connect(client, conn_id, external, root=root)
            runner = Runner(client, f"perf-{conn_id}")
            runner.connection_id = conn_id
            stats = throughput(client, runner, "/perf-50mb.bin", payload)
            rows.append((f"50MiB upload ({conn_id})", f"{stats['upload_mbps']:.1f} MiB/s"))
            rows.append((f"50MiB download ({conn_id})", f"{stats['download_mbps']:.1f} MiB/s"))
            rows.append((f"throttle events up/down (conn {conn_id})",
                         f"{stats['upload_events']}+{stats['download_events']} "
                         f"(~{THROTTLE_MS}ms floor; 200 frames/chunk)"))
            if root.startswith(tempfile.gettempdir()):
                import shutil
                shutil.rmtree(root, ignore_errors=True)
    finally:
        client.close()

    print("\n== dbx-files performance baseline ==")
    for name, value in rows:
        print(f"  {name:<44} {value}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
