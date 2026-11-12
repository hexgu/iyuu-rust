<script setup lang="ts">
/**
 * 已保存会员类型的展示 + 修改入口 + 额度进度。
 *
 * 这里不再做「选方案」的交互——选择发生在保存 Token 的弹窗里
 * （`TierSetupDialog`），因为那才是这个决定必须被做出的时刻。设置页只需要：
 *
 *  1. 说清楚现在是什么身份、额度多少；
 *  2. 给一个「修改」按钮重新走一次那个弹窗；
 *  3. 在额度快用完 / 已限流 / 未选择时给出可操作的提示。
 *
 * 服务器地址**不显示**：它来自环境变量，属于部署细节，浏览器不需要知道，
 * 也不该出现在截图里。
 */
import { computed } from 'vue'
import { AlertTriangle, Crown, ExternalLink, Info, Pencil, Zap } from 'lucide-vue-next'
import type { ApiTier, QuotaResponse } from '@/lib/api'
import { cn, formatDateTime } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'

const props = withDefaults(
  defineProps<{
    /** Saved tier, or null when the user has not answered yet. */
    tier: ApiTier | null
    quota?: QuotaResponse | null
    disabled?: boolean
  }>(),
  { quota: null, disabled: false },
)

const emit = defineEmits<{ edit: [] }>()

/** 购买/查看会员权益的官方页面。跳转，不经手。 */
const MEMBER_URL = 'https://iyuu.cn/app/user/member'

const isVip = computed(() => props.tier === 'vip')
const tierLabel = computed(() => (isVip.value ? 'SVIP 会员' : '普通用户'))

const blocked = computed(() => {
  const at = props.quota?.blocked_until
  return at && new Date(at).getTime() > Date.now() ? at : null
})
const exhausted = computed(() => (props.quota?.remaining_requests ?? 1) <= 0 && !blocked.value)
const tight = computed(() => {
  const p = props.quota?.percent_used ?? 0
  return p >= 90 && !blocked.value && !exhausted.value
})
const percent = computed(() => Math.min(100, Math.max(0, props.quota?.percent_used ?? 0)))
const barTone = computed(() => {
  if (blocked.value || exhausted.value) return 'bg-status-error'
  if (tight.value) return 'bg-status-warn'
  return 'bg-status-success'
})
</script>

