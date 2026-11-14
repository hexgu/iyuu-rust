/**
 * Typed API client.
 *
 * Every call funnels through `request` so error handling is uniform: the server
 * returns `{error: {kind, message}}`, and `kind` is what the UI branches on —
 * `iyuu_auth` means "go bind your sites", `downloader_auth` means "wrong
 * password", anything else is a generic failure. Surfacing `kind` instead of the
 * status code is what lets the UI say something useful.
 */

export type ErrorKind =
  | 'iyuu_auth'
  /**
   * Token is valid but not **activated**: no cooperative-site account is bound to
   * it. A distinct kind because the remedy is a different screen (bind a partner
   * site), not "check your token".
   */
  | 'iyuu_inactive'
  | 'rate_limited'
  | 'config'
  | 'downloader_auth'
  | 'downloader'
  | 'db'
  | 'not_found'
  | 'internal'

export class ApiError extends Error {
  kind: ErrorKind
  constructor(kind: ErrorKind, message: string) {
    super(message)
    this.name = 'ApiError'
    this.kind = kind
  }
}

const BASE = '/api'

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let resp: Response
  try {
    resp = await fetch(`${BASE}${path}`, {
      headers: { 'Content-Type': 'application/json', ...(init?.headers ?? {}) },
      ...init,
    })
  } catch (e) {
    // A connection failure is almost always "the server isn't running", which is
    // worth saying plainly rather than surfacing "Failed to fetch".
    throw new ApiError('internal', `无法连接后端服务：${(e as Error).message}`)
  }

  const text = await resp.text()
  const body = text ? safeJson(text) : null

  if (!resp.ok) {
    const err = body?.error
    throw new ApiError(
      (err?.kind as ErrorKind) ?? 'internal',
      err?.message ?? `HTTP ${resp.status}`,
    )
  }
  return body as T
}

function safeJson(text: string): any {
  try {
    return JSON.parse(text)
  } catch {
    return { error: { kind: 'internal', message: text.slice(0, 300) } }
  }
}

// ------------------------------------------------------------------ types

export type ReseedState =
  | 'pending'
  | 'skipped'
  | 'fetching'
  | 'added'
  | 'verifying'
  | 'seeding'
  | 'verify_failed'
  | 'missing_files'
  | 'fetch_failed'
  | 'add_failed'
  | 'timed_out'

export interface RunStats {
  scanned: number
  queried: number
  candidates: number
  existed: number
  added: number
  seeding: number
  verify_failed: number
  fetch_failed: number
  add_failed: number
  other_failed: number
  skipped: number
}

export interface StatusResponse {
  configured: boolean
  iyuu_ok: boolean
  downloader_ok: boolean
  running: boolean
  pending_verification: number
  verified_seeding: number
  uptime_secs: number
  /** Next scheduled discovery run (ISO-8601); null when scheduling is off. */
  next_run_at: string | null
  schedule_enabled: boolean
  /** Declared membership tier. Decides which IYUU server is called. */
  iyuu_tier: ApiTier
  /** False until the user answers the tier question. */
  iyuu_tier_chosen: boolean
  /** Requests spent today against the tier's daily allowance. */
  quota_used: number
  quota_daily: number
  /** ISO-8601 end of a rate-limit cooldown, if IYUU told us to back off. */
  quota_blocked_until: string | null
  last_run:
    | {
        id: string
        trigger: string
        status: string
        started_at: string
        finished_at: string | null
        stats: RunStats
        message: string | null
      }
    | null
}

/**
 * IYUU account tier.
 *
 * Two distinct services, not a feature flag: `free` is served by 2025.iyuu.cn
 * with 500 reseed queries/day, `vip` by vip.iyuu.cn with 2500. The user declares
 * which they hold — it cannot be detected, because a request to the wrong host
 * just fails.
 */
export type ApiTier = 'free' | 'vip'

