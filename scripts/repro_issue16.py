#!/usr/bin/env python3
"""Issue #16 reproduction driver (developer aid, not part of CI).

Simulates the docker scenario end to end without docker:
  mode=norclone  - rclone invisible (empty PATH + dead DBX_FILES_RCLONE_BIN)
  mode=datadir   - DBX_PLUGIN_DATA_DIR points under a regular file

Both must end with a LIVE sidecar, an initialize answer, and structured
errors for storage calls (never a dead process / broken pipe).
"""

from __future__ import annotations

import os
import pathlib
import sys
import tempfile

sys.path.insert(0, str(pathlib.Path(__file__).parent))
from sidecar_client import SidecarClient  # noqa: E402


def main() -> int:
    mode = sys.argv[1] if len(sys.argv) > 1 else "norclone"
    sidecar = os.environ.get(
        "DBX_PLUGIN_SIDECAR",
        str(pathlib.Path(__file__).parent.parent / "backend/target/debug/dbx-plugin-files"),
    )
    empty_dir = tempfile.mkdtemp(prefix="dbx-empty-path-")
    updates: dict[str, str | None] = {
        # Empty PATH dir + dead env binary: the engine cannot resolve rclone.
        "PATH": empty_dir,
        "DBX_FILES_RCLONE_BIN": f"{empty_dir}/no-such-rclone",
    }
    if mode == "datadir":
        blocker = pathlib.Path(tempfile.mkdtemp(prefix="dbx-blocker-")) / "file"
        blocker.write_bytes(b"x")
        updates["DBX_PLUGIN_DATA_DIR"] = str(blocker / "data")

    process = SidecarClient.start(binary=sidecar, env_updates=updates)
    failure: str | None = None
    try:
        info = process.initialize()
        print("initialize OK:", info.get("plugin"), "protocol", info.get("protocolVersion"))
        process.request(
            "connection/test",
            {
                "provider": {"id": "io.dbx.files.connection", "databaseType": "storage"},
                "connection": {
                    "id": "c-local",
                    "external_config": {"protocol": "fs", "root": "/tmp"},
                },
            },
            timeout=30,
        )
        failure = "UNEXPECTED SUCCESS (expected a structured error)"
    except Exception as error:  # SidecarError carries the structured message
        alive = process.process.poll() is None
        print(f"connection/test structured error: {error}")
        print(f"sidecar alive after the error: {alive}")
        if not alive:
            failure = "sidecar died during the request"
    finally:
        # Shut the sidecar down before reading stderr: the read blocks until
        # EOF, so the pipe must be closed first.
        if failure is None and process.process.poll() is None:
            process.close()
        if failure is not None and process.process.poll() is None:
            process.process.kill()
            process.process.wait(timeout=5)
        stderr_tail = process.drain_stderr()
        if stderr_tail:
            print("sidecar stderr tail:")
            for line in stderr_tail.strip().splitlines()[-12:]:
                print("  " + line)
    if failure:
        print(f"FAIL: {failure}")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
