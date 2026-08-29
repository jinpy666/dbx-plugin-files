#!/usr/bin/env bash
# files 插件容器化冒烟：MinIO（s3 段）+ OpenSSH（sftp 段）+ Samba（smb 段），
# 消除 smoke SKIP。
#
# 设计（合规要点）：
# - 无任何硬编码凭据：MinIO root 凭据、Samba 测试账号密码运行时随机生成，
#   仅存在于进程环境与临时 env 文件，跑完删除；SFTP 私钥运行时生成于临时
#   目录，公钥经容器创建时的 PUBLIC_KEY 环境变量注入（P-FILES §3 合规复现
#   路径，不做运行时 authorized_keys 写入）。
# - 不拼接 shell 字符串：mc 用原生 MC_HOST_ 环境变量；所有可变值走 env/参数。
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
MINIO_USER="dbxsmoke"
MINIO_PASSWORD="$(openssl rand -hex 24 2>/dev/null || head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')"
MINIO_BUCKET="dbx-files-smoke-$(date +%s)"
SFTP_USER="sftpuser"
SMB_USER="smoketest"
SMB_SHARE="smoke"
SMB_PASSWORD="$(openssl rand -hex 24 2>/dev/null || head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')"

cleanup() {
  if [ "$KEEP" = 1 ]; then
    echo "KEEP: containers left running; key dir: $TMPDIR_SMOKE"
    return
  fi
  docker rm -f dbx-files-minio-test dbx-files-sftp-test dbx-files-samba-test >/dev/null 2>&1 || true
  rm -rf "$TMPDIR_SMOKE"
}
trap cleanup EXIT

# 一次性 SFTP 密钥对（ed25519，仅存在于临时目录）
ssh-keygen -t ed25519 -N "" -f "$TMPDIR_SMOKE/smoke_key" -C dbx-files-smoke >/dev/null

echo "==> starting MinIO (:${MINIO_PORT})"
docker rm -f dbx-files-minio-test >/dev/null 2>&1 || true
docker run -d --rm --name dbx-files-minio-test \
  -p "${MINIO_PORT}:9000" \
  -e "MINIO_ROOT_USER=${MINIO_USER}" \
  -e "MINIO_ROOT_PASSWORD=${MINIO_PASSWORD}" \
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

echo "==> starting OpenSSH test server (:${SFTP_PORT})"
docker rm -f dbx-files-sftp-test >/dev/null 2>&1 || true
docker run -d --rm --name dbx-files-sftp-test \
  -p "${SFTP_PORT}:2222" \
  -e "PUBLIC_KEY=$(cat "$TMPDIR_SMOKE/smoke_key.pub")" \
  -e "USER_NAME=${SFTP_USER}" \
  -e "PASSWORD_ACCESS=true" \
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
} > "$ENV_FILE"
chmod 600 "$ENV_FILE"

echo "==> running full smoke (fs + memory + s3 + sftp + smb)"
set -a
. "$ENV_FILE"
set +a
python3 scripts/smoke_test.py
