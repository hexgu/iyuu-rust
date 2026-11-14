<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { useRoute } from 'vue-router'
import { AlertCircle, ChevronRight, Info, RefreshCw, Tag, Trash2 } from 'lucide-vue-next'
import {
  api,
  ApiError,
  type SeedAux,
  type SeedCounts,
  type SeedGroup,
  type SeedGroupState,
  type SeedMainSource,
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
  { value: 'has_aux', label: '有辅种' },
  { value: 'no_aux', label: '未辅种' },
  { value: 'has_failure', label: '有失败' },
  { value: 'verifying', label: '校验中' },
  { value: 'main_missing', label: '主种子缺失' },
  { value: 'inferred', label: '识别存疑' },
]

/**
 * 组状态的说法。主种子缺失排在最前，因为它比「辅种失败」更严重：失败只是这条没成，
 * 缺失意味着本地这份数据可能已经不在了。
 */
const GROUP_META: Record<SeedGroupState, { label: string; cls: string; hint: string }> = {
  main_missing: {
    label: '主种子缺失',
    cls: 'text-status-error',
    hint: '下载器里只剩下这个资源的辅种副本，原件已经不在了',
  },
  degraded: {
    label: '有失败',
    cls: 'text-status-error',
    hint: '有辅种没通过校验、添加失败，或记录说成功但种子已经不在了',
  },
  verifying: { label: '校验中', cls: 'text-status-active', hint: '下载器正在校验，等结果' },
  no_aux: { label: '未辅种', cls: 'text-status-pending', hint: '主种子在，还没有其它站点的副本' },
  healthy: { label: '正常', cls: 'text-status-success', hint: '主种子在，辅种全部校验通过' },
}

/** 主种子是「怎么认出来的」。推断出来的要标出来，不假装是事实。 */
const MAIN_SOURCE: Record<SeedMainSource, { label: string; hint: string }> = {
  record: { label: '辅种记录', hint: '辅种记录指名这个种子是数据来源' },
  single: { label: '唯一候选', hint: '组里只有这一个种子' },
  tag: { label: '标签', hint: '组里其它种子带着辅种标签，剩下这个就是主种子' },
  traffic: {
    label: '下载流量',
    hint: '这一份有下载记录：用户真的在这里拉过数据，其它副本是复用文件加进来的',
  },
  inferred: { label: '推断', hint: '没有记录也没有标签，按加入时间推断 —— 可能不准' },
  snapshot: { label: '记录快照', hint: '主种子已被删除，信息来自最后的辅种记录' },
}

const route = useRoute()
const items = ref<SeedGroup[]>([])
const counts = ref<SeedCounts | null>(null)
const total = ref(0)
const offset = ref(0)
const filter = ref('all')
const search = ref('')
const loading = ref(true)
const error = ref<string | null>(null)
const notice = ref<string | null>(null)
const busyId = ref<number | null>(null)
const rewriting = ref(false)
const cleaning = ref(false)
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

const FILTER_VALUES = new Set(FILTERS.map((f) => f.value))

/** 概览页的「校验中 / 有失败」是带着 ?filter= 跳过来的，得认这个参数。 */
function applyQueryFilter() {
  const value = String(route.query.filter ?? 'all')
  filter.value = FILTER_VALUES.has(value) ? value : 'all'
}

watch(filter, () => {
  offset.value = 0
  load()
})

watch(
  () => route.query.filter,
  () => {
    // filter 变了会走上面的 watch 去加载；没变就不该再打一次下载器。
    applyQueryFilter()
  },
)

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
  if (busyId.value || a.attempt_id == null) return
  busyId.value = a.attempt_id
  try {
    await api.requeueAttempt(a.attempt_id)
    // 记录还在，找到所属组保持展开，别让用户重新找一次。
    const g = items.value.find((x) => x.aux.some((y) => y.attempt_id === a.attempt_id))
    if (g) open.value = new Set(open.value).add(g.key)
    await load(true)
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e)
  } finally {
    busyId.value = null
  }
}

