#!/usr/bin/env python3
"""生成/清理 Files Studio 文件类型图标验证样例（纯标准库）。

用法:
  python3 scripts/icon_fixtures.py gen   [--dir DIR] [--force]
  python3 scripts/icon_fixtures.py clean [--dir DIR] [--force]
  python3 scripts/icon_fixtures.py run   [--dir DIR] -- CMD [ARGS...]

- gen: 写入覆盖各文件类型的样例文件，并记录清单 .dbx-icon-fixtures.json。
  目标目录已存在但不含本脚本清单时拒绝写入（防止误碰用户数据），除非 --force。
- clean: 仅删除清单里记录的文件，再尝试移除空目录；清单缺失时拒绝，
  除非 --force（整目录删除，慎用）。
- run: 先 gen，执行 CMD（dev host 等），无论退出码如何最后自动 clean，
  CMD 的退出码原样透传。Ctrl+C 同样会触发清理。POSIX only（依赖 posix_spawn）。

示例:
  python3 scripts/icon_fixtures.py run -- \
    dbx-plugin dev --path . --port 5199 --data-dir .dbx-dev-icons
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import shutil
import sys
from pathlib import Path

MANIFEST_NAME = ".dbx-icon-fixtures.json"

# 覆盖 FileTable 类型图标的代表性扩展名；文件名含中文以覆盖 Unicode 路径。
# 二进制样例只需扩展名正确 + 头部合理（图标仅按扩展名渲染，不做内容探测）。
PNG_1PX = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
)
EMPTY_ZIP = b"PK\x05\x06" + b"\x00" * 18  # 空归档的 EOCD 结构

SAMPLES: dict[str, bytes] = {
    "notes.txt": "图标验证样例（纯文本，回退通用文件图标）\n".encode(),
    "photo.png": PNG_1PX,
    "报告.docx": EMPTY_ZIP,
    "预算表.xlsx": EMPTY_ZIP,
    "路演.pptx": EMPTY_ZIP,
    "手册.pdf": b"%PDF-1.4\n%%EOF\n",
    "backup.zip": EMPTY_ZIP,
    "song.mp3": b"ID3\x03\x00\x00\x00\x00\x00\x00",
    "clip.mp4": b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom",
    "app.py": 'print("icon fixtures")\n'.encode(),
    "style.css": b"body { color: inherit; }\n",
}


def sample_dir(args: argparse.Namespace) -> Path:
    return Path(args.dir).expanduser().resolve()


def gen(args: argparse.Namespace) -> int:
    root = sample_dir(args)
    manifest_path = root / MANIFEST_NAME
    if root.exists() and not manifest_path.exists() and any(root.iterdir()) and not args.force:
        print(f"refusing to write into {root}: not created by this script (use --force)", file=sys.stderr)
        return 2
    root.mkdir(parents=True, exist_ok=True)
    for name, payload in SAMPLES.items():
        (root / name).write_bytes(payload)
    manifest_path.write_text(json.dumps({"created": sorted(SAMPLES)}, ensure_ascii=False, indent=2) + "\n")
    print(f"generated {len(SAMPLES)} sample files in {root}")
    return 0


def clean(args: argparse.Namespace) -> int:
    root = sample_dir(args)
    manifest_path = root / MANIFEST_NAME
    if not root.exists():
        print(f"nothing to clean: {root} does not exist")
        return 0
    if manifest_path.exists():
        created = json.loads(manifest_path.read_text()).get("created", [])
        for name in created:
            (root / name).unlink(missing_ok=True)
        manifest_path.unlink(missing_ok=True)
        remaining = [item for item in root.iterdir() if item.is_file()]
        if remaining:
            print(f"kept {root}: unexpected files remain ({', '.join(item.name for item in remaining)})", file=sys.stderr)
            return 1
        root.rmdir()
        print(f"cleaned {len(created)} sample files and removed {root}")
        return 0
    if args.force:
        shutil.rmtree(root)
        print(f"forced removal of {root}")
        return 0
    print(f"refusing to remove {root}: no manifest from this script (use --force)", file=sys.stderr)
    return 2


def spawn_wait(argv: list[str]) -> int:
    """按参数列表原样执行外部命令并等待退出（无 shell 解析）。"""
    if os.name != "posix":
        print("run mode requires posix_spawn; use gen/clean on this platform", file=sys.stderr)
        return 3
    resolved = shutil.which(argv[0])
    if resolved is None:
        print(f"command not found: {argv[0]}", file=sys.stderr)
        return 127
    pid = os.posix_spawn(resolved, [argv[0], *argv[1:]], os.environ)
    _, status = os.waitpid(pid, 0)
    return os.waitstatus_to_exitcode(status)


def run(args: argparse.Namespace) -> int:
    gen_args = argparse.Namespace(dir=args.dir, force=args.force)
    code = gen(gen_args)
    if code != 0:
        return code
    try:
        return spawn_wait(args.command)
    finally:
        clean(gen_args)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="action", required=True)
    default_dir = "/tmp/dbx-icons-demo" if os.name != "nt" else r"C:\temp\dbx-icons-demo"
    for name in ("gen", "clean", "run"):
        cmd = sub.add_parser(name)
        cmd.add_argument("--dir", default=default_dir, help=f"样例目录（默认 {default_dir}）")
        cmd.add_argument("--force", action="store_true", help="gen: 允许写入陌生目录；clean: 整目录删除")
        if name == "run":
            cmd.add_argument("command", nargs=argparse.REMAINDER, help="-- 之后为要包裹的命令")
    args = parser.parse_args()
    if args.action == "run":
        if args.command and args.command[0] == "--":
            args.command = args.command[1:]
        if not args.command:
            parser.error("run 需要 `-- CMD` 形式的命令")
    return {"gen": gen, "clean": clean, "run": run}[args.action](args)


if __name__ == "__main__":
    raise SystemExit(main())
