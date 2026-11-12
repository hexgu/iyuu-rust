<script setup lang="ts">
/**
 * 添加 / 编辑单个站点的凭据。
 *
 * 为什么是弹窗而不是常驻表单：站点是「按需添加」的，绝大多数站点用不到，
 * 把它们全部铺在页面上（旧版做法）等于让用户在上百行里找自己那一行，而且每次
 * 保存都要连带提交所有站点的字段。
 *
 * 关于空值：密钥类字段（passkey / cookie / api key …）服务端从不回显，所以表单
 * 永远是空的，空值必须表示「不修改」；服务端按这个语义合并。uid / user-agent
 * 会回显，空值就是真的清空。
 */
import { computed, reactive, ref, watch } from 'vue'
import { AlertCircle, Loader2, Save } from 'lucide-vue-next'
import { api, ApiError, type PublicCredentials, type SiteCredentials, type SiteInfo } from '@/lib/api'
import { cn } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Alert, AlertDescription } from '@/components/ui/alert'
import { Badge } from '@/components/ui/badge'
import { Dialog } from '@/components/ui/dialog'
import {
  credentialNeeds,
  FIELD_HINT,
  FIELD_LABEL,
  type CredentialField,
} from '@/lib/siteFields'

const props = defineProps<{
  open: boolean
  /** The site being edited; `null` while the list is still loading. */
  site: SiteInfo | null
  /** What is already stored for it (secrets are never included). */
  stored: PublicCredentials | undefined
  /** `true` when the site already has an entry — switches the wording. */
  editing: boolean
}>()

const emit = defineEmits<{
  'update:open': [boolean]
  /** The save succeeded; the new config is passed through so the page can sync. */
  saved: [config: unknown]
}>()

const form = reactive({
  passkey: '',
  torrent_key: '',
  api_key: '',
  rsskey: '',
  uid: '',
  cookie: '',
  user_agent: '',
})

const saving = ref(false)
const error = ref<string | null>(null)
const showAll = ref(false)

const need = computed(() => credentialNeeds(props.site))

/** The fields shown without expanding: the site's primary mechanism only. */
const primaryFields = computed<CredentialField[]>(() => [...need.value.primary])

const extraFields = computed<CredentialField[]>(() => {
  const out: CredentialField[] = []
  if (need.value.uid) out.push('uid')
  if (need.value.cookie) out.push('cookie')
  if (!need.value.uid) out.push('uid')
  if (!need.value.cookie) out.push('cookie')
  out.push('user_agent')
  return out
})

/** Re-seed whenever the dialog opens, so a cancelled edit never leaks in. */
watch(
  () => [props.open, props.site?.id] as const,
  ([open]) => {
    if (!open) return
    form.passkey = ''
    form.torrent_key = ''
    form.api_key = ''
    form.rsskey = ''
    form.cookie = ''
    form.uid = props.stored?.uid ?? ''
    form.user_agent = props.stored?.user_agent ?? ''
    // A site needing extras (cookie / uid) opens with them visible: hiding a
    // required field behind "更多" is how a save fails for no clear reason. So
    // does a site that already has a uid/UA stored — an edit that hides values
    // the user has set reads as if they were lost.
    showAll.value =
      need.value.uid ||
      need.value.cookie ||
      !!props.stored?.uid ||
      !!props.stored?.user_agent
    error.value = null
  },
  { immediate: true },
)

const visibleFields = computed<CredentialField[]>(() =>
  showAll.value
    ? [...new Set([...primaryFields.value, ...extraFields.value])]
    : primaryFields.value,
)

function placeholderFor(f: CredentialField): string {
  if (props.stored) {
    const has: Record<string, boolean> = {
      passkey: props.stored.has_passkey,
      torrent_key: props.stored.has_torrent_key,
      api_key: props.stored.has_api_key,
      rsskey: props.stored.has_rsskey,
      cookie: props.stored.has_cookie,
    }
    if (has[f]) return '已保存，留空表示不修改'
  }
  return FIELD_LABEL[f]
}

