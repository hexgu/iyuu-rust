# iyuu-reseed

基于 IYUU 开放 API 的自动辅种工具。Rust 后端 + Vue 前端，自托管、单容器。

支持 **qBittorrent** 与 **Transmission**。

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

1. 填 IYUU Token（[iyuu.cn](https://iyuu.cn) 获取），点「测试连接」
2. 填下载器地址与账号，点「测试连接」
3. 在「站点凭据」里给准备辅种的站点填 passkey（M-Team 填 API Key，Monika 填 RSS 链接）
4. 保存 → 回概览页点「开始辅种」

> **容器里的地址怎么写**：辅种服务在容器内，`127.0.0.1` 指的是容器自己。qBittorrent 跑在宿主机或局域网其他机器上时，要填宿主的局域网 IP（例如 `http://192.168.0.215:8080`），或者用 `host.docker.internal`（macOS/Windows）与 `--add-host` 显式指定。

### 本地开发

```bash
# 后端
cargo run -- --data-dir ./data --web-dir ./web/dist --listen 127.0.0.1:8098

# 前端（另一个终端，带热更新 + /api 代理）
cd web && bun install && bun dev
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
| Rust crate | [rsproxy.cn](https://rsproxy.cn) | `.cargo/config.toml` |
| npm 包 | [registry.npmmirror.com](https://npmmirror.com) | `web/bunfig.toml` + Dockerfile `ENV` |

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
| 最小体积 | 小于该体积的源种子不参与辅种 |
| 并发获取数 | 同时下载种子的数量，站点有流控，建议 1–3 |
| 不辅种的标签 / 分类 | 命中的源种子跳过 |
| 不辅种的路径前缀 | 例如临时下载盘 |
| 复用原种子分类 | 避免分类目录不同导致文件被移动 |
| 校验通过后自动开始 | 关闭则种子保持"已完成但停止"，便于人工确认 |
| 校验轮询间隔 | 默认 15s，越小越及时但下载器负载越高 |
| 定时任务 | 5 字段 cron，例如 `0 4 * * *` |

## API

所有接口在 `/api` 下，错误统一返回 `{error: {kind, message}}`，`kind` 可取 `iyuu_auth` / `config` / `downloader_auth` / `downloader` / `not_found` / `internal`，前端据此给出可操作提示而不是"出错了"。

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/health` `/version` `/status` | 健康检查与运行概况 |
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
  iyuu.rs             IYUU API 客户端（重试 / sid_sha1 缓存）
  fetch.rs            (站点, torrent_id) → 校验过的 .torrent
  downloader/         qB / Transmission 适配层，统一 trait
  engine.rs           发现 + 校验两阶段编排
crates/server/        axum 服务、SQLite、调度、API
web/                  Vue 3 + shadcn-vue + Tailwind v4 + Vite
```

`core` 不依赖 SQLite 或 axum，持久化通过 `AttemptStore` trait 注入——因此引擎逻辑可以脱离服务单独测试。

## 测试

```bash
cargo test          # 37 个单元测试
cd web && bun run build   # 含 vue-tsc 类型检查
```

核心正确性由单元测试覆盖：info_hash 必须基于原始字节、`checkingDL` 不得判为完成、`added` 不等于 `seeding`、cron 的 dom/dow 同时限定时取"或"、详情页多链接时不瞎猜等。

## 注意

- 辅种是把**已在本地存在的数据**注册到另一个站点，不会重复占盘。但前提是 save_path 与原种完全一致。
- 校验不通过的记录不会被自动反复重试（会污染日志和站点流控），修好路径后点「重新校验」即可。
- OpenCD 有 H&R 要求：辅种不产生下载量，但删种前仍建议确认站点 H&R 列为 GOOD。
