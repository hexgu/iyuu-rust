<script setup lang="ts">
/**
 * 合作站点 / IYUU Token 激活。
 *
 * 这一块存在的理由是一个容易混淆的概念区别：
 *
 *   · `/reseed/sites/index`（**全部站点**）—— IYUU 已适配、可以往里辅种的站点。
 *     用来在「添加辅种站点」里挑目标站点。
 *   · `/reseed/sites/recommend`（**合作站点**）—— IYUU 的*合作* PT 站。
 *     用它自己的表述：用合作站点的账号信息，激活并绑定 IYUU 的 token。
 *
 * 后者不是「可以辅种的站点列表」，而是「能让 token 生效的站点列表」。
 * 绑一个合作站点账号（`/reseed/users/bind`）之后 token 才会被激活；
 * 在激活之前，连 `/reseed/sites/index` 都会返回 `403 用户未绑定推荐站点`。
 *
 * 所以这里只做两件事：列出可绑定的合作站点，以及代用户完成绑定。
 * 站点目录的刷新在「添加辅种站点」里，两者不要混在一起。
 */
import { computed, ref, reactive, watch } from 'vue'
import {
  AlertCircle,
  CheckCircle2,
  ExternalLink,
  KeyRound,
  Loader2,
  RefreshCw,
  ShieldCheck,
} from 'lucide-vue-next'
import {
  api,
  ApiError,
  type ActivationResult,
  type PartnerSite,
} from '@/lib/api'
import { cn } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Alert, AlertDescription } from '@/components/ui/alert'
import { Badge } from '@/components/ui/badge'
import { Skeleton } from '@/components/ui/skeleton'
import { Dialog } from '@/components/ui/dialog'

const props = defineProps<{
  /**
   * Whether a token is saved. Without one there is nothing to activate, and every
   * request here would fail on a missing credential — so the buttons stay disabled
   * rather than letting the user discover that through an error.
   */
  hasToken: boolean
}>()

const emit = defineEmits<{ 'quota-changed': [] }>()

// ------------------------------------------------------------- activation

/**
 * `null` = unknown. The point is that this is never assumed: an unactivated
 * token looks exactly like an empty site list otherwise, and the user would go
 * hunting for a bug in the wrong place.
 */
const activated = ref<boolean | null>(null)
const checking = ref(false)
const checkMsg = reactive<{ kind: 'ok' | 'err' | null; text: string }>({
  kind: null,
  text: '',
})

async function checkActivation() {
  if (!props.hasToken) return
  checking.value = true
  checkMsg.kind = null
  try {
    const r: ActivationResult = await api.checkActivation()
    activated.value = r.activated
    checkMsg.kind = r.activated ? 'ok' : 'err'
    checkMsg.text = r.message
    emit('quota-changed')
  } catch (e) {
    activated.value = null
    checkMsg.kind = 'err'
    checkMsg.text = e instanceof ApiError ? e.message : String(e)
  } finally {
    checking.value = false
  }
}

// ---------------------------------------------------------- partner list

const partners = ref<PartnerSite[]>([])
const loadingPartners = ref(false)
const partnersError = ref<string | null>(null)

/**
 * Loading this list costs one IYUU request, so it happens on demand — never on
 * mount. Presenting it as a button also makes the cost visible, which is the
 * honest way to show a metered API.
 */
async function loadPartners() {
  if (!props.hasToken) return
  loadingPartners.value = true
  partnersError.value = null
  try {
    const r = await api.partners()
    partners.value = r.partners
    emit('quota-changed')
  } catch (e) {
    partners.value = []
    partnersError.value = e instanceof ApiError ? e.message : String(e)
  } finally {
    loadingPartners.value = false
  }
}

const partnerQuery = ref('')
const filteredPartners = computed(() => {
  const q = partnerQuery.value.trim().toLowerCase()
  if (!q) return partners.value
  return partners.value.filter(
    (p) =>
      p.nickname.toLowerCase().includes(q) ||
      p.site.toLowerCase().includes(q) ||
      p.base_url.toLowerCase().includes(q),
  )
})

// -------------------------------------------------------------- bind form

const bindOpen = ref(false)
const binding = ref(false)
const bindTarget = ref<PartnerSite | null>(null)
const bindForm = reactive({ uid: '', passkey: '' })
const bindResult = reactive<{ kind: 'ok' | 'err' | null; text: string }>({
  kind: null,
  text: '',
})

