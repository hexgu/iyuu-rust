#!/usr/bin/env bash
#
# dev.sh — 一条命令拉起 iyuu-reseed 的前后端开发环境。
#
#   ./dev.sh              # 后端(8098) + 前端(5173) + 交互控制台
#   ./dev.sh api          # 只起后端
#   ./dev.sh web          # 只起前端
#   ./dev.sh --watch      # 启动时即开启 Rust 源码变更自动重启
#   ./dev.sh -d           # 只起服务不接管 stdin（IDE / 后台用）
#
# 控制台快捷键（输入后回车）：
#   r   重启前后端          rb  只重启后端（重新编译）
#   rf  只重启前端          c   质量门禁 make check + 前端类型检查
#   s   进程 / 端口状态      o   浏览器打开前端
#   w   开关 Rust 自动重启   h   帮助
#   q   退出并清理全部进程   （Ctrl-C 同效）
#
# 环境变量：
#   HOST=0.0.0.0   监听地址（默认 0.0.0.0，局域网可直连；只想本机用设 127.0.0.1）
#   BACKEND_PORT=8098  FRONTEND_PORT=5173  LOG_LEVEL=info  NO_COLOR=1
#   IYUU_ALLOWED_HOSTS=a.local,.example.com  用主机名访问前端时额外放行的 Host
#
# WSL 说明：默认绑 0.0.0.0。WSL2 mirrored 模式下 eth0 就是真实局域网 IP，
# Windows 浏览器和其他设备都能直接访问；NAT 模式会在启动时提示需要做的
# netsh 端口转发命令（只提示，不擅自改系统设置）。
#
# 设计要点：后端由一个守护子进程独占管理（编译 → 启动 → 盯住），主进程和
# watcher 只通过 .dev/ 下的请求文件与状态文件跟它交流。这样任何一个终端里按
# 重启都不会出现 pid 不同步、端口抢不到的问题。
#
# 兼容 macOS 自带 bash 3.2；只依赖 cargo / bun / lsof / curl / ip。

set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$ROOT" || exit 1

BACKEND_PORT=${BACKEND_PORT:-8098}
FRONTEND_PORT=${FRONTEND_PORT:-5173}
LOG_LEVEL=${LOG_LEVEL:-info}
# 默认绑 0.0.0.0，局域网（以及 WSL 里的 Windows 浏览器）直接就能访问。
# 只想本机用时：HOST=127.0.0.1 ./dev.sh
HOST=${HOST:-0.0.0.0}

DEV_DIR="$ROOT/.dev"
mkdir -p "$DEV_DIR"
REQ_RESTART="$DEV_DIR/restart.req"
REQ_STOP="$DEV_DIR/stop.req"
ST_API="$DEV_DIR/api.state"   # starting | running | stopped | failed
ST_BOOT="$DEV_DIR/api.boot"   # 每次成功启动自增，主进程靠它判断「已换成新进程」
BIN="$ROOT/target/debug/iyuu-reseed"

# ---------------------------------------------------------------- 输出样式 ---
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
	C_RST=$'\033[0m' C_DIM=$'\033[2m' C_BOLD=$'\033[1m'
	C_API=$'\033[36m' C_WEB=$'\033[35m' C_OK=$'\033[32m' C_WARN=$'\033[33m' C_ERR=$'\033[31m'
else
	C_RST='' C_DIM='' C_BOLD='' C_API='' C_WEB='' C_OK='' C_WARN='' C_ERR=''
fi

say() { printf '%s[dev]%s %s\n' "$C_DIM" "$C_RST" "$*"; }
ok() { printf '%s[dev]%s %s\n' "$C_OK" "$C_RST" "$*"; }
warn() { printf '%s[dev]%s %s\n' "$C_WARN" "$C_RST" "$*"; }
err() { printf '%s[dev]%s %s\n' "$C_ERR" "$C_RST" "$*" >&2; }

# 给子进程输出加前缀，两路日志才不会糊在一起。
prefix() { # $1=标签 $2=颜色
	local tag=$1 color=$2 line
	while IFS= read -r line; do
		printf '%s%-3s|%s %s\n' "$color" "$tag" "$C_RST" "$line"
	done
}

