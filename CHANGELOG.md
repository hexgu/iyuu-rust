# 更新日志

本项目的版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)，变更分类参考
[Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

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
