# 接口层的硬化：IYUU 的响应形状并不稳定，而一个批次最多 200 个 hash ——
# 一行坏数据不该让整批作废，更不该让任务整体失败。
Feature: IYUU 接口的容错

  Scenario: 站点 ID 与种子 ID 以数字字符串下发时仍能解析
    Given 下载器里有一个已完成做种的种子
    And IYUU 在该资源上返回的 sid 与 torrent_id 是数字字符串
    And 已为该站点配置了 passkey
    When 触发一次辅种任务
    Then 任务应正常完成
    And 应成功新增 1 个辅种

  Scenario: 单行无法解析的候选只跳过该行，不影响同批次其它结果
    Given 下载器里有一个已完成做种的种子
    And IYUU 在同一批次里混入一行无法解析的候选
    And 已为该站点配置了 passkey
    When 触发一次辅种任务
    Then 任务应正常完成
    And 应成功新增 1 个辅种
    And 任务消息里应提示有候选行被跳过

  Scenario: sid_sha1 过期时自动重新汇报并重试一次
    Given 下载器里有一个已完成做种的种子
    And 已为该站点配置了 passkey
    And 下一次辅种查询会因 sid_sha1 过期被拒绝
    When 触发一次辅种任务
    Then 任务应正常完成
    And 应成功新增 1 个辅种
    And 仿真 IYUU 应收到至少 2 次汇报站点请求

  Scenario: 额度耗尽时保留已获得的候选并明确告知
    Given 下载器里有一个已完成做种的种子
    And 已为该站点配置了 passkey
    And IYUU 会在查询时返回限流
    When 触发一次辅种任务
    Then 任务不应崩溃
    And 任务消息里应说明限流

  Scenario: 未激活的 Token 会被明确指出来，而不是报成网络错误
    Given 一个已配置好的实例
    And IYUU 认为该 Token 尚未绑定合作站点
    When 调用检查激活接口
    Then 接口应报告未激活
    And 提示应引导用户去绑定合作站点

  Scenario: 绑定合作站点后激活状态变为已完成
    Given 一个已配置好的实例
    And IYUU 认为该 Token 尚未绑定合作站点
    When 用正确的 passkey 绑定合作站点
    Then 绑定应成功
    And 再次检查激活状态应为已激活

  Scenario: 绑定合作站点时 passkey 错误会被拒绝
    Given 一个已配置好的实例
    And IYUU 认为该 Token 尚未绑定合作站点
    When 用错误的 passkey 绑定合作站点
    Then 绑定接口应返回错误

  Scenario: 站点的下载模板无法解析时退回详情页抓取
    Given 下载器里有一个已完成做种的种子
    And 目标站点的下载模板含无法替换的占位符
    And 该站点的详情页里含有下载链接
    And 已为该站点配置了 passkey
    When 触发一次辅种任务
    Then 应成功新增 1 个辅种

  Scenario: 站点目录为空时任务是配置错误而不是「没有可辅种资源」
    Given 一个没有任何站点目录的实例
    When 直接请求启动一次辅种任务
    Then 接口应拒绝启动并提示先刷新站点目录

  Scenario: 没有配置任何站点凭据时也是配置错误
    Given 一个站点目录已初始化但没有配置任何站点凭据的实例
    When 触发一次辅种任务
    Then 任务应失败并提示先添加站点凭据
