<script setup lang="ts">
import { computed, onMounted, onUnmounted, reactive, ref, watch } from 'vue'
import {
  AlertCircle,
  CheckCircle2,
  Download,
  KeyRound,
  Loader2,
  Server,
  ShieldCheck,
  Timer,
  Zap,
} from 'lucide-vue-next'
import {
  api,
  ApiError,
  type ApiTier,
  type DownloaderAuthMode,
  type PublicConfig,
  type QuotaResponse,
} from '@/lib/api'
import { cn } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Switch } from '@/components/ui/switch'
import { Textarea } from '@/components/ui/textarea'
import { Separator } from '@/components/ui/separator'
import { Skeleton } from '@/components/ui/skeleton'
import TierPicker from '@/components/iyuu/TierPicker.vue'
import TierSetupDialog from '@/components/iyuu/TierSetupDialog.vue'
import PartnerBindCard from '@/components/iyuu/PartnerBindCard.vue'

const cfg = ref<PublicConfig | null>(null)
const loading = ref(true)

// ---- 自动保存状态 ------------------------------------------------------
//
// 这个页面没有「保存」按钮：改了就算数。文本框停下 600ms 写一次，开关和下拉也一样
// ——请求本身是合并的，所以多写一次不会覆盖别的字段。
//
// 两种情况不进防抖队列：
//   * 密文（密码 / API Key）：打字停顿的那一下就会把半截密码写进库，而且成功后字段
//     要被清空，正好把用户还没敲完的密码抹掉。它们改为失焦时提交。
//   * Token：它必须和会员类型一起提交（类型决定打哪台服务器），所以单独走弹窗。
type SaveState = 'idle' | 'saving' | 'saved' | 'error'
const saveState = ref<SaveState>('idle')
const saveError = ref('')
const saving = computed(() => saveState.value === 'saving')

// ---- local form state, seeded from the server config -------------------
const form = reactive({
  token: '',
  dlKind: 'qbittorrent',
  dlName: '',
  dlUrl: '',
  dlAuth: 'password' as DownloaderAuthMode,
  dlUser: '',
  dlPass: '',
  dlApiKey: '',
  rpcPath: '',
  minSize: '0',
  excludeTags: '',
  excludePaths: '',
  tags: '',
  reuseCategory: true,
  defaultCategory: '',
  tagWithSite: true,
  autoStart: true,
  concurrency: '2',
  verifyInterval: '15',
  scheduleEnabled: false,
  cron: '0 4 * * *',
})


const testingIyuu = ref(false)
const iyuuResult = reactive<{ ok: boolean | null; text: string }>({ ok: null, text: '' })
const testingDl = ref(false)
const dlResult = reactive<{ ok: boolean | null; text: string }>({ ok: null, text: '' })

function seedForm(c: PublicConfig) {
  form.dlKind = c.downloader.kind
  form.dlName = c.downloader.name
  form.dlUrl = c.downloader.base_url
  form.dlAuth = c.downloader.auth
  form.dlUser = c.downloader.username
  form.dlPass = '' // never returned by the API
  form.dlApiKey = '' // neither is the key
  form.rpcPath = c.downloader.extra.transmission_rpc_path ?? ''
  form.minSize = String(c.reseed.min_size_gib)
  form.excludeTags = c.reseed.exclude_tags.join(', ')
  form.excludePaths = c.reseed.exclude_paths.join('\n')
  form.tags = c.reseed.tags.join(', ')
  form.reuseCategory = c.reseed.reuse_category
  form.defaultCategory = c.reseed.default_category ?? ''
  form.tagWithSite = c.reseed.tag_with_site
  form.autoStart = c.reseed.auto_start_after_verify
  form.concurrency = String(c.reseed.fetch_concurrency)
  form.verifyInterval = String(c.reseed.verify_interval_secs)
  form.scheduleEnabled = c.schedule.enabled
  form.cron = c.schedule.cron
  form.token = '' // write-only
  rememberSaved()
}

