<script setup lang="ts">
/**
 * 从 IYUU AutoSeed 的备份文件导入站点凭据。
 *
 * 流程刻意分成两步：
 *   1. 选文件 → **预览**（dry run，服务端不写任何东西）；
 *   2. 看清单没问题 → 确认导入。
 *
 * 因为这个文件来自另一个应用、用户多半没逐行读过，直接落盘等于让一个陌生文件
 * 决定往哪些站提交种子。预览把「会导入谁、会跳过谁、为什么跳过」摆出来，再让
 * 用户点头。
 *
 * 文件从不离开浏览器去别处：读成文本 POST 给本机后端，后端解析完只回计数和
 * 字段名，不回任何凭据值。
 */
import { computed, ref, watch } from 'vue'
import { AlertCircle, CheckCircle2, FileUp, Loader2, Upload } from 'lucide-vue-next'
import { api, ApiError, type ImportResult } from '@/lib/api'
import { cn } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Alert, AlertDescription } from '@/components/ui/alert'
import { Badge } from '@/components/ui/badge'
import { Dialog } from '@/components/ui/dialog'

const props = defineProps<{ open: boolean }>()
const emit = defineEmits<{
  'update:open': [boolean]
  /** Fired after a real (non-dry-run) import, with the new config. */
  imported: [config: unknown]
}>()

const fileName = ref('')
const payload = ref<string | null>(null)
const reading = ref(false)
const previewing = ref(false)
const applying = ref(false)
const result = ref<ImportResult | null>(null)
const error = ref<string | null>(null)

function reset() {
  fileName.value = ''
  payload.value = null
  result.value = null
  error.value = null
  reading.value = false
  previewing.value = false
  applying.value = false
}

watch(
  () => props.open,
  (open) => {
    if (open) reset()
  },
)

/** A 90-row export is tens of KB; anything huge is not this file. */
const MAX_BYTES = 8 * 1024 * 1024

async function onFile(e: Event) {
  const input = e.target as HTMLInputElement
  const file = input.files?.[0]
  if (!file) return

  error.value = null
  result.value = null
  fileName.value = file.name

  if (file.size > MAX_BYTES) {
    error.value = `文件过大（${(file.size / 1024 / 1024).toFixed(1)} MB），AutoSeed 的站点备份通常只有几十 KB`
    payload.value = null
    return
  }

  reading.value = true
  try {
    payload.value = await file.text()
    await run(false)
  } catch (err) {
    error.value = err instanceof ApiError ? err.message : String(err)
    payload.value = null
  } finally {
    reading.value = false
    // Re-selecting the same file must fire `change` again.
    input.value = ''
  }
}

async function run(apply: boolean) {
  if (!payload.value) return
  if (apply) applying.value = true
  else previewing.value = true
  error.value = null
  try {
    const r = await api.importSites(payload.value, apply)
    result.value = r
    if (apply) {
      emit('imported', r.config)
    }
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : String(e)
    result.value = null
  } finally {
    applying.value = false
    previewing.value = false
  }
}

const applied = computed(() => result.value?.applied === true)

/** Grouped so the user reads "who gets imported" before "how many were filler". */
const grouped = computed(() => {
  const d = result.value?.details ?? []
  return {
    imported: d.filter((x) => x.status === 'imported'),
    replaced: d.filter((x) => x.status === 'replaced'),
    disabled: d.filter((x) => x.status === 'disabled'),
  }
})

const FIELD_LABEL: Record<string, string> = {
  passkey: 'passkey',
  api_key: 'API Key',
  rsskey: 'RSS',
  cookie: 'cookie',
  uid: 'uid',
}
</script>

