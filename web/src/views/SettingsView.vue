<script setup lang="ts">
import { computed, onMounted, ref, reactive } from 'vue'
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
  type PublicConfig,
  type SiteInfo,
  type SiteCredentials,
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
import { Alert, AlertDescription } from '@/components/ui/alert'
import { Skeleton } from '@/components/ui/skeleton'
import { Badge } from '@/components/ui/badge'

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
  iyuuBaseUrl: '',
  dlKind: 'qbittorrent',
  dlName: '',
  dlUrl: '',
  dlUser: '',
  dlPass: '',
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

const sites = ref<SiteInfo[]>([])
const sitesLoading = ref(false)
const sitesError = ref<string | null>(null)
const credDraft = reactive<Record<string, Required<SiteCredentials>>>({})

const testingIyuu = ref(false)
const iyuuResult = reactive<{ ok: boolean | null; text: string }>({ ok: null, text: '' })
const testingDl = ref(false)
const dlResult = reactive<{ ok: boolean | null; text: string }>({ ok: null, text: '' })

function seedForm(c: PublicConfig) {
  form.iyuuBaseUrl = c.iyuu.base_url ?? ''
  form.dlKind = c.downloader.kind
  form.dlName = c.downloader.name
  form.dlUrl = c.downloader.base_url
  form.dlUser = c.downloader.username
  form.dlPass = '' // never returned by the API
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
  // Load the site list opportunistically: without a token it will 502, which is
  // fine — we just show a hint instead of an error.
  loadSites()
})

async function loadSites() {
  sitesLoading.value = true
  sitesError.value = null
  try {
    const r = await api.sites()
    sites.value = r.sites
    for (const s of r.sites) {
      if (!credDraft[String(s.id)]) {
        // Seed the draft from what we know exists; secrets stay blank so an
        // empty field means "unchanged".
        const known = cfg.value?.credentials[String(s.id)]
        credDraft[String(s.id)] = {
          passkey: '',
          uid: known?.uid ?? '',
          cookie: '',
          user_agent: known?.user_agent ?? '',
          api_key: '',
          rsskey: '',
          torrent_key: '',
        }
      }
    }
  } catch (e) {
    sitesError.value = e instanceof ApiError ? e.message : String(e)
  } finally {
    sitesLoading.value = false
  }
}

function splitList(v: string): string[] {
  return v
    .split(/[,\n]/)
    .map((s) => s.trim())
    .filter(Boolean)
}