// ---- 会员类型选择 -------------------------------------------------------
//
// The choice happens in a dialog, once, at the moment a token is saved. It is
// not a form field because it is not a preference: it selects the server the
// token is valid against, and it cannot be inferred.
const tierDialogOpen = ref(false)

function openTierDialog() {
  tierDialogOpen.value = true
}

/**
 * A new token cannot be saved without a tier: the tier decides which server the
 * token is valid against, and guessing one is how a paying user silently ends up
 * on the free host. That is the one place auto-save stops and asks — everything
 * else in the form writes itself.
 */
function submitToken() {
  if (!form.token.trim()) return
  openTierDialog()
}

/** The dialog answered: store the token together with the tier it was picked for. */
async function onTierConfirmed(payload: { tier: ApiTier; token: string | null }) {
  if (form.token.trim()) {
    await saveToken(payload.tier)
    return
  }
  // Re-picking a tier on an already-saved token: patch just that field.
  saveState.value = 'saving'
  saveError.value = ''
  try {
    const r = await api.putConfig({ iyuu: { tier: payload.tier } })
    cfg.value = r.config
    flashSaved()
    await loadQuota()
  } catch (e) {
    failSave(e)
  }
}

async function saveToken(tier: ApiTier) {
  const token = form.token.trim()
  if (!token) return
  saveState.value = 'saving'
  saveError.value = ''
  try {
    const r = await api.putConfig({ iyuu: { token, tier } })
    cfg.value = r.config
    // The token is stored now; keep the field blank so nothing re-sends it.
    form.token = ''
    flashSaved()
    await loadQuota()
    // Anything edited before the token existed was held back; it can go now.
    void flushSave()
  } catch (e) {
    failSave(e)
  }
}

/** Live quota, so the picker shows the account's real position, not the plan's. */
const quota = ref<QuotaResponse | null>(null)

async function loadQuota() {
  try {
    quota.value = await api.quota()
  } catch {
    // Quota display is informational; a failure here must not break the page.
    quota.value = null
  }
}

async function loadConfig() {
  loading.value = true
  try {
    cfg.value = await api.getConfig()
    seedForm(cfg.value)
    saveState.value = 'idle'
    saveError.value = ''
  } catch (e) {
    failSave(e)
  } finally {
    loading.value = false
  }
}

onMounted(async () => {
  await loadConfig()
  loadQuota()
})

onUnmounted(() => {
  if (saveTimer) clearTimeout(saveTimer)
  if (idleTimer) clearTimeout(idleTimer)
})

function splitList(v: string): string[] {
  return v
    .split(/[,\n]/)
    .map((s) => s.trim())
    .filter(Boolean)
}

// ---- 表单 → 请求体 -----------------------------------------------------

function downloaderSection() {
  return {
    kind: form.dlKind,
    name: form.dlName,
    base_url: form.dlUrl,
    auth: form.dlAuth,
    username: form.dlUser,
    extra: { transmission_rpc_path: form.rpcPath || null },
  }
}

function reseedSection() {
  return {
    min_size_gib: Number(form.minSize) || 0,
    exclude_tags: splitList(form.excludeTags),
    exclude_paths: splitList(form.excludePaths),
    reuse_category: form.reuseCategory,
    default_category: form.defaultCategory || null,
    tags: splitList(form.tags),
    tag_with_site: form.tagWithSite,
    auto_start_after_verify: form.autoStart,
    fetch_concurrency: Number(form.concurrency) || 2,
    verify_interval_secs: Number(form.verifyInterval) || 15,
  }
}

function scheduleSection() {
  return { enabled: form.scheduleEnabled, cron: form.cron }
}

type SectionName = 'downloader' | 'reseed' | 'schedule'

const SECTIONS: Record<SectionName, () => unknown> = {
  downloader: downloaderSection,
  reseed: reseedSection,
  schedule: scheduleSection,
}

const SECTION_NAMES = Object.keys(SECTIONS) as SectionName[]

/**
 * `reseed` and `schedule` are replaced wholesale server-side, `downloader` is
 * merged field by field. Sending the whole section would be correct either way,
 * but diffing first means changing a tag does not rewrite the entire config —
 * which matters once two tabs have the page open.
 */