function startBind(p: PartnerSite) {
  bindTarget.value = p
  bindForm.uid = ''
  bindForm.passkey = ''
  bindResult.kind = null
  bindResult.text = ''
  bindOpen.value = true
}

/** Reset the form whenever the dialog is dismissed, so a stale passkey can't linger. */
watch(bindOpen, (open) => {
  if (open) return
  bindForm.uid = ''
  bindForm.passkey = ''
})

const canBind = computed(
  () =>
    !!bindTarget.value &&
    bindForm.uid.trim().length > 0 &&
    bindForm.passkey.trim().length > 0 &&
    !binding.value,
)

async function submitBind() {
  if (!canBind.value || !bindTarget.value) return
  binding.value = true
  bindResult.kind = null
  bindResult.text = ''
  try {
    const r = await api.bindPartner({
      site: bindTarget.value.site,
      sid: bindTarget.value.id,
      uid: bindForm.uid.trim(),
      passkey: bindForm.passkey.trim(),
    })
    // Clear the secret from the form the moment it is no longer needed.
    bindForm.passkey = ''
    bindResult.kind = 'ok'
    bindResult.text = r.message
    if (r.activated !== null) activated.value = r.activated
    emit('quota-changed')
    bindOpen.value = false
    // A successful bind changes what the token may read, so the catalogue prompt
    // on the sites page is now actionable — nudge a re-check rather than assume.
    if (r.activated === true) {
      checkMsg.kind = 'ok'
      checkMsg.text = 'Token 已激活，可以前往「添加辅种站点」拉取站点目录'
    }
  } catch (e) {
    bindResult.kind = 'err'
    bindResult.text = e instanceof ApiError ? e.message : String(e)
    bindOpen.value = false
    checkMsg.kind = 'err'
    checkMsg.text = bindResult.text
  } finally {
    binding.value = false
  }
}

/** Where to find the uid / passkey on a NexusPHP site — the two things people ask. */
const helpUrl = 'https://doc.iyuu.cn/reference/users_bind'
</script>

