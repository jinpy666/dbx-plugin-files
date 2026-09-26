#!/usr/bin/env bash
# files 插件 rclone 引擎——代理端到端验证的容器层。
# 搭建真实容器拓扑（gost HTTP+SOCKS5 双代理、SSH 隧道跳板、隔离 WebDAV、
# MinIO、SFTP），完成容器级独立验证后打印 DBX_PROXY_E2E_* 环境变量，
# 供 python 驱动（scripts/proxy_e2e.py，并行交付）验证 sidecar 的三种
# 按连接代理通道：
#   - HTTP/SOCKS5 代理（HTTP 系后端经 rcd 分组 env；ftp/sftp 经 rclone
#     后端选项 http_proxy/socks_proxy）
#   - SSH 隧道（sidecar spawn `ssh -N -L`，endpoint 改写 127.0.0.1:<port>，
#     仅密钥认证）
#
# 设计沿用 scripts/container_smoke.sh：
# - 无任何硬编码凭据：全部凭据运行时 openssl rand -hex 随机生成，仅存在于
#   进程环境与 600 权限的状态文件（--keep 调试时保留，默认删除）；
#   隧道私钥运行时生成于临时目录，公钥经 PUBLIC_KEY 环境变量注入。
# - 不拼接 shell 字符串：建桶用 :s3, 连接字符串（URL 引号包裹）；可变值全部走 env/参数。
# - 镜像选型 arm64 优先（minio、rclone、linuxserver/openssh-server、
#   curlimages/curl 均多架构）；gost 优先 go-gost/gost，拉取失败依次尝试
#   --platform linux/amd64（Apple Silicon Rosetta）与 ginuerzh/gost（v2，
#   arm64 原生），成功引用回打 go-gost/gost 标签统一 run。
# - 一切容器 --rm + 退出时 docker rm -f 兜底；已有同名容器先替换。
# - docker 不可用 exit 3（SKIP 语义）；宿主端口被占 exit 4（可用
#   DBX_PROXY_E2E_PORT_* 环境变量换端口）。
# - 日志走 stderr，stdout 只输出 DBX_PROXY_E2E_* 契约行（驱动按行解析）。
#
# 拓扑隔离语义：MinIO 直连口发布在 127.0.0.1（控制组用），WebDAV 容器不发布
# 任何端口；网络内主机名（dbx-proxy-*-test）对宿主 DNS 不可解析——代理/隧道
# 场景必须真的经由 gost/sshd 容器代理解析，否则无法触达目标。
#
# 用法：
#   scripts/proxy_e2e.sh            # 起容器 → 容器级验证 → 打印 env → 清理
#   scripts/proxy_e2e.sh --keep     # 调试：保留容器/网络/密钥/凭据状态
#   scripts/proxy_e2e.sh --env-only # 容器已由 --keep 保留时，跳过起容器直接打印 env
set -euo pipefail
cd "$(dirname "$0")/.."

KEEP=0
ENV_ONLY=0
for arg in "$@"; do
  case "$arg" in
    --keep) KEEP=1 ;;
    --env-only) ENV_ONLY=1 ;;
    *) echo "FAIL: unknown arg: $arg (supported: --keep, --env-only)" >&2; exit 1 ;;
  esac
done

if ! command -v docker >/dev/null 2>&1; then
  echo "SKIP: docker not available" >&2
  exit 3
fi

log() { echo "$*" >&2; }

# rclone 工具镜像钉扎：与插件自带引擎同版本（scripts/fetch-rclone.sh 的
# v1.75.1-dbx.1），工具链不使用浮动 :latest（含 webdav server、obscure、
# 探针容器）；升级改为有意的 bump。
RCLONE_IMAGE="rclone/rclone:1.75.1"
NET="dbx-proxy-net"
C_MINIO="dbx-proxy-minio-test"
C_WEBDAV="dbx-proxy-webdav-test"
C_GOST="dbx-proxy-gost-test"
C_SQUID="dbx-proxy-squid-test"
C_SSHD="dbx-proxy-sshd-test"
C_SFTP="dbx-proxy-sftp-test"
# --keep 运行留下的凭据/端口/密钥路径状态（--env-only 据此重放契约输出）
STATE_FILE="${DBX_PROXY_E2E_STATE:-/tmp/dbx-proxy-e2e.state}"

