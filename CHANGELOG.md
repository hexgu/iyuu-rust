# 更新日志

本项目的版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)，变更分类参考
[Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

## [Unreleased]

### 修复

- **一条脏数据会丢掉整批辅种结果**：`/reseed/index/index` 的响应原本被整体反序列化成
  `HashMap<String, Vec<SeedCandidate>>`，任何一个候选行的字段类型不符都会让**整个批次**
  （最多 200 个 hash 的命中结果）作废，日志只留下
  `响应数据结构不符：invalid type: string "631188", expected i64`。
  现在逐行解析：坏行被跳过并计入警告（带原始数据，便于定位是哪个站点），同批其余候选照常使用。
  `torrent_id` / `sid` 同时接受数字、数字字符串与整数值浮点（部分站点的 spider 行会以字符串返回）；
  非整数、非法字符串、越界值仍然报错而不是默认成 0——0 会拼出一个看起来正常但指向错误种子的下载链接。

- **`/reseed/sites/recommend` 被当成「已绑定站点」用**：该接口返回的是 **IYUU 合作站点**，
  用途是用合作站账号激活 Token（`/reseed/users/bind`），既不是「全部可辅种站点」，也不代表
  用户在 IYUU 上绑定了什么。原实现把它解析成 `IyuuSite`，得到一个字段几乎全空的列表（该响应
  没有 `base_url` / `download_page`），并据此判断 Token 是否有效——导致未激活的 Token 也会
  报「连接成功」，随后每次真实辅种都失败。现在两者是独立的类型（`IyuuSite` / `PartnerSite`）
  与独立的接口，混淆会变成编译错误。
- **Token 未激活被误报为 Token 无效**：IYUU 对未激活的 Token 返回 `403 用户未绑定推荐站点`。
  新增 `iyuu_inactive` 错误类型（HTTP 403），前端据此提示「去绑定合作站点」而不是「检查 Token」。
  2xx-with-error-code 与非 2xx 两条路径现在共用同一个分类函数，不会再出现只修好一边的情况。

- **`sid_sha1` 过期后不会重汇报重试**：IYUU 用 `code=0` + `data: null` + `msg="站点哈希值已过期"`
  这种形状上报过期，而 `unwrap_envelope` 在 `data` 为 null 时直接返回「成功响应但 data 为空」，
  `msg` 被丢弃，`SidSha1Expired` 永远匹配不上——「重新汇报后重试一次」这条路径在真实环境里走不到。
  现在 null `data` 会先过统一的错误分类，只有真正无法归类时才退化为空响应错误。

- **每次辅种都会把自己这条任务记录标成失败**：`execute_discovery` 先 `create_run` 写入本次记录，
  随后才调用 `reconcile_orphan_runs`，而后者把所有 `status='running'` 的记录改成 `failed`——
  包括刚刚创建的这一次。现在清理动作在创建本次记录之前完成。

- **辅种失败在界面上不可见**：获取种子 / 添加种子失败只写 `tracing`，且这两类失败不产生
  attempt 记录，于是运行日志里只剩「新增 0」，与「其它站点根本没有这个资源」完全无法区分。
  现在两者都会进入运行日志，按原因去重并封顶 10 条，避免系统性故障刷屏。

### 新增

- **合作站点与 Token 激活设置**：设置页新增「合作站点与 Token 激活」卡片，可拉取合作站点列表、
  填写站点用户数字 ID 与 passkey 完成绑定。passkey 只在本机做 sha1 后上行（IYUU 只存摘要，
  与官方 PHP 客户端 `sha1($passkey)` 一致）。绑定后会校验激活状态，并在目录为空时顺带拉取一次。
- **站点目录一次性保留**：`/reseed/sites/index` 拉取后写入 SQLite `iyuu_sites` 表，页面与每次辅种
  都从库里读；安装后首次运行（或首次保存 Token / 首次绑定合作站点）自动拉一次，之后只有用户点
  「刷新」才会重新拉取（消耗 1 次额度）。此前目录只存在内存里且带 24h TTL，进程重启就失效，
  前端每打开一次「站点凭据」页都会真实消耗一次额度。
- `POST /api/sites/refresh`、`GET /api/partners`、`POST /api/partners/bind`、
  `GET /api/partners/activated`；`GET /api/sites` 改为读库并返回 `initialised` / `synced_at`。
  移除 `GET /api/sites/bound`（其语义建立在上述误解上）。

### 变更

- `IyuuClient::sites()` 不再接受 `force` 参数、也不再自带 TTL 缓存：缓存职责上移到数据库，
  「读目录」和「花一次额度拉目录」成为两件明确不同的事。
- 辅种启动前会检查目录是否已初始化，为空时直接给出「去刷新站点目录」的提示，而不是带着空模板
  逐个站点下载失败。已配置但不在目录中的站点会按 sid 记录警告，便于区分「目录过期」和「站点真没资源」。
- 所有一次性客户端（刷新 / 测试 / 绑定 / 激活检查）发出的请求现在都会写回额度台账，
  此前这些请求花掉后不被记录，会让下一次辅种按实际不存在的剩余额度排预算。

### 测试

- 新增 10 个针对辅种响应解析的测试：`torrent_id` / `sid` 的数字字符串与整数值浮点、
  非整数与非法字符串被拒绝（而不是静默变成 0）、坏行只丢自己不影响同批其他 hash、
  坏行消息带原始数据、警告通道有界且只被消费一次；其中一个走完整 `reseed_query`
  用真实 HTTP 响应复现本次报错的原始形态。
- 新增 5 个基于真实本地 HTTP 服务的线上形制测试：`reportExisting` 的 JSON body 与 `token` 头、
  `index/index` 的「签名即所发字节」（排序 / 小写 / 紧凑 JSON）与表单字段、`sites/index` 的
  文档示例解析、`403 用户未绑定推荐站点` 的分类、`users/bind` 的表单字段与 sha1 passkey。

- **新增离线 e2e / BDD 套件（`tests/e2e`）**：在本机同时模拟 IYUU API、qBittorrent WebAPI v2
  与 PT 站点，用真实的 `iyuu-reseed` 二进制跑完整流程——真实的 HTTP 客户端、真实的 bencode
  哈希、真实的签名计算与状态机，只有对端字节是伪造的。37 个场景覆盖目录只拉一次、辅种发现与
  校验状态机、IYUU 响应容错（数字字符串 id / 坏行隔离 / `sid_sha1` 过期重试 / 限流 / 未激活 /
  模板失效退回详情页抓取）与设置凭据接口的密钥只写不回显。
  全程只访问 `127.0.0.1`，**不会**触碰真实下载器、真实 IYUU 账号或真实 tracker，
  因此可以在一台正在做种的机器上安全运行。执行：`tests/e2e/run.sh`（需要 `uv`，仓库不引入 Python 依赖）。

## [0.2.3] - 2026-11-13

### 新增

- **qBittorrent API Key 认证**：支持 qB ≥ 5.2（WebAPI ≥ 2.14.1）的
  `Authorization: Bearer` 方式。无状态认证，不调 `auth/login`、不依赖 `SID` cookie，
  因此 WebUI 重启不会再让定时任务因会话过期而失败。设置页可切换认证方式，密钥只写不回显。
  密钥被轮换或版本过低时给出明确提示，而不是沿用「重新登录再试」的无效建议。

## [0.2.1] - 2026-11-13

首个对外发布版本：把「发现 / 校验两阶段」的辅种流程、站点凭据管理与额度保护做成一个
可自托管的单容器服务。

### 新增

- **发现 / 校验两阶段流程**：扫描下载器 → 过滤 → 查 IYUU → 添加，与独立的校验巡检循环解耦；
  IYUU 不可用或未排定时任务时，已在途的校验仍然继续推进。
- **11 态辅种状态机**：从 `pending` 到 `seeding / verify_failed / timed_out`，不把「已添加」
  当成「已通过校验」。
- **四层限流与额度台账**：会员类型预算 + 持久化额度台账 + 固定间隔节流 + 熔断；额度不足时
  截断本轮并留待下次，而不是中途报错。新增额度查询接口与会员类型选择。
- **多站点下载机制**：NexusPHP 模板、详情页解析、M-Team API Key、Monika RSS，按站点特征选路。
- **站点凭据独立页面**：只显示已添加的站点，字段按站点实际需要给；支持导入 IYUU AutoSeed
  的 `cn_sites_*.json` 备份，导入前先预览会导入谁 / 跳过谁 / 为什么跳过。
- **cron 定时辅种**：5 字段表达式，按容器本地时区触发，改配置无需重启。
- **Vue 3 控制台**：TypeScript + Tailwind v4 + shadcn-vue，深色模式，含概览、辅种记录、
  站点凭据、下载器种子浏览、设置五个页面。
- **qBittorrent 与 Transmission** 两种下载器适配，均含连接测试。

### 安全

- 密钥只写不回显，IYUU API 主机名不进浏览器。
- `compose.yml` 默认只发布到 `127.0.0.1`、根文件系统只读、`no-new-privileges`、日志轮转与资源上限。

### 工程

- 质量门禁 `make check`：`cargo fmt --check` + `clippy --all-targets --all-features -D warnings`
  （pedantic 全开）+ `cargo test`。
- GitHub Actions 跑 Rust 门禁、测试、前端类型检查与构建，并发布多架构（amd64 / arm64）镜像到
  `ghcr.io/hexgu/iyuu-rust`。
- 新增 `dev.sh` 交互式本地开发控制台，统一后端 / 前端启动与端口覆盖。
- 构建走官方源，不再在仓库内预设镜像站或代理。

[0.2.1]: https://github.com/hexgu/iyuu-rust/releases/tag/v0.2.1
