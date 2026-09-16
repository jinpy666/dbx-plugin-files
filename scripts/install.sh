#!/usr/bin/env bash
# Install (or upgrade) the newest dist/*.dbxp into a DBX plugin store using
# the official PluginPackageInstaller (checksum + compatibility verified),
# then restart DBX.
#
# Usage:
#   scripts/install.sh                    # install into the real DBX app store
#   scripts/install.sh --app-data <dir>   # target a custom DBX data directory
#   scripts/install.sh --reinstall        # dev only: drop the installed version first
#   scripts/install.sh --no-restart       # do not relaunch DBX afterwards
#   scripts/install.sh --keep-old         # keep older io.dbx.files versions for rollback
#
# Environment:
#   DBX_HOST_WORKTREE   host checkout used for the installer binary
#   DBX_TEST_APP        DBX.app bundle to relaunch (default: probe the host
#                       worktree, then `open -a DBX`)
set -euo pipefail
cd "$(dirname "$0")/.."

APP_DATA="${DBX_APP_DATA:-$HOME/Library/Application Support/com.dbx.app}"
REINSTALL=0
RESTART=1
KEEP_OLD=0
while [ $# -gt 0 ]; do
  case "$1" in
    --app-data) APP_DATA="$2"; shift 2 ;;
    --reinstall) REINSTALL=1; shift ;;
    --no-restart) RESTART=0; shift ;;
    --keep-old) KEEP_OLD=1; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

DBXP="$(ls -t dist/*.dbxp 2>/dev/null | head -1 || true)"
[ -n "$DBXP" ] || { echo "no .dbxp in dist/ — run scripts/build.sh first" >&2; exit 1; }
VERSION="$(python3 -c "import json;print(json.load(open('manifest.json'))['version'])")"

DBX_HOST_WORKTREE="${DBX_HOST_WORKTREE:-$PWD/../dbx-plugin-host-worktree}"
[ -d "$DBX_HOST_WORKTREE" ] || {
  echo "DBX host worktree not found; set DBX_HOST_WORKTREE explicitly for install integration" >&2
  exit 1
}
export PATH="$HOME/.cargo/bin:$PATH"

INSTALLER="$DBX_HOST_WORKTREE/target/release/examples/install_plugin"
if [ ! -x "$INSTALLER" ]; then
  echo "==> building PluginPackageInstaller example"
  (cd "$DBX_HOST_WORKTREE" && cargo build -p dbx-core --example install_plugin --release)
fi

APP_VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' \
  /Applications/DBX.app/Contents/Info.plist 2>/dev/null || echo 0.6.0)"

WAS_RUNNING=0
if pgrep -f "DBX.app/Contents/MacOS/dbx" >/dev/null 2>&1; then
  WAS_RUNNING=1
fi
echo "==> stopping DBX"
osascript -e 'quit app id "com.dbx.app"' >/dev/null 2>&1 || true
sleep 2
pkill -f "DBX.app/Contents/MacOS/dbx" 2>/dev/null || true
sleep 1

PLUGIN_STORE="$APP_DATA/plugins"
if [ "$REINSTALL" = 1 ]; then
  VERSION_DIR="$PLUGIN_STORE/io.dbx.files/versions/$VERSION"
  if [ -d "$VERSION_DIR" ]; then
    echo "==> dev reinstall: removing installed v$VERSION"
    rm -rf "$VERSION_DIR"
    python3 - "$PLUGIN_STORE/io.dbx.files/activations" "$VERSION" <<'PY'
import json, pathlib, sys
store = pathlib.Path(sys.argv[1])
version = sys.argv[2]
if store.is_dir():
    for record in store.glob("*.json"):
        try:
            if json.load(open(record)).get("version") == version:
                record.unlink()
        except Exception:
            pass
PY
  fi
fi

echo "==> installing $(basename "$DBXP") (v$VERSION) into $PLUGIN_STORE"
"$INSTALLER" "$PLUGIN_STORE" "$DBXP" "$APP_VERSION"

if [ "$KEEP_OLD" = 0 ]; then
  echo "==> cleaning old io.dbx.files versions (keeping v$VERSION)"
  python3 - "$PLUGIN_STORE" "$VERSION" <<'PY'
import json
import pathlib
import shutil
import sys

plugin_store = pathlib.Path(sys.argv[1])
current_version = sys.argv[2]
versions_dir = plugin_store / "io.dbx.files" / "versions"
activations_dir = plugin_store / "io.dbx.files" / "activations"

removed_versions = []
if versions_dir.is_dir():
    for version_dir in sorted(versions_dir.iterdir()):
        if version_dir.name == current_version:
            continue
        if version_dir.is_dir() or version_dir.is_symlink():
            shutil.rmtree(version_dir)
            removed_versions.append(version_dir.name)

removed_activations = []
if activations_dir.is_dir():
    for record in sorted(activations_dir.glob("*.json")):
        try:
            record_version = json.loads(record.read_text()).get("version")
        except (OSError, ValueError, TypeError):
            continue
        if record_version != current_version:
            record.unlink()
            removed_activations.append(record.name)

print(f"  removed versions: {', '.join(removed_versions) or 'none'}")
print(f"  removed activations: {len(removed_activations)}")
PY
else
  echo "==> keeping old io.dbx.files versions (--keep-old)"
fi

if [ "$RESTART" = 1 ] && [ "$WAS_RUNNING" = 1 ]; then
  echo "==> restarting DBX"
  if [ -n "${DBX_TEST_APP:-}" ] && [ -d "$DBX_TEST_APP" ]; then
    open "$DBX_TEST_APP"
  elif [ -d "$DBX_HOST_WORKTREE/target/debug/bundle/macos/DBX.app" ]; then
    open "$DBX_HOST_WORKTREE/target/debug/bundle/macos/DBX.app"
  else
    open -a DBX
  fi
fi
echo "installed v$VERSION"
