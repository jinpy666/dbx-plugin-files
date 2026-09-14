#!/usr/bin/env bash
# files 插件容器化冒烟：MinIO（s3 段）+ OpenSSH（sftp 段 + sftp-native 密码段）
# + Samba（smb 段）+ Apache mod_dav（webdav 段）+ pyftpdlib（ftp 段），
# 消除 smoke SKIP——七种连接协议全部有真实容器覆盖。
#
# 设计（合规要点）：
# - 无任何硬编码凭据：MinIO root 凭据、Samba/WebDAV/FTP 测试账号密码、SFTP
#   密码全部运行时随机生成，仅存在于进程环境与临时 env 文件，跑完删除；
#   SFTP 私钥运行时生成于临时目录，公钥经容器创建时的 PUBLIC_KEY 环境变量
#   注入（P-FILES §3 合规复现路径，不做运行时 authorized_keys 写入）。
# - 不拼接 shell 字符串：mc 用原生 MC_HOST_ 环境变量；WebDAV htpasswd 经
#   容器内 env 变量生成；所有可变值走 env/参数。
# - 镜像选型均需 arm64 可用（本仓开发机 Apple Silicon）：httpd:2.4-alpine、
#   python:3-alpine（pyftpdlib 运行时安装）、ghcr.io/servercontainers/samba、
#   linuxserver/openssh-server、minio/minio 全部多架构。
# - 一切容器 --rm + 脚本退出时 docker rm -f 兜底；已有同名测试容器会被替换
#   （本脚本历史运行创建的专用测试容器）。
# - docker 不可用时整体退出码 3（调用方按 SKIP 语义处理），不伪造通过。
#
# 用法：
#   scripts/container_smoke.sh              # 全流程：起容器 → smoke → 清理
#   scripts/container_smoke.sh --keep       # 调试：保留容器与密钥目录（打印路径）
set -euo pipefail
cd "$(dirname "$0")/.."

KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

if ! command -v docker >/dev/null 2>&1; then
  echo "SKIP: docker not available" >&2
  exit 3
fi

TMPDIR_SMOKE="$(mktemp -d /tmp/dbx-files-smoke.XXXXXX)"
MINIO_PORT="${DBX_FILES_SMOKE_MINIO_PORT:-19000}"
SFTP_PORT="${DBX_FILES_SMOKE_SFTP_PORT:-2299}"
SMB_PORT="${DBX_FILES_SMOKE_SMB_PORT:-2445}"
WEBDAV_PORT="${DBX_FILES_SMOKE_WEBDAV_PORT:-18080}"
FTP_PORT="${DBX_FILES_SMOKE_FTP_PORT:-2121}"
FTP_PASV_MIN=30100
FTP_PASV_MAX=30109
MINIO_USER="dbxsmoke"
MINIO_PASSWORD="$(openssl rand -hex 24 2>/dev/null || head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')"
MINIO_BUCKET="dbx-files-smoke-$(date +%s)"
SFTP_USER="sftpuser"
SFTP_PASSWORD="$(openssl rand -hex 24 2>/dev/null || head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')"
SMB_USER="smoketest"
SMB_SHARE="smoke"
SMB_PASSWORD="$(openssl rand -hex 24 2>/dev/null || head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')"
WEBDAV_USER="webdavsmoke"
WEBDAV_PASSWORD="$(openssl rand -hex 24 2>/dev/null || head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')"
FTP_USER="ftpsmoke"
FTP_PASSWORD="$(openssl rand -hex 24 2>/dev/null || head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')"

cleanup() {
  if [ "$KEEP" = 1 ]; then
    echo "KEEP: containers left running; key dir: $TMPDIR_SMOKE"
    return
  fi
  docker rm -f dbx-files-minio-test dbx-files-sftp-test dbx-files-samba-test \
    dbx-files-webdav-test dbx-files-ftp-test >/dev/null 2>&1 || true
  rm -rf "$TMPDIR_SMOKE"
}
trap cleanup EXIT

# 一次性 SFTP 密钥对（ed25519，仅存在于临时目录）
ssh-keygen -t ed25519 -N "" -f "$TMPDIR_SMOKE/smoke_key" -C dbx-files-smoke >/dev/null