<template>
  <Dialog
    :open="open"
    title="导入 AutoSeed 备份"
    description="从 IYUU AutoSeed 导出的站点备份里恢复凭据，不用一个个手填"
    class="sm:max-w-2xl"
    @update:open="emit('update:open', $event)"
  >
    <div class="space-y-4">
      <div
        class="rounded-md border border-dashed px-4 py-5 text-center"
      >
        <input
          id="import-file"
          type="file"
          accept="application/json,.json"
          class="sr-only"
          @change="onFile"
        />
        <label
          for="import-file"
          class="hover:bg-accent inline-flex cursor-pointer items-center gap-2 rounded-md px-3 py-2 text-sm transition-colors"
        >
          <Loader2 v-if="reading || previewing" class="size-4 animate-spin" />
          <FileUp v-else class="size-4" />
          {{ fileName || '选择备份文件（.json）' }}
        </label>
        <p class="text-muted-foreground mt-2 text-[11px] leading-relaxed">
          选好文件会先在服务端做一次<strong class="text-foreground">预览</strong>，不会直接写入。<br />
          AutoSeed 里处于「禁用」状态的站点会被跳过，只报告不导入。
        </p>
      </div>

      <Alert v-if="error" variant="destructive">
        <AlertCircle />
        <AlertDescription>{{ error }}</AlertDescription>
      </Alert>

      <template v-if="result">
        <Alert :variant="applied ? 'success' : 'info'">
          <CheckCircle2 v-if="applied" />
          <AlertCircle v-else />
          <AlertDescription>
            <span class="font-medium">{{ applied ? '已导入' : '预览（尚未写入）' }}</span>
            · {{ result.summary }}
          </AlertDescription>
        </Alert>

        <div class="max-h-[38vh] space-y-3 overflow-y-auto scrollbar-slim">
          <!-- 会导入 / 会覆盖 -->
          <div v-if="grouped.imported.length || grouped.replaced.length">
            <div class="text-muted-foreground mb-1.5 text-[11px] font-medium">
              {{ applied ? '已导入' : '将导入' }}
              {{ grouped.imported.length + grouped.replaced.length }} 个
            </div>
            <div class="divide-y rounded-md border">
              <div
                v-for="d in [...grouped.imported, ...grouped.replaced]"
                :key="d.sid"
                class="flex flex-wrap items-center gap-1.5 px-3 py-2"
              >
                <span class="text-xs font-medium">{{ d.name || `站点 ${d.sid}` }}</span>
                <Badge
                  v-if="d.status === 'replaced'"
                  variant="secondary"
                  class="text-[10px]"
                >
                  覆盖原配置
                </Badge>
                <span class="text-muted-foreground font-mono text-[10px]">sid={{ d.sid }}</span>
                <span class="ml-auto flex flex-wrap gap-1">
                  <span
                    v-for="f in d.fields"
                    :key="f"
                    class="text-muted-foreground rounded border px-1 py-0.5 text-[10px]"
                  >
                    {{ FIELD_LABEL[f] ?? f }}
                  </span>
                </span>
              </div>
            </div>
          </div>

          <!-- 禁用而不导入 -->
          <div v-if="grouped.disabled.length">
            <div class="text-muted-foreground mb-1.5 text-[11px] font-medium">
              跳过（AutoSeed 里已禁用，共 {{ grouped.disabled.length }} 个）
            </div>
            <div class="divide-y rounded-md border border-dashed">
              <div
                v-for="d in grouped.disabled"
                :key="d.sid"
                class="flex items-center gap-1.5 px-3 py-2"
              >
                <span class="text-muted-foreground text-xs">{{ d.name || `站点 ${d.sid}` }}</span>
                <span class="text-muted-foreground font-mono text-[10px]">sid={{ d.sid }}</span>
                <span class="text-muted-foreground ml-auto text-[10px]">
                  备份里有凭据，但你把它关掉了
                </span>
              </div>
            </div>
          </div>

          <p v-if="result.skipped_empty" class="text-muted-foreground text-[11px]">
            另有 {{ result.skipped_empty }} 条是 IYUU 站点目录（没有任何凭据），已忽略。
          </p>
        </div>

        <p v-if="!applied" class="text-muted-foreground text-[11px] leading-relaxed">
          预览没有改动任何配置。确认无误后点「确认导入」。
        </p>
      </template>

      <div class="flex items-center justify-end gap-2 pt-1">
        <Button variant="ghost" size="sm" @click="emit('update:open', false)">
          {{ applied ? '关闭' : '取消' }}
        </Button>
        <Button
          v-if="result && !applied"
          size="sm"
          :disabled="applying"
          @click="run(true)"
        >
          <Loader2 v-if="applying" class="animate-spin" />
          <Upload v-else />
          确认导入
        </Button>
        <Button
          v-else-if="!result"
          size="sm"
          variant="outline"
          :disabled="!payload || previewing || reading"
          :class="cn((!payload || previewing) && 'opacity-60')"
          @click="run(false)"
        >
          <Loader2 v-if="previewing" class="animate-spin" />
          预览
        </Button>
      </div>
    </div>
  </Dialog>
</template>