function currentSnapshot(): Record<SectionName, string> {
  return Object.fromEntries(
    SECTION_NAMES.map((n) => [n, JSON.stringify(SECTIONS[n]())]),
  ) as Record<SectionName, string>
}

/** What the server is known to hold — the baseline "has this changed?" is measured against. */
let savedSnapshot = currentSnapshot()

function rememberSaved() {
  savedSnapshot = currentSnapshot()
}

function changedSections(): SectionName[] {
  const cur = currentSnapshot()
  return SECTION_NAMES.filter((n) => cur[n] !== savedSnapshot[n])
}

/**
 * The server rejects any config whose IYUU token is empty, so on a fresh install
 * every keystroke would paint a red error on a field that is perfectly fine. Until
 * a token exists, section edits are held instead of sent — `saveToken` flushes
 * them the moment the token lands.
 */
function tokenPresent(): boolean {
  // The *stored* token, not the one being typed: a half-typed token cannot make
  // the server accept anything, so section edits wait for the real save.
  return cfg.value?.iyuu.has_token === true
}

/**
 * Some states are "not typed yet", not "typed wrong": an address caught in the
 * middle of being retyped, an API key not pasted yet, a cron with three fields so
 * far. The server rejects all three outright, and flashing a red error at someone
 * who is still typing is worse than sending nothing — so those sections wait for
 * the next flush.
 */
function isSendable(name: SectionName): boolean {
  if (name === 'downloader') {
    if (form.dlUrl.trim() === '') return false
    if (
      form.dlAuth === 'api_key' &&
      form.dlKind === 'qbittorrent' &&
      !cfg.value?.downloader.has_api_key &&
      form.dlApiKey.trim() === ''
    ) {
      return false
    }
  }
  if (name === 'schedule' && form.scheduleEnabled) {
    return form.cron.trim().split(/\s+/).length === 5
  }
  return true
}

/**
 * Secrets are committed on blur, not by the debounce: a pause mid-typing would
 * otherwise store half a password, and clearing the field after the save would
 * eat the rest of what the user was typing. `secretDirty` says "these fields have
 * content that still needs to go out", so a retry after a failure still carries it.
 */
let secretDirty = false
let inFlight = false
let queued = false
let saveTimer: ReturnType<typeof setTimeout> | undefined
let idleTimer: ReturnType<typeof setTimeout> | undefined

function flashSaved() {
  saveState.value = 'saved'
  if (idleTimer) clearTimeout(idleTimer)
  // Back to the quiet hint after a beat, so "已保存" does not sit there forever.
  idleTimer = setTimeout(() => {
    if (saveState.value === 'saved') saveState.value = 'idle'
  }, 2000)
}

function failSave(e: unknown) {
  saveState.value = 'error'
  saveError.value = e instanceof ApiError ? e.message : String(e)
}

/**
 * The single write path. Safe to call concurrently: a call that arrives while a
 * request is in flight only raises a flag, and the loop replays once it lands, so
 * the last thing the user typed is always the last thing written.
 */
async function flushSave() {
  if (loading.value) return
  const cur = currentSnapshot()
  const dirty = tokenPresent() ? changedSections().filter(isSendable) : []
  const hasSecret = secretDirty && (form.dlPass !== '' || form.dlApiKey !== '')
  if (dirty.length === 0 && !hasSecret) {
    if (saveState.value === 'saving') return
    saveState.value = saveState.value === 'error' ? 'error' : 'idle'
    return
  }
  if (inFlight) {
    queued = true
    return
  }
  inFlight = true
  saveState.value = 'saving'
  saveError.value = ''

  const patch: Record<string, unknown> = {}
  for (const name of dirty) patch[name] = SECTIONS[name]()
  if (hasSecret) {
    // Blank means "unchanged" server-side, so only the filled-in ones go out.
    const dl = { ...(patch.downloader as Record<string, unknown> | undefined) }
    if (form.dlPass) dl.password = form.dlPass
    if (form.dlApiKey) dl.api_key = form.dlApiKey
    patch.downloader = dl
  }

  try {
    const r = await api.putConfig(patch)
    cfg.value = r.config
    // Only the sections that were actually sent are now stored. Marking the rest
    // would make a section that was skipped as "not typed yet" look saved and it
    // would never be sent again.
    const applied: Record<SectionName, string> = { ...savedSnapshot }
    for (const name of dirty) applied[name] = cur[name]
    savedSnapshot = applied
    // Do not re-seed the form: the user may already be typing the next field, and
    // writing the server copy back would reset the caret and clobber that input.
    if (hasSecret) {
      secretDirty = false
      form.dlPass = ''
      form.dlApiKey = ''
    }
    flashSaved()
  } catch (e) {
    failSave(e)
  } finally {
    inFlight = false
    if (queued) {
      queued = false
      void flushSave()
    }
  }
}

