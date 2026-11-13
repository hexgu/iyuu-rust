# 贡献指南

感谢你愿意改进 IYUU Reseed。这份文档面向要**构建、修改、测试**本项目的人；
只想部署使用的话，看 [README.md](README.md) 就够了。

提交任何改动前，请先让 `make check` 通过——CI 跑的就是同一套检查。

---

## 从源码构建

需要 **Rust stable** 与 [bun](https://bun.sh)（前端构建脚本用 bun）。

```bash
git clone https://github.com/hexgu/iyuu-rust.git
cd iyuu-rust

# 先构建前端产物到 web/dist
cd web && bun install && bun run build && cd ..

# 再构建 release 二进制
cargo build --release

./target/release/iyuu-reseed --data-dir ./data --web-dir ./web/dist
```

也可以交给 Docker（构建注意事项见 [构建与网络环境](#构建与网络环境)）：

```bash
docker build -t iyuu-reseed .
```

## 本地开发

一条命令拉起前后端：后端 8098，前端 5173 带热更新，并把 `/api` 代理到后端。

```bash
./dev.sh
```

启动后进入一个小控制台，输入指令回车：

| 指令 | 作用 |
| --- | --- |
| `r` | 重启前后端 |
| `rb` | 只重启后端（会重新编译） |
| `rf` | 只重启前端 |
| `c` | 质量门禁：`make check` + 前端类型检查 |
| `s` | 查看进程 / 端口 / `/api/status` |
| `o` | 浏览器打开前端 |
| `w` | 开关 Rust 源码变更自动重启 |
| `q` | 退出并清理所有子进程（Ctrl-C 同效） |

常用变体：

```bash
./dev.sh api                 # 只起后端
./dev.sh web                 # 只起前端
./dev.sh --watch             # 启动即开 Rust 源码自动重启
./dev.sh -d                  # 给 IDE / 后台用，不接管 stdin

BACKEND_PORT=9098 FRONTEND_PORT=5273 ./dev.sh   # 换端口
HOST=127.0.0.1 ./dev.sh                          # 只本机可用（默认绑 0.0.0.0）
```

也可以手动分别启动：

```bash
cargo run -- --data-dir ./data --web-dir ./web/dist --listen 0.0.0.0:8098
cd web && bun install && bun dev --host   # 另一个终端
```

<details>
<summary><b>WSL2 / 局域网访问注意事项</b></summary>

启动横幅会同时给出本机与局域网两个地址，各有用处：

```text
前端 http://localhost:5173              ← 本机 & Windows 浏览器用这个
后端 http://localhost:8098
局域网设备用 http://192.168.1.10:5173   ← 手机 / 另一台电脑用这个
```

**mirrored 网络模式**下 Windows 和 WSL 共用同一个 loopback：

- Windows 浏览器访问 `localhost:5173` 就能打开，不需要端口转发。
- ⚠️ 在 Windows 上访问 `192.168.1.10:5173` **会失败**：mirrored 模式让 Windows 自己也持有这个地址，
  请求打到的是宿主回环而不是 WSL。这不是 bug。
- 局域网里**其他设备**（手机等）用 `192.168.1.10:5173` 正常；不通就去 Windows 防火墙放行 5173 / 8098 入站。

`.wslconfig` 里 `firewall=false` 时 Windows 侧不会为 WSL 端口建立镜像监听，这正是「宿主只能用 localhost、外部只能用局域网 IP」的成因。

**NAT 模式**下 WSL 只有 `172.x` 内网地址，脚本会在启动时打印需要的
`netsh interface portproxy` 转发命令（需管理员 PowerShell，脚本不擅自改系统设置）。
也可以改用 mirrored：`%UserProfile%\.wslconfig` 里设 `networkingMode=mirrored`，再 `wsl --shutdown` 重启。

用主机名（而非 IP）访问前端时，Vite 会校验 Host，需要额外放行：
`IYUU_ALLOWED_HOSTS=dev-host.local,.lan ./dev.sh`。用 IP / localhost 不需要。

</details>

## 项目结构

```text
crates/core/            与存储、HTTP 无关的核心逻辑
  bencode.rs            字节精确的 torrent 解析与 info_hash
  models.rs             领域模型 + 辅种状态机
  ratelimit.rs          会员等级 / 额度 / 日界 / 限流识别（纯策略）
  iyuu.rs               IYUU API 客户端（额度记账、节流、熔断、sid_sha1 缓存）
  fetch.rs              (站点, torrent_id) → 校验过的 .torrent
  downloader/           qBittorrent / Transmission 适配层，统一 trait
  engine.rs             发现 + 校验两阶段编排
crates/server/          axum 服务、SQLite、调度、HTTP API
  api.rs                路由与 DTO
  config.rs             配置模型与脱敏后的公开视图
  db.rs                 SQLite 表结构与查询
  runner.rs             任务执行、进度与日志
  scheduler.rs          cron 解析与下次触发时间
  importer.rs           AutoSeed 备份导入
  env_config.rs         部署期环境变量（IYUU_API_BASE）
web/                    Vue 3 + shadcn-vue + Tailwind v4 + Vite
compose.yml             生产部署配置
dev.sh                  本地开发控制台
docs/screenshots/       README 截图
```

`core` 不依赖 SQLite 或 axum，持久化通过 `AttemptStore` trait 注入，因此引擎逻辑可以脱离服务单独测试。
`ratelimit.rs` 只放策略（等级、额度、日界、限流文案识别），记账与熔断在 `iyuu.rs`，用量持久化在 server 的 `db.rs`：
这样「额度怎么算」可以用纯函数测试，「到底有没有发出请求」可以在集成层（HTTP 429 + 用量不变）验证。

## 质量门禁

```bash
make check   # fmt --check + clippy -D warnings + test
```

CI 会跑同样的三件事，外加前端类型检查与 Docker 镜像构建。`clippy` 的严格档（`pedantic`）通过
`[workspace.lints]` 常驻开启，所以下面这条就是完整门禁，不需要额外参数：

```bash
cargo clippy --all-targets --all-features --locked -- -D warnings
```

- 本项目额外 `forbid(unsafe_code)`。
- 少数确实不该改的写法（例如序列化 / 配置结构体里多个独立开关）用带理由的 `#[expect(...)]` 局部标注，
  而不是全局放宽——等写法不再需要抑制，编译器会提醒删掉。
- 前端：`cd web && bun run build`（含 `vue-tsc` 类型检查）。

核心正确性由 **90 个单元测试**覆盖：info_hash 必须基于原始字节、`checkingDL` 不得判为完成、
`added` 不等于 `seeding`、cron 的 dom/dow 同时限定时取「或」、详情页多链接时不瞎猜、体积阈值边界与负数体积不放过等。

额度相关的行为同样有测试，因为算错一次就可能被封：额度只按整批发放（3 次请求 + 250 hash 只能查 1 批）、
日界按 UTC+8 而非 UTC、昨天的用量不算今天、用量为 0 时**不发请求**、单次上限先于每日额度生效、熔断后额度立刻归零。

## 构建与网络环境

仓库只使用官方源：cargo 走 crates.io，前端走 registry.npmjs.org，镜像本身在 CI 里构建。
**镜像站、代理、内网地址都不写进仓库**——这类配置只对个人网络有意义，应放在本机未跟踪文件里。
以下两个文件已在 `.gitignore` 中，适合放个人加速配置：

```toml
# .cargo/config.toml —— 只影响你自己这台机器
[source.crates-io]
replace-with = "local-mirror"

[source.local-mirror]
registry = "sparse+https://<你的 crate 镜像>/index/"

[net]
git-fetch-with-cli = true
```

```toml
# web/bunfig.toml
[install]
registry = "https://<你的 npm 镜像>/"
```

> `[net] git-fetch-with-cli = true` 值得保留：libgit2 自带的 HTTP 栈在某些环境会忽略代理配置，交给 git 二进制才可靠。

构建 Docker 镜像时同理。容器看不到宿主机的 `127.0.0.1`，如果构建机需要代理，用宿主在局域网里的地址，或用 host 网络构建：

```bash
docker build --network=host -t iyuu-reseed .
```

基础镜像（`rust:1-slim`、`oven/bun:1-alpine`、`debian:bookworm-slim`）来自 Docker Hub，网络受限时建议先手动 `docker pull` 一次。

### 为什么 cargo 编译慢

依赖下载只是其中一项，耗时大头通常是**编译**。`debug = 2`（cargo 默认）会生成完整 DWARF 调试信息；
本仓库在 `Cargo.toml` 里设了 `[profile.dev] debug = 1`：保留行号表（断点和 backtrace 仍能对到源码行），
丢掉类型 / 变量信息。

实测（12 核 WSL2，320 个 crate）：

| | 二进制大小 | 改动单文件后重编 |
| --- | --- | --- |
| `debug = 2`（默认） | 154 MB | ~6 s |
| `debug = 1`（本仓库） | 103 MB | ~6 s |

需要调试器里看变量时临时改回 `debug = 2` 即可，`--release` 不受影响。

如果 `cargo build` 卡住几十秒没输出，先看是不是**另一个 cargo 进程**占着 target 目录的锁
（`Blocking waiting for file lock on build directory`）。`dev.sh` 已把 stdin 接到 `/dev/null`，
避免 rustc 读 stdin 报 `did not contain valid UTF-8`（从管道 / CI 调用时才会暴露）。

## 提交改动

欢迎 Issue 与 PR。提交前请确保 `make check` 通过；CI 会检查 fmt、clippy、测试，以及前端类型检查与镜像构建。