echo "==> starting MinIO (:${MINIO_PORT})"
docker rm -f dbx-files-minio-test >/dev/null 2>&1 || true
# /data 走 tmpfs：MinIO 按剩余空间百分比拒绝写入（XMinioStorageFull，新版不可
# 配置关闭）；宿主盘富余度低时 sparse Docker 虚拟盘会误触发该保护。测试容器
# 数据量仅数 MB 且 --rm 即焚，tmpfs 完全够用且不落盘。O_DIRECT 需关（tmpfs
# 不支持），env 形式 MINIO_API_ODIRECT 对应 api/odirect 配置键。
docker run -d --rm --name dbx-files-minio-test \
  -p "${MINIO_PORT}:9000" \
  --tmpfs /data:size=4g \
  -e "MINIO_ROOT_USER=${MINIO_USER}" \
  -e "MINIO_ROOT_PASSWORD=${MINIO_PASSWORD}" \
  -e "MINIO_API_ODIRECT=off" \
  minio/minio server /data >/dev/null

echo "==> waiting for MinIO health"
for _ in $(seq 1 30); do
  if curl -fsS "http://127.0.0.1:${MINIO_PORT}/minio/health/live" >/dev/null 2>&1; then break; fi
  sleep 1
done
curl -fsS "http://127.0.0.1:${MINIO_PORT}/minio/health/live" >/dev/null || {
  echo "FAIL: MinIO did not become healthy" >&2; exit 1; }

echo "==> creating bucket ${MINIO_BUCKET} (minio/mc, MC_HOST_ 原生配置，无 shell 拼接)"
MC_URL="$(printf 'http://%s:%s@minio:9000' "$MINIO_USER" "$MINIO_PASSWORD")"
# 注意：minio/mc 镜像默认 entrypoint 会吞掉参数（静默 no-op），必须显式
# --entrypoint mc；否则 mc mb "成功"退出但 bucket 并不存在。
docker run --rm --link dbx-files-minio-test:minio \
  -e "MC_HOST_local=${MC_URL}" \
  --entrypoint mc \
  minio/mc mb "local/${MINIO_BUCKET}" >/dev/null

echo "==> starting OpenSSH test server (:${SFTP_PORT}, 密码+密钥双认证)"
docker rm -f dbx-files-sftp-test >/dev/null 2>&1 || true
# PASSWORD_ACCESS + USER_PASSWORD：同一容器同时服务 sftp 段（OpenDAL keyfile）
# 与 sftp-native 段（russh 密码认证）。
docker run -d --rm --name dbx-files-sftp-test \
  -p "${SFTP_PORT}:2222" \
  -e "PUBLIC_KEY=$(cat "$TMPDIR_SMOKE/smoke_key.pub")" \
  -e "USER_NAME=${SFTP_USER}" \
  -e "PASSWORD_ACCESS=true" \
  -e "USER_PASSWORD=${SFTP_PASSWORD}" \
  linuxserver/openssh-server >/dev/null

echo "==> waiting for sshd :${SFTP_PORT}"
sshd_up=0
for _ in $(seq 1 30); do
  if nc -z 127.0.0.1 "${SFTP_PORT}" 2>/dev/null; then sshd_up=1; break; fi
  sleep 1
done
[ "$sshd_up" = 1 ] || { echo "FAIL: sshd did not open port ${SFTP_PORT}" >&2; exit 1; }

echo "==> starting Samba test server (:${SMB_PORT}, share ${SMB_SHARE})"
docker rm -f dbx-files-samba-test >/dev/null 2>&1 || true
# ghcr.io/servercontainers/samba（arm64 可用）：共享经 SAMBA_VOLUME_CONFIG_*
# 声明，账号经 ACCOUNT_* 注入；共享数据留在容器内（--rm 即随容器销毁，
# 无挂载卷 → 无 uid 权限问题）。avahi/wsdd2/netbios 全关，只留 smbd:445。
docker run -d --rm --name dbx-files-samba-test \
  -p "${SMB_PORT}:445" \
  -e "SAMBA_CONF_SERVER_ROLE=standalone" \
  -e "AVAHI_DISABLE=true" \
  -e "WSDD2_DISABLE=true" \
  -e "NETBIOS_DISABLE=true" \
  -e "SAMBA_VOLUME_CONFIG_${SMB_SHARE}=[${SMB_SHARE}]; path=/shares/${SMB_SHARE}; valid users = ${SMB_USER}; read only = no; browseable = yes" \
  -e "ACCOUNT_${SMB_USER}=${SMB_PASSWORD}" \
  ghcr.io/servercontainers/samba >/dev/null

