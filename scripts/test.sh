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
  # Node 版本单一真源是仓库根 .nvmrc（CI setup-node node-version-file 同源）；
  # 精确版本缺失时回退 nvm 里最新的 v22。
  NODE_VER="$(head -n1 .nvmrc 2>/dev/null | tr -d 'vV\n ')"
  NODE_BIN="$(ls -d "$HOME/.nvm/versions/node/v${NODE_VER:-22.21.0}"/bin 2>/dev/null \
    || ls -d "$HOME"/.nvm/versions/node/v22*/bin 2>/dev/null | sort -V | tail -1 || true)"
  export PATH="${NODE_BIN:+$NODE_BIN:}$HOME/Library/pnpm:$PATH"
fi
export PATH="$HOME/.cargo/bin:$PATH"

echo "==> backend unit tests"
node scripts/connection-forms/verify.mjs files
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

echo "==> MCP smoke (release sidecar; container cases auto-SKIP)"
DBX_PLUGIN_SIDECAR="$ROOT/backend/target/release/dbx-plugin-files" python3 scripts/smoke_mcp.py

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