/**
 * How the downloader authenticates.
 *
 * `api_key` is qBittorrent ≥ 5.2 only: a stateless `Authorization: Bearer`
 * header instead of the `SID` cookie, so a WebUI restart no longer breaks a
 * scheduled run. Transmission has no equivalent and always uses password auth.
 */
export type DownloaderAuthMode = 'password' | 'api_key'

export interface TierInfo {
  id: ApiTier
  label: string
  daily_requests: number
  daily_hashes: number
}

export interface QuotaResponse {
  tier: ApiTier
  tier_label: string
  /** Quota day, `YYYY-MM-DD` (UTC+8, IYUU's reset boundary). */
  day: string
  daily_requests: number
  used_requests: number
  remaining_requests: number
  daily_hashes: number
  used_hashes: number
  remaining_hashes: number
  blocked_until: string | null
  reset_at: string | null
  percent_used: number
  denied_requests: number
  /** False until the user answers the tier question. */
  tier_chosen: boolean
  /**
   * Plan table for the picker. Note there is no host here on purpose: the API
   * base is a deployment detail (env var) and never reaches the browser.
   */
  tiers: TierInfo[]
}

export interface Run {
  id: string
  trigger: string
  status: string
  started_at: string
  finished_at: string | null
  stats: RunStats
  message: string | null
}

export interface RunProgress {
  run_id: string | null
  running: boolean
  trigger: string
  started_at: string | null
  finished_at: string | null
  stats: RunStats
  log: string[]
  phase: string
}

export interface Attempt {
  id: number
  source_hash: string
  source_name: string
  save_path: string
  target_info_hash: string
  target_sid: number
  target_site: string
  target_torrent_id: number
  state: ReseedState
  check_count: number
  last_checked_at: string | null
  verify_started_at: string | null
  added_at: string | null
  finished_at: string | null
  message: string | null
  downloader: string
  run_id: string | null
}

export interface SiteCredentials {
  passkey?: string | null
  uid?: string | null
  cookie?: string | null
  user_agent?: string | null
  torrent_key?: string | null
  rsskey?: string | null
  api_key?: string | null
}

export interface PublicCredentials {
  has_passkey: boolean
  /** Some sites (TGG, HDCity) take an authkey instead of a passkey. */
  has_torrent_key: boolean
  uid: string | null
  has_cookie: boolean
  user_agent: string | null
  has_api_key: boolean
  has_rsskey: boolean
}

export interface ReseedConfig {
  min_size_gib: number
  exclude_tags: string[]
  exclude_paths: string[]
  reuse_category: boolean
  default_category: string | null
  tags: string[]
  tag_with_site: boolean
  auto_start_after_verify: boolean
  fetch_concurrency: number
  verify_interval_secs: number
}

export interface ScheduleConfig {
  enabled: boolean
  cron: string
}

export interface PublicConfig {
  iyuu: {
    has_token: boolean
    token_hint: string
    tier: ApiTier
    /** False until the user answers the tier question. */
    tier_chosen: boolean
    daily_requests: number
    daily_hashes: number
  }
  downloader: {
    kind: string
    name: string
    base_url: string
    /** `password` or `api_key`. */
    auth: DownloaderAuthMode
    username: string
    has_password: boolean
    /** Presence only — the key itself is never returned. */
    has_api_key: boolean
    extra: { transmission_rpc_path: string | null }
  }
  reseed: ReseedConfig
  credentials: Record<string, PublicCredentials>
  schedule: ScheduleConfig
  ui: { language: string | null }
}

export interface SiteInfo {
  id: number
  site: string
  nickname: string
  base_url: string
  is_https: number
  reseed_check: string
  cookie_required: number
  has_credentials: boolean
}

/** One row of an import report. Never carries a secret value, only field names. */
export interface ImportSiteDetail {
  sid: number
  name: string
  fields: string[]
  status: 'imported' | 'replaced' | 'disabled'
}

