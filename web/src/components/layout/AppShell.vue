<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { RouterLink, useRoute } from 'vue-router'
import {
  Activity,
  Download,
  Globe,
  LayoutDashboard,
  ListChecks,
  RefreshCw,
  Settings,
  Moon,
  Sun,
} from 'lucide-vue-next'
import { api, ApiError, type StatusResponse } from '@/lib/api'
import { cn, formatDuration } from '@/lib/utils'
import { Button } from '@/components/ui/button'

const route = useRoute()

const NAV = [
  { to: '/', label: '概览', icon: LayoutDashboard },
  { to: '/attempts', label: '辅种记录', icon: ListChecks },
  { to: '/torrents', label: '下载器', icon: Download },
  { to: '/sites', label: '站点凭据', icon: Globe },
  { to: '/settings', label: '设置', icon: Settings },
]

const status = ref<StatusResponse | null>(null)
const statusError = ref<string | null>(null)
const dark = ref(true)
let timer: ReturnType<typeof setInterval> | undefined

async function refreshStatus() {
  try {
    status.value = await api.status()
    statusError.value = null
  } catch (e) {
    statusError.value = e instanceof ApiError ? e.message : String(e)
  }
}

function toggleTheme() {
  dark.value = !dark.value
  document.documentElement.classList.toggle('dark', dark.value)
  localStorage.setItem('iyuu-theme', dark.value ? 'dark' : 'light')
}

onMounted(() => {
  // Restore the theme choice; default to dark because this is a server tool
  // people leave open on a second monitor.
  const saved = localStorage.getItem('iyuu-theme')
  dark.value = saved ? saved === 'dark' : true
  document.documentElement.classList.toggle('dark', dark.value)

  refreshStatus()
  // 5s: fast enough to feel live during a run, cheap enough to leave open.
  timer = setInterval(refreshStatus, 5000)
})

onUnmounted(() => clearInterval(timer))

const running = computed(() => status.value?.running ?? false)
const pending = computed(() => status.value?.pending_verification ?? 0)
const seeding = computed(() => status.value?.verified_seeding ?? 0)
const unhealthy = computed(
  () => !!statusError.value || (status.value != null && !status.value.configured),
)
</script>

<template>
  <div class="bg-background flex min-h-screen flex-col">
    <!-- Top bar -->
    <header class="bg-background/80 sticky top-0 z-30 border-b backdrop-blur">
      <div class="mx-auto flex h-14 w-full max-w-7xl items-center gap-3 px-4">
        <RouterLink to="/" class="flex items-center gap-2.5">
          <div
            class="bg-primary text-primary-foreground flex size-7 items-center justify-center rounded-md text-[13px] font-bold"
          >
            IY
          </div>
          <span class="hidden text-sm font-semibold tracking-tight sm:block">
            iyuu-reseed
          </span>
        </RouterLink>

        <nav class="ml-2 flex items-center gap-0.5 overflow-x-auto scrollbar-slim">
          <RouterLink
            v-for="item in NAV"
            :key="item.to"
            :to="item.to"
            :class="cn(
              'flex items-center gap-1.5 rounded-md px-2.5 py-1.5 text-sm transition-colors whitespace-nowrap',
              route.path === item.to
                ? 'bg-secondary text-secondary-foreground font-medium'
                : 'text-muted-foreground hover:bg-accent hover:text-foreground',
            )"
          >
            <component :is="item.icon" class="size-4" />
            <span class="hidden md:inline">{{ item.label }}</span>
          </RouterLink>
        </nav>

        <div class="ml-auto flex items-center gap-2">
          <!-- Live counters: only the numbers that matter operationally. -->
          <div
            v-if="status && !unhealthy"
            class="hidden items-center gap-3 text-xs sm:flex"
          >
            <span v-if="running" class="text-status-active flex items-center gap-1.5">
              <RefreshCw class="size-3 animate-spin" />
              运行中
            </span>
            <span
              v-if="pending > 0"
              class="text-status-warn flex items-center gap-1.5"
              :title="`${pending} 个辅种等待校验结果`"
            >
              <Activity class="size-3" />
              {{ pending }} 待校验
            </span>
            <span
              v-if="seeding > 0"
              class="text-status-success flex items-center gap-1.5"
              :title="`${seeding} 个辅种已通过校验并在做种`"
            >
              ● {{ seeding }} 辅种中
            </span>
          </div>

          <span
            v-if="unhealthy"
            class="text-status-error hidden text-xs sm:inline"
            :title="statusError ?? '尚未配置'"
          >
            {{ statusError ? '后端未连接' : '未配置' }}
          </span>

          <span
            v-if="status"
            class="text-muted-foreground tabular hidden text-xs lg:inline"
            :title="'服务已运行时长'"
          >
            {{ formatDuration(status.uptime_secs) }}
          </span>

          <Button variant="ghost" size="icon" :title="dark ? '切换浅色' : '切换深色'" @click="toggleTheme">
            <Sun v-if="dark" class="size-4" />
            <Moon v-else class="size-4" />
          </Button>
        </div>
      </div>
    </header>

    <!-- Connection problem banner: state the actual problem, not "error". -->
    <div
      v-if="statusError"
      class="border-status-error/30 bg-status-error/10 text-status-error border-b px-4 py-2 text-center text-xs"
    >
      {{ statusError }}
    </div>
    <div
      v-else-if="status && !status.configured"
      class="border-status-warn/30 bg-status-warn/10 border-b px-4 py-2 text-center text-xs"
    >
      还没配置 IYUU Token。
      <RouterLink to="/settings" class="underline underline-offset-2">去设置</RouterLink>
    </div>

    <main class="mx-auto w-full max-w-7xl flex-1 px-4 py-6">
      <slot />
    </main>

    <footer class="text-muted-foreground border-t px-4 py-3 text-center text-[11px] leading-relaxed">
      <div>iyuu-reseed · 基于 IYUU 开放 API · 校验通过才计为成功</div>
      <div class="mt-0.5 opacity-80">
        本项目为第三方工具，<span class="text-foreground/80">并非 IYUU 官方项目</span>，
        不提供 IYUU 会员/额度相关的售后与连带责任；会员权益以
        <a
          href="https://iyuu.cn/app/user/member"
          target="_blank"
          rel="noreferrer"
          class="underline underline-offset-2"
        >IYUU 官方</a>
        为准
      </div>
    </footer>
  </div>
</template>