<template>
  <div class="space-y-4">
    <!-- Activation state: the first thing to establish, because everything else
         depends on it. -->
    <div class="flex flex-wrap items-center gap-2">
      <Badge
        v-if="activated === true"
        variant="secondary"
        class="text-[10px] text-status-success"
      >
        <ShieldCheck class="mr-0.5 size-3" />
        已激活
      </Badge>
      <Badge v-else-if="activated === false" variant="secondary" class="text-[10px] text-status-warn">
        未激活
      </Badge>
      <Badge v-else variant="secondary" class="text-[10px]">激活状态未知</Badge>

      <Button
        variant="outline"
        size="sm"
        :disabled="!hasToken || checking"
        @click="checkActivation"
      >
        <Loader2 v-if="checking" class="animate-spin" />
        <ShieldCheck v-else />
        检查激活状态
      </Button>
      <span class="text-muted-foreground text-[10px]">消耗 1 次额度</span>
    </div>

    <p
      v-if="checkMsg.kind"
      :class="
        cn(
          'flex items-start gap-1 text-[11px] leading-relaxed',
          checkMsg.kind === 'ok' ? 'text-status-success' : 'text-status-error',
        )
      "
    >
      <CheckCircle2 v-if="checkMsg.kind === 'ok'" class="mt-0.5 size-3.5 shrink-0" />
      <AlertCircle v-else class="mt-0.5 size-3.5 shrink-0" />
      <span>{{ checkMsg.text }}</span>
    </p>

    <Alert v-if="!hasToken" variant="warning">
      <AlertCircle />
      <AlertDescription>
        先在上面的「IYUU 账号」里保存 Token，再回来绑定合作站点
      </AlertDescription>
    </Alert>

    <div v-else class="space-y-2">
      <div class="flex flex-wrap items-center gap-2">
        <Button variant="outline" size="sm" :disabled="loadingPartners" @click="loadPartners">
          <Loader2 v-if="loadingPartners" class="animate-spin" />
          <RefreshCw v-else />
          获取合作站点列表
        </Button>
        <span class="text-muted-foreground text-[10px]">消耗 1 次额度；列表变化不大</span>
      </div>

      <Alert v-if="partnersError" variant="destructive">
        <AlertCircle />
        <AlertDescription>{{ partnersError }}</AlertDescription>
      </Alert>

      <div v-if="loadingPartners && !partners.length" class="space-y-2">
        <Skeleton class="h-10 w-full" />
        <Skeleton class="h-10 w-full" />
      </div>

      <template v-else-if="partners.length">
        <Input v-model="partnerQuery" placeholder="搜索合作站点…" />

        <div class="max-h-72 divide-y overflow-y-auto rounded-md border scrollbar-slim">
          <div
            v-for="p in filteredPartners"
            :key="p.id"
            class="flex flex-wrap items-center gap-2 px-3 py-2"
          >
            <div class="min-w-0 flex-1">
              <div class="flex items-center gap-1.5">
                <span class="truncate text-xs font-medium">
                  {{ p.nickname || p.site }}
                </span>
                <Badge v-if="p.needs_uid" variant="secondary" class="text-[10px]">
                  需要 uid
                </Badge>
              </div>
              <div class="text-muted-foreground truncate font-mono text-[10px]">
                {{ p.id }} · {{ p.site }}
                <span v-if="p.base_url"> · {{ p.base_url }}</span>
              </div>
            </div>
            <Button variant="outline" size="sm" @click="startBind(p)">绑定</Button>
          </div>

          <div
            v-if="!filteredPartners.length"
            class="text-muted-foreground px-4 py-6 text-center text-xs"
          >
            没有匹配的合作站点
          </div>
        </div>
      </template>

      <div
        v-else
        class="text-muted-foreground rounded-md border border-dashed px-4 py-6 text-center text-[11px] leading-relaxed"
      >
        合作站点列表尚未获取。<br />
        点「获取合作站点列表」查看你的账号可以在哪些站点激活 Token。
      </div>
    </div>

    <!-- Bind form -->
    <Dialog
      v-model:open="bindOpen"
      title="绑定合作站点"
      :description="
        bindTarget
          ? `用你在这个站点的账号激活 IYUU Token：${bindTarget.nickname || bindTarget.site}`
          : ''
      "
      class="sm:max-w-md"
    >
      <div v-if="bindTarget" class="space-y-3">
        <div class="space-y-1.5">
          <Label>
            <KeyRound class="mr-1 inline size-3" />
            站点用户数字 ID
          </Label>
          <Input
            v-model="bindForm.uid"
            inputmode="numeric"
            autocomplete="off"
            placeholder="例如 114053"
          />
          <p class="text-muted-foreground text-[11px]">
            在站点个人页 URL 或「个人资料」里能看到，纯数字
          </p>
        </div>

        <div class="space-y-1.5">
          <Label>站点密钥（passkey）</Label>
          <Input
            v-model="bindForm.passkey"
            type="password"
            autocomplete="off"
            spellcheck="false"
            placeholder="站点设置里的 passkey"
          />
          <p class="text-muted-foreground text-[11px] leading-relaxed">
            只在本机做 sha1 后再发给 IYUU（IYUU 只存摘要，原始 passkey 不出本机）。
            一般填与「站点凭据」里同一个 passkey。
          </p>
        </div>

        <p
          v-if="bindResult.kind === 'err'"
          class="text-status-error flex items-start gap-1 text-[11px]"
        >
          <AlertCircle class="mt-0.5 size-3.5 shrink-0" />
          <span>{{ bindResult.text }}</span>
        </p>

        <div class="flex flex-wrap items-center justify-between gap-2 pt-0.5">
          <a
            :href="helpUrl"
            target="_blank"
            rel="noreferrer"
            class="text-muted-foreground hover:text-foreground inline-flex items-center gap-1 text-[11px] underline underline-offset-2"
          >
            <ExternalLink class="size-3" />
            IYUU 官方文档
          </a>
          <div class="flex items-center gap-2">
            <Button variant="ghost" size="sm" @click="bindOpen = false">取消</Button>
            <Button size="sm" :disabled="!canBind" @click="submitBind">
              <Loader2 v-if="binding" class="animate-spin" />
              <ShieldCheck v-else />
              绑定并激活
            </Button>
          </div>
        </div>
      </div>
    </Dialog>
  </div>
</template>
