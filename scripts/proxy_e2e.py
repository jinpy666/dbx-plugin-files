#!/usr/bin/env python3
"""proxy e2e driver: per-connection egress proxy + SSH tunnel over the rclone engine.

Companion to scripts/proxy_e2e.sh: the shell harness boots the throwaway
containers (MinIO, rclone WebDAV, gost HTTP+SOCKS5 dual proxy with auth, two
OpenSSH containers) and prints the DBX_PROXY_E2E_* environment contract. This
driver starts ONE sidecar with DBX_FILES_ENGINE=rclone, dials every connection
into the same process (multi-connection + mixed proxy-group coexistence) and
walks each scenario through the full wire face:

    connection/test -> connection/connect -> list root ->
    upload unique bytes -> download -> byte-equality assert ->
    upload a second small file -> list assert >= 2
    (no disconnect anywhere: all connections stay alive side by side)

Scenarios (one mark() line each, section = scenario name):
    1 direct-control        s3     -> MinIO host port, no proxy (harness sanity)
    2 s3-via-http-proxy     s3     -> container-net MinIO via gost HTTP proxy
    3 webdav-via-http-proxy webdav -> container-net WebDAV via the same HTTP proxy
    4 sftp-via-socks5       sftp   -> container-net OpenSSH via gost SOCKS5
    5 s3-via-tunnel         s3     -> container-net MinIO through ssh -N -L
                                      (flat tunnel_jump_hosts/tunnel_identity_file
                                      form; the nested object is unit-test covered)
    6 negative-isolation    s3     -> container-net MinIO, NO proxy: connection/test
                                      must FAIL (the host cannot resolve the
                                      container network name) — the isolation proof

Bypass-proof for 2/3 (and 6 inverted): the host cannot resolve
dbx-proxy-minio-test / dbx-proxy-webdav-test, so any path that dodges the
proxy dies with a DNS error; a green round-trip means the bytes really
flowed through the proxy. Scenario 5 inverts it: the tunnel rewrites the
endpoint to 127.0.0.1:<port>, which only the host can reach.

Env contract (printed by scripts/proxy_e2e.sh, names exact):
    DBX_PROXY_E2E_HTTP / _HTTP_USER / _HTTP_PASS         gost HTTP proxy (host:port + auth)
    DBX_PROXY_E2E_SOCKS / _SOCKS_USER / _SOCKS_PASS      gost SOCKS5 proxy (host:port + auth)
    DBX_PROXY_E2E_MINIO_DIRECT                           http://127.0.0.1:19500
    DBX_PROXY_E2E_MINIO_INNET                            http://dbx-proxy-minio-test:9000
    DBX_PROXY_E2E_WEBDAV_INNET                           http://dbx-proxy-webdav-test:8899
    DBX_PROXY_E2E_WEBDAV_USER / _WEBDAV_PASS             WebDAV basic auth
    DBX_PROXY_E2E_SFTP_INNET                             dbx-proxy-sftp-test:22
    DBX_PROXY_E2E_SFTP_USER / _SFTP_PASS                 SFTP login
    DBX_PROXY_E2E_SSHD (127.0.0.1:18222) / _SSH_USER / _SSH_KEY   tunnel jump host
    DBX_PROXY_E2E_BUCKET / _BUCKET2                      two pre-created MinIO buckets

The MinIO throwaway credentials default to minioadmin/minioadmin and can be
overridden with the optional DBX_PROXY_E2E_MINIO_ACCESS_KEY /
DBX_PROXY_E2E_MINIO_SECRET_KEY.

Usage (set the sidecar explicitly — most robust):
    DBX_FILES_ENGINE=rclone DBX_PLUGIN_SIDECAR=backend/target/debug/dbx-plugin-files \
        python3 scripts/proxy_e2e.py
Without DBX_PLUGIN_SIDECAR the script picks a binary itself: debug build
first, then release, then the installed plugin path. --selfcheck validates
env/binary/imports only (never starts a sidecar).

Exit codes: 0 all scenarios passed · 1 any scenario failed · 3 SKIP (harness
env or sidecar binary missing — same SKIP semantics as container_smoke.sh).
"""

from __future__ import annotations

