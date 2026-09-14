# DBX Files

[中文](README.md) · [Workspace contribution guide](../CONTRIBUTING.md)

DBX Files is a unified workspace for multi-backend file operations. It gives
local folders, object storage, and remote file services a consistent browsing,
transfer, and administration experience.

![DBX Files dual-pane workspace](docs/screenshots-a-files/01-dual-pane.png)

## Use cases

- Move data between local folders, object storage, and remote file services.
- Inspect, preview, and organize files across different providers with one workflow.
- Give teams a controlled file-operations entry point with root restrictions and read-only policies.

## Highlights

- Local filesystems, S3/MinIO, Alibaba Cloud OSS, WebDAV, FTP, SFTP, SMB/CIFS,
  and other compiled OpenDAL services.
- Consistent browse, read, upload, download, copy, move, rename, and delete operations.
- Large-file transfers with progress, cancellation, asynchronous jobs, and the DBX binary channel.
- Directory synchronization, presigned links, and root-path restrictions.
- Read-only mode, delete protection, Known Hosts policies, and per-connection timeouts.
- Connection credentials managed by DBX host secret bindings rather than plugin configuration.
- Simplified Chinese, Traditional Chinese, English, Spanish, Italian, Japanese,
  and Portuguese UI.

![Object storage connection settings](docs/screenshots/files-form-s3-vhs-zhcn.png)

## MCP automation

Start standalone stdio mode with:

```bash
backend/target/release/dbx-plugin-files --mcp
```

Useful tools include `files_scan_digest`, `files_cursor_next`, `files_write`,
`files_mkdir`, `files_rename`, and `files_delete`. Use digest plus cursors for
large directories; delete and purge require two-phase confirmation. See the
[MCP guide](../docs/MCP_USAGE.en.md) and [Files MCP reference](docs/MCP.zh-CN.md).

## Security

For production connections, use root-path locking and read-only mode where possible,
and enable deletion only when required. Credentials for S3, OSS, WebDAV, FTP, SFTP,
and SMB are managed by the host secret store; the plugin does not write keys,
private keys, or connection exports to logs or local configuration.

## Development

```bash
cd frontend && pnpm install && pnpm typecheck && pnpm test && pnpm build
cd ../backend && cargo test
cd ..
scripts/test.sh
```

Provider capabilities and integration details live under `docs/`. Contributors
should read the [workspace contribution guide](../CONTRIBUTING.md) first.