export interface ImportResult {
  ok: boolean
  /** `false` when this was a dry run. */
  applied: boolean
  summary: string
  imported: number[]
  skipped_disabled: number[]
  replaced: number
  skipped_empty: number
  skipped_unkeyed: number
  details: ImportSiteDetail[]
  config: PublicConfig
}


// ------------------------------------------------------------- seeds (种子管理)

/**
 * 一组「同一份数据」的种子：主种子 + 各站点上的辅种子。
 *
 * 分组是后端推导出来的（下载器快照 + 辅种记录），不是持久化的实体，所以这里只有
 * 读模型，没有创建/更新的概念。
 */
export type SeedGroupState = 'healthy' | 'verifying' | 'degraded' | 'main_missing' | 'no_aux'

export interface SeedMain {
  hash: string
  name: string
  size: number
  save_path: string
  state: string
  phase: string
  progress: number
  seeding_time: number | null
  uploaded: number
  ratio: number
  added_on: string | null
  tags: string[]
  trackers: string[]
  /** false = 下载器里已经没有它了，其余字段是最后的记录快照。 */
  present: boolean
}

export interface SeedAuxLive {
  state: string
  phase: string
  progress: number
  seeding_time: number | null
  uploaded: number
  ratio: number
  added_on: string | null
}

export interface SeedAux {
  attempt_id: number
  site: string
  site_name: string
  sid: number
  torrent_id: number
  info_hash: string
  state: ReseedState
  check_count: number
  added_at: string | null
  last_checked_at: string | null
  finished_at: string | null
  message: string | null
  in_downloader: boolean
  live: SeedAuxLive | null
}

export interface SeedDuplicate {
  hash: string
  name: string
  uploaded: number
  added_on: string | null
}

export interface SeedGroupCounts {
  aux: number
  seeding: number
  verifying: number
  failed: number
}

export interface SeedGroup {
  key: string
  name: string
  size: number
  save_path: string
  category: string | null
  state: SeedGroupState
  main: SeedMain
  aux: SeedAux[]
  duplicates: SeedDuplicate[]
  counts: SeedGroupCounts
  updated_at: string | null
}

export interface SeedCounts {
  groups: number
  aux_total: number
  aux_seeding: number
  aux_verifying: number
  aux_failed: number
  with_failure: number
  no_aux: number
  main_missing: number
}

export interface SeedPage {
  total: number
  limit: number
  offset: number
  counts: SeedCounts
  items: SeedGroup[]
}

export interface TorrentItem {
  hash: string
  name: string
  size: number
  progress: number
  state: string
  phase: string
  category: string | null
  tags: string[]
  save_path: string
  seeding_time: number | null
  ratio: number
  uploaded: number
}

export interface DownloaderSummary {
  total: number
  complete: number
  checking: number
  errors: number
  total_size: number
  uploaded: number
}

export interface TestIyuuResult {
  ok: boolean
  tier: ApiTier
  /**
   * Whether the token is **activated**.
   *
   * `false` means the token itself is fine but no cooperative-site account has
   * been bound yet — IYUU answers `403 用户未绑定推荐站点` until one is. The UI must
   * point at the 合作站点 card, not at "check your token".
   */
  activated: boolean
  /** Human-readable outcome, already phrased for display. */
  message: string
  /** Only present when activated: how many sites IYUU can cross-seed into. */
  site_count?: number
  quota: Omit<QuotaResponse, 'tiers' | 'tier_chosen'>
}

/** Threshold/summary of the catalogue, plus whether it has ever been fetched. */
export interface SitesResponse {
  sites: SiteInfo[]
  /**
   * `false` = `/reseed/sites/index` has never succeeded. The UI shows a "pull the
   * list" prompt instead of an unexplained empty picker.
   */
  initialised: boolean
  /** ISO-8601 of the last successful fetch, or null. */
  synced_at: string | null
}

