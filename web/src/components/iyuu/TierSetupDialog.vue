<script setup lang="ts">
/**
 * 保存 IYUU Token 时的会员类型选择弹窗。
 *
 * 为什么必须显式选，而不是给个默认值：会员类型直接决定程序按多少额度排预算
 * （500 次/天 还是 2500 次/天）。默认成「普通用户」会让付费用户莫名其妙卡在
 * 500；默认成 SVIP 会让免费用户按 2500 去排，然后把临时限流撞成更长的封禁。
 * 所以没有默认值。
 *
 * 为什么**不在这里探测服务器**：两个域名后面是同一套服务，探测根本区分不出
 * 会员身份——能返回站点列表不代表你有会员，返回失败也不代表你不是会员。
 * 探测只会白耗额度并给出误导性结论。所以这里只做两件事：
 *   1. 强制用户明确表态；
 *   2. 选 SVIP 时要求一次显式确认，并说明选错的后果（额度按 2500 算，
 *      真超了会被 IYUU 那边拒绝）。
 */
import { computed, ref, watch } from 'vue'
import { AlertTriangle, CheckCircle2, Crown, ExternalLink, Zap } from 'lucide-vue-next'
import type { ApiTier } from '@/lib/api'
import { cn } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Dialog } from '@/components/ui/dialog'
import { Badge } from '@/components/ui/badge'

const props = defineProps<{
  open: boolean
  /** Token typed in the form. Empty when the user only re-picks a tier. */
  token: string
  /** Tier already saved, if any (re-open case). */
  currentTier: ApiTier | null
}>()

const emit = defineEmits<{
  'update:open': [boolean]
  /** The user has explicitly committed to a tier. */
  confirm: [payload: { tier: ApiTier; token: string | null }]
}>()

const MEMBER_URL = 'https://iyuu.cn/app/user/member'

/** `null` until the user actually clicks a plan — no default on purpose. */
const picked = ref<ApiTier | null>(null)
/** VIP additionally requires an explicit acknowledgement. */
const acked = ref(false)

const plans = [
  {
    id: 'free' as ApiTier,
    label: '普通用户',
    requests: 500,
    blurb: '每天 500 次查询，约覆盖 10 万种子',
  },
  {
    id: 'vip' as ApiTier,
    label: 'SVIP 会员',
    requests: 2500,
    blurb: '每天 2500 次查询，约覆盖 50 万种子',
  },
]

watch(
  () => props.open,
  (open) => {
    if (!open) return
    // Reset on every open: a stale acknowledgement from a previous session must
    // not let a VIP choice through without being confirmed again.
    picked.value = props.currentTier
    acked.value = false
  },
)

function pick(id: ApiTier) {
  if (picked.value === id) return
  picked.value = id
  acked.value = false
}

/**
 * Save gate.
 *
 * Free needs nothing extra. VIP needs the acknowledgement, because the cost of a
 * wrong "yes" is real: the app would budget 2500/day that the account does not
 * have, and IYUU's own limiter would then reject runs.
 */
const canConfirm = computed(() => {
  if (!picked.value) return false
  if (picked.value === 'vip') return acked.value
  return true
})

function confirm() {
  if (!picked.value || !canConfirm.value) return
  emit('confirm', { tier: picked.value, token: props.token.trim() || null })
  emit('update:open', false)
}

function openMember() {
  window.open(MEMBER_URL, '_blank', 'noreferrer')
}

const pickedLabel = computed(() => (picked.value === 'vip' ? 'SVIP' : '普通'))
</script>