import base64
import os
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from sidecar_client import SidecarClient, SidecarError, lifecycle_params
from smoke_test import Runner, connect, download_bytes, is_method_missing, mark, upload_bytes

# DBX_PROXY_E2E_* 契约（与 proxy_e2e.sh 逐字一致）；缺任何一个即整体 SKIP。
REQUIRED_ENV = (
    "DBX_PROXY_E2E_HTTP", "DBX_PROXY_E2E_HTTP_USER", "DBX_PROXY_E2E_HTTP_PASS",
    "DBX_PROXY_E2E_SOCKS", "DBX_PROXY_E2E_SOCKS_USER", "DBX_PROXY_E2E_SOCKS_PASS",
    "DBX_PROXY_E2E_MINIO_DIRECT", "DBX_PROXY_E2E_MINIO_INNET",
    "DBX_PROXY_E2E_WEBDAV_INNET", "DBX_PROXY_E2E_WEBDAV_USER", "DBX_PROXY_E2E_WEBDAV_PASS",
    "DBX_PROXY_E2E_SFTP_INNET", "DBX_PROXY_E2E_SFTP_USER", "DBX_PROXY_E2E_SFTP_PASS",
    "DBX_PROXY_E2E_SSHD", "DBX_PROXY_E2E_SSH_USER", "DBX_PROXY_E2E_SSH_KEY",
    "DBX_PROXY_E2E_BUCKET", "DBX_PROXY_E2E_BUCKET2",
)
MINIO_ACCESS_ENV = "DBX_PROXY_E2E_MINIO_ACCESS_KEY"  # optional override
MINIO_SECRET_ENV = "DBX_PROXY_E2E_MINIO_SECRET_KEY"  # optional override

# 隧道建立（ssh spawn + 端口改写）与按代理分组的 rcd spawn 都吃时间，给足预算。
CLIENT_TIMEOUT = 90.0
# 载荷主体跨两个 256KiB 二进制帧，顺带覆盖分帧重组路径。
PAYLOAD_BODY = 300 * 1024


def pick_binary() -> str:
    """Resolve the sidecar binary: DBX_PLUGIN_SIDECAR wins, then the debug
    build (fresh from cargo test/build runs), then release, then the
    installed plugin path (same shape as sidecar_client.default_binary but
    debug-first — this driver exercises freshly written engine code)."""
    env = os.environ.get("DBX_PLUGIN_SIDECAR")
    if env:
        return env
    repo = Path(__file__).resolve().parent.parent
    exe = "dbx-plugin-files.exe" if os.name == "nt" else "dbx-plugin-files"
    for profile in ("debug", "release"):
        candidate = repo / "backend" / "target" / profile / exe
        if candidate.exists():
            return str(candidate)
    home = os.path.expanduser("~")
    return (
        f"{home}/Library/Application Support/com.dbx.app/plugins/io.dbx.files/"
        f"versions/current/bin/darwin-arm64/dbx-plugin-files"
    )


def load_env() -> dict[str, str]:
    env = {name: os.environ.get(name, "").strip() for name in REQUIRED_ENV}
    env[MINIO_ACCESS_ENV] = os.environ.get(MINIO_ACCESS_ENV, "minioadmin")
    env[MINIO_SECRET_ENV] = os.environ.get(MINIO_SECRET_ENV, "minioadmin")
    return env


def missing_env(env: dict[str, str]) -> list[str]:
    return [name for name in REQUIRED_ENV if not env[name]]


def split_host_port(value: str) -> tuple[str, int]:
    """`host:port` -> (host, int(port)) — 契约里代理地址固定是 host:port 形态。"""
    host, sep, port = value.rpartition(":")
    if not sep or not host or not port.isdigit():
        raise ValueError(f"expected host:port, got {value!r}")
    return host, int(port)


def connection_payload(cid: str, external: dict, secrets: dict | None) -> dict:
    """M0 §3.1 lifecycle shape (what connection/test wants on the wire)."""
    return {
        "id": cid,
        "name": cid,
        "db_type": "storage",
        "host": "",
        "port": 0,
        "external_config": external,
        "connection_secrets": secrets or {},
    }