# ---------------------------------------------------------------- 网络信息 ---
is_wsl() { [ -n "${WSL_DISTRO_NAME:-}" ] || grep -qiE 'microsoft|wsl' /proc/version 2>/dev/null; }

# 找一个能代表本机的局域网 IPv4：优先默认路由所在的网卡，跳过 docker/veth。
lan_ip() {
	local cidr dev addr
	cidr=$(ip route show default 2>/dev/null | awk '/default/{print $5; exit}')
	if [ -n "$cidr" ]; then
		addr=$(ip -4 addr show "$cidr" 2>/dev/null | awk '/inet /{print $2}' | cut -d/ -f1 | head -1)
		[ -n "$addr" ] && { echo "$addr"; return; }
	fi
	ip -4 addr show 2>/dev/null \
		| awk '/inet /{print $2}' | cut -d/ -f1 \
		| grep -vE '^(127\.|172\.1[7-9]\.|172\.2[0-9]\.|172\.3[01]\.|10\.255\.255\.254$)' \
		| head -1
}

# 判断 WSL 是 mirrored（eth0 直接是局域网 IP）还是 NAT（172.x + Windows 转发）。
wsl_mode_label() {
	local gw
	gw=$(ip route show default 2>/dev/null | awk '/default/{print $3; exit}')
	case "$gw" in
	172.1[6-9].*|172.2*|172.3[01].*|192.168.65.*) echo NAT ;;
	*) echo mirrored ;;
	esac
}

# WSL2 mirrored 模式下 eth0 拿到的就是真实局域网 IP，绑定 0.0.0.0 即可直连；
# NAT 模式下 Windows 侧需要端口转发，这里只提示、不擅自改系统设置。
wsl_hint() {
	is_wsl || return 0
	[ "$HOST" = "127.0.0.1" ] && return 0
	[ "$(wsl_mode_label)" = mirrored ] && return 0
	warn '检测到 WSL NAT 网络：Windows 侧还需要端口转发，局域网才能访问'
	warn "  PowerShell(管理员): netsh interface portproxy add v4tov4 listenport=$FRONTEND_PORT listenaddress=0.0.0.0 connectport=$FRONTEND_PORT connectaddress=$(lan_ip)"
	warn "  PowerShell(管理员): netsh interface portproxy add v4tov4 listenport=$BACKEND_PORT listenaddress=0.0.0.0 connectport=$BACKEND_PORT connectaddress=$(lan_ip)"
	warn '（或在 %UserProfile%\\.wslconfig 里设 networkingMode=mirrored，改完 wsl --shutdown 重启）'
	return 0
}

# 显示给用户看的 URL：绑 0.0.0.0 时给局域网地址，否则给回环地址。
# 主用地址。WSL2 mirrored 模式下 Windows 和 WSL 共用同一个 loopback，
# 所以 localhost 在这里是「WSL 里、Windows 浏览器里都能打开」的那个地址，
# 比局域网 IP 更稳（Windows 自己也占着宿主的局域网 IP，直连那个会打到宿主回环）。
url_for() {
	local port=$1
	if [ "$HOST" = "0.0.0.0" ] || [ "$HOST" = "127.0.0.1" ]; then
		echo "http://localhost:$port"
	else
		echo "http://$HOST:$port"
	fi
}

# 给局域网其他设备（手机、另一台电脑）用的地址。
lan_url() {
	local port=$1 ip
	ip=$(lan_ip)
	[ -n "$ip" ] || return 1
	echo "http://$ip:$port"
}

# ---------------------------------------------------------------- 基础工具 ---
port_pids() { lsof -ti "tcp:$1" -sTCP:LISTEN 2>/dev/null; }

