# 更新日志

本项目的版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)，变更分类参考
[Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

## [0.4.1] - 2026-11-14

### 新增

- **主种子识别增加「下载流量」证据**：主种子是用户真的拉过数据的那一份，辅种复用已有
  文件、`downloaded` 为 0。`downloaded > 0` 现在排在辅种记录之后、标签之前；识别依据
  会显示 `traffic`。qBittorrent 读 `downloaded`，Transmission 读 `downloadedEver`。

- **自动清理无效辅种**：主种子从下载器里消失后，把它铺在别的站点的副本一起删掉
  （`cleanup.delete_files` 默认连文件）。只处理有证据的组（有辅种记录，或组内全部
  带辅种标签），默认 24 小时宽限期，每次动作写 `cleanup_log` 审计；默认关闭，页面
  可先预览再执行。设 `cleanup.grace_hours` 为 0 则下一次任务就清。

- **IYUU 查询只问主种子**：查询前按内容路径 / 大小 + 名称把种子聚成资源，每个资源
  只留一个代表。同一份数据在多个站点的副本不再各查一次，省额度、也避免用副本的
  hash 生成重复记录。

### 变更

- **「种子管理」的主语改成下载器**。页面不再只列参与过辅种的记录，而是列出下载器里的
  每一个种子组：主种子一行，跨站副本折叠在下面，从未辅种的种子也在（`no_aux`）。
  删掉一条辅种记录不会再让种子从页面消失——记录只是元数据。

- **识别不再依赖标签**。新增位置身份 `content_path`（缺失时 `save_path + name`）+ 大小
  一致的硬证据，Hermes / MoviePilot / 手工加的、无记录无标签的副本也能自动归组；标签
  降级为「角色提示」，只用来判断哪个是副本。组的 `main_source` 会说明主种子是靠记录、
  标签、唯一候选还是推断认出来的，推断出来的组在页面上标「推断」并可筛选。

- **辅种标签统一为 `iyuu-rust`**，不再带站点后缀；旧 `iyuu-reseed` / `iyuu-reseed:<site>`
  / `iyuu-reseed:verified` 保留识别，并提供 `POST /api/seeds/tags/rewrite`（页面「重写旧
  标签」按钮）做迁移。校验通过后不再额外写 `:verified` 标签。

- **兼容其它辅种工具**：`reseed.aux_tags`（默认 `["辅种"]`）用于识别别家留下的标记；
  同时删除了「标签中加入站点名」选项。站点信息改从辅种记录与 tracker 读取
  （qBittorrent 的 tracker 从 `magnet_uri` 解析，不额外发请求）。

- 记录说成功但下载器里已经没有的辅种，标为「已丢失」并计入失败；同数据的额外副本在
  辅种行里标「重复」。

## [0.3.3] - 2026-11-14

### 安全

- **应用进程不再以 root 运行**。镜像内新增 uid/gid 10001 的应用用户，入口脚本先把
  `./data` 的属主交还给它、再用 `setpriv` 降权，应用进程始终以 10001 执行。
  没有直接写 `USER 10001` 是因为 `./data` 是宿主机 bind mount，它的属主由宿主机决定，
  镜像里的 USER 管不到挂载点——那样做会让升级后第一次启动直接打不开 SQLite。
  入口做了幂等判断：属主已经对了就不遍历目录。

- **compose 能力收敛**：`cap_drop: ALL`，只保留入口脚本降权与移交属主必需的
  `CHOWN` / `DAC_OVERRIDE` / `SETGID` / `SETUID`；应用进程跑起来时没有任何 capability。
  另加 `deploy.resources.limits.pids` 限制进程数。

### 工程

- **Dockerfile 静态检查进 CI**（hadolint + `.hadolint.yaml`，忽略项逐条带理由），
  同一作业里顺带用 `docker compose config` 校验 compose 文件语法——写错的 compose
  不该等到部署时才发现。

- **发布前扫镜像漏洞**（Trivy，HIGH/CRITICAL，忽略尚未修复的条目）。当前不阻断发布：
  基础镜像的 CVE 会随上游更新，卡死发布对使用者没有好处，先把结果摆在发布日志里。

- **基础镜像按用途收敛**：运行时只装 `ca-certificates` / `tzdata` / `util-linux`；
  构建上下文用 `.dockerignore` 收到只留源码与清单（文档、测试、compose、lpk 都不进 daemon）。

## [0.3.2] - 2026-11-14

### 工程

- **镜像不再用 QEMU 模拟 arm64**。原来多架构镜像在 amd64 runner 上模拟 arm64 跑
  `cargo build`，实测这一步 47 分钟（amd64 原生 6 分钟），整个镜像作业 50 分钟。现在两个
  架构各在原生 runner（`ubuntu-latest` / `ubuntu-24.04-arm`）上并行构建，再用
  `docker buildx imagetools` 合并成 manifest list。

- **Dockerfile 用 cargo-chef 拆出依赖层**。原来 `COPY crates/` 与 `cargo build` 在同一层，
  改一行源码就让 ~200 个 crate 全部重编译。现在依赖只在 `Cargo.toml` / `Cargo.lock` 变化时
  重建：本地实测冷构建依赖 652s、业务代码 13s；真实改动一次源码后的完整镜像重建 **14s**。

- **工作流按职责拆分**：`ci.yml`（PR / main：fmt + clippy、test、前端构建；PR 额外验证
  Dockerfile 能否构建）与 `release.yml`（tag：多架构镜像 + GitHub Release）。
  镜像只在发版时构建，日常 push 不再跑一次 50 分钟的多架构构建。

- **发布自动化**：推 `v*.*.*` tag 即产出 `0.3.2` / `0.3` / `latest` 三个标签的镜像，并从
  CHANGELOG 提取本版说明创建 GitHub Release；构建失败可用 `workflow_dispatch` 指定已有 tag
  重跑，不必重新打 tag。

- 缓存三处各自生效：cargo 缓存由 lint/test 共用 key；bun 缓存按 `bun.lock` 哈希；
  BuildKit 缓存按平台分 scope——两个架构的层不可能互相命中，混在一个 scope 里只会互相挤掉。

- 安全与稳定性：所有 action 固定到 commit SHA（配套 dependabot 每周跟进）、工作流默认
  `contents: read` 最小权限、每个 job 设 `timeout-minutes`、新增汇总门禁 `ci-ok`
  （分支保护只需勾这一个检查，以后加 job 不用改仓库设置）。

- **镜像标签策略变化**：不再发布 `main` / `sha-xxxxxxx` 标签，只保留 semver 系列
  （`0.3.2` / `0.3`）与 `latest`。

## [0.3.1] - 2026-11-14

### 新增

- **种子管理：页面的主语从「一次尝试」变成「一份数据」**。原来的「辅种记录」是一张流水账，
  一行 = 一次源种子 → 目标种子的尝试；同一份资源在几个站点上的辅种被打散在时间轴上，用户得
  自己在脑子里聚合。现在 `/seeds`（导航「种子管理」）按**主种子 + 辅种子**展示：主种子一行，
  它在各站点的辅种折叠在下面，组状态直接回答「这一组还有没有问题」。

- **跨站识别同一份数据**（`crates/core/src/identity.rs`）：info hash 只能识别同一个 .torrent，
  同一份数据在不同站点必然不同 hash。识别按「大小硬相等 + 名称归一化后相符（词元 Jaccard ≥ 85%）」
  判定，专门对付站点各自动标题的习惯（`[中文名].` 前缀、`WEB-DL` / `WEB.DL` 之分）。
  大小不同一律不合并；大小未知（0）不做名称推断——合并错了会引导用户删错种子。

- **`GET /api/seeds`**：主种子 + 辅种子分组视图，支持 `filter`（`has_failure` / `verifying` /
  `no_aux` / `main_missing`）、`q` 搜索与分页。分组在每次请求时从「下载器快照 + 辅种记录」
  推导，不落库：写一份组表就等于造一份会过期的副本。

- 组状态区分 `main_missing`（主种子已被删除，只剩辅种记录）与 `degraded`（主种子还在但下载器
  报文件缺失/错误，或某条辅种失败）。两者的处理动作不同，不能合并成一句「出问题了」。

- 旧地址 `/attempts?state=…` 保留为重定向，映射到新的组过滤条件，书签不会 404。

- 设计说明见 `docs/seeds-management.md`（识别规则、分组算法、API 契约、页面结构、明确不做的部分）。

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

- **设置页改为自动保存，去掉「保存」按钮**：字段改动停下 600ms 即写入，开关与下拉同样即改即存；
  右上角显示「保存中…／已保存／失败原因（可重试）」。请求按段（下载器 / 辅种 / 定时）比对后只发
  变化的那一段，避免改一个标签把整份配置重写。三个例外都有原因：密码与 API Key 在**失焦**时才提交
  （防抖会在打字停顿时写入半截密码，随后清空字段把没敲完的内容吞掉）；Token 仍走会员类型弹窗，
  因为二者必须一起提交；地址被清空、API Key 还没填、Cron 字段数不足这类「还没输完」的中间态先攒着
  不发，等服务端会接受时再写，不会闪一片红色报错。
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

[0.4.1]: https://github.com/hexgu/iyuu-rust/releases/tag/v0.4.1
[0.3.3]: https://github.com/hexgu/iyuu-rust/releases/tag/v0.3.3
[0.3.2]: https://github.com/hexgu/iyuu-rust/releases/tag/v0.3.2
[0.3.1]: https://github.com/hexgu/iyuu-rust/releases/tag/v0.3.1
[0.2.3]: https://github.com/hexgu/iyuu-rust/releases/tag/v0.2.3
[0.2.1]: https://github.com/hexgu/iyuu-rust/releases/tag/v0.2.1