const AUTO_SAVE_DEBOUNCE_MS = 600

function scheduleSave() {
  if (loading.value) return
  if (saveTimer) clearTimeout(saveTimer)
  saveTimer = setTimeout(() => {
    saveTimer = undefined
    void flushSave()
  }, AUTO_SAVE_DEBOUNCE_MS)
}

/**
 * Commit whatever is in the two masked fields. Called on blur and on Enter — the
 * only moments we can tell the value is finished.
 */
function flushSecrets() {
  if (form.dlPass === '' && form.dlApiKey === '') return
  secretDirty = true
  void flushSave()
}

// Everything but the token: the token needs its tier, so it goes through the
// dialog (see `submitToken`).
watch(form, scheduleSave, { deep: true })

/**
 * Activation state learned from the last probe, so the 合作站点 card can show it
 * without spending a request of its own on mount.
 */
const activationKnown = ref<boolean | null>(null)

/**
 * Test against the saved tier. There is no tier selector in the form anymore, so
 * this only proves "the token I already saved still works"; choosing a server is
 * the dialog's job, and it probes before it saves.
 *
 * One request: `/reseed/sites/index` is the only endpoint whose answer separates
 * "token rejected" from "token valid but not activated". Probing
 * `/sites/recommend` (as this used to) proved nothing — it answers for unbound
 * tokens too, which is why an inactive setup used to report success.
 */
async function testIyuu() {
  testingIyuu.value = true
  iyuuResult.ok = null
  iyuuResult.text = ''
  try {
    const r = await api.testIyuu({})
    // The server distinguishes "token invalid" (throws) from "token valid but not
    // activated" (`ok: false`), because the fix differs: one is a typo, the other
    // is a bind step. Collapsing them told users to check a token that was fine.
    iyuuResult.ok = r.ok
    iyuuResult.text = r.message
    // Reflect activation in the badge above without a second request.
    if (r.activated === false) activationKnown.value = false
    else if (r.activated) activationKnown.value = true
    quota.value = null
    await loadQuota()
  } catch (e) {
    iyuuResult.ok = false
    if (e instanceof ApiError && e.kind === 'rate_limited') {
      iyuuResult.text = `${e.message}（等额度重置后再试）`
    } else {
      iyuuResult.text = e instanceof ApiError ? e.message : String(e)
    }
    loadQuota()
  } finally {
    testingIyuu.value = false
  }
}

async function testDownloader() {
  testingDl.value = true
  dlResult.ok = null
  dlResult.text = ''
  try {
    const r = await api.testDownloader({
      kind: form.dlKind,
      base_url: form.dlUrl,
      auth: form.dlAuth,
      username: form.dlUser,
      password: form.dlPass || undefined,
      api_key: form.dlApiKey || undefined,
      rpc_path: form.rpcPath || undefined,
    })
    dlResult.ok = true
    const via = r.auth === 'api_key' ? '（API Key）' : ''
    dlResult.text = `连接成功：${r.kind} ${r.version}${via}，共 ${r.torrent_count} 个种子（已完成 ${r.complete_count}）`
  } catch (e) {
    dlResult.ok = false
    dlResult.text = e instanceof ApiError ? e.message : String(e)
  } finally {
    testingDl.value = false
  }
}

