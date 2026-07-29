#!/usr/bin/env bash
# Kiro-rs 服务器侧重启脚本
#
# 行为：
#   1. 停止并移除现有 kiro-rs 容器
#   2. 从最新镜像 (kiro-rs-custom:latest) 启动新容器
#   3. attach 到 sub2api-deploy_sub2api-network 网络（让 sub2api 容器能直连）
#
# 使用：
#   bash tools/redeploy.sh                         # 仅重启（用现有镜像）
#   bash tools/redeploy.sh --build /path/to/src    # 先 docker build 再重启
#
# 服务器 cron / 手动重启都应走这个脚本，避免漏 docker network connect。

set -euo pipefail

CONTAINER_NAME="${CONTAINER_NAME:-kiro-rs}"
IMAGE="${IMAGE:-kiro-rs-custom:latest}"
# 默认值 = 生产真值（152.53.242.23）。改动前先确认生产拓扑，
# 用错端口会让 new2api 侧 502，用错网络名会让容器名互访 connection refused。
HOST_PORT="${HOST_PORT:-38990}"
CONFIG_DIR="${CONFIG_DIR:-/opt/kiro-rs/config}"
SUB2API_NETWORK="${SUB2API_NETWORK:-new2api-net}"

# 可选 build：bash redeploy.sh --build <src_dir>
if [[ "${1:-}" == "--build" ]]; then
  SRC_DIR="${2:-/opt/kiro-rs-src}"
  echo "[redeploy] docker build $IMAGE from $SRC_DIR ..."
  docker build -t "$IMAGE" "$SRC_DIR"
fi

echo "[redeploy] stop & remove existing container '$CONTAINER_NAME' (if any) ..."
docker stop "$CONTAINER_NAME" 2>/dev/null || true
docker rm "$CONTAINER_NAME" 2>/dev/null || true

echo "[redeploy] run new container ..."
docker run -d \
  --name "$CONTAINER_NAME" \
  --restart unless-stopped \
  -v "$CONFIG_DIR":/app/config \
  -p "$HOST_PORT":8990 \
  "$IMAGE" \
  ./kiro-rs -c /app/config/config.json --credentials /app/config/credentials.json

# 关键步骤：attach 到 sub2api 网络，让 sub2api 容器能通过容器名访问 kiro-rs
# 没这一步的话 sub2api → kiro-rs 会 connection refused（不同 docker bridge 网络互不通）
if docker network inspect "$SUB2API_NETWORK" >/dev/null 2>&1; then
  echo "[redeploy] attaching to docker network '$SUB2API_NETWORK' ..."
  if docker network connect "$SUB2API_NETWORK" "$CONTAINER_NAME"; then
    echo "[redeploy] OK: connected to $SUB2API_NETWORK"
  else
    echo "[redeploy] WARN: docker network connect failed (already attached?)" >&2
  fi
else
  echo "[redeploy] NOTE: network '$SUB2API_NETWORK' not found, skipping attach" >&2
fi

echo "[redeploy] container status:"
docker ps --filter "name=^${CONTAINER_NAME}$" --format '  {{.Names}}\t{{.Status}}\t{{.Image}}'

echo "[redeploy] done."
