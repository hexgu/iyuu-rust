<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { AlertCircle, RefreshCw } from 'lucide-vue-next'
import { api, ApiError, type TorrentItem } from '@/lib/api'
import { cn, formatBytes, formatDuration, formatPercent, shortHash } from '@/lib/utils'
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

const FILTERS = [
  { value: '', label: '全部' },
  { value: 'complete', label: '已完成' },
  { value: 'incomplete', label: '未完成' },
  { value: 'checking', label: '校验中' },
  { value: 'error', label: '异常' },
]

const filter = ref('')
const items = ref<TorrentItem[]>([])
const total = ref(0)
const loading = ref(true)
const error = ref<string | null>(null)
const search = ref('')

async function load() {
  loading.value = true
  try {
    const r = await api.torrents({ filter: filter.value || undefined, limit: 1000 })
    items.value = r.items
    total.value = r.total
    error.value = null
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e)
    items.value = []
  } finally {
    loading.value = false
  }
}

watch(filter, load)
onMounted(load)

const visible = computed(() => {
  const q = search.value.trim().toLowerCase()
  if (!q) return items.value
  return items.value.filter(
    (t) =>
      t.name.toLowerCase().includes(q) ||
      t.hash.includes(q) ||
      t.save_path.toLowerCase().includes(q) ||
      (t.category ?? '').toLowerCase().includes(q) ||
      t.tags.some((tag) => tag.toLowerCase().includes(q)),
  )
})

const summary = computed(() => {
  const complete = items.value.filter((t) => t.phase === 'complete').length
  const checking = items.value.filter((t) => t.phase === 'checking').length
  const size = items.value.reduce((a, t) => a + t.size, 0)
  return { complete, checking, size }
})

/** Map the normalized phase to a compact visual. */
function phaseMeta(t: TorrentItem) {
  const m: Record<string, { label: string; cls: string }> = {
    complete: { label: '完成', cls: 'text-status-success' },
    checking: { label: '校验中', cls: 'text-status-active' },
    incomplete: { label: '未完成', cls: 'text-status-warn' },
    missing_files: { label: '文件缺失', cls: 'text-status-error' },
    error: { label: '错误', cls: 'text-status-error' },
    downloading_metadata: { label: '获取元数据', cls: 'text-muted-foreground' },
    unknown: { label: t.state, cls: 'text-muted-foreground' },
  }
  return m[t.phase] ?? m.unknown
}
</script>

<template>
  <div class="space-y-4">
    <div class="flex flex-wrap items-center justify-between gap-3">
      <div>
        <h1 class="text-base font-semibold">下载器种子</h1>
        <p class="text-muted-foreground mt-0.5 text-xs">
          共 {{ total }} 个 · 完成 {{ summary.complete }} · 校验中 {{ summary.checking }} ·
          {{ formatBytes(summary.size) }}
        </p>
      </div>
      <div class="flex w-full gap-2 sm:w-auto">
        <Input v-model="search" placeholder="搜索名称 / hash / 路径 / 标签…" class="w-full sm:w-64" />
        <Button variant="outline" size="icon" title="刷新" @click="load">
          <RefreshCw :class="cn('size-4', loading && 'animate-spin')" />
        </Button>
      </div>
    </div>

    <Alert v-if="error" variant="destructive">
      <AlertCircle />
      <AlertDescription>{{ error }}</AlertDescription>
    </Alert>

    <div class="flex flex-wrap gap-1.5">
      <button
        v-for="f in FILTERS"
        :key="f.value"
        :class="cn(
          'rounded-md border px-2.5 py-1 text-xs transition-colors',
          filter === f.value
            ? 'bg-secondary text-secondary-foreground border-transparent font-medium'
            : 'text-muted-foreground hover:bg-accent hover:text-foreground',
        )"
        @click="filter = f.value"
      >
        {{ f.label }}
      </button>
    </div>

    <Card class="py-0">
      <CardContent class="p-0">
        <Table>
          <TableHeader>
            <TableRow class="hover:bg-transparent">
              <TableHead class="pl-5">名称</TableHead>
              <TableHead class="w-20 text-right">大小</TableHead>
              <TableHead class="w-16 text-right">进度</TableHead>
              <TableHead class="w-20">状态</TableHead>
              <TableHead class="w-24">分类</TableHead>
              <TableHead class="w-20 text-right">做种时长</TableHead>
              <TableHead class="w-16 text-right">分享率</TableHead>
              <TableHead class="w-24 pr-5">标签</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            <template v-if="loading && !items.length">
              <TableRow v-for="i in 8" :key="i" class="hover:bg-transparent">
                <TableCell v-for="c in 8" :key="c" :class="c === 1 ? 'pl-5' : undefined">
                  <Skeleton class="h-4 w-full" />
                </TableCell>
              </TableRow>
            </template>
            <TableEmpty v-else-if="!visible.length" :colspan="8" text="没有符合条件的种子" />
            <TableRow v-for="t in visible" v-else :key="t.hash">
              <TableCell class="max-w-0 pl-5">
                <div class="truncate text-xs" :title="t.name">{{ t.name }}</div>
                <div class="text-muted-foreground tabular mt-0.5 font-mono text-[10px]">
                  {{ shortHash(t.hash, 10, 6) }}
                </div>
              </TableCell>
              <TableCell class="tabular text-muted-foreground text-right text-xs">
                {{ formatBytes(t.size) }}
              </TableCell>
              <TableCell class="tabular text-right text-xs">
                {{ formatPercent(t.progress) }}
              </TableCell>
              <TableCell class="text-xs">
                <span :class="phaseMeta(t).cls" :title="t.state">
                  {{ phaseMeta(t).label }}
                </span>
              </TableCell>
              <TableCell class="text-muted-foreground truncate text-xs">
                {{ t.category || '—' }}
              </TableCell>
              <TableCell class="tabular text-muted-foreground text-right text-xs">
                {{ formatDuration(t.seeding_time) }}
              </TableCell>
              <TableCell class="tabular text-right text-xs">
                {{ t.ratio ? t.ratio.toFixed(2) : '—' }}
              </TableCell>
              <TableCell class="max-w-0 pr-5">
                <div class="flex flex-wrap gap-1 overflow-hidden">
                  <span
                    v-for="tag in t.tags.slice(0, 2)"
                    :key="tag"
                    :class="cn(
                      'truncate rounded border px-1 py-0.5 text-[10px]',
                      tag.startsWith('iyuu-reseed')
                        ? 'border-status-active/40 text-status-active'
                        : 'text-muted-foreground',
                    )"
                    :title="tag"
                  >
                    {{ tag }}
                  </span>
                </div>
              </TableCell>
            </TableRow>
          </TableBody>
        </Table>
      </CardContent>
    </Card>
  </div>
</template>
