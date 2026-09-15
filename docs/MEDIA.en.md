# Files Studio Product Media

This page collects material ready to use on GitHub, in release notes, and in product
introductions. It is text-only and matches the plugin's real, current capabilities;
screenshots and demo videos will be added in later versions, and this page never
references media files that do not exist.

## One-line positioning

`Files Studio: one file workspace for local folders, S3-compatible storage, and remote file services.`

## Long-form copy

`Files Studio is built for everyday file operations and cross-storage migration. Open one
connection to browse, upload, download, copy, move, run transfer jobs, handle zip
archives, and perform guarded deletes. The multi-protocol engine covers local fs,
S3/MinIO, Alibaba Cloud OSS, WebDAV, FTP, SFTP (dual stack: keyfile and password
auth), and SMB/CIFS. Credentials stay in host secret bindings, an MCP tool interface
serves automation, and the UI ships in seven languages.`

## Three selling points

- **One connection, many storages**: fs, S3, WebDAV, FTP, SFTP, and SMB share the
  same browse-and-transfer experience — switching backends never changes your habits.
- **From usable to trustworthy**: root-path locking, read-only mode, delete
  protection, and host secret bindings put risky operations inside explicit boundaries.
- **From manual to automated**: MCP tools reuse saved connections and policies;
  digest→cursor paging and two-phase deletes keep scripted operations just as safe.

## Capability quick view

- Browse and manage: listing/paging, sorting, stats, stat, size, quickPaths.
- Transfer: upload, download, copy, move, directory sync (syncDir), progress,
  cancellation, and history.
- Archives: zip compress, extract, and inline paged listing (archiveList).
- Sharing: presigned public links for S3 objects (publicLink).
- Safety: read_only, allow_delete, lock_to_root, known_hosts strategies, per-connection timeouts.
- Automation: 12 MCP tools over standalone stdio and the DBX MCP bridge.
- Platforms: candidate packages for linux-x64, linux-arm64, darwin-arm64,
  darwin-x64, and windows-x64.

## Usage boundaries

This page describes plugin UI and offline-verifiable capabilities. Real
S3/WebDAV/FTP/SFTP/SMB backends, the DBX.app host bridge, and platform installation
require the corresponding runtime environments; defer to CI, smoke results
(container-based cases SKIP when the environment is absent), and release notes.
On Windows the `sftp` quick protocol is unavailable — use `sftp-native`.