free_port() {
	local port=$1 pids
	pids=$(port_pids "$port")
	[ -z "$pids" ] && return 0
	warn "端口 $port 上有残留进程，清掉：$(echo "$pids" | tr '\n' ' ')"
	# shellcheck disable=SC2086
	kill -TERM $pids 2>/dev/null
	local i=0
	while [ $i -lt 10 ] && [ -n "$(port_pids "$port")" ]; do
		sleep 0.3
		i=$((i + 1))
	done
	pids=$(port_pids "$port")
	# shellcheck disable=SC2086
	[ -n "$pids" ] && kill -KILL $pids 2>/dev/null
	sleep 0.2
	return 0
}

alive() { [ -n "${1:-}" ] && kill -0 "$1" 2>/dev/null; }
http_ok() { curl -fsS -m 2 -o /dev/null "$1" 2>/dev/null; }
api_state() { cat "$ST_API" 2>/dev/null || echo stopped; }
boot_id() { cat "$ST_BOOT" 2>/dev/null || echo 0; }

# ------------------------------------------------------------ 后端守护进程 ---
# 独占管理后端生命周期：
#   restart.req 出现 → 杀掉当前进程、重新编译、再启动
#   stop.req 出现    → 杀掉当前进程并退出
#   进程自己挂掉     → 报错并停住（编译错误时不该无限重启刷屏）
backend_supervisor() {
	# 父进程 pid 靠 $PPID 不可靠：子 shell 是 fork 出来的，PPID 仍指向脚本
	# 自己的父进程（可能早就退了），这里由主进程显式传进来。
	local bin_pid='' inner_loop=0 SUP_PARENT=$1
	# boot 计数只增不清零：主进程靠「变了没」判断是否换成了新进程，
	# 一旦重置就会出现「before 恰好等于新值」而永远等不到就绪的死等。
	echo stopped >"$ST_API"
	trap 'touch "$REQ_STOP"' TERM INT

	while :; do
		rm -f "$REQ_RESTART"
		echo starting >"$ST_API"
		say '编译后端（增量，首次会比较慢）…'
		# stdin 必须断开：cargo 探测 target info 时会跑 `rustc - ...`，
		# 继承到脚本的 stdin（管道/文件）就会读到非 UTF-8 内容而直接失败。
		if ! cargo build --quiet --bin iyuu-reseed < /dev/null; then
			err '后端编译失败，看上面的报错；修好后按 rb 重试'
			echo failed >"$ST_API"
			wait_for_restart || return 0
			continue
		fi
		if [ ! -x "$BIN" ]; then
			err "没找到构建产物 $BIN"
			echo failed >"$ST_API"
			wait_for_restart || return 0
			continue
		fi

		free_port "$BACKEND_PORT"
		"$BIN" \
			--data-dir "$ROOT/data" \
			--web-dir "$ROOT/web/dist" \
			--listen "$HOST:$BACKEND_PORT" \
			--log "$LOG_LEVEL" < /dev/null > >(prefix "api" "$C_API") 2>&1 &
		bin_pid=$!

		# 等一下再宣告 running：立刻检测会因为还没 bind 就误判成挂了。
		sleep 1
		if kill -0 "$bin_pid" 2>/dev/null; then
			echo $(( $(boot_id) + 1 )) >"$ST_BOOT"
			echo running >"$ST_API"
			ok "后端已启动 (pid $bin_pid)"
		fi

		inner_loop=0
		while alive "$bin_pid" && alive "$SUP_PARENT"; do
			if [ -f "$REQ_RESTART" ]; then
				say '重启后端：停止当前进程…'
				inner_loop=1
				break
			fi
			if [ -f "$REQ_STOP" ]; then
				inner_loop=2
				break
			fi
			sleep 0.4
		done
		# 主进程没了（被 kill -9 之类），守护进程不能独自留着占端口。
		alive "$SUP_PARENT" || inner_loop=2

		# 无论是被要求停还是自己崩的，都先把这一轮进程收干净。
		alive "$bin_pid" && kill -TERM "$bin_pid" 2>/dev/null
		wait "$bin_pid" 2>/dev/null
		echo stopped >"$ST_API"

		case "$inner_loop" in
		1) say '重新编译…' ;;
		2) return 0 ;;
		*)
			# 崩了不退出：留在这里等 rb，这样「改代码 → 修好 → rb」能直接续上，
			# 不用让主进程再重新拉一个守护进程。
			echo failed >"$ST_API"
			err '后端进程自己退出了（看上面的日志）；按 rb 重新编译并拉起'
			wait_for_restart || return 0
			;;
		esac
	done
}

