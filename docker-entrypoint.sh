#!/bin/sh
# 容器以 root 起步，只做一件事：把数据目录交还给应用用户，然后立刻降权。
#
# 为什么不在 Dockerfile 里直接 `USER 10001`：`./data` 是宿主机上的 bind mount，
# 它的属主由宿主机决定（通常是 root）。镜像里的 USER 管不到挂载点，直接以非 root
# 启动会打不开 SQLite；而让用户先手工 chown 一遍是升级时最容易踩的坑。
#
# 所以这里保持「root 起步 → 修属主 → exec 降权」：应用进程本身永远以 10001 运行，
# 需要的能力（CHOWN / SETUID / SETGID / DAC_OVERRIDE）由 compose 显式授予，
# 其余能力全部 drop。第一次启动之后 /data 已经是 10001 的，chown 就是空操作。
set -eu

APP_UID=10001
APP_GID=10001
DATA_DIR="${IYUU_DATA_DIR:-/data}"

if [ "$(id -u)" = "0" ]; then
  if [ -d "$DATA_DIR" ]; then
    # 追求幂等：属主已经对了就什么都不做，避免每次启动都遍历一遍目录。
    if [ "$(stat -c '%u:%g' "$DATA_DIR" 2>/dev/null || echo '')" != "$APP_UID:$APP_GID" ]; then
      chown -R "$APP_UID:$APP_GID" "$DATA_DIR" || true
    fi
  fi
  exec setpriv --reuid="$APP_UID" --regid="$APP_GID" --clear-groups -- "$@"
fi

exec "$@"
