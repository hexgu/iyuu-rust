<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { AlertCircle, ChevronRight, Info, RefreshCw, Trash2 } from 'lucide-vue-next'
import {
  api,
  ApiError,
  type ReseedState,
  type SeedAux,
  type SeedCounts,
  type SeedGroup,
  type SeedGroupState,
} from '@/lib/api'
import { cn, formatBytes, formatDateTime, shortHash, timeAgo } from '@/lib/utils'
import StateBadge from '@/components/layout/StateBadge.vue'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Alert, AlertDescription } from '@/components/ui/alert'
import { Skeleton } from '@/components/ui/skeleton'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'

const PAGE_SIZE = 50

const FILTERS: { value: string; label: string }[] = [
  { value: 'all', label: '全部' },
  { value: 'has_failure', label: '有失败' },
  { value: 'verifying', label: '校验中' },
  { value: 'no_aux', label: '无辅种' },
  { value: 'main_missing', label: '主种子缺失' },
]

/**
 * 组状态的说法。主种子缺失排在最前，因为它比「辅种失败」更严重：失败只是这条没成，
 * 缺失意味着本地这份数据可能已经不在了。
 */
const GROUP_META: Record<SeedGroupState, { label: string; cls: string; hint: string }> = {
  main_missing: {
    label: '主种子缺失',
    cls: 'text-status-error',
    hint: '下载器里已经找不到主种子，只剩辅种记录',
  },
  degraded: {
    label: '有失败',
    cls: 'text-status-error',
    hint: '至少一条辅种没有通过校验或添加失败',
  },
  verifying: { label: '校验中', cls: 'text-status-active', hint: '下载器正在校验，等结果' },
  no_aux: { label: '未辅种', cls: 'text-status-pending', hint: '还没有发现或添加任何辅种' },
  healthy: { label: '正常', cls: 'text-status-success', hint: '主种子在，辅种全部校验通过' },
}

const items = ref<SeedGroup[]>([])
const counts = ref<SeedCounts | null>(null)
const total = ref(0)
const offset = ref(0)
const filter = ref('all')
const search = ref('')
const loading = ref(true)
const error = ref<string | null>(null)
const busyId = ref<number | null>(null)
/** 展开的组。折叠状态是视图状态，不进 URL —— 刷新一次不该丢掉用户翻到哪儿。 */
const open = ref<Set<string>>(new Set())

let poll: ReturnType<typeof setInterval> | undefined
let debounce: ReturnType<typeof setTimeout> | undefined

async function load(quiet = false) {
  if (!quiet) loading.value = true
  try {
    const r = await api.seeds({
      filter: filter.value,
      q: search.value.trim() || undefined,
      limit: PAGE_SIZE,
      offset: offset.value,
    })
    items.value = r.items
    counts.value = r.counts
    total.value = r.total
    error.value = null
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e)
    if (!quiet) items.value = []
  } finally {
    loading.value = false
  }
}

watch(filter, () => {
  offset.value = 0
  load()
})

watch(search, () => {
  clearTimeout(debounce)
  debounce = setTimeout(() => {
    offset.value = 0
    load()
  }, 250)
})

function toggle(key: string) {
  const next = new Set(open.value)
  if (next.has(key)) next.delete(key)
  else next.add(key)
  open.value = next
}

async function requeue(a: SeedAux) {
  if (busyId.value) return
  busyId.value = a.attempt_id
  try {
    await api.requeueAttempt(a.attempt_id)
    if (a.site) open.value = new Set(open.value).add(currentKeyOf(a))
    await load(true)
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e)
  } finally {
    busyId.value = null
  }
}

/** 找到某条辅种记录所属的组，用于重新校验后把那一行保持展开。 */
function currentKeyOf(a: SeedAux): string {
  const g = items.value.find((x) => x.aux.some((y) => y.attempt_id === a.attempt_id))
  return g?.key ?? ''
}

async function remove(a: SeedAux) {
  if (busyId.value) return
  if (!window.confirm('删除这条辅种记录？只删记录，不会动下载器里的种子。')) return
  busyId.value = a.attempt_id
  try {
    await api.deleteAttempt(a.attempt_id)
    await load(true)
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e)
  } finally {
    busyId.value = null
  }
}

const pageInfo = computed(() => {
  const from = total.value === 0 ? 0 : offset.value + 1
  const to = Math.min(offset.value + PAGE_SIZE, total.value)
  return `${from}–${to} / ${total.value}`
})
const canPrev = computed(() => offset.value > 0)
const canNext = computed(() => offset.value + PAGE_SIZE < total.value)