/**
 * A **cooperative** site (`/reseed/sites/recommend`).
 *
 * Not a cross-seed target: these are the sites whose accounts can *activate* an
 * IYUU token by binding. The payload has no `base_url` — the server fills it in
 * from the catalogue when it can.
 */
export interface PartnerSite {
  id: number
  site: string
  nickname: string
  /** Comma-separated fields the bind call needs, e.g. `passkey,uid`. */
  bind_check: string
  reseed_check: string
  /** Whether the form should ask for the numeric user id. */
  needs_uid: boolean
  base_url: string
}

export interface PartnersResponse {
  partners: PartnerSite[]
  count: number
  quota: Omit<QuotaResponse, 'tiers' | 'tier_chosen'>
}

export interface BindPartnerResult {
  ok: boolean
  /**
   * Result of verifying activation right after the bind. `null` = the bind was
   * accepted but the check could not run (rate limit / network).
   */
  activated: boolean | null
  message: string
  quota: Omit<QuotaResponse, 'tiers' | 'tier_chosen'>
}

export interface ActivationResult {
  activated: boolean
  message: string
  quota: Omit<QuotaResponse, 'tiers' | 'tier_chosen'>
}

export interface TestDownloaderResult {
  ok: boolean
  kind: string
  /** Which mode actually authenticated — the form may be mid-edit. */
  auth: DownloaderAuthMode
  version: string
  torrent_count: number
  complete_count: number
}

// -------------------------------------------------------------------- api