<template>
  <div class="space-y-3">
    <!-- 未选择：把用户推向那个必须做的选择 -->
    <div
      v-if="!tier"
      class="border-status-warn/40 bg-status-warn/10 flex items-start gap-2 rounded-lg border p-3"
    >
      <AlertTriangle class="text-status-warn mt-0.5 size-4 shrink-0" />
      <div class="min-w-0 flex-1 space-y-1.5">
        <p class="text-xs font-medium">还没有选择会员类型</p>
        <p class="text-muted-foreground text-[11px] leading-relaxed">
          普通用户每天 500 次查询，SVIP 2500 次，额度相差 5 倍。
          这个信息无法从 Token 判断出来，只能由你确认，程序按你的选择安排查询额度。
        </p>
        <Button variant="outline" size="sm" :disabled="disabled" @click="emit('edit')">
          现在选择
        </Button>
      </div>
    </div>

    <!-- 已选择：身份 + 修改入口 -->
    <div v-else class="flex items-center justify-between gap-3 rounded-lg border p-3">
      <div class="flex min-w-0 items-center gap-2">
        <Crown v-if="isVip" class="text-status-warn size-4 shrink-0" />
        <Zap v-else class="size-4 shrink-0 opacity-70" />
        <div class="min-w-0">
          <div class="flex items-center gap-1.5">
            <span class="text-xs font-medium">{{ tierLabel }}</span>
            <Badge v-if="isVip" variant="secondary" class="text-[10px]">SVIP 额度</Badge>
          </div>
          <p class="text-muted-foreground mt-0.5 text-[11px]">
            {{ isVip ? '2500 次/天 · 约 50 万种子' : '500 次/天 · 约 10 万种子' }}
          </p>
        </div>
      </div>
      <Button variant="ghost" size="sm" :disabled="disabled" @click="emit('edit')">
        <Pencil />
        修改
      </Button>
    </div>

    <!-- 额度进度 -->
    <div v-if="quota && tier" class="rounded-lg border p-3">
      <div class="flex items-baseline justify-between gap-2">
        <div class="text-xs">
          <span class="font-medium">今日额度</span>
          <span class="text-muted-foreground ml-1.5">
            {{ quota.used_requests }} / {{ quota.daily_requests }} 次
          </span>
        </div>
        <span class="text-muted-foreground tabular text-[11px]">
          剩 {{ quota.remaining_requests }} 次
        </span>
      </div>

      <div class="bg-muted mt-2 h-1.5 overflow-hidden rounded-full">
        <div
          :class="cn('h-full rounded-full transition-[width]', barTone)"
          :style="{ width: `${Math.max(percent, 1)}%` }"
        />
      </div>

      <div
        class="text-muted-foreground mt-2 flex flex-wrap items-center justify-between gap-x-3 gap-y-1 text-[11px]"
      >
        <span>
          种子覆盖：{{ quota.used_hashes.toLocaleString() }} /
          {{ quota.daily_hashes.toLocaleString() }}
        </span>
        <span v-if="quota.reset_at">重置于 {{ formatDateTime(quota.reset_at) }}</span>
      </div>

      <div v-if="blocked" class="text-status-error mt-2 flex items-start gap-1.5 text-[11px]">
        <AlertTriangle class="mt-px size-3.5 shrink-0" />
        <span>IYUU 已限流，暂不发送请求（至 {{ formatDateTime(blocked) }} 解除）。</span>
      </div>
      <div
        v-else-if="exhausted"
        class="text-status-error mt-2 flex items-start gap-1.5 text-[11px]"
      >
        <AlertTriangle class="mt-px size-3.5 shrink-0" />
        <span>
          今日额度已用尽，下次运行会直接失败而不是空手而归。
          <template v-if="quota.reset_at">{{ formatDateTime(quota.reset_at) }} 后重置。</template>
        </span>
      </div>
      <div v-else-if="tight" class="text-status-warn mt-2 flex items-start gap-1.5 text-[11px]">
        <AlertTriangle class="mt-px size-3.5 shrink-0" />
        <span>剩余额度可能不足以覆盖全部种子，本次运行会被自动截断。</span>
      </div>
      <div
        v-else-if="quota.denied_requests > 0"
        class="text-muted-foreground mt-2 flex items-start gap-1.5 text-[11px]"
      >
        <Info class="mt-px size-3.5 shrink-0" />
        <span>本次有 {{ quota.denied_requests }} 个请求因额度不足被跳过。</span>
      </div>
    </div>

    <!-- 升级提示：只在普通用户身上出现 -->
    <div v-if="tier === 'free'" class="border-status-warn/30 bg-status-warn/5 rounded-lg border p-3">
      <div class="flex items-start gap-2">
        <Crown class="text-status-warn mt-0.5 size-4 shrink-0" />
        <div class="min-w-0 flex-1 space-y-1.5">
          <p class="text-xs font-medium">额度不够用？IYUU 的 SVIP 提供更强的辅种能力</p>
          <ul class="text-muted-foreground space-y-0.5 text-[11px] leading-relaxed">
            <li>
              · 辅种接口 <span class="text-foreground font-medium">2500 次/天</span>，每天最多 50
              万种子（普通用户 500 次 / 10 万）
            </li>
            <li>
              · 会员专享入口，与免费请求分流
            </li>
            <li>· 超量免封禁、SVIP 身份徽章、模板消息 2400 条/天</li>
            <li>· 邀请好友得推荐佣金、免费技术支持</li>
          </ul>
          <div class="flex flex-wrap items-center gap-2 pt-0.5">
            <Button as="a" :href="MEMBER_URL" target="_blank" rel="noreferrer" variant="outline" size="sm">
              <ExternalLink />
              前往 IYUU 开通 / 了解 SVIP
            </Button>
            <span class="text-muted-foreground text-[10px]">
              开通后点「修改」把类型改成 SVIP 会员
            </span>
          </div>
        </div>
      </div>
    </div>

    <!-- 已经是会员：指向会员中心而不是再劝一次 -->
    <div v-else-if="tier === 'vip'" class="text-muted-foreground flex items-start gap-1.5 text-[11px]">
      <Crown class="text-status-warn mt-px size-3.5 shrink-0" />
      <span>
        按 SVIP 的 2500 次/天安排查询。若运行日志里出现限流提示，说明实际额度低于此，
        请先确认会员是否仍在有效期，可前往
        <a :href="MEMBER_URL" target="_blank" rel="noreferrer" class="underline underline-offset-2">
          IYUU 会员中心 </a>
        查看或续费。
      </span>
    </div>

    <p class="text-muted-foreground flex items-start gap-1.5 text-[10px] leading-relaxed">
      <Info class="mt-px size-3 shrink-0" />
      <span>
        本项目是基于 IYUU 开放 API 的第三方工具，<span class="text-foreground">并非 IYUU 官方项目</span>，
        与 IYUU 无隶属关系。会员开通、额度策略、接口变更均由 IYUU 决定，
        本项目不承担任何售后、计费或连带责任；是否开通请自行判断。
      </span>
    </p>
  </div>
</template>