# 编译失败后也停在这一步等 rb，而不是把守护进程一起结束掉。
wait_for_restart() {
	while :; do
		[ -f "$REQ_RESTART" ] && return 0
		[ -f "$REQ_STOP" ] && return 1
		alive "$SUP_PARENT" || return 1
		sleep 0.5
	done
}

wait_backend() {
	local before=$1 waited=0 failed_for=0
	local url="http://127.0.0.1:$BACKEND_PORT/api/health"
	while [ "$waited" -lt 300 ]; do
		local st; st=$(api_state)
		# 必须等到「换了新进程」且健康检查通过，否则可能读到旧进程的响应。
		if [ "$st" = running ] && [ "$(boot_id)" != "$before" ] && http_ok "$url"; then
			ok "后端就绪（${waited}s）$(url_for "$BACKEND_PORT")"
			return 0
		fi
		if [ "$st" = failed ]; then
			# 刚按过 rb 时状态可能还没来得及翻篇，给守护进程几秒反应时间。
			failed_for=$((failed_for + 1))
			if [ "$failed_for" -ge 3 ]; then
				err '后端没能启动（编译失败？看上面的报错，修好后按 rb）'
				return 1
			fi
		else
			failed_for=0
		fi
		if ! alive "${SUP_PID:-}"; then
			err '后端守护进程不在了'
			return 1
		fi
		sleep 1
		waited=$((waited + 1))
		[ $((waited % 15)) -eq 0 ] && say "后端还在编译/启动… ${waited}s"
	done
	err '后端 300s 内没就绪'
	return 1
}

start_backend() {
	if alive "${SUP_PID:-}"; then
		if [ "$(api_state)" = running ] && http_ok "http://127.0.0.1:$BACKEND_PORT/api/health"; then
			ok '后端已经在跑了'
			return 0
		fi
		# 守护进程还在（比如编译失败停在那里），让它重来一次就够了。
		restart_backend
		return
	fi
	rm -f "$REQ_STOP"
	local before; before=$(boot_id)
	backend_supervisor "$$" &
	SUP_PID=$!
	wait_backend "$before"
}

stop_backend() {
	alive "${SUP_PID:-}" || { rm -f "$REQ_STOP"; return 0; }
	say '停止后端…'
	touch "$REQ_STOP"
	local i=0
	while alive "$SUP_PID" && [ $i -lt 100 ]; do
		sleep 0.1
		i=$((i + 1))
	done
	alive "$SUP_PID" && kill -KILL "$SUP_PID" 2>/dev/null
	wait "$SUP_PID" 2>/dev/null
	SUP_PID=''
	rm -f "$REQ_STOP" "$REQ_RESTART"
	free_port "$BACKEND_PORT"
	echo stopped >"$ST_API"
}

restart_backend() {
	if ! alive "${SUP_PID:-}"; then
		start_backend
		return
	fi
	say "重启后端…"
	local before; before=$(boot_id)
	touch "$REQ_RESTART"
	wait_backend "$before"
}

# ---------------------------------------------------------------- 前端 ------ #
ensure_deps() {
	[ -d "$ROOT/web/node_modules" ] && return 0
	say '安装前端依赖（bun install）…'
	(cd "$ROOT/web" && bun install) || { err 'bun install 失败'; return 1; }
}

