<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { RouterLink } from 'vue-router'
import {
  AlertTriangle,
  ArrowRight,
  CheckCircle2,
  CircleSlash,
  Crown,
  Download,
  Loader2,
  Play,
  Square,
  Timer,
  XCircle,
} from 'lucide-vue-next'
import {
  api,
  ApiError,
  type Attempt,
  type DownloaderSummary,
  type QuotaResponse,
  type RunProgress,
  type StatusResponse,
} from '@/lib/api'
import { cn, formatBytes, formatDateTime, shortHash, timeAgo } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Alert, AlertDescription } from '@/components/ui/alert'
import { Skeleton } from '@/components/ui/skeleton'
import StateBadge from '@/components/layout/StateBadge.vue'

const status = ref<StatusResponse | null>(null)
const progress = ref<RunProgress | null>(null)
const summary = ref<DownloaderSummary | null>(null)
const recent = ref<Attempt[]>([])
const counts = ref<Record<string, number>>({})
const quota = ref<QuotaResponse | null>(null)
const loading = ref(true)
const startError = ref<string | null>(null)
const startErrorIsQuota = ref(false)
const starting = ref(false)
const cancelling = ref(false)
let timer: ReturnType<typeof setInterval> | undefined

async function loadAll() {
  // Fetch in parallel: these are independent and the page is useless until all
  // of them land.
  const [s, p, sum, att, st, q] = await Promise.allSettled([
    api.status(),
    api.progress(),
    api.downloaderSummary(),
    api.attempts({ limit: 8 }),
    api.attemptStats(),
    api.quota(),
  ])
  if (s.status === 'fulfilled') status.value = s.value
  if (p.status === 'fulfilled') progress.value = p.value
  if (sum.status === 'fulfilled') summary.value = sum.value
  if (att.status === 'fulfilled') recent.value = att.value.items
  if (st.status === 'fulfilled') counts.value = st.value.counts
  if (q.status === 'fulfilled') quota.value = q.value
  loading.value = false
}

async function startRun() {
  starting.value = true
  startError.value = null
  startErrorIsQuota.value = false
  try {
    await api.startRun()
    // The run starts asynchronously; give it a beat, then refresh so the UI
    // immediately shows "running" rather than waiting for the next poll.
    await new Promise((r) => setTimeout(r, 400))
    await loadAll()
  } catch (e) {
    startError.value = e instanceof ApiError ? e.message : String(e)
    // A quota rejection is not a bug and needs different words: it means "wait or
    // switch servers", and the banner links to exactly that.
    startErrorIsQuota.value = e instanceof ApiError && e.kind === 'rate_limited'
    loadAll()
  } finally {
    starting.value = false
  }
}

/**
 * Quota banner state.
 *
 * Only the states that should change what the user does produce a banner: an
 * active cooldown, an exhausted day, or a nearly-spent day. Everything else stays
 * silent — a permanent yellow bar would just be ignored.
 */
const quotaBanner = computed(() => {
  const q = quota.value
  if (!q) return null
  const blockedUntil = q.blocked_until ? new Date(q.blocked_until) : null
  if (blockedUntil && blockedUntil.getTime() > Date.now()) {
    return {
      tone: 'error' as const,
      title: 'IYUU 接口已限流',
      body: `为了不把临时限流变成封禁，已停止请求，${formatDateTime(q.blocked_until!)} 后自动恢复。`,
      showUpgrade: true,
    }
  }
  if (q.remaining_requests <= 0) {
    return {
      tone: 'error' as const,
      title: '今日请求额度已用尽',
      body: q.reset_at
        ? `今日已用 ${q.used_requests}/${q.daily_requests} 次（${q.tier_label}）。${formatDateTime(q.reset_at)} 后重置。`
        : `今日已用 ${q.used_requests}/${q.daily_requests} 次（${q.tier_label}）。`,
      showUpgrade: true,
    }
  }
  if (q.percent_used >= 90) {
    return {
      tone: 'warn' as const,
      title: '今日额度即将用尽',
      body: `已用 ${q.used_requests}/${q.daily_requests} 次（${q.tier_label}），剩余额度可能不足以覆盖全部种子，本次运行会自动截断。`,
      showUpgrade: true,
    }
  }
  return null
})

const isVip = computed(() => quota.value?.tier === 'vip')

