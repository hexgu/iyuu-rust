<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { ChevronLeft, ChevronRight, RefreshCw, Trash2, Info, X } from 'lucide-vue-next'
import { api, ApiError, type Attempt, type ReseedState } from '@/lib/api'
import { cn, formatDateTime, shortHash, timeAgo } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Alert, AlertDescription } from '@/components/ui/alert'
import { Skeleton } from '@/components/ui/skeleton'
import {
  Table,
  TableBody,
  TableCell,
  TableEmpty,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'
import StateBadge from '@/components/layout/StateBadge.vue'

const route = useRoute()
const router = useRouter()

const PAGE_SIZE = 50

/** Filter tabs. Order follows the lifecycle so the eye can scan left→right. */
const FILTERS: { value: string; label: string }[] = [
  { value: '', label: '全部' },
  { value: 'seeding', label: '辅种中' },
  { value: 'verifying', label: '校验中' },
  { value: 'added', label: '待校验' },
  { value: 'verify_failed', label: '校验不通过' },
  { value: 'missing_files', label: '文件缺失' },
  { value: 'fetch_failed', label: '获取失败' },
  { value: 'timed_out', label: '超时' },
  { value: 'skipped', label: '跳过' },
]

const state = ref<string>((route.query.state as string) ?? '')
const search = ref('')
const items = ref<Attempt[]>([])
const total = ref(0)
const offset = ref(0)
const loading = ref(true)
const error = ref<string | null>(null)
const busyId = ref<number | null>(null)
const counts = ref<Record<string, number>>({})
const detail = ref<Attempt | null>(null)
let timer: ReturnType<typeof setInterval> | undefined

async function load() {
  try {
    const [page, stats] = await Promise.all([
      api.attempts({ state: state.value || undefined, limit: PAGE_SIZE, offset: offset.value }),
      api.attemptStats(),
    ])
    items.value = page.items
    total.value = page.total
    counts.value = stats.counts
    error.value = null
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e)
  } finally {
    loading.value = false
  }
}

watch([state, offset], () => {
  loading.value = true
  load()
  // Keep the URL in sync so a filtered view can be bookmarked / shared.
  router.replace({ query: state.value ? { state: state.value } : {} })
})

function setFilter(v: string) {
  state.value = v
  offset.value = 0
}

/** Client-side search over the current page: the dataset per page is small and
 *  the server-side benefit of a LIKE query is not worth another param. */
const visible = computed(() => {
  const q = search.value.trim().toLowerCase()
  if (!q) return items.value
  return items.value.filter(
    (a) =>
      a.source_name.toLowerCase().includes(q) ||
      a.target_site.toLowerCase().includes(q) ||
      a.target_info_hash.includes(q) ||
      a.save_path.toLowerCase().includes(q),
  )
})

async function requeue(a: Attempt) {
  busyId.value = a.id
  try {
    await api.requeueAttempt(a.id)
    await load()
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e)
  } finally {
    busyId.value = null
  }
}

