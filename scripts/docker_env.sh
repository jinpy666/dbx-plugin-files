#!/usr/bin/env bash
# 本地持久 Docker 测试环境：MinIO/SFTP/Samba/WebDAV/FTP 五个文件协议服务端。
#
# 与 scripts/container_smoke.sh（CI 即焚冒烟、凭据一次性）互补：这套容器
# --restart unless-stopped，数据与凭据落在状态目录，供本地手工验证与 DBX
# 客户端长期连接使用。配合 scripts/docker_env_connect_dbx.py 可把七条
# docker-* 存储连接注入本地 DBX 宿主（含 docker-minio-ns：bucket 留空的
# namespace 连接，根目录直接列出桶——桶存储的一等观察面）。
#
# 用法:
#   scripts/docker_env.sh up              # 初始化并启动（幂等：已运行则跳过）
#   scripts/docker_env.sh verify          # 五协议健康检查（协议级，非端口级）
#   scripts/docker_env.sh down [--purge]  # 停止并删除容器；--purge 连状态目录一起删
#
# 状态目录: ${DBX_FILES_DOCKER_ENV_DIR:-$HOME/.dbx-files-docker-test}
#   env.conf         端口与账号名（shell 可 source；凭据不在此文件）
#   *_password 等    一次性测试凭据，运行时随机生成（目录 0700、文件 0600）
#   sftp_key(.pub)   SFTP 测试密钥对
#   minio-data/ sftp-config/ dav-data/ dav-runtime/  容器数据卷
#
# 设计要点（沿用 container_smoke.sh 的合规做法）：凭据不进仓库、不进命令行
# 可见参数；镜像拉取带备用源；健康判据用真实协议交互而非端口探活。
set -euo pipefail
cd "$(dirname "$0")/.."

STATE_DIR="${DBX_FILES_DOCKER_ENV_DIR:-$HOME/.dbx-files-docker-test}"
MINIO_NAME=dbx-files-docker-minio
SFTP_NAME=dbx-files-docker-sftp
SMB_NAME=dbx-files-docker-samba
WEBDAV_NAME=dbx-files-docker-webdav
FTP_NAME=dbx-files-docker-ftp
ALL_NAMES="$MINIO_NAME $SFTP_NAME $SMB_NAME $WEBDAV_NAME $FTP_NAME"
# rclone 工具镜像钉扎：与插件自带引擎同版本（scripts/fetch-rclone.sh 的
# v1.75.1-dbx.1），口径同 scripts/container_smoke.sh——工具链不用浮动 tag。
RCLONE_IMAGE="rclone/rclone:1.75.1"

die() { echo "FAIL: $*" >&2; exit 1; }

container_running() { docker ps --format '{{.Names}}' | grep -qx "$1"; }
container_exists() { docker ps -a --format '{{.Names}}' | grep -qx "$1"; }

port_in_use() { lsof -nP -iTCP:"$1" -sTCP:LISTEN >/dev/null 2>&1; }

ensure_image() {
  local image="$1"; shift
  docker image inspect "$image" >/dev/null 2>&1 && return 0
  docker pull "$image" >/dev/null 2>&1 && return 0
  local alt
  for alt in "$@"; do
    if docker pull "$alt" >/dev/null 2>&1; then docker tag "$alt" "$image"; return 0; fi
  done
  die "cannot pull ${image} from Docker Hub or any fallback"
}

# 首次初始化：生成端口表与一次性凭据（之后一直复用，重启不变）
init_state() {
  mkdir -p "$STATE_DIR"
  chmod 700 "$STATE_DIR"
  if [ ! -f "$STATE_DIR/env.conf" ]; then
    {
      echo "MINIO_PORT=${DBX_FILES_DOCKER_MINIO_PORT:-19001}"
      echo "SFTP_PORT=${DBX_FILES_DOCKER_SFTP_PORT:-2301}"
      echo "SMB_PORT=${DBX_FILES_DOCKER_SMB_PORT:-2446}"
      echo "WEBDAV_PORT=${DBX_FILES_DOCKER_WEBDAV_PORT:-18081}"
      echo "FTP_PORT=${DBX_FILES_DOCKER_FTP_PORT:-2122}"
      echo "FTP_PASV_MIN=${DBX_FILES_DOCKER_FTP_PASV_MIN:-30200}"
      echo "FTP_PASV_MAX=${DBX_FILES_DOCKER_FTP_PASV_MAX:-30209}"
      echo "MINIO_USER=dbxfiles"
      echo "SFTP_USER=sftpuser"
      echo "SMB_USER=smbuser"
      echo "SMB_SHARE=files"
      echo "WEBDAV_USER=davuser"
      echo "FTP_USER=ftpuser"
    } > "$STATE_DIR/env.conf"
    chmod 600 "$STATE_DIR/env.conf"
    local pw
    for f in minio_password sftp_password smb_password webdav_password ftp_password; do
      pw="$(openssl rand -hex 16 2>/dev/null || head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"
      printf '%s' "$pw" > "$STATE_DIR/$f"
      chmod 600 "$STATE_DIR/$f"
    done
    echo "dbx-files-test" > "$STATE_DIR/minio_bucket"
    echo "首次初始化状态目录: $STATE_DIR"
  fi
  # shellcheck disable=SC1091
  source "$STATE_DIR/env.conf"
  MINIO_BUCKET="$(cat "$STATE_DIR/minio_bucket")"
  # 第二桶：旧状态目录没有该文件时用固定名兜底（桶名只是观察面，非凭据）。
  MINIO_BUCKET2="$(cat "$STATE_DIR/minio_bucket2" 2>/dev/null || echo dbx-files-test2)"
}