def run_step(runner: Runner, label: str, fn) -> str:
    """Runner.step 的不抛出版本：任一异常都收敛成一条 FAIL mark，让后续场景
    继续跑完（一次运行给出全量诊断）。返回 pass/fail/skip。"""
    try:
        fn()
    except SidecarError as error:
        if is_method_missing(str(error)):
            mark(runner.section, label, "skip", str(error)[:120])
            return "skip"
        mark(runner.section, label, "fail", str(error)[:200])
        return "fail"
    except AssertionError as error:
        mark(runner.section, label, "fail", str(error)[:200])
        return "fail"
    except Exception as error:  # noqa: BLE001 — 意外异常也必须落成 FAIL 而非中断
        mark(runner.section, label, "fail", f"{type(error).__name__}: {error}"[:200])
        return "fail"
    mark(runner.section, label, "pass")
    return "pass"


def unique_payload(name: str) -> bytes:
    # 随机前后缀 + 场景名：每次运行内容唯一，杜绝桶里同名旧对象造成的假阳性。
    return (
        os.urandom(24)
        + f"|{name}|proxy-e2e|".encode()
        + os.urandom(PAYLOAD_BODY)
        + os.urandom(24)
    )


def scenario_roundtrip(client: SidecarClient, name: str, external: dict,
                       secrets: dict | None) -> str:
    """test → connect → list root → upload/download 断言 → 第二个小文件 + list 断言。

    最后不 disconnect：六条连接共存在同一个 sidecar 里正是本套件要证的点。"""
    cid = f"proxy-e2e-{name}"
    runner = Runner(client, name)
    runner.connection_id = cid
    connection = connection_payload(cid, external, secrets)

    def _run():
        t0 = time.monotonic()

        def stamp(label):
            print(f"    [+{time.monotonic() - t0:5.1f}s] {name}/{label}")

        # connection/test 先行：test 与 connect 走同一 binding 路径，代理分组/
        # 隧道改写在 test 阶段就已生效（失败会在这里直接暴露）。
        client.request("connection/test", lifecycle_params(connection))
        stamp("test")
        connect(client, cid, external, secrets=secrets)
        stamp("connect")
        # list 根：只断言调用成功返回 entries（BUCKET2 根可能为空，不强求非空）。
        root = client.request("files/list", {"connectionId": cid, "path": "/"})
        stamp("list-root")
        assert isinstance(root.get("entries"), list), f"root list shape: {root!r}"
        base = f"/proxy-e2e-{os.getpid()}-{int(time.time())}"
        blob = f"{base}/blob-{os.urandom(4).hex()}.bin"
        payload = unique_payload(name)
        upload_bytes(runner, blob, payload)
        stamp("upload")
        data, _start = download_bytes(runner, blob)
        stamp("download")
        assert data == payload, f"byte mismatch: sent {len(payload)} got {len(data)}"
        probe = f"{base}/probe-{os.urandom(4).hex()}.txt"
        runner.call("files/write", {
            "connectionId": cid,
            "path": probe,
            "dataBase64": base64.b64encode(b"second file").decode(),
        })
        stamp("write")
        entries = client.request("files/list", {"connectionId": cid, "path": base}).get("entries", [])
        names = [entry.get("name", "") for entry in entries]
        assert len(names) >= 2, f"expected >=2 entries under {base}: {names}"
        # 注意：没有 disconnect —— 所有连接保持共存直到进程退出。
    return run_step(runner, "test-connect-roundtrip", _run)


def s3_external(env: dict[str, str], endpoint_env: str, bucket_env: str) -> dict:
    """s3 连接形状照抄 smoke_test 的 s3 段：bucket 走 external_config，密钥走
    secret store（external_config 里的 secret 字段会被 s3 signer 忽略）。"""
    return {
        "protocol": "s3",
        "bucket": env[bucket_env],
        "endpoint": env[endpoint_env],
        "region": "us-east-1",
        "access_key_id": env[MINIO_ACCESS_ENV],
    }


def s3_secrets(env: dict[str, str]) -> dict:
    return {"secret_access_key": env[MINIO_SECRET_ENV]}