start_frontend() {
	if alive "${FRONTEND_PID:-}"; then
		ok '前端已经在跑了'
		return 0
	fi
	ensure_deps || return 1
	free_port "$FRONTEND_PORT"
	local shown; shown=$(url_for "$FRONTEND_PORT")
	say "启动前端 → ${C_BOLD}$shown${C_RST}"

	(
		cd "$ROOT/web" || exit 1
		# 代理目标固定走本机回环：后端就在同一个 WSL 里。
		export IYUU_API="http://127.0.0.1:$BACKEND_PORT"
		export IYUU_ALLOWED_HOSTS="${IYUU_ALLOWED_HOSTS:-}"
		# 用「主机名.local」访问时也要放行，否则 Vite 会 403。
		local ip; ip=$(lan_ip)
		[ -n "$ip" ] && export IYUU_ALLOWED_HOSTS="$IYUU_ALLOWED_HOSTS,$ip"
		exec bun run dev -- --host "$HOST" --port "$FRONTEND_PORT" --strictPort < /dev/null
	) > >(prefix "web" "$C_WEB") 2>&1 &
	FRONTEND_PID=$!

	local waited=0
	while [ "$waited" -lt 60 ]; do
		if ! alive "$FRONTEND_PID"; then
			err '前端进程退出了（看上面的输出）'
			FRONTEND_PID=''
			return 1
		fi
		if http_ok "http://127.0.0.1:$FRONTEND_PORT/"; then
			ok "前端就绪（${waited}s）"
			return 0
		fi
		sleep 1
		waited=$((waited + 1))
	done
	warn '前端 60s 内没响应，继续跑着，看日志'
	return 1
}

stop_frontend() {
	alive "${FRONTEND_PID:-}" || { FRONTEND_PID=''; return 0; }
	say '停止前端…'
	local kids
	kids=$(pgrep -P "$FRONTEND_PID" 2>/dev/null)
	[ -n "$kids" ] && kill -TERM $kids 2>/dev/null
	kill -TERM "$FRONTEND_PID" 2>/dev/null
	local i=0
	while alive "$FRONTEND_PID" && [ $i -lt 40 ]; do
		sleep 0.1
		i=$((i + 1))
	done
	alive "$FRONTEND_PID" && kill -KILL "$FRONTEND_PID" 2>/dev/null
	wait "$FRONTEND_PID" 2>/dev/null
	FRONTEND_PID=''
	free_port "$FRONTEND_PORT"
}

restart_frontend() {
	stop_frontend
	start_frontend
}

# ------------------------------------------------- Rust 变更自动重启（可选） --- #
WATCH_PID='' WATCH_ENABLED=0

# watcher 只负责「发现变更 → 写 restart.req」。真正干活的是守护进程，
# 所以这里不需要跟主进程共享任何 pid 状态。
#
# 防抖：一次保存/一次 rebase 常常同时改好几个文件，必须先等改动停下来再重启，
# 否则会连着重启三四轮，每轮都要重新编译。
watch_loop() {
	local stamp="$DEV_DIR/rs.stamp" hit first quiet
	touch "$stamp"
	while :; do
		sleep 2
		hit=$(find "$ROOT/crates" "$ROOT/Cargo.toml" -type f -newer "$stamp" 2>/dev/null | head -1)
		[ -z "$hit" ] && continue
		first=$hit

		# 等这批改动全部落地（最多静默等待 10s），别为同一次保存重启好几轮。
		quiet=0
		while [ "$quiet" -lt 10 ]; do
			sleep 1
			if [ -z "$(find "$ROOT/crates" "$ROOT/Cargo.toml" -type f -newer "$stamp" 2>/dev/null | head -1)" ]; then
				break
			fi
			touch "$stamp"
			quiet=$((quiet + 1))
		done
		touch "$stamp"

		printf '\n'
		say "检测到 Rust 变更（$(basename "$first")），自动重启后端…"
		touch "$REQ_RESTART"
		sleep 3 # 给守护进程时间去接管 restart.req
	done
}

toggle_watch() {
	if [ "$WATCH_ENABLED" = 1 ]; then
		alive "$WATCH_PID" && kill -TERM "$WATCH_PID" 2>/dev/null
		WATCH_PID=''
		WATCH_ENABLED=0
		say 'Rust 自动重启：关闭'
	else
		watch_loop &
		WATCH_PID=$!
		WATCH_ENABLED=1
		say 'Rust 自动重启：开启（保存 .rs 即重启后端）'
	fi
}