# 宿主发布端口（可覆盖；被占时 exit 4 并提示这些变量）
PORT_MINIO="${DBX_PROXY_E2E_PORT_MINIO:-19500}"
PORT_HTTP_PROXY="${DBX_PROXY_E2E_PORT_HTTP:-18081}"
PORT_SOCKS_PROXY="${DBX_PROXY_E2E_PORT_SOCKS:-18082}"
PORT_SSHD="${DBX_PROXY_E2E_PORT_SSHD:-18222}"
PORT_SFTP="${DBX_PROXY_E2E_PORT_SFTP:-18223}"
# [4/6] 转发探针的本地拨号口（复刻 sidecar `ssh -N -L` 的 127.0.0.1:<port> 改写）
PORT_TUNNEL_FWD="${DBX_PROXY_E2E_PORT_TUNNEL_FWD:-19555}"

TUNNEL_KEY_DIR="$(mktemp -d /tmp/dbx-proxy-e2e-keys.XXXXXX)"
WEBDAV_DATA_DIR="$(mktemp -d /tmp/dbx-proxy-e2e-dav.XXXXXX)"

# DBX_PROXY_E2E_* 契约输出（变量名严格一致，python 驱动按行解析）
print_env() {
  cat <<EOF
DBX_PROXY_E2E_HTTP=127.0.0.1:${PORT_HTTP_PROXY}
DBX_PROXY_E2E_HTTP_USER=${SQUID_PROXY_USER}
DBX_PROXY_E2E_HTTP_PASS=${SQUID_PROXY_PASS}
DBX_PROXY_E2E_SOCKS=127.0.0.1:${PORT_SOCKS_PROXY}
DBX_PROXY_E2E_SOCKS_USER=${GOST_PROXY_USER}
DBX_PROXY_E2E_SOCKS_PASS=${GOST_PROXY_PASS}
DBX_PROXY_E2E_MINIO_DIRECT=http://127.0.0.1:${PORT_MINIO}
DBX_PROXY_E2E_MINIO_INNET=http://${C_MINIO}:9000
DBX_PROXY_E2E_MINIO_ACCESS_KEY=${MINIO_ROOT_USER}
DBX_PROXY_E2E_MINIO_SECRET_KEY=${MINIO_ROOT_PASSWORD}
DBX_PROXY_E2E_WEBDAV_INNET=http://${C_WEBDAV}:8899
DBX_PROXY_E2E_WEBDAV_USER=${WEBDAV_USER}
DBX_PROXY_E2E_WEBDAV_PASS=${WEBDAV_PASSWORD}
DBX_PROXY_E2E_SFTP_INNET=${C_SFTP}:22
DBX_PROXY_E2E_SFTP_DIRECT=127.0.0.1:${PORT_SFTP}
DBX_PROXY_E2E_SFTP_USER=sftpuser
DBX_PROXY_E2E_SFTP_PASS=${SFTP_PASSWORD}
DBX_PROXY_E2E_SSHD=127.0.0.1:${PORT_SSHD}
DBX_PROXY_E2E_SSH_USER=${TUNNEL_SSH_USER}
DBX_PROXY_E2E_SSH_KEY=${TUNNEL_KEY_DIR}/id_ed25519
DBX_PROXY_E2E_BUCKET=${MINIO_BUCKET}
DBX_PROXY_E2E_BUCKET2=${MINIO_BUCKET2}
EOF
}

cleanup() {
  if [ "$ENV_ONLY" = 1 ]; then return; fi
  if [ "$KEEP" = 1 ]; then
    log "KEEP: containers left running (network ${NET}); state: ${STATE_FILE}; key dir: ${TUNNEL_KEY_DIR}"
    return
  fi
  if [ -n "${TUNNEL_FWD_PID:-}" ] && [ -f "${TUNNEL_KEY_DIR}/tunnel_fwd.pid" ]; then
    kill "$(cat "${TUNNEL_KEY_DIR}/tunnel_fwd.pid")" >/dev/null 2>&1 || true
  fi
  [ -z "${TUNNEL_FWD_PID:-}" ] || kill "$TUNNEL_FWD_PID" >/dev/null 2>&1 || true
  docker rm -f "$C_MINIO" "$C_WEBDAV" "$C_GOST" "$C_SQUID" "$C_SSHD" "$C_SFTP" >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
  rm -rf "$TUNNEL_KEY_DIR" "$WEBDAV_DATA_DIR" 2>/dev/null || true
  rm -f "$STATE_FILE" 2>/dev/null || true
}
trap cleanup EXIT

# --env-only：容器已由 --keep 保留，直接回放契约 env
if [ "$ENV_ONLY" = 1 ]; then
  if [ ! -f "$STATE_FILE" ]; then
    log "FAIL: --env-only needs the state file ${STATE_FILE}; run with --keep first"
    exit 1
  fi
  if ! docker ps --format '{{.Names}}' | grep -qx "$C_MINIO"; then
    log "FAIL: ${C_MINIO} is not running; the kept environment is gone. Re-run without --env-only."
    exit 1
  fi
  # shellcheck disable=SC1090
  . "$STATE_FILE"
  print_env
  exit 0
