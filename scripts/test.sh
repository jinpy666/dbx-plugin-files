#!/usr/bin/env bash
# Full verification suite for the dbx-files plugin.
#
#   scripts/test.sh                # everything available on this machine
#
# Steps: backend unit tests -> frontend typecheck/test/build -> sidecar
# release build -> framed smoke (fs + memory sections; MinIO/sftp container
# sections skip unless their env vars are set; methods owned by parallel
# tracks that have not landed yet count as SKIP, not FAIL).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

if ! command -v pnpm >/dev/null 2>&1; then
  NODE_BIN="$(ls -d "$HOME"/.nvm/versions/node/v22*/bin 2>/dev/null | sort -V | tail -1 || true)"
  export PATH="$HOME/.nvm/versions/node/v22.21.0/bin:$HOME/Library/pnpm:${NODE_BIN:+$NODE_BIN:}$PATH"
fi
export PATH="$HOME/.cargo/bin:$PATH"

echo "==> backend unit tests"
node ../shared/connection-forms/verify.mjs files
if [ -f backend/src/main.rs ] && [ "$(wc -l < backend/src/main.rs)" -gt 1 ]; then
  cargo test --manifest-path backend/Cargo.toml
else
  echo "SKIP: backend skeleton not landed yet (main.rs is a stub)"
fi

echo "==> frontend typecheck + tests + build"
[ -d frontend/node_modules ] || pnpm --dir frontend install --frozen-lockfile
pnpm --dir frontend typecheck
pnpm --dir frontend test
pnpm --dir frontend build

echo "==> sidecar release build"
cargo build --release --manifest-path backend/Cargo.toml

echo "==> framed smoke (fs + memory; containers optional)"
DBX_PLUGIN_SIDECAR="$ROOT/backend/target/release/dbx-plugin-files" python3 scripts/smoke_test.py

echo "==> package .dbxp"
if [ ! -f manifest.json ]; then
  echo "SKIP: manifest.json not present yet (F-A owns it); packaging deferred"
else
  unset DBX_PLUGIN_SDK_ROOT
  if command -v dbx-plugin >/dev/null 2>&1; then
    NO_COLOR=1 dbx-plugin package .
  else
    HOST="${DBX_HOST_WORKTREE:-$PWD/../dbx-plugin-host-worktree}"
    if [ ! -x "$HOST/plugins/sdk/cli/target/release/dbx-plugin" ]; then
      (cd "$HOST/plugins/sdk/cli" && cargo build --release)
    fi
    NO_COLOR=1 "$HOST/plugins/sdk/cli/target/release/dbx-plugin" package .
  fi
fi

echo
echo "all green"
