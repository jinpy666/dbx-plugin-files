//! Local-mount feature (docs/MOUNT.zh-CN.md, M1).
//!
//! Two strategies behind `files/mount`, chosen by the wiring layer:
//! 1. **rclone mount** (primary) — `rclone_mount.rs` in the rclone module
//!    drives the running rcd's `mount/mount`; the bundled fork carries mount
//!    support compiled in.
//! 2. **WebDAV gateway** (fallback) — [`webdav_gateway`]: a minimal
//!    read-only WebDAV server inside the sidecar, mounted by each OS's
//!    built-in WebDAV client, zero driver installs.

pub mod webdav_gateway;