# 镜像不会自动创建共享路径：缺了它会报 NT_STATUS_BAD_NETWORK_NAME。一次性
# docker exec 建目录并放开权限（777，临时测试容器、跑完即毁、无挂载卷）。
docker exec dbx-files-samba-test \
  sh -c "mkdir -p /shares/${SMB_SHARE} && chmod 777 /shares/${SMB_SHARE}" >/dev/null

echo "==> waiting for smb tree connect :${SMB_PORT}"
smb_up=0
for _ in $(seq 1 30); do
  # nc 只能证明端口开着；真正健康判据是容器内 smbclient 用测试账号完成一次
  # tree connect + 目录枚举（凭据仅出现在容器内进程，随容器销毁）。
  if nc -z 127.0.0.1 "${SMB_PORT}" 2>/dev/null \
    && docker exec dbx-files-samba-test smbclient \
         "//127.0.0.1/${SMB_SHARE}" -U "${SMB_USER}%${SMB_PASSWORD}" \
         -c 'ls' >/dev/null 2>&1; then
    smb_up=1
    break
  fi
  sleep 1
done
[ "$smb_up" = 1 ] || { echo "FAIL: samba did not become ready on port ${SMB_PORT}" >&2; exit 1; }

echo "==> preparing WebDAV config (hand-rolled minimal mod_dav httpd.conf)"
mkdir -p "$TMPDIR_SMOKE/dav-data" "$TMPDIR_SMOKE/dav-runtime"
chmod 777 "$TMPDIR_SMOKE/dav-data" "$TMPDIR_SMOKE/dav-runtime"
cat > "$TMPDIR_SMOKE/httpd.conf" <<'EOF'
# Minimal mod_dav-only httpd.conf (deterministic module set, no stock conf).
# /runtime 与 /dav 是可写挂载（DavLockDB/PidFile/共享数据），跑完随临时目录销毁。
ServerRoot /usr/local/apache2
Listen 80
ServerName localhost
LoadModule mpm_event_module modules/mod_mpm_event.so
LoadModule unixd_module modules/mod_unixd.so
LoadModule log_config_module modules/mod_log_config.so
LoadModule authn_core_module modules/mod_authn_core.so
LoadModule authn_file_module modules/mod_authn_file.so
LoadModule authz_core_module modules/mod_authz_core.so
LoadModule authz_user_module modules/mod_authz_user.so
LoadModule auth_basic_module modules/mod_auth_basic.so
LoadModule alias_module modules/mod_alias.so
LoadModule dav_module modules/mod_dav.so
LoadModule dav_fs_module modules/mod_dav_fs.so
User daemon
Group daemon
ErrorLog /proc/self/fd/2
CustomLog /proc/self/fd/1 combined
PidFile /runtime/httpd.pid
DavLockDB /runtime/DavLock
Alias /dav /dav
<Directory /dav>
    Dav On
    AuthType Basic
    AuthName "dbx-files smoke"
    AuthUserFile /conf/htpasswd
    Require valid-user
</Directory>
EOF
# htpasswd 由官方镜像自带工具生成（随机密码经容器 env 传入，不出现在命令行参数里）
docker run --rm \
  -e "DAV_USER=${WEBDAV_USER}" \
  -e "DAV_PASSWORD=${WEBDAV_PASSWORD}" \
  --entrypoint sh \
  httpd:2.4-alpine -c 'htpasswd -nb "$DAV_USER" "$DAV_PASSWORD"' > "$TMPDIR_SMOKE/htpasswd"