# ---------------------------------------------------------------- 清理 ------ #
cleanup() {
	trap - INT TERM EXIT
	alive "$WATCH_PID" && kill -TERM "$WATCH_PID" 2>/dev/null
	stop_frontend
	stop_backend
	printf '\n'
	ok '已退出，端口已释放'
}

# ---------------------------------------------------------------- 状态 ------ #
status() {
	LAN_ADDR=$(lan_ip)
	printf '\n'
	printf '  %s后端%s  %s  %s  %s\n' \
		"$C_API" "$C_RST" "$(url_for "$BACKEND_PORT")" "$(api_state)" \
		"$(http_ok "http://127.0.0.1:$BACKEND_PORT/api/health" && echo 健康 || echo 无响应)"
	printf '  %s前端%s  %s  %s\n' \
		"$C_WEB" "$C_RST" "$(url_for "$FRONTEND_PORT")" \
		"$(alive "${FRONTEND_PID:-}" && echo 运行中 || echo 未运行)"
	printf '  %s监听%s  %s:%s（后端）  %s:%s（前端）\n' \
		"$C_DIM" "$C_RST" "$HOST" "$BACKEND_PORT" "$HOST" "$FRONTEND_PORT"
	if [ -n "$lan" ] && [ "$HOST" = "0.0.0.0" ]; then
		local code
		code=$(curl -fsS -m 2 -o /dev/null -w '%{http_code}' "http://$lan:$FRONTEND_PORT/" 2>/dev/null)
		if [ "$code" = 200 ]; then
			printf '  %s局域网%s  http://%s:%s 可访问\n' "$C_DIM" "$C_RST" "$lan" "$FRONTEND_PORT"
		else
			printf '  %s局域网%s  http://%s:%s 不通（检查 Windows 防火墙）\n' "$C_WARN" "$C_RST" "$lan" "$FRONTEND_PORT"
		fi
		if is_wsl; then
			printf '  %sWSL%s  Windows 浏览器请用 http://localhost:%s（不是 %s）\n' \
				"$C_DIM" "$C_RST" "$FRONTEND_PORT" "$lan"
		fi
	fi
	printf '  %s自动重启%s %s\n' "$C_DIM" "$C_RST" "$([ "$WATCH_ENABLED" = 1 ] && echo 开启 || echo 关闭)"
	if http_ok "http://127.0.0.1:$BACKEND_PORT/api/status"; then
		printf '  %s/api/status%s\n    %s\n' "$C_DIM" "$C_RST" \
			"$(curl -fsS -m 3 "http://127.0.0.1:$BACKEND_PORT/api/status" | head -c 500)"
	fi
	printf '\n'
}

# ---------------------------------------------------------------- 质量门禁 --- #
# 逐行加前缀，同时保留命令自身退出码（进程替换不能塞进管道，否则 $? 只反映 while）。
stream() { "$@" > >(prefix "..." "$C_DIM") 2>&1; }

run_check() {
	printf '\n'
	say 'make check（fmt + clippy + test）…'
	stream make check
	local rc=$?
	if [ "$rc" -ne 0 ]; then
		err "make check 失败（exit $rc）"
		return "$rc"
	fi
	say '前端类型检查（vue-tsc）…'
	(cd "$ROOT/web" && stream bun run typecheck)
	rc=$?
	if [ "$rc" -ne 0 ]; then
		err "前端类型检查失败（exit $rc）"
		return "$rc"
	fi
	ok '全部通过'
}

open_web() {
	local url; url=$(url_for "$FRONTEND_PORT")/
	if is_wsl && command -v wslview >/dev/null 2>&1; then
		wslview "$url" >/dev/null 2>&1 && return 0
	fi
	if command -v open >/dev/null 2>&1; then open "$url"
	elif command -v xdg-open >/dev/null 2>&1; then xdg-open "$url" >/dev/null 2>&1 &
	else say "没有图形浏览器，手动访问 $url"; fi
}