fi

rand_hex() { openssl rand -hex 24 2>/dev/null || head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n'; }
rand_user() { echo "$1$(openssl rand -hex 4 2>/dev/null || head -c 4 /dev/urandom | od -An -tx1 | tr -d ' \n')"; }

MINIO_ROOT_USER="$(rand_user dbxproxy)"
MINIO_ROOT_PASSWORD="$(rand_hex)"
MINIO_BUCKET="dbx-proxy-e2e-$(date +%s)"
MINIO_BUCKET2="dbx-proxy-e2e2-$(date +%s)"
GOST_PROXY_USER="$(rand_user gost)"
GOST_PROXY_PASS="$(rand_hex)"
SQUID_PROXY_USER="$(rand_user squid)"
SQUID_PROXY_PASS="$(rand_hex)"
WEBDAV_USER="$(rand_user webdav)"
WEBDAV_PASSWORD="$(rand_hex)"
SFTP_PASSWORD="$(rand_hex)"
TUNNEL_SSH_USER="$(rand_user tunnel)"

check_port_free() {
  local port="$1" label="$2"
  local busy=0
  if command -v lsof >/dev/null 2>&1; then
    lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1 && busy=1
  elif command -v nc >/dev/null 2>&1; then
    nc -z 127.0.0.1 "$port" >/dev/null 2>&1 && busy=1
  fi
  if [ "$busy" = 1 ]; then
    log "FAIL: host port ${port} (${label}) is already in use."
    log "      Override ports via env: DBX_PROXY_E2E_PORT_MINIO (19500) / DBX_PROXY_E2E_PORT_HTTP (18081) / DBX_PROXY_E2E_PORT_SOCKS (18082) / DBX_PROXY_E2E_PORT_SSHD (18222) / DBX_PROXY_E2E_PORT_SFTP (18223)"
    exit 4
  fi
}
check_port_free "$PORT_MINIO" "minio direct"
check_port_free "$PORT_HTTP_PROXY" "gost http proxy"
check_port_free "$PORT_SOCKS_PROXY" "gost socks5 proxy"
check_port_free "$PORT_SSHD" "tunnel sshd"
check_port_free "$PORT_SFTP" "sftp direct"
check_port_free "$PORT_TUNNEL_FWD" "tunnel forward probe (DBX_PROXY_E2E_PORT_TUNNEL_FWD)"

# GitHub 托管 runner 的共享 IP 池常年被 Docker Hub 匿名限流：先拉原镜像，
# 失败则按序尝试备用引用并打回原 tag（同 container_smoke.sh）。
ensure_image() {
  local image="$1"
  shift
  docker image inspect "$image" >/dev/null 2>&1 && return 0
  if docker pull "$image" >/dev/null 2>&1; then
    return 0
  fi
  local alt
  for alt in "$@"; do
    echo "  (docker hub pull failed for ${image}; trying ${alt})" >&2
    if docker pull "$alt" >/dev/null 2>&1; then
      docker tag "$alt" "$image"
      return 0
    fi
  done
  echo "FAIL: cannot pull ${image} from Docker Hub or any fallback" >&2
  return 1
}

# gost 镜像 fallback 链：go-gost/gost（多架构）→ 同引用 --platform linux/amd64
# （Apple Silicon Rosetta）→ ginuerzh/gost（v2，arm64 原生，监听语法兼容）。
# 成功的引用统一打 go-gost/gost 标签，后续 docker run 不再感知差异。
GOST_IMAGE="go-gost/gost"
ensure_gost_image() {
  docker image inspect "$GOST_IMAGE" >/dev/null 2>&1 && return 0
  if docker pull "$GOST_IMAGE" >/dev/null 2>&1; then
    return 0
  fi
  echo "  (go-gost/gost pull failed; trying --platform linux/amd64 via Rosetta)" >&2
  if docker pull --platform linux/amd64 "$GOST_IMAGE" >/dev/null 2>&1; then
    return 0
  fi
  echo "  (trying ginuerzh/gost as gost fallback)" >&2
  if docker pull ginuerzh/gost >/dev/null 2>&1; then
    docker tag ginuerzh/gost "$GOST_IMAGE"
    return 0
  fi
  echo "FAIL: cannot pull any gost image (go-gost/gost, ginuerzh/gost)" >&2
  return 1
}

# retry <desc> <tries> <cmd...>：每秒重试一次就绪探针
retry() {
  local desc="$1" tries="$2"
  shift 2
  local i
  for i in $(seq 1 "$tries"); do
    if "$@" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  log "FAIL: ${desc} not ready after ${tries}s" >&2
  return 1
}

# 代理通道的就绪探针：宿主 curl 拨宿主发布端口，代理解析网络内主机名
# ——正是 sidecar 侧被测的语义（gost 与 MinIO 同在 dbx-proxy-net 内）。
via_http_proxy() {
  curl -fsS --max-time 5 -x "http://${SQUID_PROXY_USER}:${SQUID_PROXY_PASS}@127.0.0.1:${PORT_HTTP_PROXY}" \
    "http://${C_MINIO}:9000/minio/health/live"
}
via_socks_proxy() {
  curl -fsS --max-time 5 --socks5-hostname "${GOST_PROXY_USER}:${GOST_PROXY_PASS}@127.0.0.1:${PORT_SOCKS_PROXY}" \
    "http://${C_MINIO}:9000/minio/health/live"
}
# SSH 跳板容器内可达性探针：容器内 nc -z；无 nc 时退回 bash 的 /dev/tcp
tunnel_probe_cmd="if command -v nc >/dev/null 2>&1; then nc -z ${C_MINIO} 9000; else timeout 3 sh -c 'cat < /dev/null > /dev/tcp/${C_MINIO}/9000'; fi"
tunnel_ssh() {
  ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new \
    -o UserKnownHostsFile="${TUNNEL_KEY_DIR}/known_hosts" \
    -i "${TUNNEL_KEY_DIR}/id_ed25519" -p "${PORT_SSHD}" \
    "${TUNNEL_SSH_USER}@127.0.0.1" "$1"
}
# 密钥认证握手就绪探针（sshd s6 init 需要数秒，首连常撞上启动窗口）
TUNNEL_ECHO_OUT=""
tunnel_echo_ready() {
  TUNNEL_ECHO_OUT="$(tunnel_ssh 'echo tunnel-ok')" || return 1
  [ "$TUNNEL_ECHO_OUT" = "tunnel-ok" ]
}
via_tunnel_ssh() {
  tunnel_ssh "$tunnel_probe_cmd"
}
# 真实隧道语义探针：宿主侧 `ssh -N -L` 把 dbx-proxy-minio-test:9000 改写为
# 127.0.0.1:<PORT_TUNNEL_FWD>——与 sidecar 的隧道实现同构。镜像默认
# sshd_config 带 AllowTcpForwarding no（会挡掉 -L），隧道容器已用
# sshd_config.d 覆盖；ExitOnForwardFailure 保证转发被拒时探针失败。
TUNNEL_FWD_PID=""
tunnel_forward_check() {
  ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new \
    -o UserKnownHostsFile="${TUNNEL_KEY_DIR}/known_hosts" \
    -o ExitOnForwardFailure=yes -o ConnectTimeout=5 \
    -i "${TUNNEL_KEY_DIR}/id_ed25519" -p "${PORT_SSHD}" \
    -L "127.0.0.1:${PORT_TUNNEL_FWD}:${C_MINIO}:9000" \
    -N "${TUNNEL_SSH_USER}@127.0.0.1" \
    >/dev/null 2>&1 &
  TUNNEL_FWD_PID=$!
  echo "$TUNNEL_FWD_PID" > "${TUNNEL_KEY_DIR}/tunnel_fwd.pid"
  local _
  for _ in $(seq 1 15); do
    if curl -fsS --max-time 3 "http://127.0.0.1:${PORT_TUNNEL_FWD}/minio/health/live" >/dev/null 2>&1; then
      return 0
    fi
    kill -0 "$TUNNEL_FWD_PID" 2>/dev/null || return 1
    sleep 1
  done
  return 1
}
# WebDAV 无发布端口：一次性 curl 容器在同网络内做带认证 PROPFIND
webdav_propfind() {
  docker run --rm --network "$NET" curlimages/curl \
    -fsS --max-time 5 -u "${WEBDAV_USER}:${WEBDAV_PASSWORD}" -X PROPFIND -H "Depth: 0" \
    "http://${C_WEBDAV}:8899/"
}
# SFTP 密码认证列目录探针。macOS 系统 curl（SecureTransport 构建，无 libssh2）
# 不支持 sftp 协议（实测 Protocol "sftp" not supported），故用一次性 rclone
# 容器走网络内 :22——与 sidecar 引擎同款 sftp 客户端，语义更贴近生产路径。
SFTP_OBSCURED_PASS=""
sftp_list() {
  docker run --rm --network "$NET" \
    -e "RCLONE_CONFIG_CHK_TYPE=sftp" \
    -e "RCLONE_CONFIG_CHK_HOST=${C_SFTP}" \
    -e "RCLONE_CONFIG_CHK_PORT=22" \
    -e "RCLONE_CONFIG_CHK_USER=sftpuser" \
    -e "RCLONE_CONFIG_CHK_PASS=${SFTP_OBSCURED_PASS}" \
    -e "RCLONE_CONFIG_CHK_SET_MODTIME=false" \
    -e "RCLONE_CONFIG_CHK_KNOWN_HOSTS_FILE=none" \
    --entrypoint rclone "$RCLONE_IMAGE" \
    lsf --contimeout 10s --timeout 10s --retries 1 --low-level-retries 1 chk:
}

log "==> creating network ${NET} (reuse if exists)"
docker network inspect "$NET" >/dev/null 2>&1 || docker network create "$NET" >/dev/null

# 历史运行残留的同名容器先替换（本脚本的专用测试容器）
docker rm -f "$C_MINIO" "$C_WEBDAV" "$C_GOST" "$C_SQUID" "$C_SSHD" "$C_SFTP" >/dev/null 2>&1 || true

log "==> starting MinIO (${C_MINIO}; direct 127.0.0.1:${PORT_MINIO}, in-net :9000)"
# MinIO 社区版镜像已下架（docker.io/quay.io 均 401，见 container_smoke.sh 注释）：
# server 用 bitnamilegacy/minio:2025.7.23，建桶用 rclone/rclone 连接字符串。
ensure_image "bitnamilegacy/minio:2025.7.23"
ensure_image "$RCLONE_IMAGE" "ghcr.io/rclone/rclone:1.75.1" "mirror.gcr.io/rclone/rclone:1.75.1"
# Bitnami 镜像数据目录为 /bitnami/minio/data（entrypoint 自建 + chown 降权），
# 显式 "server /data" 反而 file access denied；去掉 tmpfs 与显式命令。
docker run -d --rm --name "$C_MINIO" --network "$NET" \
  -p "127.0.0.1:${PORT_MINIO}:9000" \
  -e "MINIO_ROOT_USER=${MINIO_ROOT_USER}" \
  -e "MINIO_ROOT_PASSWORD=${MINIO_ROOT_PASSWORD}" \
  -e "MINIO_API_ODIRECT=off" \
  bitnamilegacy/minio:2025.7.23 >/dev/null
retry "MinIO direct health" 30 curl -fsS --max-time 3 "http://127.0.0.1:${PORT_MINIO}/minio/health/live"

log "==> creating buckets ${MINIO_BUCKET} + ${MINIO_BUCKET2} (rclone mkdir 连接字符串, in-net endpoint)"
# minio/mc 镜像同步下架；RCLONE_CONFIG_* env 在 v1.75 容器内静默不生效
# （假成功回退 Local），连接字符串 + 引号包 URL 是 container_smoke 实测形态。
MINIO_CS=":s3,provider=Minio,access_key_id=${MINIO_ROOT_USER},secret_access_key=${MINIO_ROOT_PASSWORD},endpoint='http://${C_MINIO}:9000'"
docker run --rm --network "$NET" \
  "$RCLONE_IMAGE" mkdir "${MINIO_CS}:${MINIO_BUCKET}" >/dev/null
docker run --rm --network "$NET" \
  "$RCLONE_IMAGE" mkdir "${MINIO_CS}:${MINIO_BUCKET2}" >/dev/null
# 建桶成功性校验（移植 container_smoke：mc mb 吞参假成功的历史教训）。
docker run --rm --network "$NET" \
  "$RCLONE_IMAGE" lsd "${MINIO_CS}:" | grep -q "${MINIO_BUCKET}" || {
  log "FAIL: bucket ${MINIO_BUCKET} not created"; exit 1; }

log "==> starting WebDAV (${C_WEBDAV}; in-net :8899 only, NO published port)"
ensure_image "$RCLONE_IMAGE" "ghcr.io/rclone/rclone:1.75.1" "mirror.gcr.io/rclone/rclone:1.75.1"
chmod 777 "$WEBDAV_DATA_DIR"  # rclone 容器默认 uid 需要对挂载卷可写
# 新版 rclone（v1.75+）移除了 serve webdav 的 --auth 旗标，用 --user/--pass
# （明文，仅存在于临时测试容器的进程参数，随容器销毁）
docker run -d --rm --name "$C_WEBDAV" --network "$NET" \
  -v "${WEBDAV_DATA_DIR}:/data" \
  "$RCLONE_IMAGE" serve webdav /data --addr :8899 \
  --user "${WEBDAV_USER}" --pass "${WEBDAV_PASSWORD}" >/dev/null

log "==> starting gost (${C_GOST}; SOCKS5 127.0.0.1:${PORT_SOCKS_PROXY}, 认证代理)。HTTP 代理由 squid 提供（gost v2 的 http 代理转发部分 S3 请求——CreateBucket/CompleteMultipartUpload——会挂起，实测复现；SOCKS5 通道无此问题）"
ensure_gost_image
docker run -d --rm --name "$C_GOST" --network "$NET" \
  -p "127.0.0.1:${PORT_SOCKS_PROXY}:18082" \
  "$GOST_IMAGE" \
  -L "socks5://${GOST_PROXY_USER}:${GOST_PROXY_PASS}@:18082" >/dev/null

log "==> starting squid (${C_SQUID}; HTTP 代理 127.0.0.1:${PORT_HTTP_PROXY}, APR1 basic auth)"
ensure_image ubuntu/squid "mirror.gcr.io/ubuntu/squid"
# ncsa_auth 读 APR1 hash（openssl passwd -apr1）；同网络内代理解析容器名。
mkdir -p "${TUNNEL_KEY_DIR}/squid"
printf '%s:%s\n' "$SQUID_PROXY_USER" "$(openssl passwd -apr1 "$SQUID_PROXY_PASS")" \
  > "${TUNNEL_KEY_DIR}/squid/passwd"
cat > "${TUNNEL_KEY_DIR}/squid/squid.conf" <<'EOF'
auth_param basic program /usr/lib/squid/basic_ncsa_auth /etc/squid/passwd
auth_param basic realm dbx-proxy-e2e
acl authenticated proxy_auth REQUIRED
http_access allow authenticated
http_access deny all
http_port 3128
coredump_dir /var/spool/squid
EOF
docker run -d --rm --name "$C_SQUID" --network "$NET" \
  -p "127.0.0.1:${PORT_HTTP_PROXY}:3128" \
  -v "${TUNNEL_KEY_DIR}/squid/squid.conf:/etc/squid/squid.conf:ro" \
  -v "${TUNNEL_KEY_DIR}/squid/passwd:/etc/squid/passwd:ro" \
  ubuntu/squid >/dev/null

log "==> starting tunnel sshd (${C_SSHD}; 127.0.0.1:${PORT_SSHD}, 仅密钥认证)"
# 每次重建容器 host key 必变；accept-new 只信任首见 key。清掉宿主
# known_hosts 里本测试端口的旧条目（ssh-keygen -R 的标准测试用途），
# 否则 sidecar/探针的 ssh 会因 REMOTE HOST IDENTIFICATION CHANGED 退出。
ssh-keygen -R "[127.0.0.1]:${PORT_SSHD}" >/dev/null 2>&1 || true
ensure_image "linuxserver/openssh-server" "ghcr.io/linuxserver/openssh-server" "mirror.gcr.io/linuxserver/openssh-server"
ssh-keygen -t ed25519 -N "" -f "${TUNNEL_KEY_DIR}/id_ed25519" -C dbx-proxy-e2e-tunnel >/dev/null
# 镜像默认 sshd_config 是 AllowTcpForwarding no，直接废掉 sidecar 的 `ssh -N -L`；
# 镜像 init 支持 sshd_config.d 目录（Include 在配置顶部，sshd 先取值生效），
# 挂载片段压过默认值。SFTP 容器不受影响（不做转发，保持最小权限）。
mkdir -p "${TUNNEL_KEY_DIR}/sshd_config.d"
cat > "${TUNNEL_KEY_DIR}/sshd_config.d/10-allow-tcp-forwarding.conf" <<'EOF'
AllowTcpForwarding yes
EOF
chmod 755 "${TUNNEL_KEY_DIR}/sshd_config.d"
chmod 644 "${TUNNEL_KEY_DIR}/sshd_config.d/10-allow-tcp-forwarding.conf"
docker run -d --rm --name "$C_SSHD" --network "$NET" \
  -p "127.0.0.1:${PORT_SSHD}:2222" \
  -v "${TUNNEL_KEY_DIR}/sshd_config.d:/config/sshd/sshd_config.d" \
  -e "PUBLIC_KEY=$(cat "${TUNNEL_KEY_DIR}/id_ed25519.pub")" \
  -e "USER_NAME=${TUNNEL_SSH_USER}" \
  -e "PASSWORD_ACCESS=false" \
  linuxserver/openssh-server >/dev/null

log "==> starting sftp (${C_SFTP}; direct 127.0.0.1:${PORT_SFTP}, in-net :22 via LISTEN_PORT=22)"
docker run -d --rm --name "$C_SFTP" --network "$NET" \
  -p "127.0.0.1:${PORT_SFTP}:22" \
  -e "LISTEN_PORT=22" \
  -e "USER_NAME=sftpuser" \
  -e "PASSWORD_ACCESS=true" \
  -e "USER_PASSWORD=${SFTP_PASSWORD}" \
  linuxserver/openssh-server >/dev/null

# ---------------------------------------------------------------------
# 容器级独立验证（不依赖 sidecar；全部通过才算脚本成功）
# ---------------------------------------------------------------------
log "==> container-level verification"

log "  [1/6] squid HTTP proxy -> in-net MinIO"
retry "squid HTTP proxy" 30 via_http_proxy \
  || { docker logs "$C_SQUID" >&2 2>/dev/null || true; exit 1; }
log "PASS: HTTP proxy http://${SQUID_PROXY_USER}:***@127.0.0.1:${PORT_HTTP_PROXY} reached ${C_MINIO}:9000/minio/health/live"

log "  [1b/6] HTTP proxy -> CreateBucket 流（gost v2 在此挂起的回归探针）"
S3_PROBE_BUCKET="dbx-proxy-e2e-cb-$(date +%s)"
docker run --rm --network "$NET" \
  -e "RCLONE_CONFIG_CB_TYPE=s3" \
  -e "RCLONE_CONFIG_CB_PROVIDER=Other" \
  -e "RCLONE_CONFIG_CB_ENDPOINT=http://${C_MINIO}:9000" \
  -e "RCLONE_CONFIG_CB_ACCESS_KEY_ID=${MINIO_ROOT_USER}" \
  -e "RCLONE_CONFIG_CB_SECRET_ACCESS_KEY=${MINIO_ROOT_PASSWORD}" \
  -e "RCLONE_CONFIG_CB_REGION=us-east-1" \
  -e "HTTP_PROXY=http://${SQUID_PROXY_USER}:${SQUID_PROXY_PASS}@${C_SQUID}:3128" \
  -e "HTTPS_PROXY=http://${SQUID_PROXY_USER}:${SQUID_PROXY_PASS}@${C_SQUID}:3128" \
  --entrypoint rclone "$RCLONE_IMAGE" \
  mkdir "cb:${S3_PROBE_BUCKET}" \
  || { docker logs "$C_SQUID" >&2 2>/dev/null || true; exit 1; }
log "PASS: CreateBucket (rclone mkdir) via HTTP proxy created ${S3_PROBE_BUCKET}"

log "  [2/6] gost SOCKS5 proxy -> in-net MinIO"
retry "gost SOCKS5 proxy" 30 via_socks_proxy \
  || { docker logs "$C_GOST" >&2 2>/dev/null || true; exit 1; }
log "PASS: SOCKS5 proxy socks5h://${GOST_PROXY_USER}:***@127.0.0.1:${PORT_SOCKS_PROXY} reached ${C_MINIO}:9000/minio/health/live"

log "  [3/6] isolation: direct port alive, in-net hostname unreachable from host"
curl -fsS --max-time 5 "http://127.0.0.1:${PORT_MINIO}/minio/health/live" >/dev/null \
  || { log "FAIL: MinIO direct control port ${PORT_MINIO} not reachable"; exit 1; }
# 跨平台宽容判断。解析层：fake-IP DNS（Surge/Clash 等，198.18.0.0/15）会给
# 任何域名返回占位 IP，"解析失败"在 writes fake-IP 的宿主上永不成立，故解析
# 结果只分类不硬判——占位段 WARN，真实地址判 FAIL。真正的隔离判据是端到端
# 不可达：宿主按网络内主机名发请求必须失败（解析失败与 fake-IP 不可达都算过）。
resolve_innet_name() {
  local ip=""
  if command -v getent >/dev/null 2>&1; then
    ip="$(getent hosts "$C_MINIO" 2>/dev/null | awk '{print $1; exit}')"
    [ -n "$ip" ] && { echo "$ip"; return 0; }
  fi
  if command -v python3 >/dev/null 2>&1; then
    ip="$(python3 -c "import socket; print(socket.gethostbyname('${C_MINIO}'))" 2>/dev/null || true)"
    [ -n "$ip" ] && { echo "$ip"; return 0; }
  fi
  return 1
}
if resolved_ip="$(resolve_innet_name)"; then
  case "$resolved_ip" in
    198.18.*|198.19.*)
      log "WARN: host resolver returned fake-IP ${resolved_ip} for ${C_MINIO} (fake-IP DNS); end-to-end check below is the real isolation proof"
      ;;
    *)
      log "FAIL: host resolves ${C_MINIO} to ${resolved_ip} — network isolation broken"
      exit 1
      ;;
  esac
