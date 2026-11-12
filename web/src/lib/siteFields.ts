import type { PublicCredentials, SiteInfo } from '@/lib/api'

/**
 * Which credentials a site actually needs, taken from IYUU's own metadata
 * (`reseed_check`, `cookie_required`) plus the two hosts whose mechanism is
 * known to differ from the template.
 *
 * The distinction between `primary` and the rest is what the dialog shows by
 * default: a user adding OpenCD needs one passkey field, not seven boxes whose
 * meaning they have to look up. Everything else stays reachable behind
 * "其他字段" so no site is unconfigurable from the UI.
 */
export interface CredentialNeeds {
  /** The field(s) without which nothing can be downloaded. */
  primary: ('passkey' | 'torrent_key' | 'api_key' | 'rsskey')[]
  uid: boolean
  cookie: boolean
}

export function credentialNeeds(site: SiteInfo | null): CredentialNeeds {
  if (!site) {
    // Unknown site (its metadata is missing from the catalogue): assume the
    // common NexusPHP shape rather than showing every box.
    return { primary: ['passkey'], uid: false, cookie: false }
  }

  const check = (site.reseed_check || '').toLowerCase()
  const primary: CredentialNeeds['primary'] = []

  // M-Team mints download tokens from an API key; Monika-style sites embed an
  // rsskey in the RSS URL. Both are identified by host because their
  // `reseed_check` does not say so.
  if (site.base_url.includes('m-team')) {
    primary.push('api_key')
  } else if (site.base_url.includes('monikadesign')) {
    primary.push('rsskey')
  } else if (check.includes('authkey') || check.includes('torrent_key')) {
    // TGG / HDCity: an authkey plays the passkey's role.
    primary.push('torrent_key')
  } else {
    // Default for NexusPHP, including sites whose `reseed_check` is blank.
    primary.push('passkey')
  }

  return {
    primary,
    uid: check.includes('uid'),
    cookie: site.cookie_required === 1,
  }
}

export type CredentialField =
  | 'passkey'
  | 'torrent_key'
  | 'api_key'
  | 'rsskey'
  | 'uid'
  | 'cookie'
  | 'user_agent'

/** Human labels — the API field names would mean nothing in a PT context. */
export const FIELD_LABEL: Record<CredentialField, string> = {
  passkey: 'Passkey',
  torrent_key: 'Authkey',
  api_key: 'API Key',
  rsskey: 'RSS 链接 / rsskey',
  uid: 'UID',
  cookie: 'Cookie',
  user_agent: 'User-Agent',
}

export const FIELD_HINT: Partial<Record<CredentialField, string>> = {
  passkey: '站点「控制面板 → 密钥」里的 passkey（不是登录密码）',
  torrent_key: '部分站点用 authkey 代替 passkey',
  api_key: 'M-Team 的 API Key，用于换取下载令牌',
  rsskey: '粘贴完整 RSS 链接也可以，会自行提取 rsskey',
  uid: '站点用户 ID，数字；多数站点不需要',
  cookie: '整段 Cookie（含 uid/passkey 等字段）',
  user_agent: '站点要求与浏览器 UA 一致时填写',
}

/** Whether an already-saved entry covers a site's primary requirement. */
export function hasPrimary(
  need: CredentialNeeds,
  cred: PublicCredentials | undefined,
): boolean {
  if (!cred) return false
  return need.primary.some((f) => {
    switch (f) {
      case 'passkey':
        return cred.has_passkey
      case 'torrent_key':
        return cred.has_torrent_key
      case 'api_key':
        return cred.has_api_key
      case 'rsskey':
        return cred.has_rsskey
    }
  })
}

/** Whether anything at all is stored for a site. */
export function hasAnyCredential(cred: PublicCredentials | undefined): boolean {
  if (!cred) return false
  return (
    cred.has_passkey ||
    cred.has_torrent_key ||
    cred.has_cookie ||
    cred.has_api_key ||
    cred.has_rsskey
  )
}

/** One-line summary of what is stored, for the site card. */
export function storedSummary(cred: PublicCredentials | undefined): string[] {
  if (!cred) return []
  const out: string[] = []
  if (cred.has_passkey) out.push('passkey')
  if (cred.has_torrent_key) out.push('authkey')
  if (cred.has_api_key) out.push('api key')
  if (cred.has_rsskey) out.push('rsskey')
  if (cred.has_cookie) out.push('cookie')
  if (cred.uid) out.push(`uid ${cred.uid}`)
  return out
}
