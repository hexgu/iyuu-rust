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

// Declared up here because `refreshSites` clears them: the two action slots are
// shared by "refresh", "import" and "remove", and a stale success line from one
// action must not survive into another.
const busySid = ref<number | null>(null)
const actionError = ref<string | null>(null)
const actionOk = ref<string | null>(null)

/**
 * Whether `/reseed/sites/index` has ever been fetched successfully.
 *
 * `false` on a fresh install is normal, not an error: the catalogue is pulled
 * once (first start, or the first click on 刷新) and then persisted, so pages and
 * runs read it from SQLite without spending quota. Without this flag an empty
 * picker would look like a broken token.
 */
const sitesInitialised = ref(true)
/** ISO-8601 of the last successful fetch. */
const sitesSyncedAt = ref<string | null>(null)

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
 * Read the persisted catalogue. **Does not spend IYUU quota** — the server serves
 * it from SQLite, which is why this is safe on every mount.
 *
 * The catalogue is optional for rendering: without it the added sites still show
 * by their sid, and only the "add" flow needs the full list — hence the separate
 * error slot instead of failing the page.
 */
async function loadSites() {
  sitesLoading.value = true
  sitesError.value = null
  try {
    const r = await api.sites()
    sites.value = r.sites
    sitesInitialised.value = r.initialised
    sitesSyncedAt.value = r.synced_at
  } catch (e) {
    sites.value = []
    sitesError.value = e instanceof ApiError ? e.message : String(e)
  } finally {
    sitesLoading.value = false
  }
}

/**
 * Refetch `/reseed/sites/index` and persist it — the 刷新 button.
 *
 * The only action in this view that spends an IYUU request, so it is never
 * automatic. Two reasons a user needs it: the first fetch failed (usually an
 * unactivated token), or IYUU added a site they want to seed into.
 */
async function refreshSites() {
  sitesLoading.value = true
  sitesError.value = null
  actionError.value = null
  actionOk.value = null
  try {
    const r = await api.refreshSites()
    sites.value = r.sites
    sitesInitialised.value = r.initialised
    sitesSyncedAt.value = r.synced_at
    actionOk.value = `站点目录已刷新：${r.count} 个站点（消耗 1 次额度）`
    loadQuota()
  } catch (e) {
    sitesError.value = e instanceof ApiError ? e.message : String(e)
    // An unactivated token is the most common first-run cause, and the fix is a
    // different page — say so instead of leaving a bare 403 on screen.
    if (e instanceof ApiError && e.kind === 'iyuu_inactive') {
      sitesError.value = `${e.message}（去「设置 → 合作站点与 Token 激活」绑定一个合作站点）`
    }
  } finally {
    sitesLoading.value = false
  }
}

/**
 * Today's spend, shown next to the refresh buttons.
 *
 * Refresh is the only action on this page that costs an IYUU request, so the
 * counter belongs right there: a metered button with no visible meter is how you
 * end up surprised at the limit. Read from the local ledger, so it is free.
 */
const quotaUsed = ref<number | null>(null)
const quotaDaily = ref<number | null>(null)
async function loadQuota() {
  try {
    const q = await api.quota()
    quotaUsed.value = q.used_requests
    quotaDaily.value = q.daily_requests
  } catch {
    quotaUsed.value = null
    quotaDaily.value = null
  }
}

/** Human-readable age of the catalogue, so "stale" is visible rather than guessed. */
const syncedLabel = computed(() => {
  if (!sitesSyncedAt.value) return ''
  const at = new Date(sitesSyncedAt.value)
  if (Number.isNaN(at.getTime())) return ''
  const days = Math.floor((Date.now() - at.getTime()) / 86_400_000)
  if (days <= 0) return '今天'
  if (days === 1) return '昨天'
  return `${days} 天前`
})

onMounted(async () => {
  // `loadSites` reads the persisted catalogue and `loadQuota` reads the local
  // counter — neither spends an IYUU request, so both are safe on every mount.
  await Promise.all([loadConfig(), loadSites(), loadQuota()])
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
      <div class="flex flex-wrap items-center gap-2">
        <span
          v-if="quotaUsed !== null && quotaDaily"
          class="text-muted-foreground font-mono text-[10px]"
          title="今日已用的 IYUU 请求次数。打开页面不消耗，只有点刷新才会"
        >
          今日额度 {{ quotaUsed }}/{{ quotaDaily }}
        </span>
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

      <!-- Never fetched: normal on a fresh install, and the fix is one click. -->
      <Alert v-if="!sitesInitialised && !sitesLoading">
        <AlertCircle />
        <AlertDescription>
          还没有拉取过 IYUU 的辅种站点目录
          <span v-if="!cfg?.iyuu.has_token">（先在设置里填写并保存 IYUU Token）</span>
          <button
            v-else
            type="button"
            class="ml-1 underline underline-offset-2"
            :disabled="sitesLoading"
            @click="refreshSites"
          >
            立即拉取（消耗 1 次额度）
          </button>
        </AlertDescription>
      </Alert>

      <Alert v-if="sitesError" variant="warning">
        <AlertCircle />
        <AlertDescription>
          {{ sitesError }}
          <button
            type="button"
            class="ml-1 underline underline-offset-2"
            :disabled="sitesLoading"
            @click="refreshSites"
          >
            重新拉取
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
          <!--
            Real refresh: refetches `/reseed/sites/index` and rewrites the stored
            catalogue. Distinct from re-reading the page, which is free and
            happens on mount — this one costs an IYUU request, so it is only ever
            an explicit click, never automatic.
          -->
          <Button
            variant="outline"
            size="icon"
            title="重新拉取 IYUU 站点目录（消耗 1 次额度）"
            :disabled="sitesLoading"
            @click="refreshSites"
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
                : sitesInitialised
                  ? '站点目录为空，点右上角刷新重新拉取'
                  : '还没有拉取站点目录，点右上角刷新拉取（消耗 1 次额度）'
            }}
          </div>
        </div>

        <p class="text-muted-foreground text-[11px] leading-relaxed">
          列表来自 IYUU 的站点目录，已保存在本地，打开页面不会消耗额度；
          点右上角刷新才会重新拉取（消耗 1 次额度）。
          <span v-if="syncedLabel">上次更新：{{ syncedLabel }}。</span>
          添加后此处不再显示该站点。
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
