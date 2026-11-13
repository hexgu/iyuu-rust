# 核心辅种流程：扫描已完成的种子 → 用 IYUU 找其它站点的同名资源 → 从那些站点
# 下载种子 → 加到下载器 → 等校验通过才算成功。
#
# 「成功」只有一个定义：下载器报告 progress == 1.0 且不在校验中。仅仅添加成功
# 不算，那正是本工具要避免的「加了但永远没法做种」。
Feature: 辅种发现与校验

  Scenario: 在两个目标站点上都辅种成功
    Given 下载器里有一个已完成做种的种子
    And IYUU 在 2 个站点上都能找到该资源
    And 已为这两个站点配置了 passkey
    When 触发一次辅种任务
    Then 任务应正常完成
    And 任务统计应为新增 2、失败 0
    And 两条辅种记录最终都应进入 seeding 状态
    And 下载器里应确实多了这两个目标种子

  Scenario: 只从匹配的站点下载，且种子 hash 必须与 IYUU 声明的一致
    Given 下载器里有一个已完成做种的种子
    And IYUU 在 2 个站点上都能找到该资源
    And 已为这两个站点配置了 passkey
    When 触发一次辅种任务
    Then 每个目标站点都应收到 1 次下载请求
    And 下载请求应带上该站点的 passkey
    And 新增的种子 hash 应与 IYUU 声明的 info_hash 完全一致

  Scenario: 已经存在的目标种子不会重复添加
    Given 下载器里有一个已完成做种的种子
    And IYUU 在 2 个站点上都能找到该资源
    And 已为这两个站点配置了 passkey
    And 其中一个目标种子已经在下载器里
    When 触发一次辅种任务
    Then 任务统计应为新增 1、已存在 1
    And 下载器收到的添加请求总数应为 1

  Scenario: 校验发现数据不完整时判为失败，而不是当作成功
    Given 下载器里有一个已完成做种的种子
    And IYUU 在 1 个站点上都能找到该资源
    And 已为该站点配置了 passkey
    And 目标种子加入后校验结果是数据不完整
    When 触发一次辅种任务
    Then 该辅种记录应进入 verify_failed 状态
    And 记录里应说明是保存路径或资源不匹配

  Scenario: 下载器报告文件缺失时判为失败
    Given 下载器里有一个已完成做种的种子
    And IYUU 在 1 个站点上都能找到该资源
    And 已为该站点配置了 passkey
    And 目标种子加入后下载器报告文件缺失
    When 触发一次辅种任务
    Then 该辅种记录应进入 missing_files 状态

  Scenario: 站点返回 HTML 登录页时不会把网页当成种子保存
    Given 下载器里有一个已完成做种的种子
    And IYUU 在 1 个站点上都能找到该资源
    And 该站点的 passkey 是错的
    When 触发一次辅种任务
    Then 任务统计里的获取失败数应为 1
    And 下载器不应收到任何添加请求
    And 任务日志里应说明该站点的种子获取失败

  Scenario: 未完成的种子不会被拿来当辅种源
    Given 下载器里有一个尚未下载完成的种子
    And 已为全部三个站点配置了 passkey
    When 触发一次辅种任务
    Then 任务应跳过它而不是去查询 IYUU

  Scenario: 本身就是辅种的种子不会被再次辅种
    Given 下载器里有一个已完成做种的种子
    And 该种子已带有本工具的辅种标记
    And 已为全部三个站点配置了 passkey
    When 触发一次辅种任务
    Then 任务应跳过它而不是去查询 IYUU