<template>
  <Dialog
    :open="open"
    :persistent="true"
    title="选择你的 IYUU 会员类型"
    description="会员类型决定程序按多少额度来安排查询：普通用户 500 次/天，SVIP 2500 次/天。这个信息无法从 Token 判断，只能由你确认。"
    @update:open="emit('update:open', $event)"
  >
    <div class="space-y-3">
      <!-- Plan cards: no pre-selected default -->
      <div class="grid gap-2">
        <button
          v-for="p in plans"
          :key="p.id"
          type="button"
          :aria-pressed="picked === p.id"
          :class="
            cn(
              'rounded-lg border p-3 text-left transition-colors',
              picked === p.id
                ? 'border-foreground/50 bg-secondary/70'
                : 'hover:border-foreground/25 hover:bg-accent/40',
            )
          "
          @click="pick(p.id)"
        >
          <div class="flex items-center gap-1.5">
            <Crown v-if="p.id === 'vip'" class="text-status-warn size-3.5" />
            <Zap v-else class="size-3.5 opacity-70" />
            <span class="text-xs font-medium">{{ p.label }}</span>
            <Badge v-if="picked === p.id" variant="secondary" class="ml-auto text-[10px]">
              已选择
            </Badge>
          </div>
          <p class="text-muted-foreground mt-1 text-[11px]">{{ p.blurb }}</p>
        </button>
      </div>

      <!-- Nothing picked yet: say why it is not optional -->
      <p v-if="!picked" class="text-status-warn flex items-start gap-1.5 text-[11px]">
        <AlertTriangle class="mt-px size-3.5 shrink-0" />
        <span>必须选择一项才能保存。不确定的话选「普通用户」，之后可以在设置里改。</span>
      </p>

      <!-- 选普通用户：给 VIP 购买入口 -->
      <div
        v-if="picked === 'free'"
        class="border-status-warn/30 bg-status-warn/5 rounded-lg border p-3"
      >
        <div class="flex items-start gap-2">
          <Crown class="text-status-warn mt-0.5 size-4 shrink-0" />
          <div class="min-w-0 flex-1 space-y-1.5">
            <p class="text-xs font-medium">种子多、额度不够用？IYUU SVIP 额度是 5 倍</p>
            <ul class="text-muted-foreground space-y-0.5 text-[11px] leading-relaxed">
              <li>· 辅种查询 2500 次/天（普通 500 次），每天最多 50 万种子</li>
              <li>· 超量免封禁、SVIP 徽章、模板消息 2400 条/天、邀请返佣、免费技术支持</li>
            </ul>
            <div class="flex flex-wrap items-center gap-2 pt-0.5">
              <Button variant="outline" size="sm" @click="openMember">
                <ExternalLink />
                前往 IYUU 开通 SVIP
              </Button>
              <span class="text-muted-foreground text-[10px]">
                开通后回到这里把类型改成「SVIP 会员」
              </span>
            </div>
          </div>
        </div>
      </div>

      <!-- 选 SVIP：只做确认，不探测 -->
      <div v-if="picked === 'vip'" class="space-y-2.5">
        <div class="border-status-warn/40 bg-status-warn/10 rounded-lg border p-3">
          <div class="flex items-start gap-2">
            <AlertTriangle class="text-status-warn mt-0.5 size-4 shrink-0" />
            <p class="text-[11px] leading-relaxed">
              <span class="font-medium">请确认你确实持有 SVIP 会员。</span>
              选 SVIP 后，程序会按 <span class="font-medium">2500 次/天</span> 安排查询；
              如果账号其实不是会员，超出实际额度的请求会被 IYUU 拒绝，运行日志里会显示限流。
            </p>
          </div>
        </div>

        <label
          :class="
            cn(
              'flex cursor-pointer items-start gap-2 rounded-lg border p-3 transition-colors',
              acked ? 'border-foreground/40 bg-secondary/60' : 'hover:bg-accent/40',
            )
          "
        >
          <input
            v-model="acked"
            type="checkbox"
            class="accent-primary mt-0.5 size-3.5 shrink-0 cursor-pointer"
          />
          <span class="text-[11px] leading-relaxed">
            我确认自己是 IYUU 的 <span class="font-medium">SVIP 会员</span>，
            并理解选错会导致查询请求被 IYUU 拒绝。
          </span>
        </label>

        <p class="text-muted-foreground text-[11px]">
          如果还没开通，可以先去
          <button type="button" class="underline underline-offset-2" @click="openMember">
            IYUU 会员中心
          </button>
          开通，或
          <button type="button" class="underline underline-offset-2" @click="pick('free')">
            先按普通用户保存
          </button>
          （之后随时能改）。
        </p>
      </div>

      <!-- 免责声明：任何会员相关的界面都要有 -->
      <p class="text-muted-foreground border-t pt-2.5 text-[10px] leading-relaxed">
        本项目是基于 IYUU 开放 API 的第三方工具，
        <span class="text-foreground">并非 IYUU 官方项目</span>，与 IYUU 无隶属关系；
        会员开通、额度策略与接口变更由 IYUU 决定，本项目不承担售后与连带责任。
        会员链接直接跳转 IYUU 官方页面，本项目不经手任何费用。
      </p>

      <div class="flex items-center justify-end gap-2 pt-0.5">
        <Button variant="ghost" size="sm" @click="emit('update:open', false)">取消</Button>
        <Button size="sm" :disabled="!canConfirm" @click="confirm">
          <CheckCircle2 v-if="canConfirm" />
          保存
          <span v-if="picked" class="text-primary-foreground/70 ml-1 text-[10px]">
            （{{ pickedLabel }}）
          </span>
        </Button>
      </div>
    </div>
  </Dialog>
</template>