# ---------------------------------------------------------------- 控制台 ---- #
help_console() {
	cat <<EOF

  ${C_BOLD}快捷键${C_RST}
    r    重启前后端          rb   只重启后端（重新编译）
    rf   只重启前端          c    质量门禁 + 前端类型检查
    s    进程 / 端口状态      o    浏览器打开前端
    w    开关 Rust 自动重启   h    这份帮助
    q    退出并清理           (Ctrl-C 同效)

EOF
}

console() {
	help_console
	local cmd
	while :; do
		printf '%s[dev]%s > ' "$C_DIM" "$C_RST"
		if ! IFS= read -r cmd; then break; fi
		case "${cmd%% *}" in
		'' | h | help | '?') help_console ;;
		r | restart | rs)
			say '重启全部…'
			restart_frontend
			restart_backend
			status
			;;
		rb | b | api) restart_backend; status ;;
		rf | f | web) restart_frontend; status ;;
		c | check | test) run_check ;;
		s | status | ps) status ;;
		o | open) open_web ;;
		w | watch) toggle_watch ;;
		q | quit | exit | ':q') break ;;
		*) warn "未知指令：$cmd（h 看帮助）" ;;
		esac
	done
}

usage() { sed -n '3,31p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

# ---------------------------------------------------------------- 主流程 ---- #
MODE=all CONSOLE=1 WATCH=0
for arg in "$@"; do
	case "$arg" in
	all) : ;;
	api | backend) MODE=api ;;
	web | frontend) MODE=web ;;
	--watch) WATCH=1 ;;
	-d | --daemon | --no-console) CONSOLE=0 ;;
	-h | --help | help) usage; exit 0 ;;
	*)
		err "未知参数：$arg（--help 看用法）"
		exit 2
		;;
	esac
done

for bin in cargo bun; do
	command -v "$bin" >/dev/null 2>&1 || { err "缺少 $bin，先装上再跑"; exit 1; }
done
command -v lsof >/dev/null 2>&1 || warn '没有 lsof，端口残留检测会失效'

SUP_PID='' FRONTEND_PID=''
trap cleanup INT TERM EXIT

printf '\n%s╭─ iyuu-reseed dev%s\n' "$C_BOLD" "$C_RST"
printf '%s│%s  前端 %s  (热更新，/api 代理到后端)\n' "$C_DIM" "$C_RST" "$(url_for "$FRONTEND_PORT")"
printf '%s│%s  后端 %s  (data: ./data)\n' "$C_DIM" "$C_RST" "$(url_for "$BACKEND_PORT")"
if [ "$HOST" = "0.0.0.0" ]; then
	printf '%s│%s  监听 0.0.0.0（前后端都对外）\n' "$C_DIM" "$C_RST"
	LAN_ADDR=$(lan_ip)
	if [ -n "$LAN_ADDR" ]; then
		printf '%s│%s  局域网设备用 http://%s:%s\n' "$C_DIM" "$C_RST" "$LAN_ADDR" "$FRONTEND_PORT"
	fi
	if is_wsl; then
		printf '%s│%s  环境 WSL2（%s 网络，宿主浏览器用 localhost 即可）\n' "$C_DIM" "$C_RST" "$(wsl_mode_label)"
	fi
fi
printf '%s╰─%s %s\n\n' "$C_BOLD" "$C_RST" "$(date '+%H:%M:%S')"

wsl_hint

case "$MODE" in
api) start_backend ;;
web) start_frontend ;;
*) start_backend && start_frontend ;;
esac

[ "$WATCH" = 1 ] && toggle_watch

# 只有 stdin 真的是终端才接管控制台。被 IDE / nohup / 管道拉起时 read 会立刻
# 拿到 EOF，若还进控制台就会秒退。
if [ "$CONSOLE" = 1 ] && { [ -t 0 ] || [ "${DEV_CONSOLE:-0}" = 1 ]; }; then
	console
	exit 0
fi

if [ "$CONSOLE" = 1 ]; then
	warn 'stdin 不是终端，跳过控制台（用 -d 可显式关掉这条提示）'
fi
say '运行中，Ctrl-C 退出'
while :; do sleep 5; done