async function save() {
  saving.value = true
  saveMsg.kind = null
  try {
    const credentials: Record<string, SiteCredentials> = {}
    for (const [sid, draft] of Object.entries(credDraft)) {
      // Only send fields the user actually typed (or a known uid/ua). Sending
      // empty strings would wipe stored secrets, so they are dropped.
      const entry: SiteCredentials = {}
      if (draft.passkey) entry.passkey = draft.passkey
      if (draft.cookie) entry.cookie = draft.cookie
      if (draft.api_key) entry.api_key = draft.api_key
      if (draft.rsskey) entry.rsskey = draft.rsskey
      if (draft.uid) entry.uid = draft.uid
      if (draft.user_agent) entry.user_agent = draft.user_agent
      if (Object.keys(entry).length) credentials[sid] = entry
    }

    const patch: Record<string, unknown> = {
      downloader: {
        kind: form.dlKind,
        name: form.dlName,
        base_url: form.dlUrl,
        username: form.dlUser,
        password: form.dlPass || undefined,
        extra: { transmission_rpc_path: form.rpcPath || null },
      },
      reseed: {
        min_size_gib: Number(form.minSize) || 0,
        exclude_tags: splitList(form.excludeTags),
        exclude_paths: splitList(form.excludePaths),
        site_ids: cfg.value?.reseed.site_ids ?? [],
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

    // Only include the token when the user typed one.
    if (form.token) {
      patch.iyuu = { token: form.token, base_url: form.iyuuBaseUrl || null }
    } else if (form.iyuuBaseUrl !== (cfg.value?.iyuu.base_url ?? '')) {
      patch.iyuu = { base_url: form.iyuuBaseUrl || null }
    }

    if (Object.keys(credentials).length) patch.credentials = credentials

    const r = await api.putConfig(patch)
    cfg.value = r.config
    seedForm(r.config)
    // Clear the token field and the secret drafts: they are stored now.
    form.token = ''
    for (const d of Object.values(credDraft)) {
      d.passkey = ''
      d.cookie = ''
      d.api_key = ''
      d.rsskey = ''
    }
    saveMsg.kind = 'ok'
    saveMsg.text = '已保存'
    loadSites()
  } catch (e) {
    saveMsg.kind = 'err'
    saveMsg.text = e instanceof ApiError ? e.message : String(e)
  } finally {
    saving.value = false
  }
}

async function testIyuu() {
  testingIyuu.value = true
  iyuuResult.ok = null
  iyuuResult.text = ''
  try {
    const r = await api.testIyuu(form.token || undefined)
    iyuuResult.ok = true
    iyuuResult.text = `连接成功：支持 ${r.site_count} 个站点，已绑定 ${r.bound_count} 个`
    await loadSites()
  } catch (e) {
    iyuuResult.ok = false
    iyuuResult.text = e instanceof ApiError ? e.message : String(e)
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
      username: form.dlUser,
      password: form.dlPass || undefined,
      rpc_path: form.rpcPath || undefined,
    })
    dlResult.ok = true
    dlResult.text = `连接成功：${r.kind} ${r.version}，共 ${r.torrent_count} 个种子（已完成 ${r.complete_count}）`
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

/** Which credential fields a site actually needs, from IYUU's own metadata. */
function fieldsFor(site: SiteInfo) {
  const check = site.reseed_check.toLowerCase()
  const isMteam = site.base_url.includes('m-team')
  const isMonika = site.base_url.includes('monikadesign')
  return {
    // A passkey (or the authkey/torrent_key aliases) is the default mechanism;
    // sites whose `reseed_check` is blank are NexusPHP sites that still use one.
    passkey:
      check.includes('passkey') ||
      check.includes('authkey') ||
      check.includes('torrent_key') ||
      check === '',
    uid: check.includes('uid'),
    // Cookie is only needed when IYUU flags it, but always offer it: a site can
    // start requiring one without the metadata saying so.
    cookie: site.cookie_required === 1,
    apiKey: isMteam,
    rsskey: isMonika,
  }
}

const searchCred = ref('')
const filteredSites = computed(() => {
  const q = searchCred.value.trim().toLowerCase()
  if (!q) return sites.value
  return sites.value.filter(
    (s) => s.nickname.toLowerCase().includes(q) || s.site.toLowerCase().includes(q),
  )
})

const configuredCount = computed(
  () => Object.values(cfg.value?.credentials ?? {}).filter(
    (c) => c.has_passkey || c.has_cookie || c.has_api_key || c.has_rsskey,
  ).length,
)
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
            <KeyRound class="size-4" /> IYUU
          </CardTitle>
          <CardDescription>
            Token 在 <a href="https://iyuu.cn" target="_blank" rel="noreferrer" class="underline underline-offset-2">iyuu.cn</a>
            获取；需要在站点账号里完成绑定，否则查询结果会为空
          </CardDescription>
        </CardHeader>
        <CardContent class="space-y-4">
          <div class="grid gap-4 sm:grid-cols-2">
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
              />
            </div>
            <div class="space-y-1.5">
              <Label for="baseurl">API 地址（可选）</Label>
              <Input id="baseurl" v-model="form.iyuuBaseUrl" placeholder="https://2025.iyuu.cn" />
            </div>
          </div>

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

      <!-- Per-site credentials -->
      <Card>
        <CardHeader>
          <CardTitle class="text-sm">
            站点凭据
            <span class="text-muted-foreground ml-1 font-normal">
              （已配置 {{ configuredCount }} / {{ sites.length || '—' }}）
            </span>
          </CardTitle>
          <CardDescription>
            辅种需要能下载目标站的种子。passkey 与 cookie 至少填一项，特殊站点另有要求
          </CardDescription>
        </CardHeader>
        <CardContent class="space-y-3">
          <Alert v-if="sitesError" variant="warning">
            <AlertCircle />
            <AlertDescription>
              无法获取站点列表：{{ sitesError }}
              <span v-if="!cfg?.iyuu.has_token">（先填写并保存 IYUU Token）</span>
            </AlertDescription>
          </Alert>

          <div class="flex gap-2">
            <Input v-model="searchCred" placeholder="筛选站点…" class="max-w-xs" />
            <Button variant="outline" size="sm" :disabled="sitesLoading" @click="loadSites">
              <Loader2 v-if="sitesLoading" class="animate-spin" />
              <Download v-else />
              重新获取
            </Button>
          </div>

          <div v-if="sitesLoading && !sites.length" class="space-y-2">
            <Skeleton v-for="i in 4" :key="i" class="h-12 w-full" />
          </div>

          <div v-else class="divide-y rounded-md border">
            <div
              v-for="s in filteredSites"
              :key="s.id"
              class="grid gap-2 p-3 sm:grid-cols-[10rem_1fr]"
            >
              <div class="min-w-0">
                <div class="flex items-center gap-1.5">
                  <span class="truncate text-xs font-medium">{{ s.nickname || s.site }}</span>
                  <Badge
                    v-if="cfg?.credentials[String(s.id)]?.has_passkey || cfg?.credentials[String(s.id)]?.has_cookie || cfg?.credentials[String(s.id)]?.has_api_key"
                    variant="secondary"
                    class="shrink-0 text-[10px]"
                  >
                    已配置
                  </Badge>
                </div>
                <div class="text-muted-foreground truncate font-mono text-[10px]">
                  {{ s.base_url }}
                </div>
                <div class="text-muted-foreground mt-0.5 text-[10px]">
                  需要：{{ s.reseed_check || 'passkey' }}
                  <span v-if="s.cookie_required === 1">· cookie</span>
                </div>
              </div>

              <div class="grid gap-2 sm:grid-cols-3">
                <Input
                  v-if="fieldsFor(s).passkey"
                  v-model="credDraft[String(s.id)].passkey"
                  type="password"
                  placeholder="passkey"
                  autocomplete="off"
                  class="h-8 text-xs"
                />
                <Input
                  v-if="fieldsFor(s).uid"
                  v-model="credDraft[String(s.id)].uid"
                  placeholder="uid"
                  class="h-8 text-xs"
                />
                <Input
                  v-if="fieldsFor(s).cookie"
                  v-model="credDraft[String(s.id)].cookie"
                  type="password"
                  placeholder="cookie"
                  autocomplete="off"
                  class="h-8 text-xs"
                />
                <Input
                  v-if="fieldsFor(s).apiKey"
                  v-model="credDraft[String(s.id)].api_key"
                  type="password"
                  placeholder="M-Team API Key"
                  autocomplete="off"
                  class="h-8 text-xs"
                />
                <Input
                  v-if="fieldsFor(s).rsskey"
                  v-model="credDraft[String(s.id)].rsskey"
                  type="password"
                  placeholder="RSS 链接或 rsskey"
                  autocomplete="off"
                  class="h-8 text-xs"
                />
              </div>
            </div>
            <div v-if="!filteredSites.length" class="text-muted-foreground p-6 text-center text-xs">
              {{ sites.length ? '没有匹配的站点' : '暂无站点数据' }}
            </div>
          </div>
        </CardContent>
      </Card>
    </template>
  </div>
</template>
