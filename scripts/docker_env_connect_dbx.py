#!/usr/bin/env python3
"""把 scripts/docker_env.sh 起的 docker 测试存储连接注入本地 DBX 宿主。

读取状态目录（默认 ~/.dbx-files-docker-test，可用 --state-dir 或
DBX_FILES_DOCKER_ENV_DIR 覆盖）里的 env.conf 与凭据文件，按名字 upsert 六条
io.dbx.files 存储连接：docker-minio / docker-sftp / docker-sftp-native /
docker-smb / docker-webdav / docker-ftp。重复执行会替换同名连接。

安全边界：只写本机 DBX 宿主库；写之前自动备份；DBX 运行中拒绝执行（宿主
只在启动时加载连接，运行中写入既不生效也可能被覆盖）。凭据仅写入宿主本地
connection_secrets 表，不经过任何网络。

用法:
  python3 scripts/docker_env_connect_dbx.py           # 注入/更新六条连接
  python3 scripts/docker_env_connect_dbx.py --remove  # 删除这六条连接
  python3 scripts/docker_env_connect_dbx.py --list    # 只打印将要写入的内容
"""
import argparse
import json
import pathlib
import sqlite3
import subprocess
import sys
import time
import uuid

STATE_DIR = pathlib.Path(
    pathlib.Path.home(), ".dbx-files-docker-test"
)
DB = pathlib.Path.home() / "Library/Application Support/com.dbx.app/dbx.db"
NAMES = (
    "docker-minio", "docker-sftp", "docker-sftp-native",
    "docker-smb", "docker-webdav", "docker-ftp",
)


def load_state(state_dir: pathlib.Path) -> dict:
    env = {}
    for line in (state_dir / "env.conf").read_text().splitlines():
        if "=" in line and not line.strip().startswith("#"):
            key, _, value = line.partition("=")
            env[key.strip()] = value.strip()
    state = dict(env)
    for key in ("minio", "sftp", "smb", "webdav", "ftp"):
        state[f"{key}_password"] = (state_dir / f"{key}_password").read_text()
    state["minio_bucket"] = (state_dir / "minio_bucket").read_text().strip()
    state["sftp_key_path"] = str(state_dir / "sftp_key")
    return state


def dbx_running() -> bool:
    return subprocess.run(
        ["pgrep", "-f", "DBX.app/Contents/MacOS/dbx"],
        stdout=subprocess.DEVNULL,
    ).returncode == 0


def connection_rows(state: dict):
    """(name, protocol, external 覆盖, {secret 字段: 值}) 六元组列表。"""
    minio_ep = f"http://127.0.0.1:{state['MINIO_PORT']}"
    sftp_ep = f"ssh://{state['SFTP_USER']}@127.0.0.1:{state['SFTP_PORT']}"
    smb_ep = f"127.0.0.1:{state['SMB_PORT']}"
    webdav_ep = f"http://127.0.0.1:{state['WEBDAV_PORT']}/dav"
    ftp_ep = f"ftp://127.0.0.1:{state['FTP_PORT']}"
    return [
        ("docker-minio", "s3",
         {"endpoint": minio_ep, "bucket": state["minio_bucket"],
          "access_key_id": state["MINIO_USER"], "region": "us-east-1"},
         {"secret_access_key": state["minio_password"]}),
        ("docker-sftp", "sftp",
         {"endpoint": sftp_ep, "key": state["sftp_key_path"]}, {}),
        ("docker-sftp-native", "sftp-native",
         {"endpoint": sftp_ep, "username": state["SFTP_USER"]},
         {"password": state["sftp_password"]}),
        ("docker-smb", "smb",
         {"endpoint": smb_ep, "share": state["SMB_SHARE"],
          "username": state["SMB_USER"]},
         {"password": state["smb_password"]}),
        ("docker-webdav", "webdav",
         {"endpoint": webdav_ep, "username": state["WEBDAV_USER"]},
         {"password": state["webdav_password"]}),
        ("docker-ftp", "ftp",
         {"endpoint": ftp_ep, "user": state["FTP_USER"]},
         {"password": state["ftp_password"]}),
    ]


