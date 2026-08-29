#!/usr/bin/env python3
"""Drive the dbx-plugin-files sidecar over its stdio-framed protocol.

Frame format (dbx-plugin-sdk): 5-byte header [kind:u8][length:u32 BE] + payload.
kind 0 = JSON (jsonrpc 2.0 requests/notifications), kind 1 = binary.

Usage as a library:
    from sidecar_client import SidecarClient
    client = SidecarClient.start_default()
    client.initialize()
    client.request("connection/connect", {...})
"""

from __future__ import annotations

import json
import os
import select
import socket
import struct
import subprocess
import sys
import time

FRAME_JSON = 0
FRAME_BINARY = 1
PROTOCOL_VERSION = 1

# sentinel returned via on_event handling to signal "handled, keep waiting"
_EVENT_HANDLED = object()


def default_binary() -> str:
    env = os.environ.get("DBX_PLUGIN_SIDECAR")
    if env:
        return env
    repo = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "backend", "target",
                        "release", "dbx-plugin-files")
    if os.path.exists(repo):
        return repo
    home = os.path.expanduser("~")
    return (
        f"{home}/Library/Application Support/com.dbx.app/plugins/io.dbx.files/"
        f"versions/current/bin/darwin-arm64/dbx-plugin-files"
    )


class SidecarError(RuntimeError):
    pass


class SidecarClient:
    def __init__(self, process: subprocess.Popen, read_fd: int, timeout: float = 20.0):
        self.process = process
        self.read_fd = read_fd
        self.timeout = timeout
        self.next_id = 1
        self.events: list[dict] = []
        self.binary_frames: list[bytes] = []
        self._pending: dict[int, dict] = {}

    @classmethod
    def start(cls, binary: str | None = None, data_dir: str | None = None, timeout: float = 20.0) -> "SidecarClient":
        binary = binary or default_binary()
        if not os.path.exists(binary):
            raise SidecarError(f"sidecar binary not found: {binary} (set DBX_PLUGIN_SIDECAR)")
        env = dict(os.environ)
        if data_dir:
            env["DBX_PLUGIN_DATA_DIR"] = data_dir
        process = subprocess.Popen(
            [binary],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )
        return cls(process, process.stdout.fileno(), timeout)

    # -- low-level framing ---------------------------------------------------

    def _send_raw(self, kind: int, payload: bytes) -> None:
        assert self.process.stdin
        self.process.stdin.write(struct.pack(">BI", kind, len(payload)))
        self.process.stdin.write(payload)
        self.process.stdin.flush()

    def _read_frame(self, deadline: float) -> tuple[int, bytes]:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise SidecarError("timeout waiting for sidecar frame")
        ready, _, _ = select.select([self.read_fd], [], [], remaining)
        if not ready:
            raise SidecarError("timeout waiting for sidecar frame")
        header = os.read(self.read_fd, 5)
        if len(header) < 5:
            raise SidecarError(f"sidecar closed (header={header!r})")
        kind = header[0]
        (length,) = struct.unpack(">I", header[1:5])
        payload = b""
        while len(payload) < length:
            chunk = os.read(self.read_fd, length - len(payload))
            if not chunk:
                raise SidecarError("sidecar closed mid-payload")
            payload += chunk
        return kind, payload

    def _pump(self, want_id: int | None = None, on_event=None) -> object:
        """Read frames until a response for want_id arrives; stash events.

        on_event(message) may return a dict (sent as a follow-up request,
        e.g. resolving a host-key challenge) to keep flows single-threaded.
        """
        deadline = time.monotonic() + self.timeout
        while True:
            kind, payload = self._read_frame(deadline)
            if kind == FRAME_BINARY:
                # binary payload: [u16 BE channel_len][channel][data]
                (channel_len,) = struct.unpack(">H", payload[:2])
                channel = payload[2:2 + channel_len].decode(errors="replace")
                self.binary_frames.append((channel, payload[2 + channel_len:]))
                if want_id is None:
                    return None
                continue
            message = json.loads(payload)
            if want_id is not None and message.get("id") == want_id:
                if "error" in message and message["error"] is not None:
                    raise SidecarError(f"{message['error'].get('message')}: {message['error'].get('data', '')}")
                return message.get("result")
            # notification / unrelated response
            self.events.append(message)
            if want_id is None:
                return None
            if on_event is not None:
                reply = on_event(message)
                if isinstance(reply, dict):
                    sub = {"jsonrpc": "2.0", "id": self.next_id, "method": reply["method"],
                           "params": reply.get("params", {})}
                    self.next_id += 1
                    self._send_raw(FRAME_JSON, json.dumps(sub).encode())
            # events arriving while a request is in flight must not consume
            # its timeout budget
            deadline = time.monotonic() + self.timeout

    # -- protocol ------------------------------------------------------------

    def initialize(self) -> dict:
        return self.request(
            "plugin/initialize",
            {"host": {"protocolVersions": [PROTOCOL_VERSION]}},
        )

    def request(self, method: str, params: dict | None = None, timeout: float | None = None,
                on_event=None) -> dict:
        """Send a request and wait for its response.

        on_event(message) is invoked synchronously for every notification that
        arrives while waiting; return a dict from it to send it as a request.
        This keeps challenge/response flows single-threaded.
        """
        if timeout:
            previous, self.timeout = self.timeout, timeout
        request_id = self.next_id
        self.next_id += 1
        message = {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params or {}}
        self._send_raw(FRAME_JSON, json.dumps(message).encode())
        try:
            result = self._pump(request_id, on_event=on_event)
            return result if isinstance(result, dict) else {}
        finally:
            if timeout:
                self.timeout = previous

    def send_binary(self, channel: str, data: bytes) -> None:
        # payload layout: [u16 BE channel_len][channel bytes][data]
        channel_bytes = channel.encode()
        self._send_raw(FRAME_BINARY, struct.pack(">H", len(channel_bytes)) + channel_bytes + data)

    def wait_event(self, method: str, timeout: float = 10.0) -> dict | None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for event in self.events:
                if event.get("method") == method or str(event.get("params", {}).get("method", "")).endswith(method):
                    return event
            self.timeout = max(0.5, deadline - time.monotonic())
            try:
                self._pump(None)
            except SidecarError:
                break
        for event in self.events:
            if event.get("method") == method:
                return event
        return None

    def drain_stderr(self) -> str:
        try:
            return self.process.stderr.read().decode(errors="replace") if self.process.stderr else ""
        except Exception:
            return ""

    def close(self) -> None:
        try:
            if self.process.stdin:
                self.process.stdin.close()
            self.process.wait(timeout=5)
        except Exception:
            self.process.kill()


def lifecycle_params(connection: dict) -> dict:
    """Build connection lifecycle params the way the DBX host does."""
    return {
        "provider": {"id": "io.dbx.files.connection", "databaseType": "storage"},
        "connection": connection,
        "runtime": {"host": connection.get("host"), "port": connection.get("port", 22)},
    }


if __name__ == "__main__":
    client = SidecarClient.start()
    try:
        info = client.initialize()
        print(json.dumps(info, indent=2, ensure_ascii=False))
    finally:
        client.close()
