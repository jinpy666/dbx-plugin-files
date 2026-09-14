# DBX Files

[![CI](https://github.com/jinpy666/dbx-plugin-files/actions/workflows/ci.yml/badge.svg)](https://github.com/jinpy666/dbx-plugin-files/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/jinpy666/dbx-plugin-files?display_name=tag)](https://github.com/jinpy666/dbx-plugin-files/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

[中文](README.md) · [Product media](docs/MEDIA.en.md) · [Feature comparison](docs/COMPARISON.en.md) · [MCP usage guide](docs/MCP_USAGE.en.md) · [Files MCP reference](docs/MCP.zh-CN.md) · [Repository split notes](docs/REPOSITORY_SPLIT.en.md)

DBX Files (`io.dbx.files`) is a unified workspace for multi-backend file operations:
local folders, object storage, and remote file services share one browsing,
transfer, and administration experience, turning "moving files between storages"
into a coherent, auditable, and automatable workflow.

> Files · Storage · Transfer: fs, S3, WebDAV, FTP, SFTP, SMB plus transfers,
> archives, and automation in a single DBX workbench panel.

## Why it is worth using

| What you need to do | What DBX Files gives you |
| --- | --- |
| Move data between local and remote storage | Unified browse, copy, move, and directory sync with transfer jobs: progress, cancel, and history |
| Inspect and organize files across providers | digest-paged browsing for large directories with sorting, stats, and quick paths, consistent across protocols |
| Handle archives and sharing | zip compress/extract/inline browsing, presigned public links for S3 objects |
| Constrain risky operations | Read-only mode, delete protection, root-path locking, and per-connection timeouts |
| Automate repetitive work | MCP tools reuse connections, policies, and permission boundaries: digest→cursor paging, two-phase deletes |

## Use cases

- Move or synchronize data between local folders, object storage, and remote file services.
- Inspect, preview, and organize files across different providers with one workflow.
- Give teams a controlled file-operations entry point with root restrictions and read-only policies.

## Highlights

- Multi-protocol engine (Apache OpenDAL plus in-house adapters): local filesystems,
  S3/MinIO, Alibaba Cloud OSS, WebDAV, FTP, SFTP, SMB/CIFS, and other compiled
  OpenDAL services such as gcs/azblob/obs/cos.
- SFTP dual stack: the `sftp` quick protocol (OpenDAL, keyfile auth) and
  `sftp-native` (russh, password or keyfile) coexist; on Windows use `sftp-native`.
- Consistent browse, read, upload, download, copy, move, rename, and delete operations.
- Large-file transfers with progress, cancellation, asynchronous jobs, and the DBX
  binary channel; transfer history is per-connection and clearable.
- zip archives: compress, extract, and inline listing (paged archiveList); directory
  synchronization (syncDir), presigned links, and root-path restrictions.
- Read-only mode, delete protection, Known Hosts policies, and per-connection timeouts.
- Connection credentials managed by DBX host secret bindings rather than plugin configuration.
- Simplified Chinese, Traditional Chinese, English, Spanish, Italian, Japanese,
  and Portuguese UI.

See [Feature comparison](docs/COMPARISON.en.md) for the full positioning and
[Product media](docs/MEDIA.en.md) for promotional material.

## MCP automation

Prefer the DBX MCP bridge so calls reuse saved connections, approvals, and
permission boundaries. Standalone stdio mode:

```bash
backend/target/release/dbx-plugin-files --mcp
```

There are 12 tools: `files_scan_digest`, `files_cursor_next`,
`files_ui_focus/search/select/state`, `files_ui_quick_paths`, `files_write`,
`files_mkdir`, `files_rename`, `files_delete`, and `files_purge`. Use digest plus
cursor paging for large directories; delete and purge require two-phase confirmation.
See the [MCP usage guide](docs/MCP_USAGE.en.md) and the
[Files MCP reference](docs/MCP.zh-CN.md).

## Security

For production connections, use root-path locking and read-only mode where possible,
and enable deletion only when required. Credentials for S3, OSS, WebDAV, FTP, SFTP,
and SMB are managed by the host secret store; the plugin does not write keys,
private keys, or connection exports to logs or local configuration.

## Installation

Download the `.dbxp` package for your platform from
[GitHub Releases](https://github.com/jinpy666/dbx-plugin-files/releases) and install it
locally from the DBX plugin center. Developers can build candidate packages following
the [repository split notes](docs/REPOSITORY_SPLIT.en.md).

## Development

```bash
pnpm --dir frontend install
pnpm --dir frontend typecheck && pnpm --dir frontend test && pnpm --dir frontend build
cargo test --locked --manifest-path backend/Cargo.toml
python3 scripts/validate_repo.py && node scripts/connection-forms/verify.mjs files
scripts/test.sh
```

Repository contracts (manifest/backend identity/connection forms) are enforced by
`scripts/validate_repo.py` and `scripts/connection-forms/verify.mjs`; CI builds
candidate packages for five targets (linux-x64, linux-arm64, darwin-arm64,
darwin-x64, windows-x64). Provider capabilities and integration details live under `docs/`.
