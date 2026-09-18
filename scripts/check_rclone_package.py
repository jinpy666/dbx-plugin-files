#!/usr/bin/env python3
"""Assert a packaged .dbxp candidate bundles the pinned rclone binary.

Usage: check_rclone_package.py <package.dbxp> --target <dbx-target>

Verifies, for the given target:
- bin/<target>/rclone[.exe] is present next to the sidecar binary,
- its zip entry carries the unix exec mode (0755) the packager records,
- checksums.json lists it and the recorded sha256 matches the entry bytes.

Exits non-zero with a diagnosis on any violation. Read-only.
"""

import argparse
import hashlib
import json
import sys
import zipfile

TARGETS = ("linux-x64", "linux-arm64", "darwin-arm64", "darwin-x64", "windows-x64")
EXEC_MODE = 0o755
CHECKSUMS_NAME = "checksums.json"


def expected_names(target: str) -> tuple[str, str]:
    exe = ".exe" if target.startswith("windows") else ""
    return f"bin/{target}/rclone{exe}", f"bin/{target}/dbx-plugin-files{exe}"


def zip_unix_mode(info: zipfile.ZipInfo) -> int | None:
    mode = info.external_attr >> 16
    return mode & 0o777 if mode else None


def fail(messages: list[str], problem: str) -> None:
    messages.append(problem)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("package", help="path to the .dbxp candidate")
    parser.add_argument("--target", required=True, choices=TARGETS)
    args = parser.parse_args()

    rclone_name, sidecar_name = expected_names(args.target)
    problems: list[str] = []

    try:
        archive = zipfile.ZipFile(args.package)
    except (OSError, zipfile.BadZipFile) as error:
        print(f"check_rclone_package: cannot open {args.package}: {error}", file=sys.stderr)
        return 1

    names = set(archive.namelist())
    if rclone_name not in names:
        fail(problems, f"missing bundled rclone: {rclone_name} is not in the package")
    if sidecar_name not in names:
        fail(problems, f"missing sidecar: {sidecar_name} is not in the package")

    rclone_info = archive.getinfo(rclone_name) if rclone_name in names else None
    if rclone_info is not None:
        mode = zip_unix_mode(rclone_info)
        if mode != EXEC_MODE:
            fail(problems, f"{rclone_name} has unix mode {oct(mode) if mode is not None else 'unset'}, want {oct(EXEC_MODE)}")
        if rclone_info.file_size == 0:
            fail(problems, f"{rclone_name} is empty")

    recorded = None
    if CHECKSUMS_NAME in names:
        try:
            checksums = json.loads(archive.read(CHECKSUMS_NAME))
            files = checksums.get("files", {})
            recorded = files.get(rclone_name)
            if recorded is None:
                fail(problems, f"{CHECKSUMS_NAME} has no entry for {rclone_name}")
        except (ValueError, KeyError) as error:
            fail(problems, f"{CHECKSUMS_NAME} is unreadable: {error}")
    else:
        fail(problems, f"{CHECKSUMS_NAME} missing from package")

    if rclone_info is not None and recorded is not None:
        digest = hashlib.sha256(archive.read(rclone_name)).hexdigest()
        if digest != recorded:
            fail(problems, f"{rclone_name} sha256 mismatch: checksums.json says {recorded}, actual {digest}")

    if problems:
        for problem in problems:
            print(f"check_rclone_package: {problem}", file=sys.stderr)
        return 1

    size_mib = rclone_info.file_size / (1024 * 1024)
    print(
        f"check_rclone_package: OK — {rclone_name} present ({size_mib:.1f} MiB), "
        f"mode {oct(EXEC_MODE)}, checksums.json verified"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