const dlOptions = [
  { value: 'qbittorrent', label: 'qBittorrent' },
  { value: 'transmission', label: 'Transmission' },
]

// Transmission has no API-key auth. Switching to it silently keeps the field at
// `api_key` would make 保存 fail with a server-side error the user cannot see a
// cause for, so fall back to password auth the moment the type changes.
watch(
  () => form.dlKind,
  (kind) => {
    if (kind !== 'qbittorrent') form.dlAuth = 'password'
  },
)

/** How many sites are already added — a summary only, the list lives on /sites. */
const addedSiteCount = computed(() => Object.keys(cfg.value?.credentials ?? {}).length)

</script>

<template>
  <div class="space-y-5 pb-10">
    <div class="flex flex-wrap items-center justify-between gap-3">
      <div>
        <h1 class="text-base font-semibold">设置</h1>
        <p class="text-muted-foreground mt-0.5 text-xs">
          改动即时生效，无需手动保存；敏感信息只存在本地数据库，写入后不回显
        </p>
      </div>
      <div class="flex flex-wrap items-center gap-2">
        <span
          v-if="saveState === 'saving'"
          class="text-muted-foreground flex items-center gap-1 text-xs"
        >
          <Loader2 class="size-3.5 animate-spin" /> 保存中…
        </span>
        <span
          v-else-if="saveState === 'saved'"
          class="text-status-success flex items-center gap-1 text-xs"
        >
          <CheckCircle2 class="size-3.5" /> 已保存
        </span>
        <template v-else-if="saveState === 'error'">
          <span class="text-status-error flex max-w-[22rem] items-start gap-1 text-xs">
            <AlertCircle class="mt-0.5 size-3.5 shrink-0" />
            <span>{{ saveError }}</span>
          </span>
          <Button variant="outline" size="sm" @click="flushSave">重试</Button>
        </template>
        <span v-else-if="!cfg?.iyuu.has_token" class="text-status-warn text-xs">
          先保存 IYUU Token，其它设置才能写入
        </span>
        <span v-else class="text-muted-foreground text-xs">更改会自动保存</span>
      </div>
    </div>

    <div v-if="loading" class="space-y-4">
      <Skeleton v-for="i in 3" :key="i" class="h-48 w-full rounded-xl" />
    </div>

    <template v-else>
      <!-- IYUU -->
      <Card>
        <CardHeader>
          <CardTitle class="flex items-center gap-2 text-sm">
            <KeyRound class="size-4" /> IYUU 账号
          </CardTitle>
          <CardDescription>
            Token 在 <a href="https://iyuu.cn" target="_blank" rel="noreferrer" class="underline underline-offset-2">iyuu.cn</a>
            获取；需要在 IYUU 里完成站点绑定，否则查询结果会为空
          </CardDescription>
        </CardHeader>
        <CardContent class="space-y-4">
          <div class="space-y-1.5">
            <Label for="token">
              Token
              <span v-if="cfg?.iyuu.has_token" class="text-muted-foreground ml-1 font-normal">
                （已保存 {{ cfg.iyuu.token_hint }}，留空表示不修改）
              </span>
            </Label>
            <Input
              id="token"
              v-model="form.token"
              type="password"
              autocomplete="off"
              :placeholder="cfg?.iyuu.has_token ? '••••••••（已保存）' : 'IYUU0812xxxxxxx'"
              @blur="submitToken"
              @keyup.enter="submitToken"
            />
            <p class="text-muted-foreground text-[11px]">
              离开输入框时会让你选择会员类型（普通用户 / SVIP）——两者额度差 5 倍，且无法从 Token 判断
            </p>
          </div>

          <Separator />

          <!-- 会员类型状态 + 额度 -->
          <TierPicker
            :tier="cfg?.iyuu.tier_chosen ? (cfg?.iyuu.tier ?? null) : null"
            :quota="quota"
            :disabled="saving"
            @edit="openTierDialog"
          />

          <div class="flex flex-wrap items-center gap-3">
            <Button variant="outline" size="sm" :disabled="testingIyuu" @click="testIyuu">
              <Loader2 v-if="testingIyuu" class="animate-spin" />
              <Zap v-else />
              测试连接
            </Button>
            <span
              v-if="iyuuResult.ok !== null"
              :class="
                cn(
                  'flex items-start gap-1 text-xs',
                  iyuuResult.ok ? 'text-status-success' : 'text-status-warn',
                )
              "
            >
              <CheckCircle2 v-if="iyuuResult.ok" class="mt-0.5 size-3.5 shrink-0" />
              <AlertCircle v-else class="mt-0.5 size-3.5 shrink-0" />
              <span>{{ iyuuResult.text }}</span>
            </span>
            <span v-else class="text-muted-foreground text-[11px]">
              使用已保存的会员类型真实请求一次，同时确认 Token 是否已激活
            </span>
          </div>
        </CardContent>
      </Card>

      <!-- 合作站点 / Token 激活 -->
      <Card>
        <CardHeader>
          <CardTitle class="flex items-center gap-2 text-sm">
            <ShieldCheck class="size-4" /> 合作站点与 Token 激活
          </CardTitle>
          <CardDescription>
            IYUU 的 Token 需要用一个<span class="text-foreground">合作站点</span>的账号激活；
            激活前所有查询都会被拒绝。这里列的是合作站点，不是"可以辅种的站点"
          </CardDescription>
        </CardHeader>
        <CardContent class="space-y-3">
          <div
            v-if="activationKnown !== null"
            :class="
              cn(
                'flex items-start gap-1.5 rounded-md border px-3 py-2 text-[11px] leading-relaxed',
                activationKnown
                  ? 'border-status-success/30 bg-status-success/5'
                  : 'border-status-warn/30 bg-status-warn/5',
              )
            "
          >
            <CheckCircle2
              v-if="activationKnown"
              class="text-status-success mt-0.5 size-3.5 shrink-0"
            />
            <AlertCircle v-else class="text-status-warn mt-0.5 size-3.5 shrink-0" />
            <span>
              {{
                activationKnown
                  ? '上次检查：Token 已激活'
                  : '上次检查：Token 未激活 —— 在下方绑定一个合作站点账号即可'
              }}
            </span>
          </div>

          <PartnerBindCard
            :has-token="!!cfg?.iyuu.has_token"
            @quota-changed="loadQuota"
          />
        </CardContent>
      </Card>

      <!-- Downloader -->
      <Card>
        <CardHeader>
          <CardTitle class="flex items-center gap-2 text-sm">
            <Server class="size-4" /> 下载器
          </CardTitle>
          <CardDescription>
            辅种会把种子添加到这个下载器，并复用原种子的保存路径
          </CardDescription>
        </CardHeader>
        <CardContent class="space-y-4">
          <div class="grid gap-4 sm:grid-cols-2">
            <div class="space-y-1.5">
              <Label>类型</Label>
              <select
                v-model="form.dlKind"
                class="border-input focus-visible:border-ring focus-visible:ring-ring/50 flex h-9 w-full cursor-pointer rounded-md border bg-transparent px-3 py-1 text-sm shadow-xs outline-none focus-visible:ring-[3px]"
              >
                <option v-for="o in dlOptions" :key="o.value" :value="o.value">
                  {{ o.label }}
                </option>
              </select>
            </div>
            <div class="space-y-1.5">
              <Label>名称（显示用）</Label>
              <Input v-model="form.dlName" placeholder="qBittorrent" />
            </div>
            <div class="space-y-1.5 sm:col-span-2">
              <Label>地址</Label>
              <Input v-model="form.dlUrl" placeholder="例如 127.0.0.1:8080（不带 /api/v2）" />
              <p class="text-muted-foreground text-[11px]">
                qBittorrent 填 WebUI 地址即可（不要带 /api/v2）
              </p>
            </div>
            <div v-if="form.dlKind === 'qbittorrent'" class="space-y-1.5">
              <Label>认证方式</Label>
              <select
                v-model="form.dlAuth"
                class="border-input focus-visible:border-ring focus-visible:ring-ring/50 flex h-9 w-full cursor-pointer rounded-md border bg-transparent px-3 py-1 text-sm shadow-xs outline-none focus-visible:ring-[3px]"
              >
                <option value="password">用户名 / 密码</option>
                <option value="api_key">API Key（qB ≥ 5.2）</option>
              </select>
              <p class="text-muted-foreground text-[11px]">
                API Key 不需要登录、重启 WebUI 也不失效；生成位置：设置 → WebUI → API Key
              </p>
            </div>
            <template v-if="form.dlAuth === 'api_key' && form.dlKind === 'qbittorrent'">
              <div class="space-y-1.5 sm:col-span-2">
                <Label>
                  API Key
                  <span v-if="cfg?.downloader.has_api_key" class="text-muted-foreground ml-1 font-normal">
                    （已保存，留空表示不修改）
                  </span>
                  <span v-else class="text-muted-foreground ml-1 font-normal">（离开输入框即保存）</span>
                </Label>
                <Input
                  v-model="form.dlApiKey"
                  type="password"
                  autocomplete="off"
                  spellcheck="false"
                  :placeholder="
                    cfg?.downloader.has_api_key ? '••••••••（已保存）' : 'qbt_xxxxxxxxxxxxxxxxxxxxxxxxxxxx'
                  "
                  @blur="flushSecrets"
                  @keyup.enter="flushSecrets"
                />
              </div>
            </template>
            <template v-else>
              <div class="space-y-1.5">
                <Label>用户名</Label>
                <Input v-model="form.dlUser" autocomplete="off" />
              </div>
              <div class="space-y-1.5">
                <Label>
                  密码
                  <span v-if="cfg?.downloader.has_password" class="text-muted-foreground ml-1 font-normal">
                    （已保存，留空表示不修改）
                  </span>
                  <span v-else class="text-muted-foreground ml-1 font-normal">（离开输入框即保存）</span>
                </Label>
                <Input
                  v-model="form.dlPass"
                  type="password"
                  autocomplete="new-password"
                  :placeholder="cfg?.downloader.has_password ? '••••••••' : ''"
                  @blur="flushSecrets"
                  @keyup.enter="flushSecrets"
                />
              </div>
            </template>
            <div v-if="form.dlKind === 'transmission'" class="space-y-1.5">
              <Label>RPC 路径</Label>
              <Input v-model="form.rpcPath" placeholder="/transmission/rpc" />
            </div>
          </div>

          <div class="flex flex-wrap items-center gap-3">
            <Button variant="outline" size="sm" :disabled="testingDl" @click="testDownloader">
              <Loader2 v-if="testingDl" class="animate-spin" />
              <Download v-else />
              测试连接
            </Button>
            <span
              v-if="dlResult.ok !== null"
              :class="cn('text-xs', dlResult.ok ? 'text-status-success' : 'text-status-error')"
            >
              {{ dlResult.text }}
            </span>
          </div>
        </CardContent>
      </Card>

      <!-- Reseed behaviour -->
      <Card>
        <CardHeader>
          <CardTitle class="text-sm">辅种行为</CardTitle>
          <CardDescription>
            保存路径一律复用原种子；校验必须通过才算成功，不提供「跳过校验」
          </CardDescription>
        </CardHeader>
        <CardContent class="space-y-4">
          <div class="grid gap-4 sm:grid-cols-2">
            <div class="space-y-1.5">
              <Label>最小体积（GiB）</Label>
              <Input v-model="form.minSize" type="number" min="0" step="0.5" />
              <p class="text-muted-foreground text-[11px]">小于该体积的原种不参与辅种，0 表示不限</p>
            </div>
            <div class="space-y-1.5">
              <Label>并发获取数</Label>
              <Input v-model="form.concurrency" type="number" min="1" max="16" />
              <p class="text-muted-foreground text-[11px]">
                同时从站点下载种子的数量。站点有流控，建议 1–3
              </p>
            </div>
            <div class="space-y-1.5">
              <Label>不辅种的标签 / 分类</Label>
              <Input v-model="form.excludeTags" placeholder="用逗号分隔，例如：限速, 保种" />
            </div>
            <div class="space-y-1.5">
              <Label>辅种后添加的标签</Label>
              <Input v-model="form.tags" placeholder="用逗号分隔" />
            </div>
            <div class="space-y-1.5">
              <Label>默认分类</Label>
              <Input v-model="form.defaultCategory" placeholder="原种没有分类时使用" />
            </div>
            <div class="space-y-1.5">
              <Label>校验轮询间隔（秒）</Label>
              <Input v-model="form.verifyInterval" type="number" min="5" max="600" />
              <p class="text-muted-foreground text-[11px]">越小越及时，但下载器负载越高</p>
            </div>
            <div class="space-y-1.5 sm:col-span-2">
              <Label>不辅种的路径前缀（每行一个）</Label>
              <Textarea
                v-model="form.excludePaths"
                :rows="2"
                placeholder="/downloads/scratch"
              />
            </div>
          </div>

          <Separator />

          <div class="space-y-3">
            <div class="flex items-center justify-between gap-4">
              <div>
                <Label class="cursor-pointer">复用原种子分类</Label>
                <p class="text-muted-foreground text-[11px]">
                  开启后辅种沿用原种子的分类，避免分类目录不同导致文件被移动
                </p>
              </div>
              <Switch v-model="form.reuseCategory" />
            </div>
            <div class="flex items-center justify-between gap-4">
              <div>
                <Label class="cursor-pointer">标签中加入站点名</Label>
                <p class="text-muted-foreground text-[11px]">
                  例如 iyuu-reseed:springsunday，方便按站点筛选
                </p>
              </div>
              <Switch v-model="form.tagWithSite" />
            </div>
            <div class="flex items-center justify-between gap-4">
              <div>
                <Label class="cursor-pointer">校验通过后自动开始</Label>
                <p class="text-muted-foreground text-[11px]">
                  关闭时种子会保持「已完成但停止」，便于人工确认后再启动
                </p>
              </div>
              <Switch v-model="form.autoStart" />
            </div>
          </div>
        </CardContent>
      </Card>

      <!-- Schedule -->
      <Card>
        <CardHeader>
          <CardTitle class="flex items-center gap-2 text-sm">
            <Timer class="size-4" /> 定时任务
          </CardTitle>
        </CardHeader>
        <CardContent class="space-y-4">
          <div class="flex items-center justify-between gap-4">
            <div>
              <Label class="cursor-pointer">启用定时辅种</Label>
              <p class="text-muted-foreground text-[11px]">
                校验巡检始终运行，与定时任务无关
              </p>
            </div>
            <Switch v-model="form.scheduleEnabled" />
          </div>
          <div v-if="form.scheduleEnabled" class="space-y-1.5">
            <Label>Cron 表达式</Label>
            <Input v-model="form.cron" placeholder="0 4 * * *" class="font-mono" />
            <p class="text-muted-foreground text-[11px]">
              5 个字段：分 时 日 月 周。例如 <code>0 4 * * *</code> 表示每天 04:00
            </p>
          </div>
        </CardContent>
      </Card>

      <!-- Per-site credentials live on their own page now: the settings page
           must not carry a table of every site IYUU knows. -->
      <Card>
        <CardHeader>
          <CardTitle class="text-sm">站点凭据</CardTitle>
          <CardDescription>
            已添加 {{ addedSiteCount }} 个辅种站点；站点的添加、凭据填写与移除都在独立页面
          </CardDescription>
        </CardHeader>
        <CardContent>
          <Button variant="outline" size="sm" as-child>
            <RouterLink to="/sites">
              管理站点凭据
            </RouterLink>
          </Button>
        </CardContent>
      </Card>
    </template>

    <!-- 会员类型选择：保存 Token 或点「修改」时打开 -->
    <TierSetupDialog
      v-model:open="tierDialogOpen"
      :token="form.token"
      :current-tier="cfg?.iyuu.tier_chosen ? (cfg?.iyuu.tier ?? null) : null"
      @confirm="onTierConfirmed"
    />
  </div>
</template>
