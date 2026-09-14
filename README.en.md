# DBX Files

[![CI](https://github.com/jinpy666/dbx-plugin-files/actions/workflows/ci.yml/badge.svg)](https://github.com/jinpy666/dbx-plugin-files/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/jinpy666/dbx-plugin-files?display_name=tag)](https://github.com/jinpy666/dbx-plugin-files/releases)

[中文](README.md) · [Repository split notes](docs/REPOSITORY_SPLIT.en.md) · [Files MCP reference](docs/MCP.zh-CN.md)

DBX Files (`io.dbx.files`) is a unified workspace for multi-backend file
operations. It gives local folders, object storage, and remote file services a
consistent browsing, transfer, and administration experience.

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

## MCP automation

Start standalone stdio mode with:

```bash
backend/target/release/dbx-plugin-files --mcp
```

Useful tools include `files_scan_digest`, `files_cursor_next`, `files_write`,
`files_mkdir`, `files_rename`, and `files_delete`. Use digest plus cursors for
large directories; delete and purge require two-phase confirmation. See the
[Files MCP reference](docs/MCP.zh-CN.md).

## Security

For production connections, use root-path locking and read-only mode where possible,
and enable deletion only when required. Credentials for S3, OSS, WebDAV, FTP, SFTP,
and SMB are managed by the host secret store; the plugin does not write keys,
private keys, or connection exports to logs or local configuration.

## Development

```bash
pnpm --dir frontend install
pnpm --dir frontend typecheck && pnpm --dir frontend test && pnpm --dir frontend build
cargo test --manifest-path backend/Cargo.toml
scripts/test.sh
```

Repository contracts (manifest/backend identity/connection forms) are enforced by
`scripts/validate_repo.py` and `scripts/connection-forms/verify.mjs`; CI builds
candidate packages for five targets. Provider capabilities and integration
details live under `docs/`.
