#!/usr/bin/env bash
# plugin-cli 打包用的 cargo 垫片（仅 CI/发布流程的 Package 步骤经 PATH 注入）。
#
# plugin-cli 的 build_rust_backend 以 CARGO_TARGET_DIR=<staging 或注入目录>
# 调 `cargo build --release` 且不传 --target，随后固定从 <dir>/release/ 拷贝
# 二进制。工作流为 musl 静态构建导出的 CARGO_BUILD_TARGET 会把产物重定向到
# <dir>/<triple>/release/，拷贝必报 "Failed to copy Rust backend ... No such
# file or directory"。本垫片在 cargo 成功后把 triple 目录下的二进制同步回
# <dir>/release/ 供 CLI 拷贝；其余调用原样透传。
#
# 上游修复方向：plugin-cli 拷贝时感知 CARGO_BUILD_TARGET 的 triple 子目录，
# 届时本垫片可删除。
set -u
"${REAL_CARGO:-cargo}" "$@"
status=$?
if [ "$status" -eq 0 ] && [ -n "${CARGO_BUILD_TARGET:-}" ] && [ -n "${CARGO_TARGET_DIR:-}" ]; then
  binary="${SHIM_BINARY_NAME:-dbx-plugin-files}"
  built="$CARGO_TARGET_DIR/$CARGO_BUILD_TARGET/release/$binary"
  if [ -f "$built" ]; then
    mkdir -p "$CARGO_TARGET_DIR/release"
    cp -f "$built" "$CARGO_TARGET_DIR/release/$binary"
  fi
fi
exit "$status"