else
  log "NOTE: host cannot resolve ${C_MINIO} at all (strict isolation)"
fi
if curl -fsS --max-time 5 --noproxy '*' \
  "http://${C_MINIO}:9000/minio/health/live" >/dev/null 2>&1; then
  log "FAIL: host CAN reach in-net service via hostname ${C_MINIO} — isolation broken"
  exit 1
fi
log "PASS: direct 127.0.0.1:${PORT_MINIO} alive AND host cannot reach ${C_MINIO}:9000 via in-net name"

log "  [4/6] SSH tunnel jumpbox (key-only auth) -> in-net MinIO"
retry "tunnel sshd key-auth handshake" 30 tunnel_echo_ready \
  || { log "FAIL: tunnel sshd echo: ${TUNNEL_ECHO_OUT}"; docker logs "$C_SSHD" >&2 2>/dev/null || true; exit 1; }
log "PASS: ssh ${TUNNEL_SSH_USER}@127.0.0.1:${PORT_SSHD} (key-only) echoed tunnel-ok"
retry "in-net MinIO reachable from inside tunnel sshd" 30 via_tunnel_ssh \
  || { docker logs "$C_SSHD" >&2 2>/dev/null || true; exit 1; }
log "PASS: ${C_MINIO}:9000 reachable from inside ${C_SSHD} (tunnel target viability)"
retry "ssh -N -L local forward" 5 tunnel_forward_check \
  || { log "FAIL: ssh -N -L forward refused or dead (AllowTcpForwarding?)"; docker logs "$C_SSHD" >&2 2>/dev/null || true; exit 1; }