async function remove(a: Attempt) {
  // Deleting the *record* is harmless (it does not touch the torrent), but it
  // does mean a future run may re-add that target, so confirm.
  if (!confirm(`删除记录「${a.source_name}」？\n\n只会删除这条辅种记录，不会影响下载器中的种子。下次运行可能重新辅种该目标。`)) {
    return
  }
  busyId.value = a.id
  try {
    await api.deleteAttempt(a.id)
    if (detail.value?.id === a.id) detail.value = null
    await load()
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

function countFor(v: string) {
  if (!v) return Object.values(counts.value).reduce((a, b) => a + b, 0)
  return counts.value[v] ?? 0
}

onMounted(() => {
  load()
  // Poll while any attempt is mid-verification so states advance on screen
  // without a manual refresh.
  timer = setInterval(() => {
    const active = items.value.some((a) => a.state === 'added' || a.state === 'verifying')
    if (active) load()
  }, 5000)
})
onUnmounted(() => clearInterval(timer))

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
        <h1 class="text-base font-semibold">辅种记录</h1>
        <p class="text-muted-foreground mt-0.5 text-xs">
          校验通过才算成功；「待校验」只是已添加，不代表可用
        </p>
      </div>
      <Input v-model="search" placeholder="搜索名称 / 站点 / hash / 路径…" class="w-full sm:w-64" />
    </div>

    <Alert v-if="error" variant="destructive">
      <X />
      <AlertDescription>{{ error }}</AlertDescription>
    </Alert>

    <!-- Filter pills -->
    <div class="flex flex-wrap gap-1.5">
      <button
        v-for="f in FILTERS"
        :key="f.value"
        :class="cn(
          'rounded-md border px-2.5 py-1 text-xs transition-colors',
          state === f.value
            ? 'bg-secondary text-secondary-foreground border-transparent font-medium'
            : 'text-muted-foreground hover:bg-accent hover:text-foreground',
        )"
        @click="setFilter(f.value)"
      >
        {{ f.label }}
        <span v-if="countFor(f.value) > 0" class="tabular ml-1 opacity-60">
          {{ countFor(f.value) }}
        </span>
      </button>
    </div>

    <Card class="py-0">
      <CardContent class="p-0">
        <Table>
          <TableHeader>
            <TableRow class="hover:bg-transparent">
              <TableHead class="w-24 pl-5">状态</TableHead>
              <TableHead>源种子</TableHead>
              <TableHead class="w-28">目标站点</TableHead>
              <TableHead class="w-32">目标 hash</TableHead>
              <TableHead class="w-20 text-right">校验次数</TableHead>
              <TableHead class="w-24">更新时间</TableHead>
              <TableHead class="w-20 pr-5 text-right">操作</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            <template v-if="loading && !items.length">
              <TableRow v-for="i in 6" :key="i" class="hover:bg-transparent">
                <TableCell v-for="c in 7" :key="c" class="pl-5">
                  <Skeleton class="h-4 w-full" />
                </TableCell>
              </TableRow>
            </template>
            <TableEmpty
              v-else-if="!visible.length"
              :colspan="7"
              :text="search ? '没有匹配的记录' : '还没有辅种记录，先运行一次任务'"
            />
            <TableRow v-for="a in visible" v-else :key="a.id" class="group">
              <TableCell class="pl-5">
                <StateBadge :state="a.state" size="sm" />
              </TableCell>
              <TableCell class="max-w-0">
                <button
                  class="hover:text-foreground block w-full truncate text-left text-xs"
                  :title="a.source_name"
                  @click="detail = a"
                >
                  {{ a.source_name }}
                </button>
              </TableCell>
              <TableCell class="text-muted-foreground text-xs">{{ a.target_site }}</TableCell>
              <TableCell class="text-muted-foreground tabular text-xs">
                {{ shortHash(a.target_info_hash) }}
              </TableCell>
              <TableCell class="tabular text-muted-foreground text-right text-xs">
                {{ a.check_count }}
              </TableCell>
              <TableCell class="text-muted-foreground text-xs">
                {{ timeAgo(a.last_checked_at ?? a.added_at) }}
              </TableCell>
              <TableCell class="pr-5 text-right">
                <div class="flex justify-end gap-0.5 opacity-0 transition-opacity group-hover:opacity-100">
                  <Button
                    variant="ghost"
                    size="icon"
                    class="size-7"
                    title="重新加入校验队列"
                    :disabled="busyId === a.id"
                    @click="requeue(a)"
                  >
                    <RefreshCw :class="cn('size-3.5', busyId === a.id && 'animate-spin')" />
                  </Button>
                  <Button
                    variant="ghost"
                    size="icon"
                    class="size-7"
                    title="删除记录"
                    :disabled="busyId === a.id"
                    @click="remove(a)"
                  >
                    <Trash2 class="size-3.5" />
                  </Button>
                </div>
              </TableCell>
            </TableRow>
          </TableBody>
        </Table>
      </CardContent>
    </Card>

    <!-- Pagination -->
    <div class="flex items-center justify-between text-xs">
      <span class="text-muted-foreground tabular">{{ pageInfo }}</span>
      <div class="flex gap-1.5">
        <Button variant="outline" size="sm" :disabled="!canPrev" @click="offset -= PAGE_SIZE">
          <ChevronLeft /> 上一页
        </Button>
        <Button variant="outline" size="sm" :disabled="!canNext" @click="offset += PAGE_SIZE">
          下一页 <ChevronRight />
        </Button>
      </div>
    </div>

    <!-- Legend: explains why a paused torrent is not a success. -->
    <Card class="bg-muted/30 py-4">
      <CardContent class="space-y-2">
        <div class="flex items-center gap-1.5 text-xs font-medium">
          <Info class="size-3.5" /> 状态说明
        </div>
        <div class="grid gap-1.5 sm:grid-cols-2">
          <div v-for="l in stateLegend" :key="l.state" class="flex items-start gap-2 text-xs">
            <StateBadge :state="l.state" size="sm" class="shrink-0" />
            <span class="text-muted-foreground">{{ l.meaning }}</span>
          </div>
        </div>
      </CardContent>
    </Card>

    <!-- Detail drawer -->
    <div
      v-if="detail"
      class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4"
      @click.self="detail = null"
    >
      <Card class="max-h-[80vh] w-full max-w-lg overflow-y-auto scrollbar-slim">
        <CardContent class="space-y-3 pt-5">
          <div class="flex items-start justify-between gap-3">
            <div class="min-w-0">
              <div class="truncate text-sm font-semibold">{{ detail.source_name }}</div>
              <div class="mt-1"><StateBadge :state="detail.state" /></div>
            </div>
            <Button variant="ghost" size="icon" class="size-7 shrink-0" @click="detail = null">
              <X class="size-4" />
            </Button>
          </div>

          <dl class="space-y-2 text-xs">
            <div v-for="row in [
              ['源 hash', detail.source_hash],
              ['目标 hash', detail.target_info_hash],
              ['目标站点', `${detail.target_site} (sid=${detail.target_sid})`],
              ['站点种子 ID', String(detail.target_torrent_id)],
              ['保存路径', detail.save_path],
              ['校验次数', String(detail.check_count)],
              ['添加时间', formatDateTime(detail.added_at)],
              ['校验开始', formatDateTime(detail.verify_started_at)],
              ['最后检查', formatDateTime(detail.last_checked_at)],
              ['结束时间', formatDateTime(detail.finished_at)],
            ]" :key="row[0]" class="grid grid-cols-[5.5rem_1fr] gap-2">
              <dt class="text-muted-foreground">{{ row[0] }}</dt>
              <dd class="break-all font-mono">{{ row[1] || '—' }}</dd>
            </div>
          </dl>

          <div v-if="detail.message" class="bg-muted/50 rounded-md p-2.5">
            <div class="text-muted-foreground mb-1 text-[11px]">消息</div>
            <div class="text-xs break-all">{{ detail.message }}</div>
          </div>

          <div class="flex justify-end gap-2 border-t pt-3">
            <Button variant="outline" size="sm" @click="requeue(detail)">
              <RefreshCw /> 重新校验
            </Button>
            <Button variant="destructive" size="sm" @click="remove(detail)">
              <Trash2 /> 删除记录
            </Button>
          </div>
        </CardContent>
      </Card>
    </div>
  </div>
</template>