/** 筛选按钮上的数字。校验中按组数没有单独统计，就不显示数字，避免口径不一致。 */
function chipCount(value: string): number | null {
  const c = counts.value
  if (!c) return null
  switch (value) {
    case 'all':
      return c.groups
    case 'has_failure':
      return c.with_failure
    case 'no_aux':
      return c.no_aux
    case 'main_missing':
      return c.main_missing
    default:
      return null
  }
}

function sitesOf(g: SeedGroup): string[] {
  return [...new Set(g.aux.map((a) => a.site_name || a.site))]
}

function mainPhaseLabel(g: SeedGroup): string {
  if (!g.main.present) return '已不在下载器'
  switch (g.main.phase) {
    case 'complete':
      return g.main.state.toLowerCase().includes('paused') ? '已完成·暂停' : '做种中'
    case 'checking':
      return '校验中'
    case 'missing_files':
      return '文件缺失'
    case 'error':
      return '错误'
    case 'incomplete':
      return '未完成'
    default:
      return g.main.state || '—'
  }
}

onMounted(() => {
  load()
  // 只在有在途校验时轮询：闲置页面每 5 秒拉一次下载器全量列表是白费 qB 的 CPU。
  poll = setInterval(() => {
    if (items.value.some((g) => g.state === 'verifying')) load(true)
  }, 5000)
})
onUnmounted(() => {
  clearInterval(poll)
  clearTimeout(debounce)
})

const stateLegend: { state: ReseedState; meaning: string }[] = [
  { state: 'added', meaning: '已交给下载器，尚未得出校验结果 —— 此时不算成功' },
  { state: 'verifying', meaning: '下载器正在校验，请勿干预' },
  { state: 'seeding', meaning: '校验通过、数据完整，已开始做种' },
  { state: 'verify_failed', meaning: '校验完成但数据不完整：保存路径不一致或资源不同' },
]
</script>