echo "==> starting WebDAV test server (:${WEBDAV_PORT})"
docker rm -f dbx-files-webdav-test >/dev/null 2>&1 || true
# httpd:alpine 不带 apr-util 的 DBM 驱动（mod_dav_fs 锁库需要），缺了它一切
# 写方法（MKCOL/PUT/…）都 500 “The DBM driver could not be loaded”——读方法
# PROPFIND 却正常，极易误判为认证/权限问题。启动时补装 gdbm 驱动再起 httpd。
docker run -d --rm --name dbx-files-webdav-test \
  -p "${WEBDAV_PORT}:80" \
  -v "$TMPDIR_SMOKE/httpd.conf:/usr/local/apache2/conf/httpd.conf:ro" \
  -v "$TMPDIR_SMOKE/htpasswd:/conf/htpasswd:ro" \
  -v "$TMPDIR_SMOKE/dav-data:/dav" \
  -v "$TMPDIR_SMOKE/dav-runtime:/runtime" \
  --entrypoint sh \
  httpd:2.4-alpine -c "apk add --no-cache apr-util-dbm_gdbm >/dev/null 2>&1 && exec httpd-foreground" >/dev/null

echo "==> waiting for webdav authenticated PROPFIND"
webdav_up=0
for _ in $(seq 1 30); do
  # 真正健康判据：带账号的 PROPFIND 返回 207 Multi-Status，而非端口开放。
  webdav_code=$(curl -s -o /dev/null -w '%{http_code}' -X PROPFIND -H "Depth: 0" \
    -u "${WEBDAV_USER}:${WEBDAV_PASSWORD}" "http://127.0.0.1:${WEBDAV_PORT}/dav/" 2>/dev/null || true)
  if [ "$webdav_code" = "207" ]; then webdav_up=1; break; fi
  sleep 1
done
[ "$webdav_up" = 1 ] || {
  echo "FAIL: webdav did not become ready on port ${WEBDAV_PORT}" >&2
  docker logs dbx-files-webdav-test >&2 || true
  exit 1
}

echo "==> preparing FTP server script (pyftpdlib, PASV masquerade 127.0.0.1)"
cat > "$TMPDIR_SMOKE/ftp_server.py" <<'EOF'
import os
from pyftpdlib.authorizers import DummyAuthorizer
from pyftpdlib.handlers import FTPHandler
from pyftpdlib.servers import FTPServer

authorizer = DummyAuthorizer()
home = "/home/smoke"
os.makedirs(home, exist_ok=True)
authorizer.add_user(os.environ["FTP_USER"], os.environ["FTP_PASSWORD"], home, perm="elradfmwMT")
handler = FTPHandler
handler.authorizer = authorizer
# 数据通道地址伪装成宿主环回：PASV 返回的 IP 必须能从宿主侧直连（端口已映射）。
handler.masquerade_address = "127.0.0.1"
handler.passive_ports = range(int(os.environ["FTP_PASV_MIN"]), int(os.environ["FTP_PASV_MAX"]) + 1)
handler.banner = "dbx-files smoke ftp ready."
server = FTPServer(("0.0.0.0", 21), handler)
server.serve_forever()
EOF

echo "==> starting FTP test server (:${FTP_PORT}, pasv ${FTP_PASV_MIN}-${FTP_PASV_MAX})"
docker rm -f dbx-files-ftp-test >/dev/null 2>&1 || true
docker run -d --rm --name dbx-files-ftp-test \
  -p "${FTP_PORT}:21" \
  -p "${FTP_PASV_MIN}-${FTP_PASV_MAX}:${FTP_PASV_MIN}-${FTP_PASV_MAX}" \
  -v "$TMPDIR_SMOKE/ftp_server.py:/srv/ftp_server.py:ro" \
  -e "FTP_USER=${FTP_USER}" \
  -e "FTP_PASSWORD=${FTP_PASSWORD}" \
  -e "FTP_PASV_MIN=${FTP_PASV_MIN}" \
  -e "FTP_PASV_MAX=${FTP_PASV_MAX}" \
  python:3-alpine sh -c "pip install --no-cache-dir -q pyftpdlib && exec python /srv/ftp_server.py" >/dev/null

