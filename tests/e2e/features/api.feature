# 设置、凭据与运行记录的对外约定。密钥是只写的：可以设置，但从不出现在任何
# 响应里 —— 这样截图和浏览器都拿不到 Token / passkey。
Feature: 设置与凭据接口

  Scenario: 保存配置后能读回非敏感部分
    Given 一个已配置好的实例
    When 读取配置
    Then 配置里应显示 IYUU Token 已配置
    And 配置里应显示会员类型为 free
    And 下载器类型应为 qbittorrent

  Scenario: 接口从不回传任何密钥原文
    Given 一个已配置好的实例
    When 读取配置
    Then 响应正文里不应包含 IYUU Token 原文
    And 响应正文里不应包含下载器 API Key 原文
    And 响应正文里不应包含任何站点 passkey 原文

  Scenario: 未知的会员类型会被拒绝而不是静默降级
    Given 一个已配置好的实例
    When 把会员类型改成 vpi
    Then 接口应返回 400

  Scenario: 选择 API Key 认证却从没填过密钥时保存会被拒绝
    Given 一个用用户名密码认证的实例
    When 把下载器认证方式切到 api_key 而不提供密钥
    Then 接口应返回 400 并提示缺少密钥

  Scenario: 已经存过密钥时，空值表示保持原密钥而不是清空
    Given 一个已配置好的实例
    When 把下载器认证方式切到 api_key 并提交空白密钥
    Then 接口应返回 200
    And 配置里仍显示已有 API Key

  Scenario: 站点凭据可以按站点单独设置与删除
    Given 一个已配置好的实例
    When 为站点 alpha 设置 passkey
    Then 该站点应显示为已配置凭据
    When 删除该站点凭据
    Then 该站点应显示为未配置凭据

  Scenario: 删除一个没配置过的站点凭据返回 404
    Given 一个已配置好的实例
    When 删除一个从未配置过的站点凭据
    Then 接口应返回 404

  Scenario: 空凭据会被拒绝
    Given 一个已配置好的实例
    When 为站点提交一份全空的凭据
    Then 接口应返回 400

  Scenario: 下载器连通性测试会报告版本与种子数
    Given 一个已配置好的实例
    When 调用下载器连通性测试
    Then 应报告版本号 v5.2.0
    And 应报告种子总数

  Scenario: IYUU 连通性测试在没有 Token 时给出明确提示
    Given 一个全新实例
    When 用空 Token 调用 IYUU 连通性测试
    Then 接口应返回 400 并提示尚未配置 Token

  Scenario: 状态接口如实反映就绪情况
    Given 一个已配置好的实例
    When 读取状态
    Then 状态应报告已配置
    And 状态应报告 IYUU 与下载器都可用

  Scenario: 运行记录会保留下来并可查询
    Given 一个已配置好的实例
    When 触发一次辅种任务并等待完成
    Then 运行记录里应有这次任务
    And 该记录的触发方式应为 manual