load_passwords() {
  MINIO_PASSWORD="$(cat "$STATE_DIR/minio_password")"
  SFTP_PASSWORD="$(cat "$STATE_DIR/sftp_password")"
  SMB_PASSWORD="$(cat "$STATE_DIR/smb_password")"
  WEBDAV_PASSWORD="$(cat "$STATE_DIR/webdav_password")"
  FTP_PASSWORD="$(cat "$STATE_DIR/ftp_password")"
}

up() {
  command -v docker >/dev/null 2>&1 || die "docker 不可用"
  init_state
  load_passwords

  if container_running "$MINIO_NAME" && container_running "$FTP_NAME"; then
    echo "已全部运行（容器 ${ALL_NAMES}），跳过启动。凭据在 ${STATE_DIR}"
    return 0
  fi

  # 端口预检：只在需要新建对应容器时执行（已运行容器占着端口是正常状态，
  # 包括本环境其他仍存活的容器——部分重建不能被存活容器自己的端口拦死）
  local item name p
  for item in "$MINIO_NAME:$MINIO_PORT" "$SFTP_NAME:$SFTP_PORT" \
              "$SMB_NAME:$SMB_PORT" "$WEBDAV_NAME:$WEBDAV_PORT" \
              "$FTP_NAME:$FTP_PORT"; do
    name="${item%%:*}"
    p="${item##*:}"
    if ! container_exists "$name" && port_in_use "$p"; then
      die "端口 $p 已被占用（非本环境容器），换 DBX_FILES_DOCKER_*_PORT 或释放端口"
    fi
  done

  mkdir -p "$STATE_DIR/minio-data" "$STATE_DIR/sftp-config" "$STATE_DIR/dav-data" "$STATE_DIR/dav-runtime"
  chmod 777 "$STATE_DIR/dav-data" "$STATE_DIR/dav-runtime"

  echo "==> MinIO (:${MINIO_PORT})"
  # minio/minio 与 minio/mc 2025-09-07 起上游停止发布预编译镜像（docker.io/
  # quay.io 均 401，重试无效），持久环境沿用即坏。镜像与建桶工具改用
  # bitnamilegacy/minio（Bitnami 归档的社区版最后镜像，多架构）+ rclone 官方
  # 镜像，与 scripts/container_smoke.sh 同一套选型。
  # Bitnami 容器数据目录是 /bitnami/minio/data（entrypoint 自动建并 chown 到
  # 非 root 运行用户），默认 Cmd 已带 server 启动——显式追加 "server /data"
  # 反而 file access denied；旧数据卷（官方镜像布局）原样挂入即可，MinIO
  # 数据布局本身不换。
  ensure_image "bitnamilegacy/minio:2025.7.23"
  ensure_image "$RCLONE_IMAGE"
  if container_running "$MINIO_NAME"; then echo "  already running"
  else
    docker rm -f "$MINIO_NAME" >/dev/null 2>&1 || true
    docker run -d --name "$MINIO_NAME" --restart unless-stopped \
      -p "${MINIO_PORT}:9000" \
      -v "$STATE_DIR/minio-data:/bitnami/minio/data" \
      -e "MINIO_ROOT_USER=${MINIO_USER}" \
      -e "MINIO_ROOT_PASSWORD=${MINIO_PASSWORD}" \
      -e "MINIO_API_ODIRECT=off" \
      bitnamilegacy/minio:2025.7.23 >/dev/null
  fi
  for _ in $(seq 1 30); do
    curl -fsS "http://127.0.0.1:${MINIO_PORT}/minio/health/live" >/dev/null 2>&1 && break
    sleep 1
  done
  curl -fsS "http://127.0.0.1:${MINIO_PORT}/minio/health/live" >/dev/null || die "MinIO 未就绪"
  # 建桶持久化在数据卷里，rclone mkdir 对已存在桶幂等；双桶——主桶 +
  # 第二桶，namespace 连接的根列表才有「桶集合」可看（桶管理面测试与
  # docker-minio-ns 连接的观察面）。凭据经 :s3, 连接字符串进一次性 --rm
  # 容器 argv（container_smoke.sh 已评审的口径）；建桶后 lsd 显式校验——
  # mc mb 吞参假成功的历史教训，建桶不允许「静默没建」。
  MINIO_CS=":s3,provider=Minio,access_key_id=${MINIO_USER},secret_access_key=${MINIO_PASSWORD},endpoint='http://minio:9000'"
  for b in "$MINIO_BUCKET" "$MINIO_BUCKET2"; do
    docker run --rm --link "${MINIO_NAME}:minio" \
      "$RCLONE_IMAGE" mkdir "${MINIO_CS}:${b}" >/dev/null
  done
  docker run --rm --link "${MINIO_NAME}:minio" \
    "$RCLONE_IMAGE" lsd "${MINIO_CS}:" | grep -q "${MINIO_BUCKET}" \
    || die "bucket ${MINIO_BUCKET} 未建成"

  echo "==> OpenSSH SFTP (:${SFTP_PORT}, 密码+密钥双认证)"
  ensure_image "linuxserver/openssh-server" "ghcr.io/linuxserver/openssh-server" "mirror.gcr.io/linuxserver/openssh-server"
  if [ ! -f "$STATE_DIR/sftp_key" ]; then
    ssh-keygen -t ed25519 -N "" -f "$STATE_DIR/sftp_key" -C dbx-files-docker-test >/dev/null
    chmod 600 "$STATE_DIR/sftp_key"
  fi
  if container_running "$SFTP_NAME"; then echo "  already running"
  else
    docker rm -f "$SFTP_NAME" >/dev/null 2>&1 || true
    docker run -d --name "$SFTP_NAME" --restart unless-stopped \
      -p "${SFTP_PORT}:2222" \
      -v "$STATE_DIR/sftp-config:/config" \
      -e "PUBLIC_KEY=$(cat "$STATE_DIR/sftp_key.pub")" \
      -e "USER_NAME=${SFTP_USER}" \
      -e "PASSWORD_ACCESS=true" \
      -e "USER_PASSWORD=${SFTP_PASSWORD}" \
      linuxserver/openssh-server >/dev/null
  fi
  for _ in $(seq 1 30); do
    nc -z 127.0.0.1 "${SFTP_PORT}" 2>/dev/null && break
    sleep 1
  done
  nc -z 127.0.0.1 "${SFTP_PORT}" 2>/dev/null || die "sshd 未监听 ${SFTP_PORT}"

  echo "==> Samba (:${SMB_PORT}, share ${SMB_SHARE})"
  if container_running "$SMB_NAME"; then echo "  already running"
  else
    docker rm -f "$SMB_NAME" >/dev/null 2>&1 || true
    docker run -d --name "$SMB_NAME" --restart unless-stopped \
      -p "${SMB_PORT}:445" \
      -e "SAMBA_CONF_SERVER_ROLE=standalone" \
      -e "AVAHI_DISABLE=true" -e "WSDD2_DISABLE=true" -e "NETBIOS_DISABLE=true" \
      -e "SAMBA_VOLUME_CONFIG_${SMB_SHARE}=[${SMB_SHARE}]; path=/shares/${SMB_SHARE}; valid users = ${SMB_USER}; read only = no; browseable = yes" \
      -e "ACCOUNT_${SMB_USER}=${SMB_PASSWORD}" \
      ghcr.io/servercontainers/samba >/dev/null
  fi
  docker exec "$SMB_NAME" sh -c "mkdir -p /shares/${SMB_SHARE} && chmod 777 /shares/${SMB_SHARE}" >/dev/null
  for _ in $(seq 1 30); do
    if nc -z 127.0.0.1 "${SMB_PORT}" 2>/dev/null && docker exec "$SMB_NAME" smbclient \
         "//127.0.0.1/${SMB_SHARE}" -U "${SMB_USER}%${SMB_PASSWORD}" -c 'ls' >/dev/null 2>&1; then
      break
    fi
    sleep 1
  done
  sleep 3
  docker exec "$SMB_NAME" smbclient \
    "//127.0.0.1/${SMB_SHARE}" -U "${SMB_USER}%${SMB_PASSWORD}" -c 'ls' >/dev/null \
    || die "samba tree connect 失败"

  echo "==> WebDAV (:${WEBDAV_PORT})"
  ensure_image "httpd:2.4-alpine" "mirror.gcr.io/library/httpd:2.4-alpine" "public.ecr.aws/docker/library/httpd:2.4-alpine"
  cat > "$STATE_DIR/httpd.conf" <<'EOF'
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
    AuthName "dbx-files docker test"
    AuthUserFile /conf/htpasswd
    Require valid-user
</Directory>
EOF
  # htpasswd 只在缺失时生成：凭据来自状态文件，重启后保持不变
  if [ ! -f "$STATE_DIR/htpasswd" ]; then
    docker run --rm \
      -e "DAV_USER=${WEBDAV_USER}" -e "DAV_PASSWORD=${WEBDAV_PASSWORD}" \
      --entrypoint sh httpd:2.4-alpine \
      -c 'htpasswd -nb "$DAV_USER" "$DAV_PASSWORD"' > "$STATE_DIR/htpasswd"
    chmod 600 "$STATE_DIR/htpasswd"
  fi
  if container_running "$WEBDAV_NAME"; then echo "  already running"
  else
    docker rm -f "$WEBDAV_NAME" >/dev/null 2>&1 || true
    docker run -d --name "$WEBDAV_NAME" --restart unless-stopped \
      -p "${WEBDAV_PORT}:80" \
      -v "$STATE_DIR/httpd.conf:/usr/local/apache2/conf/httpd.conf:ro" \
      -v "$STATE_DIR/htpasswd:/conf/htpasswd:ro" \
      -v "$STATE_DIR/dav-data:/dav" \
      -v "$STATE_DIR/dav-runtime:/runtime" \
      --entrypoint sh httpd:2.4-alpine \
      -c "apk add --no-cache apr-util-dbm_gdbm >/dev/null 2>&1 && exec httpd-foreground" >/dev/null
  fi
  for _ in $(seq 1 30); do
    code=$(curl -s -o /dev/null -w '%{http_code}' -X PROPFIND -H "Depth: 0" \
      -u "${WEBDAV_USER}:${WEBDAV_PASSWORD}" "http://127.0.0.1:${WEBDAV_PORT}/dav/" 2>/dev/null || true)
    [ "$code" = "207" ] && break
    sleep 1
  done
  [ "${code:-}" = "207" ] || die "webdav 未就绪"

  echo "==> FTP (:${FTP_PORT}, pasv ${FTP_PASV_MIN}-${FTP_PASV_MAX})"
  cat > "$STATE_DIR/ftp_server.py" <<'EOF'
import os
from pyftpdlib.authorizers import DummyAuthorizer
from pyftpdlib.handlers import FTPHandler
from pyftpdlib.servers import FTPServer

authorizer = DummyAuthorizer()
home = "/" + os.environ["FTP_USER"]
os.makedirs(home, exist_ok=True)
authorizer.add_user(os.environ["FTP_USER"], os.environ["FTP_PASSWORD"], home, perm="elradfmwMT")
handler = FTPHandler
handler.authorizer = authorizer
handler.masquerade_address = "127.0.0.1"
handler.passive_ports = range(int(os.environ["FTP_PASV_MIN"]), int(os.environ["FTP_PASV_MAX"]) + 1)
handler.banner = "dbx-files docker ftp ready."
server = FTPServer(("0.0.0.0", 21), handler)
server.serve_forever()
EOF
  if container_running "$FTP_NAME"; then echo "  already running"
  else
    docker rm -f "$FTP_NAME" >/dev/null 2>&1 || true
    docker run -d --name "$FTP_NAME" --restart unless-stopped \
      -p "${FTP_PORT}:21" -p "${FTP_PASV_MIN}-${FTP_PASV_MAX}:${FTP_PASV_MIN}-${FTP_PASV_MAX}" \
      -v "$STATE_DIR/ftp_server.py:/srv/ftp_server.py:ro" \
      -e "FTP_USER=${FTP_USER}" -e "FTP_PASSWORD=${FTP_PASSWORD}" \
      -e "FTP_PASV_MIN=${FTP_PASV_MIN}" -e "FTP_PASV_MAX=${FTP_PASV_MAX}" \
      python:3-alpine sh -c "pip install --no-cache-dir -q pyftpdlib && exec python /srv/ftp_server.py" >/dev/null
  fi
  for _ in $(seq 1 60); do
    curl -s --connect-timeout 2 "ftp://127.0.0.1:${FTP_PORT}/" \
      --user "${FTP_USER}:${FTP_PASSWORD}" >/dev/null 2>&1 && break
    sleep 2
  done
  curl -s --connect-timeout 3 "ftp://127.0.0.1:${FTP_PORT}/" \
    --user "${FTP_USER}:${FTP_PASSWORD}" >/dev/null || die "ftp 登录失败"

  echo
  echo "DONE: 五个协议服务已就绪并随 Docker 自动重启。"
  echo "  状态目录: ${STATE_DIR}（凭据 0600，勿提交）"
  echo "  下一步: python3 scripts/docker_env_connect_dbx.py 把 docker-* 连接注入本地 DBX"
}