/**
 * The account has a token but never answered the tier question.
 *
 * Everything downstream (server choice, budget, level of throttling) depends on
 * that answer, so this outranks every other banner: without it the numbers
 * displayed are guesses.
 */
const tierUnchosen = computed(() => !!status.value?.configured && !status.value.iyuu_tier_chosen)

/** The upgrade prompt only makes sense for a free account that is out of room. */
const upgradeApplies = computed(() => quotaBanner.value?.showUpgrade && !isVip.value)

/** IYUU 会员中心。Not our product — every place that links here says so. */
const MEMBER_URL = 'https://iyuu.cn/app/user/member'

/**
 * Whether starting a run is pointless right now.
 *
 * Disabling the button is better than letting the click fail: the run would abort
 * on its first request anyway, and an enabled button implies "this will work".
 * The server enforces this too — this is only the visible half.
 */
const blockedByQuota = computed(() => {
  const q = quota.value
  if (!q) return false
  if (q.blocked_until && new Date(q.blocked_until).getTime() > Date.now()) return true
  return q.remaining_requests <= 0
})

async function cancelRun() {
  cancelling.value = true
  try {
    await api.cancelRun()
    await loadAll()
  } catch (e) {
    startError.value = e instanceof ApiError ? e.message : String(e)
  } finally {
    cancelling.value = false
  }
}

onMounted(() => {
  loadAll()
  // 2s while a run is in flight so the log feels live; the handler below decides
  // whether to keep that cadence. 5s otherwise.
  timer = setInterval(loadAll, 3000)
})
onUnmounted(() => clearInterval(timer))

const running = computed(() => progress.value?.running ?? status.value?.running ?? false)

/**
 * Which stats to show.
 *
 * `progress` only exists in the server's memory, so after a restart it is empty
 * while `last_run` still holds the persisted result of the last completed run.
 * Preferring progress unconditionally made a finished run look like it had
 * scanned nothing. So: use progress only when it belongs to a run that is
 * actually running, or when it carries data.
 */
const stats = computed(() => {
  const p = progress.value
  if (p && (p.running || p.stats.scanned > 0 || p.stats.added > 0)) {
    return p.stats
  }
  return status.value?.last_run?.stats ?? p?.stats ?? null
})

/**
 * The headline numbers. Ordered by what an operator actually needs to know:
 * did anything break, and is the ratio-protecting work landing.
 */
const headline = computed(() => {
  const s = stats.value
  return [
    {
      key: 'seeding',
      label: '辅种中',
      value: status.value?.verified_seeding ?? counts.value.seeding ?? 0,
      icon: CheckCircle2,
      tone: 'text-status-success',
      hint: '校验通过、数据完整、正在做种的辅种数量',
      to: '/seeds',
    },
    {
      key: 'pending',
      label: '等待校验',
      value: status.value?.pending_verification ?? 0,
      icon: Timer,
      tone: 'text-status-warn',
      hint: '已添加到下载器、尚未得出校验结果的辅种',
      to: '/seeds?filter=verifying',
    },
    {
      key: 'failed',
      label: '校验不通过',
      value: (counts.value.verify_failed ?? 0) + (counts.value.missing_files ?? 0),
      icon: XCircle,
      tone: 'text-status-error',
      hint: '校验完成但数据不完整 —— 通常是保存路径与原种不一致',
      to: '/seeds?filter=has_failure',
    },
    {
      key: 'added',
      label: '本次新增',
      value: s?.added ?? 0,
      icon: Download,
      tone: 'text-status-active',
      hint: '本次任务成功交给下载器的种子数（不等于成功）',
      to: '/seeds',
    },
  ]
})

const stateRows = computed(() => {
  const order = [
    'seeding',
    'verifying',
    'added',
    'verify_failed',
    'missing_files',
    'fetch_failed',
    'add_failed',
    'timed_out',
    'skipped',
    'pending',
  ] as const
  const total = Object.values(counts.value).reduce((a, b) => a + b, 0) || 1
  return order
    .filter((k) => (counts.value[k] ?? 0) > 0)
    .map((k) => ({
      state: k,
      count: counts.value[k] ?? 0,
      pct: ((counts.value[k] ?? 0) / total) * 100,
    }))
})

