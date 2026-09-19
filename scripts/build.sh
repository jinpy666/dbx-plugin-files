#!/usr/bin/env bash
# Build the dbx-files plugin frontend and package a .dbxp for the current platform.
# Requires: node + pnpm on PATH, cargo for the Rust sidecar.
# The dbx-plugin CLI bundles its own SDK (no DBX source checkout needed).
set -euo pipefail
cd "$(dirname "$0")/.."

fast=0
for arg in "$@"; do
  case "$arg" in
    --fast) fast=1 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

if ! command -v pnpm >/dev/null; then
  export PATH="$HOME/.nvm/versions/node/v22.21.0/bin:$HOME/Library/pnpm:$PATH"
fi
# Prepend only when cargo is not already resolvable: the release CI installs a
# cargo→cargo-zigbuild wrapper ahead of the rustup shim (Linux sidecars must
# link a low glibc baseline), and an unconditional prepend here would shadow
# the wrapper and silently revert Linux builds to the runner's native glibc.
if ! command -v cargo >/dev/null 2>&1; then
  export PATH="$HOME/.cargo/bin:$PATH"
fi

if [ "$fast" -eq 1 ]; then
  echo "==> frontend: install + build (--fast: skipping typecheck/test)"
else
  echo "==> frontend: install + typecheck + test + build"
fi
# Skip install when node_modules is fresh (lockfile unchanged since); saves
# seconds on every warm build — same trade-off ldap already makes.
if [ ! -d frontend/node_modules ] || [ frontend/pnpm-lock.yaml -nt frontend/node_modules ]; then
  pnpm --dir frontend install --frozen-lockfile
fi
if [ "$fast" -eq 1 ]; then
  pnpm --dir frontend build
else
  pnpm --dir frontend typecheck
  pnpm --dir frontend test
  pnpm --dir frontend build
fi

# dbx-plugin package runs its own `cargo build` for the Rust backend; without
# CARGO_TARGET_DIR it builds into a throwaway dist/.build-rust-<triple> staging
# dir (wiped afterwards) = full dep-tree rebuild on every release. Redirect it
# to the warm backend/target — verified the CLI reuses and keeps it. Do NOT pin
# RUSTUP_TOOLCHAIN here (docker builds via rust:1 + cargo-zigbuild lack 1.97.1);
# the release pipeline pins it itself.
export CARGO_TARGET_DIR="$PWD/backend/target"

echo "==> sidecar release build"
cargo build --release --manifest-path backend/Cargo.toml

echo "==> package .dbxp"
if [ ! -f manifest.json ]; then
  echo "SKIP: manifest.json not present yet (F-A owns it); packaging deferred"
  exit 0
fi

echo "==> bundle pinned rclone next to the sidecar binary"
# The rclone engine resolves its binary: DBX_FILES_RCLONE_BIN -> exe sibling
# (backend/src/rclone/proc.rs) -> PATH. dbx-plugin package stages the sidecar
# at bin/<target>/, and [package].include "bin" merges our fetched rclone into
# the same directory inside the .dbxp, so the installed plugin ships with it
# (bundled-first; system PATH remains the runtime fallback).
# Target naming matches the dbx-plugin CLI targets (packaging.md §5).
host_os="$(uname -s)"
host_arch="$(uname -m)"
case "$host_os" in
  Darwin)               cli_os=darwin;  rclone_os=darwin ;;
  Linux)                cli_os=linux;   rclone_os=linux ;;
  MINGW*|MSYS*|CYGWIN*) cli_os=windows; rclone_os=windows ;;
  *) echo "unsupported build host OS: $host_os" >&2; exit 1 ;;
esac
case "$host_arch" in
  arm64|aarch64)     rclone_arch=arm64; cli_arch=arm64 ;;
  x86_64|amd64)      rclone_arch=amd64; cli_arch=x64 ;;
  *) echo "unsupported build host arch: $host_arch" >&2; exit 1 ;;
esac
rclone_target="${cli_os}-${cli_arch}"
# bin/ is fully generated: only the current target may ship, stale target dirs
# from other-platform builds would trip inspect-dbxp's bin-target consistency.
mkdir -p bin
for stale in bin/*; do
  [ -e "$stale" ] || continue
  [ "$stale" = "bin/$rclone_target" ] || rm -rf "$stale"
done
if [ "${SKIP_RCLONE:-0}" = "1" ]; then
  echo "SKIP_RCLONE=1: not fetching rclone (sidecar falls back to system PATH at runtime)"
else
  scripts/fetch-rclone.sh "$rclone_os" "$rclone_arch" "bin/$rclone_target"
fi

unset DBX_PLUGIN_SDK_ROOT
# The dbx-plugin CLI ships with the SDK checkout; build it on first use.
if ! command -v dbx-plugin >/dev/null 2>&1; then
  HOST="${DBX_HOST_WORKTREE:-$PWD/../dbx-plugin-host-worktree}"
  if [ ! -x "$HOST/plugins/sdk/cli/target/release/dbx-plugin" ]; then
    (cd "$HOST/plugins/sdk/cli" && cargo build --release)
  fi
  export PATH="$HOST/plugins/sdk/cli/target/release:$PATH"
fi
NO_COLOR=1 dbx-plugin package .

# Same assertions release CI enforces: fails fast if the bin/ include lost the
# rclone binary or its exec bit between fetch and packaging.
if [ "${SKIP_RCLONE:-0}" != "1" ]; then
  if py="$(command -v python3 || command -v python)"; then
    "$py" scripts/check_rclone_package.py dist/*.dbxp --target "$rclone_target"
  else
    echo "python not found; skipping local rclone package check (release CI enforces it)"
  fi
fi

echo
echo "Artifacts:"
ls -la dist/*.dbxp dist/*.artifact.json 2>/dev/null || true
