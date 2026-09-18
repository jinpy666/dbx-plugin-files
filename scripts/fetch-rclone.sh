#!/usr/bin/env bash
# Fetch the pinned rclone release binary for one target platform into dest-dir.
#
# Usage: fetch-rclone.sh <os> <arch> <dest-dir>
#   os:   darwin | linux | windows
#   arch: arm64 | amd64   (x64 accepted as an alias of amd64)
#
# Downloads rclone-v<RCLONE_VERSION>-<rclone-os>-<arch>.zip from the dbx fork's
# GitHub Releases (RCLONE_REPO=rclone/rclone restores the official source with
# a downloads.rclone.org fallback), verifies the pinned SHA256 below, extracts
# the rclone binary to <dest-dir> and smoke-checks `--version`. Idempotent:
# when <dest-dir> already holds a binary reporting the pinned version, nothing
# is downloaded (CI-cache friendly).
#
# rclone asset naming: darwin -> osx, linux -> linux, windows -> windows.
# The release zips contain a single top-level dir with the binary inside.
set -euo pipefail

RCLONE_VERSION="v1.75.1-dbx.1"
# Fork releases carry the cmount-tagged build with the DBX plugin fixes; the
# pinned SHA256 list below must be updated in the same change as the version
# (contrib/rclone-fork/README.md). Override to rclone/rclone for the official
# source.
RCLONE_REPO="${RCLONE_REPO:-jinpy666/rclone}"

# Pinned SHA256 of the release zips, from the fork's checksum list:
# https://github.com/jinpy666/rclone/releases/download/v1.75.1-dbx.1/SHA256SUMS
pinned_sha256() {
  case "$1" in
    rclone-v1.75.1-dbx.1-osx-arm64.zip)      echo "f27caad79bb18af872706df46614c14997b1002254c068f9d30630dbd5d759b8" ;;
    rclone-v1.75.1-dbx.1-osx-amd64.zip)      echo "f158a0d1ef7a2945a87a3eb2a375af072822882cf369c69cdf2de92e15c1eed6" ;;
    rclone-v1.75.1-dbx.1-linux-amd64.zip)    echo "a012b6edd9445a225609d9bbd51245fbeff67064b71eb59e238894ad06cab1d7" ;;
    rclone-v1.75.1-dbx.1-linux-arm64.zip)    echo "5379cf0c39cb95858e93b7283a221ded1fcc08ecfee521342c56ff3217559c97" ;;
    rclone-v1.75.1-dbx.1-windows-amd64.zip)  echo "4e4fbb680b9bb7300cb7d71e27f95a685d5c70f5e98a3404e3f9cfefd435b66d" ;;
    *) return 1 ;;
  esac
}

if [ $# -ne 3 ]; then
  echo "usage: $0 <darwin|linux|windows> <arm64|amd64> <dest-dir>" >&2
  exit 2
fi

os="$1"
arch="$2"
dest_dir="$3"

[ "$arch" = "x64" ] && arch="amd64"
case "$os" in
  darwin)  rclone_os="osx" ;;
  linux)   rclone_os="linux" ;;
  windows) rclone_os="windows" ;;
  *) echo "fetch-rclone.sh: unsupported os '$os' (want darwin|linux|windows)" >&2; exit 2 ;;
esac
case "$arch" in
  arm64|amd64) ;;
  *) echo "fetch-rclone.sh: unsupported arch '$arch' (want arm64|amd64)" >&2; exit 2 ;;
esac

zip_name="rclone-${RCLONE_VERSION}-${rclone_os}-${arch}.zip"
expected_sha="$(pinned_sha256 "$zip_name")"

if [ "$os" = "windows" ]; then
  binary_name="rclone.exe"
else
  binary_name="rclone"
fi
dest_binary="$dest_dir/$binary_name"

rclone_reported_version() {
  "$1" --version 2>/dev/null | head -n1 || true
}

if [ -f "$dest_binary" ]; then
  reported="$(rclone_reported_version "$dest_binary")"
  if [ "$reported" = "rclone $RCLONE_VERSION" ]; then
    echo "fetch-rclone: $dest_binary already reports '$reported'; skipping download"
    exit 0
  fi
fi

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/fetch-rclone.XXXXXX")"
trap 'rm -rf "$tmp_dir"' EXIT

download() {
  local url="$1" out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fSL --retry 3 --connect-timeout 30 -o "$out" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$out" "$url"
  else
    echo "fetch-rclone.sh: need curl or wget to download $url" >&2
    return 1
  fi
}

zip_path="$tmp_dir/$zip_name"
# RCLONE_REPO=<owner>/<repo> switches the GitHub source (e.g. to the dbx fork);
# the downloads.rclone.org fallback below only makes sense for the official
# repo and is skipped for custom sources.
github_url="https://github.com/${RCLONE_REPO}/releases/download/${RCLONE_VERSION}/${zip_name}"
downloads_url="https://downloads.rclone.org/${RCLONE_VERSION}/${zip_name}"

echo "fetch-rclone: downloading $zip_name from ${RCLONE_REPO}"
if ! download "$github_url" "$zip_path"; then
  if [ "$RCLONE_REPO" = "rclone/rclone" ]; then
    echo "fetch-rclone: GitHub download failed, falling back to downloads.rclone.org"
    download "$downloads_url" "$zip_path"
  else
    echo "fetch-rclone.sh: download from ${RCLONE_REPO} failed" >&2
    exit 1
  fi
fi

if command -v sha256sum >/dev/null 2>&1; then
  actual_sha="$(sha256sum "$zip_path" | awk '{print $1}')"
else
  actual_sha="$(shasum -a 256 "$zip_path" | awk '{print $1}')"
fi
if [ "$actual_sha" != "$expected_sha" ]; then
  echo "fetch-rclone.sh: SHA256 mismatch for $zip_name" >&2
  echo "  expected: $expected_sha" >&2
  echo "  actual:   $actual_sha" >&2
  exit 1
fi

if command -v unzip >/dev/null 2>&1; then
  unzip -q "$zip_path" -d "$tmp_dir/extract"
else
  # Git Bash on the Windows runners ships neither unzip nor a python3 alias,
  # so fall back through every interpreter name we might find.
  py="$(command -v python3 || command -v python)" || {
    echo "fetch-rclone.sh: need unzip or python to extract $zip_name" >&2
    exit 1
  }
  "$py" - "$zip_path" "$tmp_dir/extract" <<'PY'
import sys, zipfile
zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])
PY
fi

found_binary="$(find "$tmp_dir/extract" -type f \( -name rclone -o -name rclone.exe \) | head -n1)"
if [ -z "$found_binary" ]; then
  echo "fetch-rclone.sh: no rclone binary found inside $zip_name" >&2
  exit 1
fi

mkdir -p "$dest_dir"
cp "$found_binary" "$dest_binary"
chmod 0755 "$dest_binary" 2>/dev/null || true

reported="$(rclone_reported_version "$dest_binary")"
if [ "$reported" != "rclone $RCLONE_VERSION" ]; then
  echo "fetch-rclone.sh: $dest_binary failed version check (got: '$reported', want 'rclone $RCLONE_VERSION')" >&2
  exit 1
fi
echo "fetch-rclone: installed $reported at $dest_binary"
