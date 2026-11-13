<script setup lang="ts">
import { computed, onMounted, ref, reactive, watch } from 'vue'
import {
  AlertCircle,
  CheckCircle2,
  Download,
  KeyRound,
  Loader2,
  Save,
  Server,
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

const cfg = ref<PublicConfig | null>(null)
const loading = ref(true)
const saving = ref(false)
const saveMsg = reactive<{ kind: 'ok' | 'err' | null; text: string }>({
  kind: null,
  text: '',
})

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
}

// ---- 会员类型选择 -------------------------------------------------------
//
// The choice happens in a dialog, once, at the moment a token is saved. It is
// not a form field because it is not a preference: it selects the server the
// token is valid against, and it cannot be inferred.
const tierDialogOpen = ref(false)
/** Tier confirmed in the dialog, waiting to be written by the save. */
const pendingTier = ref<ApiTier | null>(null)

function openTierDialog() {
  tierDialogOpen.value = true
}

/**
 * The dialog confirmed a choice (already probed, for VIP). Store it and, when a
 * token is waiting to be saved, continue the save immediately — making the user
 * click 保存 twice for one action would be a pointless trap.
 */
async function onTierConfirmed(payload: { tier: ApiTier; token: string | null }) {
  pendingTier.value = payload.tier
  if (form.token.trim()) {
    await commitSave()
    return
  }
  // Re-picking a tier on an already-saved token: patch just that field.
  saving.value = true
  saveMsg.kind = null
  try {
    const r = await api.putConfig({ iyuu: { tier: payload.tier } })
    cfg.value = r.config
    saveMsg.kind = 'ok'
    saveMsg.text = '会员类型已更新'
    await loadQuota()
  } catch (e) {
    saveMsg.kind = 'err'
    saveMsg.text = e instanceof ApiError ? e.message : String(e)
  } finally {
    saving.value = false
    // Keep the confirmed tier only for this save; a later save must re-ask if the
    // token changed again.
    pendingTier.value = null
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
  } catch (e) {
    saveMsg.kind = 'err'
    saveMsg.text = e instanceof ApiError ? e.message : String(e)
  } finally {
    loading.value = false
  }
}

onMounted(async () => {
  await loadConfig()
  loadQuota()
})

function splitList(v: string): string[] {
  return v
    .split(/[,\n]/)
    .map((s) => s.trim())
    .filter(Boolean)
}

/**
 * Save entry point.
 *
 * A new token cannot be saved without a tier: the tier decides which server the
 * token is valid against, and guessing one is how a paying user silently ends up
 * on the free host. So instead of validating here, we open the selection dialog
 * and let `commitSave` run once the user has answered.
 */
async function save() {
  saveMsg.kind = null
  if (form.token.trim() && !pendingTier.value) {
    openTierDialog()
    return
  }
  await commitSave()
}

/** Run the save with everything the form + dialog have collected so far. */
async function commitSave() {
  saving.value = true
  try {
    const patch: Record<string, unknown> = {
      downloader: {
        kind: form.dlKind,
        name: form.dlName,
        base_url: form.dlUrl,
        auth: form.dlAuth,
        username: form.dlUser,
        password: form.dlPass || undefined,
        // Blank means "keep the saved key", matching the server's patch rule.
        api_key: form.dlApiKey || undefined,
        extra: { transmission_rpc_path: form.rpcPath || null },
      },
      reseed: {
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
      },
      schedule: { enabled: form.scheduleEnabled, cron: form.cron },
    }

    // A new token is always saved together with its tier — the server rejects one
    // without the other, because the tier decides which server the token is valid
    // against. `save()` guarantees `pendingTier` is set before we get here.
    if (form.token.trim() && pendingTier.value) {
      patch.iyuu = { token: form.token.trim(), tier: pendingTier.value }
    }

    const r = await api.putConfig(patch)
    cfg.value = r.config
    seedForm(r.config)
    // The token is stored now; a later save must not resend it.
    form.token = ''
    saveMsg.kind = 'ok'
    saveMsg.text = '已保存'
    loadQuota()
  } catch (e) {
    saveMsg.kind = 'err'
    saveMsg.text = e instanceof ApiError ? e.message : String(e)
  } finally {
    saving.value = false
    // The token (if any) is stored now; a second save must ask for the tier again
    // only if the token field is filled in again.
    pendingTier.value = null
  }
}

/**
 * Test against the saved tier. There is no tier selector in the form anymore, so
 * this only proves "the token I already saved still works"; choosing a server is
 * the dialog's job, and it probes before it saves.
 */
async function testIyuu() {
  testingIyuu.value = true
  iyuuResult.ok = null
  iyuuResult.text = ''
  try {
    const r = await api.testIyuu({})
    iyuuResult.ok = true
    iyuuResult.text = `连接成功：支持 ${r.site_count} 个站点，已绑定 ${r.bound_count} 个 · 本次消耗 2 次额度`
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
          敏感信息只保存在本地数据库，保存后不会回显
        </p>
      </div>
      <div class="flex items-center gap-3">
        <span
          v-if="saveMsg.kind"
          :class="cn(
            'flex items-center gap-1 text-xs',
            saveMsg.kind === 'ok' ? 'text-status-success' : 'text-status-error',
          )"
        >
          <CheckCircle2 v-if="saveMsg.kind === 'ok'" class="size-3.5" />
          <AlertCircle v-else class="size-3.5" />
          {{ saveMsg.text }}
        </span>
        <Button :disabled="saving || loading" @click="save">
          <Loader2 v-if="saving" class="animate-spin" />
          <Save v-else />
          保存
        </Button>
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
              @keyup.enter="save"
            />
            <p class="text-muted-foreground text-[11px]">
              保存时会让你选择会员类型（普通用户 / SVIP）——两者额度差 5 倍，且无法从 Token 判断
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
              :class="cn(
                'text-xs',
                iyuuResult.ok ? 'text-status-success' : 'text-status-error',
              )"
            >
              {{ iyuuResult.text }}
            </span>
            <span v-else class="text-muted-foreground text-[11px]">
              使用已保存的会员类型真实请求一次
            </span>
          </div>
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
                </Label>
                <Input
                  v-model="form.dlApiKey"
                  type="password"
                  autocomplete="off"
                  spellcheck="false"
                  :placeholder="
                    cfg?.downloader.has_api_key ? '••••••••（已保存）' : 'qbt_xxxxxxxxxxxxxxxxxxxxxxxxxxxx'
                  "
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
                </Label>
                <Input
                  v-model="form.dlPass"
                  type="password"
                  autocomplete="new-password"
                  :placeholder="cfg?.downloader.has_password ? '••••••••' : ''"
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