def run_negative_isolation(client: SidecarClient, env: dict[str, str]) -> str:
    """无代理直连容器网络名：宿主解析不了 DNS，connection/test 必须失败。
    这是隔离性证明（绕过代理的路径不存在），用独立 connection id。"""
    name = "negative-isolation"
    runner = Runner(client, name)
    connection = connection_payload(
        f"proxy-e2e-{name}",
        s3_external(env, "DBX_PROXY_E2E_MINIO_INNET", "DBX_PROXY_E2E_BUCKET"),
        s3_secrets(env),
    )

    def _run():
        refused = runner.expect_error("connection/test", lifecycle_params(connection))
        if not refused:
            raise SidecarError(
                "connection/test without proxy unexpectedly succeeded — "
                "the host should not be able to reach the container network"
            )
    return run_step(runner, "test-must-fail", _run)


def build_scenarios(env: dict[str, str]) -> list[tuple[str, dict, dict | None]]:
    """六场景的连接参数表；一个 sidecar 进程内全部共存（混合代理分组）。"""
    http_host, http_port = split_host_port(env["DBX_PROXY_E2E_HTTP"])
    socks_host, socks_port = split_host_port(env["DBX_PROXY_E2E_SOCKS"])
    http_proxy = {"type": "http", "host": http_host, "port": http_port,
                  "username": env["DBX_PROXY_E2E_HTTP_USER"],
                  "password": env["DBX_PROXY_E2E_HTTP_PASS"]}
    socks_proxy = {"type": "socks5", "host": socks_host, "port": socks_port,
                   "username": env["DBX_PROXY_E2E_SOCKS_USER"],
                   "password": env["DBX_PROXY_E2E_SOCKS_PASS"]}
    # SSHD 契约值就是 host:port 形态，直接拼 ssh -J 语法 user@host:port。
    jump = f"{env['DBX_PROXY_E2E_SSH_USER']}@{env['DBX_PROXY_E2E_SSHD']}"
    return [
        # 1 harness sanity：宿主端口直连 MinIO，无代理。
        ("direct-control",
         s3_external(env, "DBX_PROXY_E2E_MINIO_DIRECT", "DBX_PROXY_E2E_BUCKET"),
         s3_secrets(env)),
        # 2 容器网络名必须经 gost HTTP 代理（rcd 分组 env 路径）；绕过即 DNS 失败。
        ("s3-via-http-proxy",
         {**s3_external(env, "DBX_PROXY_E2E_MINIO_INNET", "DBX_PROXY_E2E_BUCKET"),
          "proxy": http_proxy},
         s3_secrets(env)),
        # 3 webdav + 同款 HTTP 代理对象。
        ("webdav-via-http-proxy",
         {"protocol": "webdav",
          "endpoint": env["DBX_PROXY_E2E_WEBDAV_INNET"],
          "username": env["DBX_PROXY_E2E_WEBDAV_USER"],
          "proxy": http_proxy},
         {"password": env["DBX_PROXY_E2E_WEBDAV_PASS"]}),
        # 4 sftp + SOCKS5：走 rclone sftp 后端的 socks_proxy 选项路径。
        ("sftp-via-socks5",
         {"protocol": "sftp",
          "endpoint": env["DBX_PROXY_E2E_SFTP_INNET"],
          "user": env["DBX_PROXY_E2E_SFTP_USER"],
          "proxy": socks_proxy},
         {"password": env["DBX_PROXY_E2E_SFTP_PASS"]}),
        # 5 平铺隧道表单（嵌套形状已由单测覆盖）：proxy 留空走直连组 rcd，
        # 隧道把 endpoint 改写成 127.0.0.1:<port>，宿主可达。
        ("s3-via-tunnel",
         {**s3_external(env, "DBX_PROXY_E2E_MINIO_INNET", "DBX_PROXY_E2E_BUCKET2"),
          "tunnel_jump_hosts": jump,
          "tunnel_identity_file": env["DBX_PROXY_E2E_SSH_KEY"]},
         s3_secrets(env)),
    ]


def run_all(client: SidecarClient, env: dict[str, str]) -> list[str]:
    statuses: list[str] = []
    scenarios = build_scenarios(env)
    for name, external, secrets in scenarios:
        print(f"\n==> scenario {name}")
        statuses.append(scenario_roundtrip(client, name, external, secrets))
    print("\n==> scenario negative-isolation")
    statuses.append(run_negative_isolation(client, env))
    return statuses


