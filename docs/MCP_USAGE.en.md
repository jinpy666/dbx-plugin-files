# Files MCP Usage Guide

For protocol details, parameter tables, and safety boundaries see the
[Files MCP reference](MCP.zh-CN.md) (Chinese). This page covers day-to-day usage.

## Starting the server

- **Recommended: the DBX MCP bridge**. Enable the `dbx-files` connection in an MCP
  client that supports DBX plugin MCP (e.g. ZCode); calls reuse saved connections,
  approvals, and permission boundaries, and credentials never reach the client.
- **Standalone stdio mode** (for debugging without the host):

```bash
cargo build --release --manifest-path backend/Cargo.toml
backend/target/release/dbx-plugin-files --mcp
```

The stdio frame ceiling defaults to 16 MiB and is tunable via
`DBX_FILES_MCP_STDIO_MAX_LINE` (violations return `-32700` naming the ceiling).

## Tool list (13 tools, matching the smoke suite)

| Tool | Purpose |
| --- | --- |
| `files_scan_digest` | Scan a directory; returns aggregate counts/distributions/samples plus a paging cursor |
| `files_cursor_next` | Fetch the next batch of locator rows by cursorId without re-sending filters |
| `files_ui_focus` / `files_ui_search` / `files_ui_select` | Drive host workbench panel/navigation/location |
| `files_ui_state` | Read a UI intent result or the latest workbench snapshot |
| `files_ui_quick_paths` | Quick-jump paths for a connection |
| `files_write` | Write a small file (base64, ≤4 MiB; larger payloads use the transfer channel) |
| `files_mkdir` | Create a directory |
| `files_rename` | Rename/move |
| `files_delete` / `files_purge` | Delete a file / purge a directory tree (both two-phase) |
| `files_sync` | Cross-connection directory sync/copy (incremental compare; `sync:true` mirror-deletes target extras — preview with `dryRun:true` first; returns a jobId to poll via `files/transfer/status`) |

## Typical patterns

**Large-directory discovery**: start with `files_scan_digest` (aggregate + samples);
when you need details, page through `files_cursor_next` with the returned `cursorId`.
Cursors expire after 10 minutes — re-run the digest when they do.

**Two-phase deletes** (`files_delete` / `files_purge`):

```text
First call   files_delete {connectionId, path:"…"}   → preview + one-time confirmToken (60s)
Second call  same arguments + confirmToken (hash must match) → execute + audit
```

`files_purge` refuses the connection root and `/` (red line). Every MCP write is
marked `source:"mcp"` in the audit log.

## Offline verification

```bash
cargo build --release --manifest-path backend/Cargo.toml
python3 scripts/smoke_mcp.py --binary backend/target/release/dbx-plugin-files
```

The smoke suite needs no real storage service; cases that require real backends
(S3/WebDAV/FTP/SFTP/SMB etc.) must be enabled explicitly via environment variables
and print `SKIP` when the environment is absent — never treat an offline pass as a
live-connection pass. On Windows the `sftp` quick protocol is unavailable; use
`sftp-native` for the related live cases.