const isSecret = (f: CredentialField) =>
  f !== 'uid' && f !== 'user_agent'

/** At least one primary secret must be filled for a first-time add. */
const canSave = computed(() => {
  if (props.editing) return true
  return need.value.primary.some((f) => form[f].trim().length > 0) ||
    form.cookie.trim().length > 0
})

async function save() {
  if (!props.site) return
  saving.value = true
  error.value = null
  try {
    const body: SiteCredentials = {}
    // Secrets: only send what was typed, because a blank means "unchanged".
    if (form.passkey.trim()) body.passkey = form.passkey.trim()
    if (form.torrent_key.trim()) body.torrent_key = form.torrent_key.trim()
    if (form.api_key.trim()) body.api_key = form.api_key.trim()
    if (form.rsskey.trim()) body.rsskey = form.rsskey.trim()
    if (form.cookie.trim()) body.cookie = form.cookie.trim()
    // Identity: always send, so clearing a field actually clears it.
    body.uid = form.uid.trim()
    body.user_agent = form.user_agent.trim()

    const r = await api.putSiteCredentials(props.site.id, body)
    emit('saved', r.config)
    emit('update:open', false)
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e)
  } finally {
    saving.value = false
  }
}
</script>

<template>
  <Dialog
    :open="open"
    :title="editing ? '编辑站点凭据' : '添加站点'"
    :description="
      site
        ? `${site.nickname || site.site} · ${site.base_url}`
        : '站点元数据还没加载出来'
    "
    @update:open="emit('update:open', $event)"
  >
    <div class="space-y-4">
      <div v-if="site" class="flex flex-wrap items-center gap-1.5">
        <Badge variant="outline" class="font-mono text-[10px]">{{ site.site }}</Badge>
        <Badge v-if="site.reseed_check" variant="secondary" class="text-[10px]">
          需要 {{ site.reseed_check }}
        </Badge>
        <Badge v-if="site.cookie_required === 1" variant="secondary" class="text-[10px]">
          需要 cookie
        </Badge>
        <Badge
          v-if="site.is_https === 0"
          variant="secondary"
          class="text-[10px]"
          title="该站点只支持 http"
        >
          仅 http
        </Badge>
      </div>

      <div class="space-y-3">
        <div v-for="f in visibleFields" :key="f" class="space-y-1.5">
          <Label :for="`cred-${f}`" class="flex items-center gap-1.5">
            {{ FIELD_LABEL[f] }}
            <span
              v-if="need.primary.includes(f as never)"
              class="text-status-error text-[10px] font-normal"
            >必填</span>
          </Label>
          <Input
            :id="`cred-${f}`"
            v-model="form[f]"
            :type="isSecret(f) ? 'password' : 'text'"
            :placeholder="placeholderFor(f)"
            autocomplete="off"
            class="font-mono text-xs"
          />
          <p v-if="FIELD_HINT[f]" class="text-muted-foreground text-[11px]">
            {{ FIELD_HINT[f] }}
          </p>
        </div>
      </div>

      <button
        type="button"
        class="text-muted-foreground hover:text-foreground text-xs underline underline-offset-2"
        @click="showAll = !showAll"
      >
        {{ showAll ? '收起其他字段' : '其他字段（uid / cookie / UA）' }}
      </button>

      <p
        v-if="editing"
        class="text-muted-foreground text-[11px] leading-relaxed"
      >
        留空的密钥字段保持原值不变；uid / User-Agent 清空即删除。
      </p>

      <Alert v-if="error" variant="destructive">
        <AlertCircle />
        <AlertDescription>{{ error }}</AlertDescription>
      </Alert>

      <div class="flex items-center justify-end gap-2 pt-1">
        <Button variant="ghost" size="sm" @click="emit('update:open', false)">取消</Button>
        <Button
          size="sm"
          :disabled="saving || !canSave || !site"
          :class="cn(!canSave && 'pointer-events-none')"
          @click="save"
        >
          <Loader2 v-if="saving" class="animate-spin" />
          <Save v-else />
          保存
        </Button>
      </div>
    </div>
  </Dialog>
</template>