echo "==> waiting for ftp authenticated login (首次运行需 pip 安装)"
ftp_up=0
for _ in $(seq 1 60); do
  # 真正健康判据：账号登录 + 列目录一次（curl 的 FTP 客户端走完整 USER/PASS/PASV）。
  if curl -s --connect-timeout 2 "ftp://127.0.0.1:${FTP_PORT}/" --user "${FTP_USER}:${FTP_PASSWORD}" >/dev/null 2>&1; then
    ftp_up=1
    break
  fi
  sleep 2
done
[ "$ftp_up" = 1 ] || {
  echo "FAIL: ftp did not become ready on port ${FTP_PORT}" >&2
  docker logs dbx-files-ftp-test >&2 || true
  exit 1
}

# 冒烟所需环境写入运行时临时 env 文件后 source（无字面凭据；跑完随临时目录删除）
ENV_FILE="$TMPDIR_SMOKE/smoke.env"
{
  printf 'DBX_FILES_S3_ENDPOINT=http://127.0.0.1:%s\n' "$MINIO_PORT"
  printf 'DBX_FILES_S3_BUCKET=%s\n' "$MINIO_BUCKET"
  printf 'DBX_FILES_S3_ACCESS_KEY=%s\n' "$MINIO_USER"
  printf 'DBX_FILES_S3_SECRET_KEY=%s\n' "$MINIO_PASSWORD"
  printf 'DBX_FILES_S3_REGION=us-east-1\n'
  printf 'DBX_FILES_SFTP_HOST=127.0.0.1\n'
  printf 'DBX_FILES_SFTP_PORT=%s\n' "$SFTP_PORT"
  printf 'DBX_FILES_SFTP_USER=%s\n' "$SFTP_USER"
  printf 'DBX_FILES_SFTP_KEY=%s/smoke_key\n' "$TMPDIR_SMOKE"
  printf 'DBX_FILES_SMB_HOST=127.0.0.1\n'
  printf 'DBX_FILES_SMB_PORT=%s\n' "$SMB_PORT"
  printf 'DBX_FILES_SMB_SHARE=%s\n' "$SMB_SHARE"
  printf 'DBX_FILES_SMB_USER=%s\n' "$SMB_USER"
  printf 'DBX_FILES_SMB_PASSWORD=%s\n' "$SMB_PASSWORD"
  printf 'DBX_FILES_WEBDAV_ENDPOINT=http://127.0.0.1:%s/dav\n' "$WEBDAV_PORT"
  printf 'DBX_FILES_WEBDAV_USER=%s\n' "$WEBDAV_USER"
  printf 'DBX_FILES_WEBDAV_PASSWORD=%s\n' "$WEBDAV_PASSWORD"
  printf 'DBX_FILES_FTP_ENDPOINT=ftp://127.0.0.1:%s\n' "$FTP_PORT"
  printf 'DBX_FILES_FTP_USER=%s\n' "$FTP_USER"
  printf 'DBX_FILES_FTP_PASSWORD=%s\n' "$FTP_PASSWORD"
  printf 'DBX_FILES_SFTP_NATIVE_HOST=127.0.0.1\n'
  printf 'DBX_FILES_SFTP_NATIVE_PORT=%s\n' "$SFTP_PORT"
  printf 'DBX_FILES_SFTP_NATIVE_USER=%s\n' "$SFTP_USER"
  printf 'DBX_FILES_SFTP_NATIVE_PASSWORD=%s\n' "$SFTP_PASSWORD"
  printf 'DBX_FILES_SFTP_NATIVE_BASE=/config/smoke-native-%s\n' "$(date +%s)"
} > "$ENV_FILE"
chmod 600 "$ENV_FILE"

echo "==> running full smoke (fs + memory + s3 + sftp + webdav + ftp + smb + sftp-native)"
set -a
. "$ENV_FILE"
set +a
python3 scripts/smoke_test.py

# MCP smoke 带同一份容器 env 重跑：smoke_mcp.py 的 R1/R2/R3 场景（stdio 内联
# 凭据下 s3/webdav/ftp 的 write→digest→cursor→两阶段 delete/purge 全链路）
# 由 env 存在性自动从 SKIP 翻成真跑；无容器环境下它们保持 SKIP 不 FAIL。
echo "==> running MCP smoke (local + container-backed remote stdio sections)"
python3 scripts/smoke_mcp.py