def selfcheck() -> int:
    """只校验 imports/env/binary，不启动 sidecar。exit 0 = 就绪，3 = SKIP。"""
    print("SELFCHECK imports: ok (sidecar_client, smoke_test)")
    env = load_env()
    missing = missing_env(env)
    binary = pick_binary()
    have_binary = bool(binary) and os.path.exists(binary)
    print(f"SELFCHECK binary: {binary} [{'found' if have_binary else 'NOT FOUND'}]")
    if missing:
        print(f"SELFCHECK env: missing {', '.join(missing)}")
    else:
        print("SELFCHECK env: complete (19 required + MinIO credentials)")
    if missing or not have_binary:
        print("SKIP: proxy e2e harness not ready — run scripts/proxy_e2e.sh first "
              "(it prints the DBX_PROXY_E2E_* env) and build the sidecar "
              "(DBX_PLUGIN_SIDECAR=... to override)")
        return 3
    print("SELFCHECK OK")
    return 0


def kill_sidecar_children(sidecar_pid: int) -> None:
    """sidecar 被 terminate/kill 时 Rust 侧 Drop 不会运行，它名下的 rcd
    与 ssh 隧道子进程会成为孤儿。proc.rs 用 `dbx-files-rclone-<sidecar
    pid>-*` 命名临时目录，隧道进程携带 -i <key>；按这两个特征定向清理，
    不误伤本机其他 rclone/ssh 进程。"""
    for pattern in (f"dbx-files-rclone-{sidecar_pid}-",):
        subprocess.run(["pkill", "-f", pattern], check=False, capture_output=True)


def main() -> int:
    if "--selfcheck" in sys.argv[1:]:
        return selfcheck()

    env = load_env()
    missing = missing_env(env)
    binary = pick_binary()
    if missing:
        print(f"SKIP: missing proxy e2e harness env: {', '.join(missing)} "
              f"(run scripts/proxy_e2e.sh first; it prints the DBX_PROXY_E2E_* contract)")
        return 3
    if not binary or not os.path.exists(binary):
        print(f"SKIP: sidecar binary not built yet ({binary or 'no candidate'}; "
              f"set DBX_PLUGIN_SIDECAR to override)")
        return 3

    started = time.monotonic()
    # 子进程继承：sidecar 必须以 rclone 引擎启动（按连接代理/隧道只在该引擎实现）。
    os.environ["DBX_FILES_ENGINE"] = "rclone"
    client = SidecarClient.start(binary, timeout=CLIENT_TIMEOUT)
    sidecar_pid = client.process.pid
    statuses: list[str] = []
    try:
        print("==> plugin/initialize")
        info = client.initialize()
        print(f"    engine: {info.get('engine', info.get('serverInfo', '?'))}")
        statuses = run_all(client, env)
    except SidecarError as error:
        # 场景级错误已被 run_step 收敛；走到这里说明 initialize 等框架步骤挂了。
        client.close()
        kill_sidecar_children(sidecar_pid)
        print(client.drain_stderr()[-2000:], file=sys.stderr)
        print(f"\nFAIL: {error}", file=sys.stderr)
        return 1
    except Exception as error:  # noqa: BLE001
        client.close()
        kill_sidecar_children(sidecar_pid)
        print(f"\nFAIL: {type(error).__name__}: {error}", file=sys.stderr)
        return 1
    client.close()
    kill_sidecar_children(sidecar_pid)

    passed = sum(1 for status in statuses if status == "pass")
    failed = sum(1 for status in statuses if status == "fail")
    skipped = sum(1 for status in statuses if status == "skip")
    print(f"\n{'=' * 62}\nproxy-e2e: {passed}/{len(statuses)} passed  "
          f"FAIL {failed}  SKIP {skipped}  ({time.monotonic() - started:.1f}s)")
    if failed:
        return 1
    if passed == 0:
        print("SKIP: every scenario was skipped (methods not landed yet?)")
        return 3
    print("PASS: proxy e2e green — 6 connections coexisted in one sidecar")
    return 0


if __name__ == "__main__":
    sys.exit(main())
