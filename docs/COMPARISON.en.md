# Feature and Solution Comparison

This is a positioning comparison, not a performance, pricing, or security audit.
Third-party products change with versions, platforms, plugins, and business plans;
"—" means the capability is not a core built-in experience of that solution, and
"external tools" means it usually requires a command line, plugin, or extra setup.
Third-party annotations follow public product positioning; re-verify against your
target platform and exact version before choosing.

## Capability matrix

| Capability | Files Studio | rclone | WinSCP | Traditional file managers (Finder/Explorer) |
| --- | --- | --- | --- | --- |
| Graphical browsing of remote storage | Built-in | — (CLI-first; third-party GUI wrappers exist) | Built-in (SFTP/SCP/FTP/WebDAV/S3) | Local disks only |
| Local folders | Built-in | Built-in | Built-in | Built-in |
| S3 and compatible object storage | Built-in | Built-in (~70 backends) | S3 | — (external tools) |
| WebDAV / FTP | Built-in | Built-in | Built-in | Partial (version-dependent) |
| SFTP | Dual stack (keyfile/password) | Built-in | Built-in | — (external tools) |
| SMB/CIFS | Built-in (pure-Rust client) | — | — (relies on OS mounts) | Built-in (OS mounts) |
| Graphical transfer queue with progress | Built-in | — (CLI progress) | Built-in | Partial (copy dialogs) |
| Directory sync | Built-in (syncDir) | Built-in (core strength, many strategies) | Built-in (Keep remote directory up to date) | — |
| zip archive compress/extract/inline browse | Built-in | — | — | Partial (OS-dependent) |
| Presigned public links | Built-in (S3) | Built-in (link command) | — | — |
| Read-only / delete protection / root locking | Built-in (policy gates) | CLI flags or config | Config-dependent | — |
| Credential management | DBX host secret binding, never on disk | Config file (obfuscatable) | Session/site config | OS keychain |
| Scripting / CLI | — (focused on DBX and MCP channels) | Built-in (core strength) | Built-in (scripting/CLI) | — |
| MCP automation tools | Built-in (12 tools) | — | — | — |
| Deep integration into a host workbench | Native (DBX plugin) | — | — | — |

## Positioning differences

- **rclone** is the CLI-first swiss army knife for sync and migration: the widest
  backend coverage, the richest sync strategies, and the most mature scripting
  ecosystem. It has no graphical workbench; if everything happens in a terminal,
  rclone is still the first choice.
- **WinSCP** is a mature graphical SFTP/FTP client on Windows with practical transfer
  queues and keep-remote-up-to-date sync; object storage support is mostly S3, and
  host integration or policy gating is out of scope.
- **Traditional file managers** target local disks; remote protocols either depend on
  OS mounts (SMB) or need external tools, and they lack transfer queues and
  protocol-level safety gates.
- **Files Studio** folds "multi-protocol browsing + transfer jobs + archives + guarded
  policies" into the DBX host workbench: credentials are managed by the host, and the
  same capabilities are exposed to automation clients via MCP tools. It does not try
  to replace rclone's CLI ecosystem — it makes graphical and automated work share one
  set of connections and permission boundaries.

## Position within the DBX plugin family

| Plugin | Primary objects | Typical tasks |
| --- | --- | --- |
| Files Studio | Filesystems and object storage | File browsing, uploads/downloads, archives, cross-storage organizing |
| DBX SSH Terminal | SSH hosts, terminal, SFTP, remote ops | Log into servers, run commands, browse and transfer files |
| DBX LDAP | LDAP directories | Query, aggregate, and edit directory entries |
| DBX Kafka | Kafka clusters | Topic, message, consumer group, and Schema operations |

Files does not re-implement the SSH terminal, LDAP, or Kafka protocols; it focuses on
unified file operations over filesystems and object storage, reusing host
capabilities for connections, credentials, and workbench bridging. For file transfer
over SSH, choose either the DBX SSH terminal (SFTP) or this plugin's
sftp/sftp-native connections as needed.

## How to choose

- Everything happens in a terminal and you need scripting plus the widest backend
  coverage: choose rclone.
- Your primary need on Windows is a mature graphical SFTP/FTP client: choose WinSCP.
- You only manage local disks: the OS file manager is enough.
- You already use DBX host connections, MCP, or plugin workbenches, or you want
  multi-protocol file operations and automation inside one set of connections and
  permission boundaries: choose Files Studio.
