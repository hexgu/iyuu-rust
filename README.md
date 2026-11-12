# iyuu-reseed

基于 IYUU 开放 API 的自动辅种工具。Rust 后端 + Vue 前端，自托管、单容器。

支持 **qBittorrent** 与 **Transmission**。

> ## ⚠️ 非官方项目声明
>
> 本项目是**第三方工具**，通过 IYUU 的公开 API 查询辅种数据，与 IYUU（iyuu.cn）
> **没有任何隶属、合作或代理关系**，也不是 IYUU 官方客户端。
>
> - **不做任何售后**：IYUU 账号、Token、会员开通与续费、额度策略、接口变更、站点绑定失败，均由 IYUU 官方负责，本项目不承担任何责任。
> - **不代收任何费用**：本项目不卖会员、不提供代开通，程序内出现的会员入口只是跳转到 IYUU 官方页面，是否购买请自行判断。
> - **不保证可用性**：IYUU 的接口、限流规则、会员权益随时可能调整，由此导致的查询失败、额度不足或账号受限，本项目不承担连带责任。
> - 辅种属于 PT 站点规则范畴，请自行确认各站点对辅种、H&R、分享率的要求。
>
> 会员权益与价格以 [IYUU 官方](https://iyuu.cn/app/user/member) 为准。

## IYUU 额度与会员类型

IYUU 的辅种接口是**按天计量**的。普通用户和 SVIP 的额度差 5 倍，这是使用本工具最先
会撞到的约束，所以程序里把它做成一等公民而不是隐藏设置。

| | 普通用户 | SVIP 会员 |
| --- | --- | --- |
| 默认入口 | `2025.iyuu.cn` | `vip.iyuu.cn` |
| 辅种查询 | 500 次/天 | 2500 次/天 |
| 种子覆盖 | 约 10 万/天 | 约 50 万/天 |
| 其他 | — | 超量免封禁、SVIP 徽章、模板消息 2400 条/天、邀请返佣、免费技术支持 |

每次查询最多带 **200 个 info hash**，所以「500 次 ÷ 2500 次」实际就是「10 万 ÷ 50 万种子」。

**会员类型必须你自己选，因为程序无法判断它。** 两个域名后面是同一套服务，同一个
Token 在两边都能用：能返回数据不代表你有会员，报错也不代表你没有。所以探测没有任何
意义——本程序不做这种探测，只在保存 Token 时让你明确表态，然后：

1. 请求发往该类型对应的入口（可用环境变量覆盖，见下文）；
2. **按对应额度做预算**，而不是先花完再被拒绝；
3. 如果实际额度小于你声明的，IYUU 那边会拒绝，日志里会显示限流——这才是唯一的判据。

### 限流是怎么处理的

不是简单加个 `sleep`，而是四层，缺一层就会真的被限流或封停：

| 层 | 位置 | 行为 |
| --- | --- | --- |
| 预算 | `ratelimit::QuotaBudget` | 按天记账，只按**整批**发放：一个请求无论带 1 个还是 200 个 hash 都算一次，所以零散的小批不占额度 |
| 持久化 | `db::load_api_usage` | 用量写进 SQLite。客户端每次运行都重建，但台账不能跟着丢——否则重启一次就等于「额度重置」，下一次运行必然超限 |
| 节流 | `IyuuClient::spend` | 每次请求前先扣账再发**并**排队；间隔固定 1200ms，不按等级区分（IYUU 按突发限流，与额度档位无关） |
| 熔断 | `IyuuClient::trip_breaker` | 一旦 IYUU 返回「超限/频繁/429」，立刻进入 30 分钟冷却，期间全部请求本地直接拒绝。重试只会把临时限流变成更长的封禁 |

额度不够时的行为是**截断而不是报错**：跑到一半没额度，已完成的部分照常入库，
日志里写明「还有 N 个种子留待下次」。查询结果为空和「压根没查」是两件事，
不能混为一谈。额度为 0 时则在**启动前**就拒绝（HTTP 429 + 重置时间），
不会先扫一遍下载器再失败。

### 会员入口

界面上（设置页 / 概览页横幅）会提示 SVIP 的额度与专享服务器，并提供跳转到
[IYUU 会员中心](https://iyuu.cn/app/user/member) 的按钮。**这只是个链接**：
本项目不代收费用、不代开通、不提供相关售后，也不会因为你在别处开通而获得任何分成。

## 为什么重写

MoviePilot 的 `iyuuautoseed` 插件在长期运行中会出现"加了一堆辅种，但全部都停在暂停状态"的现象。翻它的源码可以看到四个结构性缺陷：

| 问题 | 后果 |
| --- | --- |
| 没有处理 `checkingDL` 状态 | 校验中的种子被当成"还不能做种"，直接放弃 |
| `_auto_rechecked` 只补发一次校验 | qB 5.3+ 移除了 `skip_checking` 后，种子停在未校验状态且永不重试 |
| `_recheck_torrents` 是纯内存队列 | 插件/容器重启后，所有在途校验全部丢失，种子永久卡在暂停 |
| `success` 在 `add_torrent` 时计数 | 统计显示成功，实际大量种子校验未通过 —— **报告与事实脱节** |

本项目针对这四点重新设计：

1. **显式的状态机**，`checkingDL / checkingResumeData / stoppedDL / stoppedUP / missingFiles` 各有分支，校验中绝不算成功也不算失败。
2. **校验队列持久化到 SQLite**，进程重启后从数据库读回 `added/verifying` 记录继续推进。
3. **`seeding` 与 `added` 是两个不同的状态**，只有观察到"进度 100% 且不在校验中"才计入成功。
4. **不提供"跳过校验"开关**。保存路径不一致时跳过校验会让 qB 从 0% 重新下载整个资源，直接烧掉分享率——这正是这个工具要防止的事。

## 快速开始

### Docker（推荐）

```bash
docker compose up -d --build
# 打开 http://localhost:8098
```

首次打开进入「设置」：

1. 填 IYUU Token（[iyuu.cn](https://iyuu.cn) 获取），**选择会员类型**（普通 / SVIP），
   点「测试连接」。测试会按所选等级真实请求一次，并扣减 2 次额度
2. 填下载器地址与账号，点「测试连接」
3. 在「站点凭据」里给准备辅种的站点填 passkey（M-Team 填 API Key，Monika 填 RSS 链接）
4. 保存 → 回概览页点「开始辅种」

> **容器里的地址怎么写**：辅种服务在容器内，`127.0.0.1` 指的是容器自己。qBittorrent 跑在宿主机或局域网其他机器上时，要填宿主的局域网 IP（例如 `http://192.168.0.215:8080`），或者用 `host.docker.internal`（macOS/Windows）与 `--add-host` 显式指定。

### 本地开发

一条命令搞定前后端（后端 8098，前端 5173 带热更新并把 `/api` 代理到后端）：

```bash
./dev.sh
```

起来后是个小控制台，输指令回车：

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

也支持只跑一半：`./dev.sh api`、`./dev.sh web`；启动即开自动重启用
`./dev.sh --watch`；给 IDE 或后台用 `./dev.sh -d`（不接管 stdin）。
端口可覆盖：`BACKEND_PORT=9098 FRONTEND_PORT=5273 ./dev.sh`。

#### 局域网 / Windows 宿主机访问

默认就绑 `0.0.0.0`，前后端都对外。启动横幅会同时给出两个地址，各有用处：

```
前端 http://localhost:5173        ← 本机 & Windows 浏览器用这个
后端 http://localhost:8098
局域网设备用 http://192.168.0.215:5173   ← 手机 / 另一台电脑用这个
```

只想本机用：`HOST=127.0.0.1 ./dev.sh`。

**WSL2 注意事项（mirrored 模式，本机即如此）**

`networkingMode=mirrored` 下 Windows 和 WSL 共用同一个 loopback，所以：

- Windows 浏览器访问 **`localhost:5173`** 就能打开，不需要端口转发。
- ⚠️ 在 Windows 上访问 `192.168.0.215:5173` **会失败**：mirrored 模式让 Windows
  自己也持有这个地址，请求打到的是宿主自身的回环而不是 WSL。这不是 bug。
- 局域网里**其他设备**（手机等）用 `192.168.0.215:5173` 是正常的；不通就去 Windows
  防火墙放行 5173 / 8098 的入站规则。

`.wslconfig` 里 `firewall=false` 时 Windows 侧不会为 WSL 端口建立镜像监听，
这正是「宿主只能用 localhost、外部只能用局域网 IP」的成因。

**NAT 模式**下 WSL 只有 `172.x` 内网地址，脚本会在启动时打印需要的
`netsh interface portproxy` 转发命令（需要管理员 PowerShell，脚本不擅自改系统设置）。
也可以改用 mirrored：`%UserProfile%\.wslconfig` 里设 `networkingMode=mirrored`，
再 `wsl --shutdown` 重启。

用主机名（而非 IP）访问前端时，Vite 会校验 Host，需要额外放行：
`IYUU_ALLOWED_HOSTS=jarry-win.local,.lan ./dev.sh`。用 IP / localhost 不需要。

想手动起也行：

```bash
cargo run -- --data-dir ./data --web-dir ./web/dist --listen 0.0.0.0:8098
cd web && bun install && bun dev --host   # 另一个终端
```

需要 Rust stable 与 [bun](https://bun.sh)。生产构建：

```bash
cd web && bun run build      # 产出 web/dist，由后端直接托管
cargo build --release
```

### 中国网络环境

仓库已经把两处最主要的依赖源固定到国内镜像，**开箱即用，无需额外配置**：

| 依赖 | 镜像 | 配置位置 |
| --- | --- | --- |
| Rust crate | [rsproxy.cn](https://rsproxy.cn)（字节跳动 Rust 团队的 crates.io 稀疏索引全量镜像） | `.cargo/config.toml` |
| npm 包 | [registry.npmmirror.com](https://npmmirror.com) | `web/bunfig.toml` + Dockerfile `ENV` |

> **`[net] git-fetch-with-cli = true`** 是有意为之：libgit2 自带 HTTP 栈在某些环境会
> 忽略代理配置，交给 git 二进制才可靠。

#### 为什么 cargo 还是觉得慢

镜像只是解决了「下载」，实际耗时的大头通常是**编译**，尤其 `debug = 2`（cargo 默认）
会生成完整 DWARF 调试信息。本仓库在 `Cargo.toml` 里设了 `[profile.dev] debug = 1`：
保留行号表（断点和 backtrace 仍能对到源码行），丢掉类型/变量信息。

实测（12 核 WSL2，320 个 crate）：

| | 二进制大小 | 改动单文件后重编 |
| --- | --- | --- |
| `debug = 2`（默认） | 154 MB | ~6 s |
| `debug = 1`（本仓库） | 103 MB | ~6 s |

需要调试器里看变量时临时改回 `debug = 2` 即可，`--release` 不受影响。

如果 `cargo build` 卡住几十秒没有任何输出，先看是不是**并发的另一个 cargo 进程**
占着 target 目录的锁（`Blocking waiting for file lock on build directory`）。
`dev.sh` 已经把 stdin 接到 `/dev/null`，避免 rustc 读 stdin 报
`did not contain valid UTF-8`（这个问题在从管道/CI 里调用时才会暴露）。

Docker 构建还有两处需要留意：

1. **基础镜像**（`rust:1-slim`、`oven/bun:1-alpine`）只能从 Docker Hub 拉取，国内没有长期稳定的官方镜像站。建议在 `docker-compose.yml` 同目录先手动拉一次，或用你自己惯用的加速器：
   ```bash
   docker pull rust:1-slim
   docker pull oven/bun:1-alpine
   ```
2. **构建期代理**：如果构建机需要走代理才能访问外网，注意容器**看不到**宿主机的 `127.0.0.1`。用宿主在局域网里的地址，或者直接给构建加 `--network=host`：
   ```bash
   docker build --network=host -t iyuu-reseed .
   ```

如果还没有 Rust 工具链，用镜像安装最快：

```bash
export RUSTUP_DIST_SERVER=https://rsproxy.cn
export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
curl -sSf https://rsproxy.cn/rustup-init.sh | sh -s -- -y --profile minimal
```

## 工作流程

```
扫描下载器已完成种子
      │  本地过滤：未完成 / 校验中 / 我们自己的辅种 / 体积 / 标签 / 路径
      ▼
IYUU 查询（200 个 hash 一批，sha1 签名）
      │  候选去重：已在下载器 / 已辅种成功 / 已知失败
      ▼
构造下载链接 → 取 .torrent → 校验 info_hash 与候选一致
      │
      ▼
以「暂停」加入下载器，复用原种子 save_path，关闭自动分类（autoTMM）
      │  记录写入 SQLite（state=added）
      ▼
校验巡检（独立循环，默认 15s）
      │
      ├── checking* ──────────────► verifying（继续等，不算成功/失败）
      ├── 100% 且不在校验 ────────► seeding  ✅ 成功，自动开始做种
      ├── 校验完但仍不完整 ──────► verify_failed（保存路径不一致/资源不同）
      ├── missingFiles ──────────► missing_files
      └── 超过 3 小时仍未结束 ────► timed_out
```

关键细节：

- **save_path 一律复用原种子**，并强制 `autoTMM=false`。开着自动分类管理时 qB 会把辅种挪到分类目录，原种文件不复用就会"文件缺失"。
- **info_hash 由原始字节计算**（不是重新序列化后的 bencode），否则 hash 永远对不上。
- **.torrent 内容必须校验**：拿到 HTML 登录页、或 hash 不匹配时直接判失败，而不是把坏文件塞给下载器。
- **`is_https=0` 的站点走 http**：给纯 http 站点发 https 会在 TLS 握手阶段失败，看起来像"站点挂了"。

## 站点适配

| 类型 | 处理方式 |
| --- | --- |
| 常规 NexusPHP | 用 IYUU 的 `download_page` 模板填 passkey |
| TGG / HDCity / GPW 等（模板含不可解析占位符） | 抓 `details.php` 页面解析 `torrents.php?action=download` 链接，按 `id=` 精确匹配 |
| M-Team | `x-api-key` 调 `api/torrent/genDlToken` |
| Monika | 从 RSS 链接取 rsskey 拼 `torrents/download/{id}.{key}` |

详情页有多个下载链接且没有一个精确匹配 `id` 时，**宁可失败也不猜**——猜错会去给别人的资源做种。

## 配置项

| 配置 | 说明 |
| --- | --- |
| 会员类型 | `free` / `vip`，决定默认入口与每日额度。保存 Token 时弹窗必选 |
| 最小体积 | 小于该体积的源种子不参与辅种 |
| 并发获取数 | 同时下载种子的数量，站点有流控，建议 1–3 |
| 不辅种的标签 / 分类 | 命中的源种子跳过 |
| 不辅种的路径前缀 | 例如临时下载盘 |
| 复用原种子分类 | 避免分类目录不同导致文件被移动 |
| 校验通过后自动开始 | 关闭则种子保持"已完成但停止"，便于人工确认 |
| 校验轮询间隔 | 默认 15s，越小越及时但下载器负载越高 |
| 定时任务 | 5 字段 cron，例如 `0 4 * * *` |

## 环境变量

| 变量 | 说明 |
| --- | --- |
| `IYUU_API_BASE` | 覆盖 IYUU 接口地址（自建反代 / 镜像）。必须带 `http://` 或 `https://`，否则启动时告警并忽略 |
| `TZ` | 容器时区，影响 cron 触发与日志时间 |
| `IYUU_LOG` | 日志级别，例如 `info` / `iyuu_core=debug,info` |
| `IYUU_DATA_DIR` / `IYUU_WEB_DIR` / `IYUU_LISTEN` | 数据目录 / 前端产物 / 监听地址 |

`IYUU_API_BASE` **只从环境变量读，界面上没有这个配置项，也不会出现在任何 API 响应里**。
它是部署层面的事（镜像、反代），不该由浏览器决定，更不该出现在截图里：

```yaml
# docker-compose.yml
environment:
  IYUU_API_BASE: https://iyuu.example.com
```

## API

所有接口在 `/api` 下，错误统一返回 `{error: {kind, message}}`，`kind` 可取 `iyuu_auth` / `rate_limited` / `config` / `downloader_auth` / `downloader` / `not_found` / `internal`，前端据此给出可操作提示而不是"出错了"。

`rate_limited` 对应 HTTP 429，含义很明确：**现在别请求，等重置或换服务器**。它不是"再试一次"的信号。

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/health` `/version` `/status` | 健康检查与运行概况 |
| GET | `/quota` | 会员等级、当前服务器、今日额度与重置时间 |
| GET/PUT | `/config` | 读写配置（密钥只写不回显） |
| POST | `/test/iyuu` `/test/downloader` | 连通性测试 |
| GET | `/sites` `/sites/bound` | IYUU 支持/已绑定站点 |
| GET/POST | `/runs` | 任务列表 / 触发任务 |
| GET | `/runs/progress` | 当前任务实时进度与日志 |
| POST | `/runs/{id}/cancel` | 取消任务 |
| GET | `/attempts` `/attempts/stats` | 辅种记录与状态统计 |
| POST | `/attempts/{id}/requeue` | 手动重新校验 |
| DELETE | `/attempts/{id}` | 删除记录（不动下载器） |
| GET | `/downloader/torrents` `/downloader/summary` | 下载器种子浏览 |

## 项目结构

```
crates/core/          与存储无关的核心逻辑
  bencode.rs          字节精确的 torrent 解析与 info_hash
  models.rs           领域模型 + 辅种状态机
  ratelimit.rs        会员等级 / 额度 / 日界 / 限流识别（纯策略）
  iyuu.rs             IYUU API 客户端（额度记账、节流、熔断、sid_sha1 缓存）
  fetch.rs            (站点, torrent_id) → 校验过的 .torrent
  downloader/         qB / Transmission 适配层，统一 trait
  engine.rs           发现 + 校验两阶段编排
crates/server/        axum 服务、SQLite、调度、API
web/                  Vue 3 + shadcn-vue + Tailwind v4 + Vite
```

`core` 不依赖 SQLite 或 axum，持久化通过 `AttemptStore` trait 注入——因此引擎逻辑可以脱离服务单独测试。

`ratelimit.rs` 只放策略（等级、额度、日界、限流文案识别），记账与熔断在 `iyuu.rs`，
用量持久化在 server 的 `db.rs`。这样「额度怎么算」可以用纯函数测试，
「到底有没有发出请求」可以在集成层（HTTP 429 + 用量不变）验证。

## 测试与代码质量

```bash
make check   # 门禁：fmt --check + clippy -D warnings + test
cargo test   # 62 个单元测试
cd web && bun run build   # 含 vue-tsc 类型检查
```

`clippy` 的严格档（`pedantic`）通过 `[workspace.lints]` 常驻开启，所以下面这条就是完整门禁，不需要额外参数：

```bash
cargo clippy --all-targets --all-features --locked -- -D warnings
```

本项目额外禁用了 `unsafe_code`。少量确实不该改的写法（例如序列化/配置结构体里多个独立开关）用带理由的 `#[expect(...)]` 局部标注，而不是全局放宽——这样写法一旦不再需要抑制，编译器会提醒删掉。

核心正确性由单元测试覆盖：info_hash 必须基于原始字节、`checkingDL` 不得判为完成、`added` 不等于 `seeding`、cron 的 dom/dow 同时限定时取"或"、详情页多链接时不瞎猜、体积阈值边界与负数体积不放过等。

额度相关的行为同样有测试，因为算错一次就会被封：额度只按整批发放（3 次请求 + 250 hash 只能查 1 批）、
日界按 UTC+8 而不是 UTC、昨天的用量不算今天、用量为 0 时**不发请求**、
单次上限先于每日额度生效、熔断后 `budget()` 立刻归零。

## 注意

- **额度是硬约束**：普通用户 500 次/天意味着最多 10 万种子。种子多的时候，要么分几天跑，
  要么开通 IYUU 的 SVIP（2500 次/天）。
- **会员类型只影响预算，不影响能不能连上**。如果你其实是 SVIP 却选了「普通用户」，程序会按 500 次/天
  排预算，白白浪费额度；反过来把免费账号标成 SVIP，程序会按 2500 次排，超出部分由 IYUU 那边拒绝、
  日志显示限流。两种情况都不是故障，改一下类型即可。

- 辅种是把**已在本地存在的数据**注册到另一个站点，不会重复占盘。但前提是 save_path 与原种完全一致。
- 校验不通过的记录不会被自动反复重试（会污染日志和站点流控），修好路径后点「重新校验」即可。
- OpenCD 有 H&R 要求：辅种不产生下载量，但删种前仍建议确认站点 H&R 列为 GOOD。
- 本项目的会员/额度相关提示只是**告知与跳转**，不构成任何购买建议，也不承担 IYUU 侧的售后责任。
