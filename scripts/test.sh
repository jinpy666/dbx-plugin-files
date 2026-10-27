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
# Windows 的 cargo 产物带 .exe 后缀：smoke 车道与打包占位都要按平台解析，
# 否则 smoke 整轮被 "sidecar binary not found" 静默 SKIP。
SIDECAR="$ROOT/backend/target/release/dbx-plugin-files"
[ -f "$SIDECAR.exe" ] && SIDECAR="$SIDECAR.exe"
# framed smoke 的 fs/connection 段需要 rclone：PATH / DBX_FILES_RCLONE_BIN
# 之外，再探测 bin/（打包 include 目录，CI 由 fetch-rclone.sh 预置）与
# ~/.workbuddy/bin（本机常见下载位）；都缺则由 smoke 自己报可操作错误。
if ! command -v rclone >/dev/null 2>&1 && [ -z "${DBX_FILES_RCLONE_BIN:-}" ]; then
  for cand in "$ROOT"/bin/rclone.exe "$ROOT"/bin/rclone "$HOME"/.workbuddy/bin/rclone*/rclone.exe; do
    if [ -f "$cand" ]; then export DBX_FILES_RCLONE_BIN="$cand"; break; fi
  done
fi

echo "==> framed smoke (fs + memory; containers optional)"
DBX_PLUGIN_SIDECAR="$SIDECAR" python3 scripts/smoke_test.py

echo "==> MCP smoke (release sidecar; container cases auto-SKIP)"
DBX_PLUGIN_SIDECAR="$SIDECAR" python3 scripts/smoke_mcp.py

echo "==> package .dbxp"
if [ ! -f manifest.json ]; then
  echo "SKIP: manifest.json not present yet (F-A owns it); packaging deferred"
else
  # [package].include 的 bin/ 必须存在（CI 由 fetch-rclone.sh 预置 rclone，
  # 本地打包用 release 产物占位；正式发版由 release workflow 走完整链）。
  mkdir -p bin
  cp -f backend/target/release/dbx-plugin-files* bin/ 2>/dev/null || true
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
