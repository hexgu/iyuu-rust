<script setup lang="ts">
import { computed } from 'vue'
import type { ReseedState } from '@/lib/api'
import { cn } from '@/lib/utils'

const props = defineProps<{ state: ReseedState; size?: 'sm' | 'md' }>()

/**
 * One place that decides how a state looks and reads.
 *
 * The critical distinction the UI must make obvious: `added` is NOT success.
 * A cross-seed that has been handed to the downloader but has not passed
 * verification is still unknown, and showing it as green is exactly the
 * reporting bug that hides hundreds of broken torrents.
 */
const META: Record<ReseedState, { label: string; cls: string; hint: string }> = {
  pending: {
    label: '待处理',
    cls: 'border-status-pending/40 text-status-pending bg-status-pending/10',
    hint: '已发现，尚未处理',
  },
  skipped: {
    label: '跳过',
    cls: 'border-border text-muted-foreground bg-muted/40',
    hint: '本地过滤或已存在，未发起请求',
  },
  fetching: {
    label: '获取种子',
    cls: 'border-status-active/40 text-status-active bg-status-active/10',
    hint: '正在从目标站点下载种子文件',
  },
  added: {
    label: '已添加·待校验',
    cls: 'border-status-warn/50 text-status-warn bg-status-warn/10',
    hint: '已交给下载器，等待校验结果（尚未成功）',
  },
  verifying: {
    label: '校验中',
    cls: 'border-status-active/50 text-status-active bg-status-active/10',
    hint: '下载器正在校验数据，请勿干预',
  },
  seeding: {
    label: '辅种中',
    cls: 'border-status-success/50 text-status-success bg-status-success/10',
    hint: '校验通过，数据完整，已开始辅种',
  },
  verify_failed: {
    label: '校验不通过',
    cls: 'border-status-error/50 text-status-error bg-status-error/10',
    hint: '校验完成但数据不完整：保存路径与原种不一致，或资源不同',
  },
  missing_files: {
    label: '文件缺失',
    cls: 'border-status-error/50 text-status-error bg-status-error/10',
    hint: '下载器找不到数据文件',
  },
  fetch_failed: {
    label: '获取失败',
    cls: 'border-status-error/40 text-status-error bg-status-error/10',
    hint: '站点拒绝或种子无效（cookie/passkey 失效？）',
  },
  add_failed: {
    label: '添加失败',
    cls: 'border-status-error/40 text-status-error bg-status-error/10',
    hint: '下载器拒绝了该种子',
  },
  timed_out: {
    label: '校验超时',
    cls: 'border-status-warn/40 text-status-warn bg-status-warn/10',
    hint: '超过校验时限仍未完成',
  },
}

const meta = computed(() => META[props.state] ?? META.pending)
</script>

<template>
  <span
    :title="meta.hint"
    :class="cn(
      'inline-flex items-center gap-1.5 rounded-md border font-medium whitespace-nowrap',
      size === 'sm' ? 'px-1.5 py-0.5 text-[11px]' : 'px-2 py-0.5 text-xs',
      meta.cls,
    )"
  >
    <span
      :class="cn(
        'size-1.5 rounded-full',
        state === 'seeding' && 'bg-status-success',
        state === 'verifying' && 'bg-status-active animate-pulse',
        state === 'added' && 'bg-status-warn animate-pulse',
        (state === 'verify_failed' || state === 'missing_files' || state === 'fetch_failed' || state === 'add_failed') && 'bg-status-error',
        state === 'timed_out' && 'bg-status-warn',
        state === 'pending' && 'bg-status-pending',
        state === 'skipped' && 'bg-muted-foreground',
        state === 'fetching' && 'bg-status-active',
      )"
    />
    {{ meta.label }}
  </span>
</template>
