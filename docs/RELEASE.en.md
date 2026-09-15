# Release checklist (files-v*)

The end-to-end path to one Files Studio plugin release. The release
automation lives in `.github/workflows/release.yml`: creating a GitHub
Release named `files-v<manifest.version>` (or pushing that tag first)
triggers validation, five-platform builds and asset upload automatically.
This checklist covers the parts a human must gate.

## 1. Version (must match in two places)

- `"version"` in `manifest.json` (the release tag is `files-v<that value>`)
- `version` in `backend/Cargo.toml` (update `Cargo.lock` accordingly)

`scripts/validate_repo.py` rejects a repository where the two drift apart.

## 2. Local pre-release verification (all green before shipping)

```bash
python3 scripts/validate_repo.py                    # repo contracts + version match
node scripts/connection-forms/verify.mjs            # connection form contract (conflict guards)
pnpm --dir frontend install --frozen-lockfile
pnpm --dir frontend typecheck && pnpm --dir frontend test
pnpm --dir frontend build                           # output lands in ui/, must be committed
cargo test --locked --manifest-path backend/Cargo.toml   # includes the form×engine matrix
cargo build --locked --release --manifest-path backend/Cargo.toml
python3 scripts/smoke_mcp.py --binary backend/target/release/dbx-plugin-files
```

Optional but recommended (needs Docker; full seven-protocol wire smoke against
real MinIO/OpenSSH/Samba/mod_dav/pyftpdlib containers):

```bash
DBX_PLUGIN_SIDECAR="$PWD/backend/target/release/dbx-plugin-files" \
  scripts/container_smoke.sh
```

## 3. Commit and tag

1. Commit everything (including the `ui/` build output — CI enforces its
   freshness).
2. Push to `main` and confirm CI is green (validate / frontend / backend /
   container-smoke / five-platform candidates / candidates-check).
3. Create the tag `files-v<version>` (e.g. `files-v0.1.57`) and a GitHub
   Release with the same name (`release.yml` only reacts to the `files-v`
   prefix).

## 4. Post-release verification

- The Release page shows `*.dbxp` packages for **all five targets** plus
  `release-candidates.json` (linux-x64 / linux-arm64 / darwin-arm64 /
  darwin-x64 / windows-x64).
- `release-candidates.json` carries all five platform digests (produced and
  verified by `scripts/check_candidates.py --write-release-candidates`).
- The release workflow run is green; if any build-matrix leg fails, nothing
  is uploaded (publish requires the full build matrix plus the candidate
  count check).

## 5. Troubleshooting

- **Tag pushed but no workflow ran**: the tag must start with `files-v` and
  match the manifest version.
- **CI ui/ freshness gate failed**: run `pnpm --dir frontend build` again and
  commit the `ui/` output.
- **container-smoke failed**: reproduce locally with
  `scripts/container_smoke.sh` (`--keep` retains the containers); failures
  are usually an upstream test-container image change — pin the image digest
  if the plugin code is innocent.