<template>
  <div class="space-y-4">
    <div class="flex flex-wrap items-center justify-between gap-3">
      <div>
        <h1 class="text-base font-semibold">种子管理</h1>
        <p class="text-muted-foreground mt-0.5 text-xs">
          主种子 + 它在各站点的辅种子；校验通过才算成功
        </p>
      </div>
      <div class="flex w-full gap-2 sm:w-auto">
        <Input v-model="search" placeholder="搜索名称 / 站点 / hash / 路径…" class="w-full sm:w-64" />
        <Button variant="outline" size="icon" title="刷新" @click="load()">
          <RefreshCw :class="cn('size-4', loading && 'animate-spin')" />
        </Button>
      </div>
    </div>

    <Alert v-if="error" variant="destructive">
      <AlertCircle />
      <AlertDescription>{{ error }}</AlertDescription>
    </Alert>

    <div v-if="counts" class="text-muted-foreground flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
      <span class="tabular">{{ counts.groups }} 组</span>
      <span class="tabular">
        辅种
        <span class="text-status-success font-medium">{{ counts.aux_seeding }}</span>/{{ counts.aux_total }}
        成功
      </span>
      <span v-if="counts.aux_verifying > 0" class="tabular text-status-active">
        {{ counts.aux_verifying }} 校验中
      </span>
      <span v-if="counts.aux_failed > 0" class="tabular text-status-error">
        {{ counts.aux_failed }} 失败
      </span>
      <span v-if="counts.main_missing > 0" class="tabular text-status-error">
        {{ counts.main_missing }} 组主种子缺失
      </span>
    </div>

    <div class="flex flex-wrap gap-1.5">
      <button
        v-for="f in FILTERS"
        :key="f.value"
        :class="
          cn(
            'rounded-md border px-2.5 py-1 text-xs transition-colors',
            filter === f.value
              ? 'bg-secondary text-secondary-foreground border-transparent font-medium'
              : 'text-muted-foreground hover:bg-accent hover:text-foreground',
          )
        "
        @click="filter = f.value"
      >
        {{ f.label }}
        <span v-if="(chipCount(f.value) ?? 0) > 0" class="tabular ml-1 opacity-60">
          {{ chipCount(f.value) }}
        </span>
      </button>
    </div>

    <Card class="py-0">
      <CardContent class="p-0">
        <Table>
          <TableHeader>
            <TableRow class="hover:bg-transparent">
              <TableHead class="pl-5">主种子</TableHead>
              <TableHead class="w-20 text-right">大小</TableHead>
              <TableHead class="w-24">状态</TableHead>
              <TableHead class="w-28">辅种</TableHead>
              <TableHead class="w-40">站点</TableHead>
              <TableHead class="w-24 pr-5 text-right">更新</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            <template v-if="loading && !items.length">
              <TableRow v-for="i in 6" :key="i" class="hover:bg-transparent">
                <TableCell v-for="c in 6" :key="c" :class="c === 1 ? 'pl-5' : undefined">
                  <Skeleton class="h-4 w-full" />
                </TableCell>
              </TableRow>
            </template>
            <TableRow v-else-if="!items.length" class="hover:bg-transparent">
              <td colspan="6" class="text-muted-foreground px-5 py-10 text-center text-xs">
                {{
                  search
                    ? '没有匹配的种子'
                    : '还没有辅种数据。运行一次任务后，这里会按主种子聚合出辅种情况。'
                }}
              </td>
            </TableRow>
            <template v-for="g in items" v-else :key="g.key">
              <TableRow class="cursor-pointer" @click="toggle(g.key)">
                <TableCell class="max-w-0 pl-5">
                  <div class="flex items-center gap-1.5">
                    <ChevronRight
                      :class="
                        cn(
                          'text-muted-foreground size-3.5 shrink-0 transition-transform',
                          open.has(g.key) && 'rotate-90',
                        )
                      "
                    />
                    <span
                      v-if="g.state === 'main_missing' || g.counts.failed > 0"
                      class="bg-status-error size-1.5 shrink-0 rounded-full"
                      :title="GROUP_META[g.state].hint"
                    />
                    <div class="min-w-0">
                      <div class="truncate text-xs" :title="g.name">{{ g.name || '（未知名称）' }}</div>
                      <div class="text-muted-foreground tabular mt-0.5 font-mono text-[10px]">
                        {{ shortHash(g.main.hash || g.key, 10, 6) }}
                        <span v-if="g.category" class="ml-1.5 font-sans">{{ g.category }}</span>
                      </div>
                    </div>
                  </div>
                </TableCell>
                <TableCell class="tabular text-muted-foreground text-right text-xs">
                  {{ formatBytes(g.size) }}
                </TableCell>
                <TableCell class="text-xs">
                  <span :class="GROUP_META[g.state].cls" :title="GROUP_META[g.state].hint">
                    {{ GROUP_META[g.state].label }}
                  </span>
                  <div class="text-muted-foreground mt-0.5 text-[10px]">{{ mainPhaseLabel(g) }}</div>
                </TableCell>
                <TableCell class="tabular text-xs">
                  <span
                    :class="
                      cn(
                        g.counts.failed > 0
                          ? 'text-status-error'
                          : g.counts.seeding === g.counts.aux
                            ? 'text-status-success'
                            : 'text-status-active',
                      )
                    "
                  >
                    {{ g.counts.seeding }}/{{ g.counts.aux }}
                  </span>
                  <span class="text-muted-foreground ml-1 text-[10px]">通过</span>
                </TableCell>
                <TableCell class="max-w-0">
                  <div class="flex flex-wrap gap-1">
                    <span
                      v-for="site in sitesOf(g).slice(0, 3)"
                      :key="site"
                      class="text-muted-foreground truncate rounded border px-1 py-0.5 text-[10px]"
                    >
                      {{ site }}
                    </span>
                    <span v-if="sitesOf(g).length > 3" class="text-muted-foreground text-[10px]">
                      +{{ sitesOf(g).length - 3 }}
                    </span>
                    <span v-if="!g.aux.length" class="text-muted-foreground text-[10px]">—</span>
                  </div>
                </TableCell>
                <TableCell class="text-muted-foreground pr-5 text-right text-xs">
                  {{ timeAgo(g.updated_at) }}
                </TableCell>
              </TableRow>

              <!-- 展开：这个主种子在各站点的辅种子 -->
              <tr v-if="open.has(g.key)" class="bg-muted/20 border-b">
                <td colspan="6" class="px-0 py-0">
                  <div class="px-5 py-3">
                    <div class="text-muted-foreground mb-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px]">
                      <span>保存路径</span>
                      <span class="text-foreground/80 font-mono">{{ g.save_path || '—' }}</span>
                      <span v-if="g.main.uploaded > 0" class="tabular">
                        主种子上传 {{ formatBytes(g.main.uploaded) }}
                      </span>
                      <span v-if="g.main.added_on" class="tabular">
                        加入于 {{ formatDateTime(g.main.added_on) }}
                      </span>
                      <span v-if="g.duplicates.length" class="text-status-warn">
                        组内另有 {{ g.duplicates.length }} 个同数据副本
                      </span>
                    </div>

                    <div v-if="!g.aux.length" class="text-muted-foreground py-1 text-xs">
                      这个主种子还没有任何辅种记录。
                    </div>
                    <table v-else class="w-full text-xs">
                      <thead>
                        <tr class="text-muted-foreground text-[11px]">
                          <th class="w-28 py-1 text-left font-normal">站点</th>
                          <th class="w-32 py-1 text-left font-normal">状态</th>
                          <th class="w-36 py-1 text-left font-normal">目标 hash</th>
                          <th class="w-16 py-1 text-right font-normal">校验</th>
                          <th class="w-24 py-1 text-right font-normal">上传</th>
                          <th class="py-1 text-left font-normal">说明</th>
                          <th class="w-32 py-1 text-right font-normal">操作</th>
                        </tr>
                      </thead>
                      <tbody>
                        <tr
                          v-for="a in g.aux"
                          :key="a.attempt_id"
                          class="border-border/50 border-t"
                        >
                          <td class="py-1.5">
                            <div class="truncate" :title="a.site">{{ a.site_name || a.site }}</div>
                            <div
                              v-if="!a.in_downloader"
                              class="text-status-warn text-[10px]"
                              title="记录还在，但下载器里没有这个种子"
                            >
                              不在下载器
                            </div>
                          </td>
                          <td class="py-1.5">
                            <StateBadge :state="a.state" size="sm" />
                          </td>
                          <td class="text-muted-foreground tabular py-1.5 font-mono text-[10px]">
                            {{ shortHash(a.info_hash, 8, 6) }}
                          </td>
                          <td class="tabular text-muted-foreground py-1.5 text-right">
                            {{ a.check_count }}
                          </td>
                          <td class="tabular text-muted-foreground py-1.5 text-right">
                            {{ a.live ? formatBytes(a.live.uploaded) : '—' }}
                          </td>
                          <td class="text-muted-foreground py-1.5">
                            <span class="line-clamp-2" :title="a.message ?? ''">
                              {{ a.message || '—' }}
                            </span>
                          </td>
                          <td class="py-1.5 text-right">
                            <div class="flex justify-end gap-0.5">
                              <Button
                                variant="ghost"
                                size="icon"
                                class="size-6"
                                title="重新加入校验队列"
                                :disabled="busyId === a.attempt_id"
                                @click.stop="requeue(a)"
                              >
                                <RefreshCw
                                  :class="
                                    cn('size-3', busyId === a.attempt_id && 'animate-spin')
                                  "
                                />
                              </Button>
                              <Button
                                variant="ghost"
                                size="icon"
                                class="size-6"
                                title="删除这条辅种记录"
                                :disabled="busyId === a.attempt_id"
                                @click.stop="remove(a)"
                              >
                                <Trash2 class="size-3" />
                              </Button>
                            </div>
                          </td>
                        </tr>
                      </tbody>
                    </table>
                  </div>
                </td>
              </tr>
            </template>
          </TableBody>
        </Table>
      </CardContent>
    </Card>

    <div class="flex items-center justify-between text-xs">
      <span class="text-muted-foreground tabular">{{ pageInfo }}</span>
      <div class="flex gap-1.5">
        <Button variant="outline" size="sm" :disabled="!canPrev" @click="offset -= PAGE_SIZE">
          上一页
        </Button>
        <Button variant="outline" size="sm" :disabled="!canNext" @click="offset += PAGE_SIZE">
          下一页
        </Button>
      </div>
    </div>

    <!-- 状态说明：校验通过才算成功，这句话得一直看得见。 -->
    <Card class="bg-muted/30 py-4">
      <CardContent class="space-y-2">
        <div class="flex items-center gap-1.5 text-xs font-medium">
          <Info class="size-3.5" /> 辅种状态说明
        </div>
        <div class="grid gap-1.5 sm:grid-cols-2">
          <div v-for="l in stateLegend" :key="l.state" class="flex items-start gap-2 text-xs">
            <StateBadge :state="l.state" size="sm" class="shrink-0" />
            <span class="text-muted-foreground">{{ l.meaning }}</span>
          </div>
        </div>
      </CardContent>
    </Card>
  </div>
</template>