[ -z "$TUNNEL_FWD_PID" ] || kill "$TUNNEL_FWD_PID" >/dev/null 2>&1 || true
log "PASS: ssh -N -L 127.0.0.1:${PORT_TUNNEL_FWD}->${C_MINIO}:9000 served MinIO health (sidecar tunnel semantics)"

log "  [5/6] SFTP password auth (published port + in-net :22 via rclone)"
SFTP_OBSCURED_PASS="$(docker run --rm --entrypoint rclone "$RCLONE_IMAGE" obscure "$SFTP_PASSWORD")"
retry "sftp password list" 30 sftp_list \
  || { docker logs "$C_SFTP" >&2 2>/dev/null || true; exit 1; }
log "PASS: sftp sftpuser@${C_SFTP}:22 (in-net) + 127.0.0.1:${PORT_SFTP} (published) listed home with password auth"

log "  [6/6] WebDAV authenticated PROPFIND (in-net one-shot client, no published port)"
ensure_image curlimages/curl "mirror.gcr.io/curlimages/curl"
retry "webdav in-net PROPFIND" 30 webdav_propfind \
  || { docker logs "$C_WEBDAV" >&2 2>/dev/null || true; exit 1; }
log "PASS: PROPFIND http://${C_WEBDAV}:8899/ with auth (in-net only) returned 207-class success"