verify() {
  [ -f "$STATE_DIR/env.conf" ] || die "环境未初始化，先执行 scripts/docker_env.sh up"
  # shellcheck disable=SC1091
  source "$STATE_DIR/env.conf"
  load_passwords
  local ok=0

  if curl -fsS "http://127.0.0.1:${MINIO_PORT}/minio/health/live" >/dev/null 2>&1; then
    echo "[  ok] s3/minio   http://127.0.0.1:${MINIO_PORT}"; ok=$((ok+1))
  else
    echo "[FAIL] s3/minio   http://127.0.0.1:${MINIO_PORT}"
  fi

  # sftp 密钥认证端到端（BatchMode 只用密钥）；sftp-native 与 sftp 同一 sshd，
  # 密码认证由 up 的容器配置保证
  if ssh -i "$STATE_DIR/sftp_key" -p "${SFTP_PORT}" \
      -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o BatchMode=yes -o ConnectTimeout=5 \
      "${SFTP_USER}@127.0.0.1" exit >/dev/null 2>&1; then
    echo "[  ok] sftp       ssh://${SFTP_USER}@127.0.0.1:${SFTP_PORT} (key)"; ok=$((ok+1))
  else
    echo "[FAIL] sftp       ssh://${SFTP_USER}@127.0.0.1:${SFTP_PORT}"
  fi

  if docker exec "$SMB_NAME" smbclient "//127.0.0.1/${SMB_SHARE}" \
      -U "${SMB_USER}%${SMB_PASSWORD}" -c 'ls' >/dev/null 2>&1; then
    echo "[  ok] smb        127.0.0.1:${SMB_PORT} share=${SMB_SHARE}"; ok=$((ok+1))
  else
    echo "[FAIL] smb        127.0.0.1:${SMB_PORT}"
  fi

  code=$(curl -s -o /dev/null -w '%{http_code}' -X PROPFIND -H "Depth: 0" \
    -u "${WEBDAV_USER}:${WEBDAV_PASSWORD}" "http://127.0.0.1:${WEBDAV_PORT}/dav/" 2>/dev/null || true)
  if [ "$code" = "207" ]; then
    echo "[  ok] webdav     http://127.0.0.1:${WEBDAV_PORT}/dav"; ok=$((ok+1))
  else
    echo "[FAIL] webdav     http://127.0.0.1:${WEBDAV_PORT}/dav (code=${code:-none})"
  fi

  if curl -s --connect-timeout 3 "ftp://127.0.0.1:${FTP_PORT}/" \
      --user "${FTP_USER}:${FTP_PASSWORD}" >/dev/null 2>&1; then
    echo "[  ok] ftp        ftp://127.0.0.1:${FTP_PORT}"; ok=$((ok+1))
  else
    echo "[FAIL] ftp        ftp://127.0.0.1:${FTP_PORT}"
  fi

  [ "$ok" = 5 ] || die "${ok}/5 通过"
  echo "verify: 5/5 通过"
}

down() {
  local name
  for name in $ALL_NAMES; do
    if docker rm -f "$name" >/dev/null 2>&1; then echo "removed $name"; fi
  done
  if [ "${1:-}" = "--purge" ]; then
    rm -rf "$STATE_DIR"
    echo "purged ${STATE_DIR}（凭据/密钥/数据已删除；DBX 里的 docker-* 连接需自行删除或重跑 connect 前先 down）"
  else
    echo "容器已停止；数据与凭据保留在 ${STATE_DIR}，重新 up 即可恢复"
  fi
}

case "${1:-}" in
  up) up ;;
  verify) verify ;;
  down) shift || true; down "${1:-}" ;;
  *) sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