async function remove(a: SeedAux) {
  if (busyId.value || a.attempt_id == null) return
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

async function cleanInvalid() {
  if (cleaning.value) return
  cleaning.value = true
  notice.value = null
  try {
    const preview = await api.runCleanup(true)
    if (preview.candidates === 0) {
      notice.value = '没有发现主种子缺失的辅种。'
      return
    }
    const ready = preview.actions.filter((a) => a.deleted)
    const names = ready.slice(0, 3).map((a) => a.name)
    const more = ready.length > names.length ? `\n…以及另外 ${ready.length - names.length} 组` : ''
    const confirmed = window.confirm(
      `将删除 ${preview.deleted_groups} 组辅种（${preview.deleted_torrents} 个种子${preview.delete_files ? '，连同文件' : ''}）：\n\n${names.join('\n')}${more}\n\n删除后不可恢复，确认继续？`,
    )
    if (!confirmed) return
    const done = await api.runCleanup(false)
    notice.value = `已清理 ${done.deleted_groups} 组，删除 ${done.deleted_torrents} 个种子${done.delete_files ? '（含文件）' : ''}${
      done.errors.length ? `；${done.errors.length} 个失败` : ''
    }。`
    await load(true)
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e)
  } finally {
    cleaning.value = false
  }
}

async function rewriteTags() {
  if (rewriting.value) return
  if (!window.confirm('把下载器里的 iyuu-reseed* 旧标签重写成 iyuu-rust？')) return
  rewriting.value = true
  notice.value = null
  try {
    const r = await api.rewriteSeedTags()
    if (r.skipped > 0 && r.rewritten === 0) {
      notice.value = `${r.client} 不支持移除标签，${r.skipped} 个种子没有迁移，旧的 iyuu-reseed* 标签原样保留。`
    } else {
      notice.value = [
        `扫描 ${r.scanned} 个种子，命中旧标签 ${r.matched} 个，重写 ${r.rewritten} 个`,
        r.skipped > 0 ? `跳过 ${r.skipped} 个（客户端不支持移除标签）` : '',
        r.errors.length > 0 ? `${r.errors.length} 个失败：${r.errors[0]}` : '',
      ]
        .filter(Boolean)
        .join('；')
    }
    await load(true)
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e)
  } finally {
    rewriting.value = false
  }
}

const pageInfo = computed(() => {
  const from = total.value === 0 ? 0 : offset.value + 1
  const to = Math.min(offset.value + PAGE_SIZE, total.value)
  return `${from}–${to} / ${total.value}`
})
const canPrev = computed(() => offset.value > 0)
const canNext = computed(() => offset.value + PAGE_SIZE < total.value)

/** 筛选按钮上的数字。校验中不单独统计组数，就不显示数字，避免口径不一致。 */
function chipCount(value: string): number | null {
  const c = counts.value
  if (!c) return null
  switch (value) {
    case 'all':
      return c.groups
    case 'has_aux':
      return c.with_aux
    case 'no_aux':
      return c.no_aux
    case 'has_failure':
      return c.with_failure
    case 'main_missing':
      return c.main_missing
    case 'inferred':
      return c.inferred
    default:
      return null
  }
}