export const api = {
  status: () => request<StatusResponse>('/status'),

  quota: () => request<QuotaResponse>('/quota'),

  getConfig: () => request<PublicConfig>('/config'),

  putConfig: (patch: unknown) =>
    request<{ ok: boolean; config: PublicConfig }>('/config', {
      method: 'PUT',
      body: JSON.stringify(patch),
    }),

  /**
   * Probe a token, optionally against a tier/host before saving it.
   *
   * Costs two real requests against the daily allowance, so the UI must tell the
   * user that rather than presenting it as free.
   */
  testIyuu: (body: { token?: string; tier?: ApiTier } = {}) =>
    request<TestIyuuResult>('/test/iyuu', {
      method: 'POST',
      body: JSON.stringify({
        token: body.token ?? null,
        tier: body.tier ?? null,
      }),
    }),

  /** A blank `password` / `api_key` means "use the saved one", as with the patch. */
  testDownloader: (body: {
    kind?: string
    base_url?: string
    auth?: DownloaderAuthMode
    username?: string
    password?: string
    api_key?: string
    rpc_path?: string
  }) =>
    request<TestDownloaderResult>('/test/downloader', {
      method: 'POST',
      body: JSON.stringify(body),
    }),

  /**
   * The catalogue, read from the server's database.
   *
   * Never spends IYUU quota — that is the point. `initialised: false` means the
   * first fetch has not succeeded yet, so the UI must prompt the user to refresh
   * rather than quietly showing an empty list.
   */
  sites: () => request<SitesResponse>('/sites'),

  /**
   * Refetch `/reseed/sites/index` and persist it. **Costs one IYUU request**, so
   * it belongs behind an explicit button, never on mount.
   */
  refreshSites: () => request<SitesResponse & { count: number }>('/sites/refresh', { method: 'POST' }),

  /**
   * Cooperative sites — the accounts that can activate the token.
   *
   * A different list from `sites()`: no `base_url` of its own, and it exists to
   * drive the bind form. **Costs one IYUU request.**
   */
  partners: () => request<PartnersResponse>('/partners'),

  /**
   * Bind a cooperative-site account, which activates the IYUU token.
   *
   * The passkey is sent in clear to our own server, which sha1s it before calling
   * IYUU — IYUU stores only the digest, and the raw value never leaves the host.
   */
  bindPartner: (body: { site: string; sid?: number | null; uid: string; passkey: string }) =>
    request<BindPartnerResult>('/partners/bind', {
      method: 'POST',
      body: JSON.stringify(body),
    }),

  /** Cheap one-request check of whether the token is activated yet. */
  checkActivation: () => request<ActivationResult>('/partners/activated'),

  /**
   * Save one site's credentials.
   *
   * Per-site rather than a whole-map patch: two browser tabs editing different
   * sites must not overwrite each other, and the blank secret fields in the form
   * only have a defined meaning ("unchanged") when the server merges into the
   * stored entry.
   */
  putSiteCredentials: (sid: number, body: SiteCredentials) =>
    request<{ ok: boolean; config: PublicConfig }>(`/sites/${sid}/credentials`, {
      method: 'PUT',
      body: JSON.stringify(body),
    }),

  /**
   * Import IYUU AutoSeed's site export.
   *
   * `apply: false` is a dry run that reports what would change and writes
   * nothing — the file comes from another app and the user should see the
   * outcome before it lands in their config.
   */
  importSites: (data: string, apply = true) =>
    request<ImportResult>('/sites/import', {
      method: 'POST',
      body: JSON.stringify({ data, apply }),
    }),

  /** Remove a site entirely, so runs stop targeting it. */
  deleteSiteCredentials: (sid: number) =>
    request<{ ok: boolean; config: PublicConfig }>(`/sites/${sid}/credentials`, {
      method: 'DELETE',
    }),

  runs: (limit = 20) => request<Run[]>(`/runs?limit=${limit}`),
  startRun: () =>
    request<{ accepted: boolean }>('/runs', { method: 'POST', body: JSON.stringify({}) }),
  progress: () => request<RunProgress>('/runs/progress'),
  cancelRun: () => request<{ cancelled: boolean }>('/runs/current/cancel', { method: 'POST' }),

  attempts: (params: { state?: string; limit?: number; offset?: number } = {}) => {
    const q = new URLSearchParams()
    if (params.state) q.set('state', params.state)
    if (params.limit != null) q.set('limit', String(params.limit))
    if (params.offset != null) q.set('offset', String(params.offset))
    const qs = q.toString()
    return request<{ total: number; limit: number; offset: number; items: Attempt[] }>(
      `/attempts${qs ? `?${qs}` : ''}`,
    )
  },
  attemptStats: () => request<{ counts: Record<string, number> }>('/attempts/stats'),
  requeueAttempt: (id: number) =>
    request<{ ok: boolean }>(`/attempts/${id}/requeue`, { method: 'POST' }),
  deleteAttempt: (id: number) =>
    request<{ ok: boolean }>(`/attempts/${id}`, { method: 'DELETE' }),

  /**
   * 主种子 + 辅种子分组视图。
   *
   * `counts` 是过滤之前的全量统计——页面顶部的数字要回答「一共还有多少问题」，
   * 不是「这一页筛出来多少」。
   */
  seeds: (params: { filter?: string; q?: string; limit?: number; offset?: number } = {}) => {
    const q = new URLSearchParams()
    if (params.filter && params.filter !== 'all') q.set('filter', params.filter)
    if (params.q) q.set('q', params.q)
    if (params.limit != null) q.set('limit', String(params.limit))
    if (params.offset != null) q.set('offset', String(params.offset))
    const qs = q.toString()
    return request<SeedPage>(`/seeds${qs ? `?${qs}` : ''}`)
  },

  torrents: (params: { filter?: string; limit?: number } = {}) => {
    const q = new URLSearchParams()
    if (params.filter) q.set('filter', params.filter)
    if (params.limit != null) q.set('limit', String(params.limit))
    const qs = q.toString()
    return request<{ items: TorrentItem[]; total: number }>(`/downloader/torrents${qs ? `?${qs}` : ''}`)
  },
  downloaderSummary: () => request<DownloaderSummary>('/downloader/summary'),
}