BASE_EXTERNAL = {
    "protocol": "", "root": "", "lock_to_root": False, "service": "",
    "config": "{}", "bucket": "", "endpoint": "", "share": "", "region": "",
    "access_key_id": "", "username": "", "domain": "", "user": "", "key": "",
    "known_hosts_strategy": "Tolerate", "read_only": False, "allow_delete": True,
    "connection_mode": "direct", "dbx_ssh_connection": "", "timeout_secs": 30,
    "enable_virtual_host_style": False,
}


def build_config(name: str, protocol: str, external: dict, secrets: dict) -> dict:
    ext = dict(BASE_EXTERNAL)
    ext["protocol"] = protocol
    ext.update(external)
    return {
        "id": str(uuid.uuid4()), "name": name, "db_type": "plugin",
        "driver_profile": "plugin", "driver_label": "存储", "url_params": "",
        "host": "", "port": 0, "username": "", "password": "", "database": None,
        "color": "", "connect_timeout_secs": 10, "query_timeout_secs": 60,
        "idle_timeout_secs": 60, "keepalive_interval_secs": 30, "ssl": False,
        "sysdba": False, "connection_string": None, "external_config": ext,
        "plugin_id": "io.dbx.files",
        "plugin_connection_provider": "io.dbx.files.connection",
        "plugin_connection_type": "storage",
        "connection_secrets": {k: "" for k in secrets},
        "jdbc_driver_class": None, "jdbc_driver_paths": [],
        "save_password": True, "database_info": {"productName": "存储"},
    }, secrets


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--state-dir", default=str(STATE_DIR))
    parser.add_argument("--remove", action="store_true", help="删除六条 docker-* 连接")
    parser.add_argument("--list", action="store_true", help="只打印将要写入的内容")
    args = parser.parse_args()
    state_dir = pathlib.Path(args.state_dir).expanduser()

    if not (state_dir / "env.conf").exists():
        print(f"FAIL: {state_dir} 未初始化，先执行 scripts/docker_env.sh up", file=sys.stderr)
        return 1
    if not DB.exists():
        print(f"FAIL: 未找到 DBX 宿主库 {DB}", file=sys.stderr)
        return 1
    if not args.remove and not args.list and dbx_running():
        print("FAIL: DBX 正在运行。请先退出 DBX（宿主只在启动时加载连接），再执行本脚本。", file=sys.stderr)
        return 2

    state = load_state(state_dir)
    specs = connection_rows(state)

    if args.list:
        for name, protocol, external, _ in specs:
            shown = {k: ("***" if k.endswith(("password", "key", "id")) and v else v)
                     for k, v in external.items()}
            print(f"{name}  {protocol}  {shown}")
        return 0

    backup = DB.with_name(f"dbx.db.backup-dockerconn-{time.strftime('%Y%m%d-%H%M%S')}")
    backup.write_bytes(DB.read_bytes())

    db = sqlite3.connect(DB)
    db.execute("BEGIN IMMEDIATE")
    for name in NAMES:
        for (cid,) in db.execute(
            "SELECT id FROM connections WHERE json_extract(config_json,'$.name')=?", (name,)
        ).fetchall():
            db.execute("DELETE FROM connection_secrets WHERE connection_id=?", (cid,))
            db.execute("DELETE FROM connections WHERE id=?", (cid,))
    if not args.remove:
        for name, protocol, external, secrets in specs:
            config, secrets = build_config(name, protocol, external, secrets)
            db.execute("INSERT INTO connections (id, config_json) VALUES (?, ?)",
                       (config["id"], json.dumps(config, ensure_ascii=False)))
            for key, value in secrets.items():
                db.execute(
                    "INSERT INTO connection_secrets (connection_id, key, secret) VALUES (?, ?, ?)",
                    (config["id"], f"plugin_connection.{key}", value))
    db.commit()

    action = "removed" if args.remove else "upserted"
    print(f"backup: {backup}")
    for name in NAMES:
        print(f"{action}: {name}")
    if not args.remove:
        print("下一步: 启动 DBX，侧边栏搜索 docker 即可见六条连接。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
