# Files standalone repository migration notes

This repository was split from `/Users/Jinpy/btroot/dbx-plugins/files` at source
commit `ce19beb4fa75cc79048e3ba8852c298d674fd9b3` (subtree head
`a3802c9fd74e5bc9fa00d862a56a112d4069de80`). The initial import used
`git subtree split --prefix=files`, preserving the Files directory history.

The split is self-contained: Files-only frontend adapters live under
`shared/frontend/`, the connection-form contract check is under
`scripts/connection-forms/`, and the matching DBX sidecar SDK is vendored under
`shared/sdk/rust/dbx-plugin-sdk/`. No SSH, LDAP, or Kafka implementation was
copied, and no `../shared` or `../host` path is required for the normal build.
The screenshots the README referenced in the monorepo were never committed
there (repository hygiene rules), so this repository only references paths
that `git ls-files` really contains.

`ci.yml` validates manifest/backend identity, runs the frontend and Rust tests,
executes the offline MCP stdio smoke, and builds candidate packages for five
targets (Linux x64/arm64, macOS arm64/x64, Windows x64), followed by a
cross-target consistency check. `aws-lc-sys` (via `opendal → reqwest →
rustls`) needs NASM on Linux and uses its bundled prebuilt objects on Windows
(`AWS_LC_SYS_PREBUILT_NASM=1`); this dependency tree has no keyring/dbus, so
`libdbus-1-dev` is not required. `release.yml` builds the same matrix on a
`files-v<version>` GitHub Release and uploads the `.dbxp` packages plus
`release-candidates.json`. It does not invent a signing key or secret.

The manifest `source`/`homepage` point at
`https://github.com/jinpy666/dbx-plugin-files`; publish only a matching
`files-v<version>` tag after the GitHub repository exists.

Bugs are fixed in this repository first; DBX Host API, installer, or shared SDK
changes go through separate host/SDK changes with recorded versions. Do not
point this repository back at the monorepo `../shared`; share code through
versioned public packages (frontend adapters and the form contract) or synced
copies across standalone repositories. The vendored SDK switches to a locked
crates.io/Git dependency once one is available, keeping the protocol
init/binary-frame regression tests.

Live storage containers (MinIO / mod_dav / pyftpdlib / OpenSSH / Samba / russh
targets) and the DBX.app host install pipeline are not offline CI gates; when
those environments are unavailable, the R1-R7 container sections of
`scripts/container_smoke.sh`, `smoke_mcp.py`, and `smoke_test.py` must report
`SKIP`, never a fake `PASS`.