function sitesOf(g: SeedGroup): string[] {
  return [...new Set(g.aux.map((a) => a.site_name).filter(Boolean))]
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

function auxNote(a: SeedAux): string {
  if (a.message) return a.message
  if (a.duplicate) return '同站点的重复副本'
  switch (a.origin) {
    case 'record':
      return '辅种记录'
    case 'tag':
      return '带辅种标签'
    default:
      return '同一份数据的副本'
  }
}

onMounted(() => {
  const before = filter.value
  applyQueryFilter()
  if (filter.value === before) load()
  // 只在有在途校验时轮询：闲置页面每 5 秒拉一次下载器全量列表是白费 qB 的 CPU。
  poll = setInterval(() => {
    if (items.value.some((g) => g.state === 'verifying')) load(true)
  }, 5000)
})
onUnmounted(() => {
  clearInterval(poll)
  clearTimeout(debounce)
})
</script>

<template>
  <div class="space-y-4">
    <div class="flex flex-wrap items-center justify-between gap-3">
      <div>
        <h1 class="text-base font-semibold">种子管理</h1>
        <p class="text-muted-foreground mt-0.5 text-xs">
          下载器里的每一份资源：主种子一行，跨站副本折叠在下面
        </p>
      </div>
      <div class="flex w-full flex-wrap gap-2 sm:w-auto">
        <Input v-model="search" placeholder="搜索名称 / 站点 / hash / 标签…" class="w-full sm:w-64" />
        <Button
          variant="outline"
          :disabled="cleaning"
          title="删除主种子已消失的辅种（会先预览）"
          @click="cleanInvalid"
        >
          <Trash2 :class="cn('size-4', cleaning && 'animate-pulse')" />
          清理无效辅种
        </Button>
        <Button variant="outline" :disabled="rewriting" title="把旧 iyuu-reseed* 标签重写为 iyuu-rust" @click="rewriteTags">
          <Tag :class="cn('size-4', rewriting && 'animate-pulse')" />
          重写旧标签
        </Button>
        <Button variant="outline" size="icon" title="刷新" @click="load()">
          <RefreshCw :class="cn('size-4', loading && 'animate-spin')" />
        </Button>
      </div>
    </div>

    <Alert v-if="error" variant="destructive">
      <AlertCircle />
      <AlertDescription>{{ error }}</AlertDescription>
    </Alert>
    <Alert v-if="notice">
      <Info />
      <AlertDescription>{{ notice }}</AlertDescription>
    </Alert>

    <div v-if="counts" class="text-muted-foreground flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
      <span class="tabular">{{ counts.torrents }} 个种子 · {{ counts.groups }} 份资源</span>
      <span class="tabular">
        辅种
        <span class="text-status-success font-medium">{{ counts.aux_seeding }}</span>/{{ counts.aux_total }}
        成功
      </span>
      <span v-if="counts.no_aux > 0" class="tabular">{{ counts.no_aux }} 份未辅种</span>
      <span v-if="counts.aux_verifying > 0" class="tabular text-status-active">
        {{ counts.aux_verifying }} 校验中
      </span>
      <span v-if="counts.aux_failed > 0" class="tabular text-status-error">
        {{ counts.aux_failed }} 失败
      </span>
      <span v-if="counts.aux_missing > 0" class="tabular text-status-error">
        {{ counts.aux_missing }} 已丢失
      </span>
      <span v-if="counts.main_missing > 0" class="tabular text-status-error">
        {{ counts.main_missing }} 份主种子缺失
      </span>
      <span v-if="counts.inferred > 0" class="tabular text-status-warn">
        {{ counts.inferred }} 份靠推断识别
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
              <TableHead class="pl-5">资源（主种子）</TableHead>
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
                    : '下载器里还没有种子。添加种子或跑一次辅种任务后，这里会按资源聚合展示。'
                }}
              </td>
            </TableRow>
            <template v-for="g in items" v-else :key="g.key">
              <TableRow
                :class="g.aux.length ? 'cursor-pointer' : undefined"
                @click="g.aux.length && toggle(g.key)"
              >
                <TableCell class="max-w-0 pl-5">
                  <div class="flex items-center gap-1.5">
                    <ChevronRight
                      v-if="g.aux.length"
                      :class="
                        cn(
                          'text-muted-foreground size-3.5 shrink-0 transition-transform',
                          open.has(g.key) && 'rotate-90',
                        )
                      "
                    />
                    <span v-else class="size-3.5 shrink-0" />
                    <span
                      v-if="g.state === 'main_missing' || g.counts.failed > 0"
                      class="bg-status-error size-1.5 shrink-0 rounded-full"
                      :title="GROUP_META[g.state].hint"
                    />
                    <div class="min-w-0">
                      <div class="truncate text-xs" :title="g.name">{{ g.name || '（未知名称）' }}</div>
                      <div class="text-muted-foreground tabular mt-0.5 flex items-center gap-1.5 font-mono text-[10px]">
                        <span>{{ shortHash(g.main.hash || g.key, 10, 6) }}</span>
                        <span v-if="g.category" class="font-sans">{{ g.category }}</span>
                        <span
                          v-if="g.main_source === 'inferred'"
                          class="text-status-warn border-status-warn/40 rounded border px-1 font-sans text-[9px]"
                          :title="MAIN_SOURCE.inferred.hint"
                        >
                          推断
                        </span>
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
                  <template v-if="g.counts.aux">
                    <span
                      :class="
                        cn(
                          g.counts.failed + g.counts.missing > 0
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
                  </template>
                  <span v-else class="text-muted-foreground">—</span>
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

              <!-- 展开：这个资源在各站点的副本 -->
              <tr v-if="open.has(g.key)" class="bg-muted/20 border-b">
                <td colspan="6" class="px-0 py-0">
                  <div class="px-5 py-3">
                    <div class="text-muted-foreground mb-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px]">
                      <span>保存路径</span>
                      <span class="text-foreground/80 truncate font-mono">{{ g.save_path || '—' }}</span>
                      <span v-if="g.main.uploaded > 0" class="tabular">
                        主种子上传 {{ formatBytes(g.main.uploaded) }}
                      </span>
                      <span v-if="g.main.downloaded > 0" class="tabular">
                        下载 {{ formatBytes(g.main.downloaded) }}
                      </span>
                      <span v-if="g.main.added_on" class="tabular">
                        加入于 {{ formatDateTime(g.main.added_on) }}
                      </span>
                      <span :title="MAIN_SOURCE[g.main_source].hint">
                        识别依据：{{ MAIN_SOURCE[g.main_source].label }}
                      </span>
                      <span class="tabular">{{ g.downloaders.join(' / ') }}</span>
                    </div>

                    <table class="w-full text-xs">
                      <thead>
                        <tr class="text-muted-foreground text-[11px]">
                          <th class="w-32 py-1 text-left font-normal">站点</th>
                          <th class="w-28 py-1 text-left font-normal">状态</th>
                          <th class="w-32 py-1 text-left font-normal">hash</th>
                          <th class="w-40 py-1 text-left font-normal">标签</th>
                          <th class="w-24 py-1 text-right font-normal">上传</th>
                          <th class="py-1 text-left font-normal">说明</th>
                          <th class="w-24 py-1 text-right font-normal">操作</th>
                        </tr>
                      </thead>
                      <tbody>
                        <tr
                          v-for="a in g.aux"
                          :key="a.info_hash"
                          class="border-border/50 border-t"
                        >
                          <td class="py-1.5">
                            <div class="flex items-center gap-1">
                              <span class="truncate" :title="a.site || a.site_name">
                                {{ a.site_name || '未知站点' }}
                              </span>
                              <span
                                v-if="a.duplicate"
                                class="text-status-warn shrink-0 rounded border px-1 text-[9px]"
                                title="同一个站点上有两份同数据种子"
                              >
                                重复
                              </span>
                            </div>
                            <div
                              v-if="!a.in_downloader"
                              class="text-status-error text-[10px]"
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
                          <td class="py-1.5">
                            <div class="flex flex-wrap gap-1">
                              <span
                                v-for="tag in a.tags.slice(0, 3)"
                                :key="tag"
                                class="text-muted-foreground truncate rounded border px-1 py-0.5 text-[10px]"
                              >
                                {{ tag }}
                              </span>
                              <span v-if="!a.tags.length" class="text-muted-foreground text-[10px]">—</span>
                            </div>
                          </td>
                          <td class="tabular text-muted-foreground py-1.5 text-right">
                            {{ a.in_downloader ? formatBytes(a.uploaded) : '—' }}
                          </td>
                          <td class="text-muted-foreground py-1.5">
                            <span class="line-clamp-2" :title="a.message ?? ''">{{ auxNote(a) }}</span>
                          </td>
                          <td class="py-1.5 text-right">
                            <div v-if="a.attempt_id != null" class="flex justify-end gap-0.5">
                              <Button
                                variant="ghost"
                                size="icon"
                                class="size-6"
                                title="重新加入校验队列"
                                :disabled="busyId === a.attempt_id"
                                @click.stop="requeue(a)"
                              >
                                <RefreshCw
                                  :class="cn('size-3', busyId === a.attempt_id && 'animate-spin')"
                                />
                              </Button>
                              <Button
                                variant="ghost"
                                size="icon"
                                class="size-6"
                                title="删除这条辅种记录（不碰下载器里的种子）"
                                :disabled="busyId === a.attempt_id"
                                @click.stop="remove(a)"
                              >
                                <Trash2 class="size-3" />
                              </Button>
                            </div>
                            <span v-else class="text-muted-foreground text-[10px]">无记录</span>
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

    <!-- 识别规则：这个页面靠下载器的事实工作，tag 只是提示。 -->
    <Card class="bg-muted/30 py-4">
      <CardContent class="space-y-2 text-xs">
        <div class="flex items-center gap-1.5 font-medium">
          <Info class="size-3.5" /> 主种子与辅种是怎么认出来的
        </div>
        <p class="text-muted-foreground">
          先看种子落盘的位置（<span class="font-mono text-[11px]">content_path</span>）——路径相同就是同一份数据；
          再看大小与名称；辅种记录用来补站点和校验结果。标签只用来判断「哪个是副本」，
          不决定两个种子是不是一回事，所以别的工具加的、没有标签的副本也会自动归组。
        </p>
        <div class="text-muted-foreground flex flex-wrap gap-x-4 gap-y-1">
          <span>校验通过才算成功；记录说成功但下载器里已经没有的，标成「已丢失」。</span>
          <span>「推断」的组是没有任何记录/标签时按加入时间猜的，可以点「识别存疑」筛出来。</span>
        </div>
      </CardContent>
    </Card>
  </div>
</template>
