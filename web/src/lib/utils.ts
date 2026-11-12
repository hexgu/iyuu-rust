import { clsx, type ClassValue } from 'clsx'
import { twMerge } from 'tailwind-merge'

/** Tailwind-aware class merge, the shadcn-vue convention. */
export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}

/** Human-readable byte size. */
export function formatBytes(bytes: number | null | undefined): string {
  if (!bytes || bytes <= 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB']
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1)
  const value = bytes / Math.pow(1024, i)
  // Keep 1 decimal for GB+ so a 8.71 GB torrent reads as such.
  return `${value.toFixed(i >= 3 ? 2 : i === 0 ? 0 : 1)} ${units[i]}`
}

/** Shorten a 40-char info-hash for table display. */
export function shortHash(hash: string, head = 8, tail = 6): string {
  if (!hash || hash.length <= head + tail + 1) return hash ?? ''
  return `${hash.slice(0, head)}…${hash.slice(-tail)}`
}

/**
 * Duration in seconds → "3天 4小时" / "12分钟".
 * Used for seeding time and run duration; both are routinely days long, so a
 * plain mm:ss would be unreadable.
 */
export function formatDuration(seconds: number | null | undefined): string {
  if (!seconds || seconds <= 0) return '—'
  const d = Math.floor(seconds / 86400)
  const h = Math.floor((seconds % 86400) / 3600)
  const m = Math.floor((seconds % 3600) / 60)
  if (d > 0) return `${d}天 ${h}小时`
  if (h > 0) return `${h}小时 ${m}分`
  if (m > 0) return `${m}分`
  return `${Math.floor(seconds)}秒`
}

/** Relative time from an ISO timestamp, e.g. "3分钟前". */
export function timeAgo(iso: string | null | undefined): string {
  if (!iso) return '—'
  const then = new Date(iso).getTime()
  if (Number.isNaN(then)) return '—'
  const diff = Math.floor((Date.now() - then) / 1000)
  if (diff < 0) return '刚刚'
  if (diff < 60) return `${diff}秒前`
  if (diff < 3600) return `${Math.floor(diff / 60)}分钟前`
  if (diff < 86400) return `${Math.floor(diff / 3600)}小时前`
  if (diff < 2592000) return `${Math.floor(diff / 86400)}天前`
  return new Date(iso).toLocaleDateString('zh-CN')
}

/** Absolute local timestamp for tooltips. */
export function formatDateTime(iso: string | null | undefined): string {
  if (!iso) return '—'
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return '—'
  return d.toLocaleString('zh-CN', { hour12: false })
}

/** 0..1 → "98.5%". */
export function formatPercent(v: number | null | undefined): string {
  if (v == null) return '—'
  return `${(v * 100).toFixed(v >= 0.9995 ? 0 : 1)}%`
}
