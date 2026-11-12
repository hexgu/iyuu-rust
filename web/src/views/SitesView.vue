<script setup lang="ts">
/**
 * 站点凭据页。
 *
 * 这一页的原型是设置页里的一段「所有站点 × 所有字段」的表格，两个问题：
 *   1. IYUU 的站点目录上百个，绝大多数永远用不到，铺出来就是让人在一堆无关
 *      行里找自己那几行；
 *   2. 表单把每个站点都提交一次，保存语义变成「整张表覆盖」，一个 tab 放久了
 *      就会用旧值盖掉别处刚改的内容。
 *
 * 所以这里改成「按需添加」：列表只显示已经添加的站点（它们才是辅种目标），
 * 新增时从目录里挑一个再填该站点真正需要的字段。
 */
import { computed, onMounted, ref } from 'vue'
import {
  AlertCircle,
  CheckCircle2,
  Globe,
  Loader2,
  Plus,
  RefreshCw,
  Trash2,
  Upload,
} from 'lucide-vue-next'
import {
  api,
  ApiError,
  type PublicConfig,
  type SiteInfo,
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
import { Alert, AlertDescription } from '@/components/ui/alert'
import { Skeleton } from '@/components/ui/skeleton'
import { Badge } from '@/components/ui/badge'
import { Dialog } from '@/components/ui/dialog'
import SiteCredentialDialog from '@/components/iyuu/SiteCredentialDialog.vue'
import ImportSitesDialog from '@/components/iyuu/ImportSitesDialog.vue'
import {
  credentialNeeds,
  hasAnyCredential,
  hasPrimary,
  storedSummary,
} from '@/lib/siteFields'

const cfg = ref<PublicConfig | null>(null)
const loading = ref(true)
const loadError = ref<string | null>(null)

const sites = ref<SiteInfo[]>([])
const sitesLoading = ref(false)
const sitesError = ref<string | null>(null)

/** Sid → catalogue metadata, so an added site can show its real name. */
const siteById = computed(() => {
  const map = new Map<number, SiteInfo>()
  for (const s of sites.value) map.set(s.id, s)
  return map
})

/** Added sites, in the order IYUU lists them so the page is stable. */
const addedSites = computed(() => {
  const entries = Object.keys(cfg.value?.credentials ?? {})
    .map((k) => Number(k))
    .filter((n) => Number.isFinite(n))

  return entries
    .map((sid) => ({
      sid,
      site: siteById.value.get(sid) ?? null,
      cred: cfg.value?.credentials[String(sid)],
    }))
    .sort((a, b) => {
      const an = a.site?.nickname || a.site?.site || String(a.sid)
      const bn = b.site?.nickname || b.site?.site || String(b.sid)
      return an.localeCompare(bn)
    })
})

async function loadConfig() {
  loading.value = true
  loadError.value = null
  try {
    cfg.value = await api.getConfig()
  } catch (e) {
    loadError.value = e instanceof ApiError ? e.message : String(e)
  } finally {
    loading.value = false
  }
}

/**
 * The catalogue is optional: without a saved token (or during an IYUU outage)
 * the added sites still render by their sid, and only the "add" flow needs the
 * full list — hence the separate error slot instead of failing the page.
 */
async function loadSites() {
  sitesLoading.value = true
  sitesError.value = null
  try {
    const r = await api.sites()
    sites.value = r.sites
  } catch (e) {
    sites.value = []
    sitesError.value = e instanceof ApiError ? e.message : String(e)
  } finally {
    sitesLoading.value = false
  }
}

onMounted(async () => {
  await Promise.all([loadConfig(), loadSites()])
})

// ---------------------------------------------------------------- add flow

const pickerOpen = ref(false)
const pickerQuery = ref('')

const availableSites = computed(() => {
  const added = new Set(Object.keys(cfg.value?.credentials ?? {}).map(Number))
  const q = pickerQuery.value.trim().toLowerCase()
  return sites.value
    .filter((s) => !added.has(s.id))
    .filter(
      (s) =>
        !q ||
        s.nickname.toLowerCase().includes(q) ||
        s.site.toLowerCase().includes(q) ||
        s.base_url.toLowerCase().includes(q),
    )
    .sort((a, b) => (a.nickname || a.site).localeCompare(b.nickname || b.site))
})

const credDialogOpen = ref(false)
const editingSite = ref<SiteInfo | null>(null)
const editingStored = ref<PublicConfig['credentials'][string] | undefined>()
const editingIsNew = ref(true)

function startAdd(site: SiteInfo) {
  pickerOpen.value = false
  editingSite.value = site
  editingStored.value = undefined
  editingIsNew.value = true
  credDialogOpen.value = true
}

function startEdit(sid: number) {
  editingSite.value = siteById.value.get(sid) ?? null
  editingStored.value = cfg.value?.credentials[String(sid)]
  editingIsNew.value = false
  credDialogOpen.value = true
}

function onSaved(config: unknown) {
  cfg.value = config as PublicConfig
}

// -------------------------------------------------------------- import flow

const importOpen = ref(false)

function onImported(config: unknown) {
  cfg.value = config as PublicConfig
  actionError.value = null
  actionOk.value = '备份已导入，下面的列表就是当前的辅种站点'
  // The catalogue may have been fetched before this page had a token; the new
  // sites need their names, so refresh it when we can.
  if (!sites.value.length) loadSites()
}

// ------------------------------------------------------------- delete flow

const busySid = ref<number | null>(null)
const actionError = ref<string | null>(null)
const actionOk = ref<string | null>(null)

async function remove(sid: number) {
  const label = siteById.value.get(sid)?.nickname || `站点 ${sid}`
  if (
    !confirm(
      `从辅种站点里移除「${label}」？\n\n只会删除本地保存的凭据，不会影响 IYUU 上的绑定，也不会动下载器里的种子。`,
    )
  ) {
    return
  }
  busySid.value = sid
  actionError.value = null
  actionOk.value = null
  try {
    const r = await api.deleteSiteCredentials(sid)
    cfg.value = r.config
    actionOk.value = `已移除「${label}」，后续辅种不会再向该站点提交`
  } catch (e) {
    actionError.value = e instanceof ApiError ? e.message : String(e)
  } finally {
    busySid.value = null
  }
}

const readyCount = computed(
  () => addedSites.value.filter((a) => hasAnyCredential(a.cred)).length,
)
</script>

<template>
  <div class="space-y-5 pb-10">
    <div class="flex flex-wrap items-start justify-between gap-3">
      <div>
        <h1 class="text-base font-semibold">站点凭据</h1>
        <p class="text-muted-foreground mt-0.5 text-xs">
          只辅种这里添加过的站点；凭据保存在本地，保存后不会回显
        </p>
      </div>
      <div class="flex items-center gap-2">
        <Button variant="outline" @click="importOpen = true">
          <Upload />
          导入备份
        </Button>
        <Button :disabled="sitesLoading" @click="pickerOpen = true">
          <Plus />
          添加站点
        </Button>
      </div>
    </div>

    <Alert v-if="loadError" variant="destructive">
      <AlertCircle />
      <AlertDescription>{{ loadError }}</AlertDescription>
    </Alert>

    <div v-if="loading" class="space-y-3">
      <Skeleton class="h-20 w-full rounded-xl" />
      <Skeleton class="h-20 w-full rounded-xl" />
    </div>

    <template v-else>
      <div v-if="actionError || actionOk" class="flex items-start gap-2">
        <span
          :class="
            cn(
              'flex items-center gap-1 text-xs',
              actionError ? 'text-status-error' : 'text-status-success',
            )
          "
        >
          <AlertCircle v-if="actionError" class="size-3.5" />
          <CheckCircle2 v-else class="size-3.5" />
          {{ actionError || actionOk }}
        </span>
      </div>

      <!-- Added sites: these are the reseed targets, nothing else. -->
      <Card>
        <CardHeader>
          <CardTitle class="flex items-center gap-2 text-sm">
            <Globe class="size-4" />
            已添加的站点
            <span class="text-muted-foreground font-normal">
              （{{ addedSites.length }} 个
              <span v-if="addedSites.length !== readyCount">
                · {{ readyCount }} 个可直接下载
              </span>
              ）
            </span>
          </CardTitle>
          <CardDescription>
            辅种时只会查询这些站点，没添加的站点即使 IYUU 有也不会去碰
          </CardDescription>
        </CardHeader>
        <CardContent class="space-y-2">
          <div
            v-if="!addedSites.length"
            class="text-muted-foreground rounded-md border border-dashed px-4 py-8 text-center text-xs leading-relaxed"
          >
            还没有添加任何站点。<br />
            点右上角「添加站点」，从 IYUU 的站点目录里挑要辅种的那几个。
          </div>

          <div
            v-for="a in addedSites"
            :key="a.sid"
            class="flex flex-wrap items-start gap-3 rounded-md border px-3 py-2.5"
          >
            <div class="min-w-0 flex-1">
              <div class="flex flex-wrap items-center gap-1.5">
                <span class="truncate text-xs font-medium">
                  {{ a.site?.nickname || a.site?.site || `站点 ${a.sid}` }}
                </span>
                <Badge
                  v-if="!hasAnyCredential(a.cred)"
                  variant="secondary"
                  class="text-[10px]"
                >
                  未填凭据
                </Badge>
                <Badge
                  v-else-if="!hasPrimary(credentialNeeds(a.site), a.cred)"
                  variant="secondary"
                  class="text-[10px] text-status-warn"
                >
                  缺少必需字段
                </Badge>
              </div>
              <div class="text-muted-foreground mt-0.5 truncate font-mono text-[10px]">
                {{ a.site?.base_url || `sid=${a.sid}` }}
              </div>
              <div class="text-muted-foreground mt-1 flex flex-wrap gap-x-2 gap-y-0.5 text-[10px]">
                <span v-if="storedSummary(a.cred).length">
                  已填：{{ storedSummary(a.cred).join(' · ') }}
                </span>
                <span v-else class="text-status-warn">还没有填写任何凭据</span>
                <span v-if="a.site?.reseed_check">
                  · 站点要求：{{ a.site.reseed_check }}
                </span>
              </div>
            </div>

            <div class="flex shrink-0 items-center gap-1">
              <Button variant="outline" size="sm" @click="startEdit(a.sid)">
                编辑
              </Button>
              <Button
                variant="ghost"
                size="icon"
                class="text-muted-foreground hover:text-status-error"
                title="移除该站点"
                :disabled="busySid === a.sid"
                @click="remove(a.sid)"
              >
                <Loader2 v-if="busySid === a.sid" class="animate-spin" />
                <Trash2 v-else class="size-4" />
              </Button>
            </div>
          </div>
        </CardContent>
      </Card>

      <Alert v-if="sitesError" variant="warning">
        <AlertCircle />
        <AlertDescription>
          无法获取 IYUU 站点目录：{{ sitesError }}
          <span v-if="!cfg?.iyuu.has_token">（先在设置里填写并保存 IYUU Token）</span>
          <button
            type="button"
            class="ml-1 underline underline-offset-2"
            @click="loadSites"
          >
            重试
          </button>
        </AlertDescription>
      </Alert>
    </template>

    <!-- Site picker: everything not yet added, searchable. -->
    <Dialog
      v-model:open="pickerOpen"
      title="添加辅种站点"
      description="从 IYUU 站点目录里选一个站点，下一步再填它需要的凭据"
      class="sm:max-w-xl"
    >
      <div class="space-y-3">
        <div class="flex gap-2">
          <Input
            v-model="pickerQuery"
            placeholder="搜索站点名 / 域名…"
            class="flex-1"
          />
          <Button
            variant="outline"
            size="icon"
            title="重新获取站点目录"
            :disabled="sitesLoading"
            @click="loadSites"
          >
            <RefreshCw :class="cn('size-4', sitesLoading && 'animate-spin')" />
          </Button>
        </div>

        <Alert v-if="sitesError" variant="warning">
          <AlertCircle />
          <AlertDescription>{{ sitesError }}</AlertDescription>
        </Alert>

        <div v-if="sitesLoading && !sites.length" class="space-y-2">
          <Skeleton v-for="i in 5" :key="i" class="h-11 w-full" />
        </div>

        <div
          v-else
          class="max-h-[55vh] divide-y overflow-y-auto rounded-md border scrollbar-slim"
        >
          <button
            v-for="s in availableSites"
            :key="s.id"
            type="button"
            class="hover:bg-accent flex w-full items-center gap-3 px-3 py-2.5 text-left transition-colors"
            @click="startAdd(s)"
          >
            <div class="min-w-0 flex-1">
              <div class="flex items-center gap-1.5">
                <span class="truncate text-xs font-medium">
                  {{ s.nickname || s.site }}
                </span>
                <Badge
                  v-if="credentialNeeds(s).primary[0] === 'api_key'"
                  variant="secondary"
                  class="text-[10px]"
                >API Key</Badge>
                <Badge
                  v-else-if="credentialNeeds(s).primary[0] === 'rsskey'"
                  variant="secondary"
                  class="text-[10px]"
                >RSS</Badge>
              </div>
              <div class="text-muted-foreground truncate font-mono text-[10px]">
                {{ s.base_url }}
              </div>
            </div>
            <Plus class="text-muted-foreground size-3.5 shrink-0" />
          </button>

          <div
            v-if="!availableSites.length"
            class="text-muted-foreground px-4 py-8 text-center text-xs"
          >
            {{
              sites.length
                ? '所有站点都已添加，或者没有匹配的结果'
                : '站点目录为空，先确认 IYUU Token 是否已保存'
            }}
          </div>
        </div>

        <p class="text-muted-foreground text-[11px]">
          列表来自 IYUU 的站点目录；添加后此处不再显示该站点。
        </p>
      </div>
    </Dialog>

    <ImportSitesDialog v-model:open="importOpen" @imported="onImported" />

    <SiteCredentialDialog
      v-model:open="credDialogOpen"
      :site="editingSite"
      :stored="editingStored"
      :editing="!editingIsNew"
      @saved="onSaved"
    />
  </div>
</template>