# ---------------------------------------------------------------------
# 状态持久化 + 契约 env 输出（stdout 仅此块，python 驱动按行解析）
# ---------------------------------------------------------------------
umask 077
cat > "$STATE_FILE" <<EOF
PORT_MINIO=${PORT_MINIO}
PORT_HTTP_PROXY=${PORT_HTTP_PROXY}
PORT_SOCKS_PROXY=${PORT_SOCKS_PROXY}
PORT_SSHD=${PORT_SSHD}
PORT_SFTP=${PORT_SFTP}
MINIO_ROOT_USER=${MINIO_ROOT_USER}
MINIO_ROOT_PASSWORD=${MINIO_ROOT_PASSWORD}
MINIO_BUCKET=${MINIO_BUCKET}
MINIO_BUCKET2=${MINIO_BUCKET2}
GOST_PROXY_USER=${GOST_PROXY_USER}
GOST_PROXY_PASS=${GOST_PROXY_PASS}
SQUID_PROXY_USER=${SQUID_PROXY_USER}
SQUID_PROXY_PASS=${SQUID_PROXY_PASS}
WEBDAV_USER=${WEBDAV_USER}
WEBDAV_PASSWORD=${WEBDAV_PASSWORD}
SFTP_PASSWORD=${SFTP_PASSWORD}
TUNNEL_SSH_USER=${TUNNEL_SSH_USER}
TUNNEL_KEY_DIR=${TUNNEL_KEY_DIR}
EOF

log "==> all container-level checks passed; DBX_PROXY_E2E_* env:"
print_env