const activityText = computed(() => {
  const p = progress.value
  if (!p) return ''
  const map: Record<string, string> = {
    idle: '空闲',
    starting: '正在启动…',
    scanning: '正在扫描下载器…',
    processing: '正在查询并添加辅种…',
    done: '已完成',
    failed: '失败',
    cancelled: '已取消',
  }
  return map[p.phase] ?? p.phase
})
</script>

<template>
  <div class="space-y-5">
    <!-- Run control -->
    <Card>
      <CardContent class="flex flex-col gap-4 pt-0 sm:flex-row sm:items-center">
        <div class="min-w-0 flex-1">
          <div class="flex items-center gap-2">
            <span
              :class="cn(
                'size-2 shrink-0 rounded-full',
                running ? 'bg-status-active animate-pulse' : 'bg-muted-foreground/40',
              )"
            />
            <h1 class="truncate text-base font-semibold">
              {{ running ? activityText : '自动辅种' }}
            </h1>
          </div>
          <p class="text-muted-foreground mt-1 text-xs">
            <template v-if="running">
              开始于 {{ timeAgo(progress?.started_at) }} ·
              已扫描 {{ stats?.scanned ?? 0 }} · 新增 {{ stats?.added ?? 0 }}
            </template>
            <template v-else-if="status?.last_run">
              上次任务 {{ timeAgo(status.last_run.started_at) }} · {{ status.last_run.status }}
              <span v-if="status.next_run_at">
                · 下次 {{ formatDateTime(status.next_run_at) }}
              </span>
            </template>
            <template v-else-if="status?.next_run_at">
              尚未运行过 · 下次 {{ formatDateTime(status.next_run_at) }}
            </template>
            <template v-else>尚未运行过</template>
          </p>
        </div>

        <div class="flex shrink-0 gap-2">
          <Button v-if="running" variant="outline" :disabled="cancelling" @click="cancelRun">
            <Loader2 v-if="cancelling" class="animate-spin" />
            <Square v-else />
            取消
          </Button>
          <Button
            v-else
            :disabled="starting || !status?.configured || blockedByQuota || tierUnchosen"
            :title="
              tierUnchosen
                ? '请先在设置里选择 IYUU 会员类型'
                : blockedByQuota
                  ? '今日额度已用尽或正在限流冷却，请等额度重置'
                  : ''
            "
            @click="startRun"
          >
            <Loader2 v-if="starting" class="animate-spin" />
            <Play v-else />
            开始辅种
          </Button>
        </div>
      </CardContent>
    </Card>

    <Alert v-if="startError" variant="destructive">
      <AlertTriangle />
      <div>
        <div class="font-medium">
          {{ startErrorIsQuota ? 'IYUU 今日额度不足，任务未启动' : '任务未能启动' }}
        </div>
        <AlertDescription class="text-destructive/90">{{ startError }}</AlertDescription>
        <div v-if="startErrorIsQuota" class="mt-1.5 flex flex-wrap items-center gap-2 text-xs">
          <RouterLink to="/settings" class="underline underline-offset-2">检查会员类型</RouterLink>
          <span class="opacity-60">·</span>
          <a
            :href="MEMBER_URL"
            target="_blank"
            rel="noreferrer"
            class="underline underline-offset-2"
          >了解 IYUU SVIP</a>
        </div>
      </div>
    </Alert>

    <!-- Unanswered tier outranks every other banner: server and budget both
         depend on it, so the numbers below would be guesses. -->
    <Alert v-if="tierUnchosen" variant="warning">
      <AlertTriangle />
      <div>
        <div class="font-medium">还没选择 IYUU 会员类型</div>
        <AlertDescription>
          普通用户每天 500 次查询，SVIP 2500 次。这个信息无法从 Token 判断，需要你确认。
          <RouterLink to="/settings" class="ml-0.5 underline underline-offset-2">
            去设置里选择
          </RouterLink>
        </AlertDescription>
      </div>
    </Alert>

    <!-- Quota / rate-limit banner: only for states that change what to do next -->
    <Alert
      v-else-if="quotaBanner"
      :variant="quotaBanner.tone === 'error' ? 'destructive' : 'warning'"
    >
      <AlertTriangle />
      <div class="min-w-0">
        <div class="font-medium">{{ quotaBanner.title }}</div>
        <AlertDescription
          :class="quotaBanner.tone === 'error' ? 'text-destructive/90' : ''"
        >
          {{ quotaBanner.body }}
        </AlertDescription>
        <div class="mt-1.5 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs">
          <RouterLink to="/settings" class="underline underline-offset-2">
            查看会员类型与额度设置
          </RouterLink>
          <template v-if="upgradeApplies">
            <span class="opacity-60">·</span>
            <a
              :href="MEMBER_URL"
              target="_blank"
              rel="noreferrer"
              class="inline-flex items-center gap-1 underline underline-offset-2"
            >
              <Crown class="size-3" />
              额度不够？IYUU SVIP 每天 2500 次
            </a>
            <span class="text-muted-foreground text-[10px] opacity-70">
              （第三方提示，非官方项目）
            </span>
          </template>
        </div>
      </div>
    </Alert>

    <!-- Headline stats -->
    <div class="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
      <RouterLink v-for="h in headline" :key="h.key" :to="h.to" class="group">
        <Card class="hover:border-foreground/20 h-full gap-2 py-4 transition-colors">
          <CardContent class="flex items-start justify-between gap-2">
            <div>
              <div class="text-muted-foreground text-xs">{{ h.label }}</div>
              <Skeleton v-if="loading" class="mt-2 h-7 w-14" />
              <div v-else class="tabular mt-1 text-2xl font-semibold">{{ h.value }}</div>
            </div>
            <component
              :is="h.icon"
              :class="cn('mt-0.5 size-4 opacity-70', h.tone)"
              :title="h.hint"
            />
          </CardContent>
        </Card>
      </RouterLink>
    </div>

    <div class="grid gap-5 lg:grid-cols-3">
      <!-- Live log -->
      <Card class="lg:col-span-2">
        <CardHeader class="flex-row items-center justify-between">
          <CardTitle class="text-sm">运行日志</CardTitle>
          <span v-if="progress?.run_id" class="text-muted-foreground tabular text-xs">
            #{{ progress.run_id.slice(0, 8) }}
          </span>
        </CardHeader>
        <CardContent>
          <div
            v-if="progress?.log?.length"
            class="scrollbar-slim bg-muted/30 max-h-72 overflow-y-auto rounded-md border p-3 font-mono text-[11px] leading-relaxed"
          >
            <div
              v-for="(line, i) in progress.log"
              :key="i"
              :class="cn(
                'whitespace-pre-wrap break-all',
                i === 0 ? 'text-foreground' : 'text-muted-foreground/80',
              )"
            >
              {{ line }}
            </div>
          </div>
          <div
            v-else
            class="text-muted-foreground flex h-32 flex-col items-center justify-center gap-1 rounded-md border border-dashed text-xs"
          >
            <span>暂无实时日志</span>
            <span class="text-[11px] opacity-70">
              日志只保存在本次进程内；重启后请查看下方「上次任务结果」与辅种记录
            </span>
          </div>
        </CardContent>
      </Card>

      <div class="space-y-5">
        <!-- IYUU quota: the constraint that decides how much of a run can happen -->
        <Card>
          <CardHeader class="flex-row items-center justify-between">
            <CardTitle class="text-sm">IYUU 额度</CardTitle>
            <Crown v-if="isVip" class="text-status-warn size-3.5" />
          </CardHeader>
          <CardContent class="space-y-2 text-xs">
            <!-- 未选择会员类型时，下面所有数字都是按猜测算的，先让人去选 -->
            <div v-if="tierUnchosen" class="space-y-2">
              <p class="text-muted-foreground leading-relaxed">
                还没有选择会员类型。普通用户每天 500 次、SVIP 2500 次，
                在选定之前这里的额度数字没有意义。
              </p>
              <RouterLink
                to="/settings"
                class="text-foreground flex items-center gap-1 text-[11px] underline underline-offset-2"
              >
                去设置里选择 <ArrowRight class="size-3" />
              </RouterLink>
            </div>
            <template v-else-if="quota">
              <div class="flex justify-between">
                <span class="text-muted-foreground">会员类型</span>
                <span class="font-medium">{{ quota.tier_label }}</span>
              </div>
              <div class="flex justify-between">
                <span class="text-muted-foreground">今日请求</span>
                <span class="tabular font-medium">
                  {{ quota.used_requests }} / {{ quota.daily_requests }}
                </span>
              </div>
              <div class="bg-muted h-1 overflow-hidden rounded-full">
                <div
                  :class="cn(
                    'h-full rounded-full',
                    quota.remaining_requests <= 0 || quota.blocked_until
                      ? 'bg-status-error'
                      : quota.percent_used >= 90
                        ? 'bg-status-warn'
                        : 'bg-status-success',
                  )"
                  :style="{ width: `${Math.max(quota.percent_used, 1)}%` }"
                />
              </div>
              <div class="flex justify-between">
                <span class="text-muted-foreground">剩余</span>
                <span class="tabular font-medium">{{ quota.remaining_requests }} 次</span>
              </div>
              <div class="flex justify-between">
                <span class="text-muted-foreground">种子覆盖</span>
                <span class="tabular font-medium">
                  {{ quota.used_hashes.toLocaleString() }} / {{ quota.daily_hashes.toLocaleString() }}
                </span>
              </div>
              <div v-if="quota.reset_at" class="flex justify-between border-t pt-2">
                <span class="text-muted-foreground">额度重置</span>
                <span class="tabular">{{ formatDateTime(quota.reset_at) }}</span>
              </div>
              <RouterLink
                to="/settings"
                class="text-muted-foreground hover:text-foreground flex items-center gap-1 pt-1 text-[11px]"
              >
                会员类型与额度 <ArrowRight class="size-3" />
              </RouterLink>
            </template>
            <div v-else-if="loading" class="space-y-2">
              <Skeleton v-for="i in 4" :key="i" class="h-4 w-full" />
            </div>
            <p v-else class="text-muted-foreground py-2 text-center">
              还没有请求记录
            </p>
          </CardContent>
        </Card>

        <!-- Downloader summary -->
        <Card>
          <CardHeader>
            <CardTitle class="text-sm">下载器</CardTitle>
          </CardHeader>
          <CardContent class="space-y-2 text-xs">
            <template v-if="summary">
              <div class="flex justify-between">
                <span class="text-muted-foreground">种子总数</span>
                <span class="tabular font-medium">{{ summary.total }}</span>
              </div>
              <div class="flex justify-between">
                <span class="text-muted-foreground">已完成</span>
                <span class="tabular font-medium text-status-success">{{ summary.complete }}</span>
              </div>
              <div class="flex justify-between">
                <span class="text-muted-foreground">校验中</span>
                <span class="tabular font-medium text-status-active">{{ summary.checking }}</span>
              </div>
              <div class="flex justify-between">
                <span class="text-muted-foreground">异常</span>
                <span
                  :class="cn('tabular font-medium', summary.errors > 0 ? 'text-status-error' : '')"
                >
                  {{ summary.errors }}
                </span>
              </div>
              <div class="flex justify-between border-t pt-2">
                <span class="text-muted-foreground">总大小</span>
                <span class="tabular font-medium">{{ formatBytes(summary.total_size) }}</span>
              </div>
              <div class="flex justify-between">
                <span class="text-muted-foreground">总上传</span>
                <span class="tabular font-medium">{{ formatBytes(summary.uploaded) }}</span>
              </div>
            </template>
            <div v-else-if="loading" class="space-y-2">
              <Skeleton v-for="i in 4" :key="i" class="h-4 w-full" />
            </div>
            <Alert v-else variant="warning">
              <CircleSlash />
              <AlertDescription>无法读取下载器，请检查设置</AlertDescription>
            </Alert>
          </CardContent>
        </Card>

        <!-- State distribution -->
        <Card>
          <CardHeader>
            <CardTitle class="text-sm">辅种状态分布</CardTitle>
          </CardHeader>
          <CardContent>
            <div v-if="stateRows.length" class="space-y-2">
              <div v-for="row in stateRows" :key="row.state">
                <div class="mb-1 flex items-center justify-between gap-2">
                  <StateBadge :state="row.state" size="sm" />
                  <span class="tabular text-muted-foreground text-xs">{{ row.count }}</span>
                </div>
                <div class="bg-muted h-1 overflow-hidden rounded-full">
                  <div
                    :class="cn(
                      'h-full rounded-full',
                      row.state === 'seeding' && 'bg-status-success',
                      (row.state === 'added' || row.state === 'verifying') && 'bg-status-warn',
                      (row.state === 'verify_failed' || row.state === 'missing_files' || row.state === 'fetch_failed' || row.state === 'add_failed') && 'bg-status-error',
                      row.state === 'timed_out' && 'bg-status-warn',
                      (row.state === 'skipped' || row.state === 'pending') && 'bg-muted-foreground/40',
                    )"
                    :style="{ width: `${Math.max(row.pct, 2)}%` }"
                  />
                </div>
              </div>
            </div>
            <p v-else class="text-muted-foreground py-4 text-center text-xs">
              还没有辅种记录
            </p>
          </CardContent>
        </Card>
      </div>
    </div>

    <!-- Last run breakdown -->
    <Card v-if="stats">
      <CardHeader class="flex-row items-center justify-between">
        <CardTitle class="text-sm">{{ running ? '本次进度' : '上次任务结果' }}</CardTitle>
        <RouterLink
          to="/seeds"
          class="text-muted-foreground hover:text-foreground flex items-center gap-1 text-xs"
        >
          全部种子 <ArrowRight class="size-3" />
        </RouterLink>
      </CardHeader>
      <CardContent>
        <div class="grid grid-cols-2 gap-x-6 gap-y-3 text-xs sm:grid-cols-3 lg:grid-cols-6">
          <div>
            <div class="text-muted-foreground">扫描种子</div>
            <div class="tabular mt-0.5 text-base font-semibold">{{ stats.scanned }}</div>
          </div>
          <div>
            <div class="text-muted-foreground">参与查询</div>
            <div class="tabular mt-0.5 text-base font-semibold">{{ stats.queried }}</div>
          </div>
          <div>
            <div class="text-muted-foreground">IYUU 命中</div>
            <div class="tabular mt-0.5 text-base font-semibold">{{ stats.candidates }}</div>
          </div>
          <div>
            <div class="text-muted-foreground">已存在（跳过）</div>
            <div class="tabular mt-0.5 text-base font-semibold">{{ stats.existed }}</div>
          </div>
          <div>
            <div class="text-muted-foreground">获取/添加失败</div>
            <div
              :class="cn(
                'tabular mt-0.5 text-base font-semibold',
                stats.fetch_failed + stats.add_failed > 0 && 'text-status-error',
              )"
            >
              {{ stats.fetch_failed + stats.add_failed }}
            </div>
          </div>
          <div>
            <div class="text-muted-foreground">校验不通过</div>
            <div
              :class="cn(
                'tabular mt-0.5 text-base font-semibold',
                stats.verify_failed > 0 && 'text-status-error',
              )"
            >
              {{ stats.verify_failed }}
            </div>
          </div>
        </div>
        <p
          v-if="status?.last_run?.message"
          class="text-muted-foreground mt-3 border-t pt-3 text-xs"
        >
          {{ status.last_run.message }}
        </p>
      </CardContent>
    </Card>

    <!-- Recent attempts -->
    <Card>
      <CardHeader class="flex-row items-center justify-between">
        <CardTitle class="text-sm">最近辅种</CardTitle>
        <RouterLink
          to="/seeds"
          class="text-muted-foreground hover:text-foreground flex items-center gap-1 text-xs"
        >
          查看全部 <ArrowRight class="size-3" />
        </RouterLink>
      </CardHeader>
      <CardContent class="p-0">
        <div v-if="recent.length" class="divide-y">
          <div
            v-for="a in recent"
            :key="a.id"
            class="flex items-center gap-3 px-5 py-2.5 text-xs"
          >
            <StateBadge :state="a.state" size="sm" />
            <span class="min-w-0 flex-1 truncate" :title="a.source_name">
              {{ a.source_name }}
            </span>
            <span class="text-muted-foreground hidden shrink-0 sm:inline">
              {{ a.target_site }}
            </span>
            <span class="text-muted-foreground tabular hidden shrink-0 md:inline">
              {{ shortHash(a.target_info_hash) }}
            </span>
            <span class="text-muted-foreground tabular w-16 shrink-0 text-right">
              {{ timeAgo(a.last_checked_at ?? a.added_at) }}
            </span>
          </div>
        </div>
        <p v-else class="text-muted-foreground px-5 py-8 text-center text-xs">
          还没有辅种记录
        </p>
      </CardContent>
    </Card>
  </div>
</template>
