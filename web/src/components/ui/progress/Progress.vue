<script setup lang="ts">
import { computed } from 'vue'
import { cn } from '@/lib/utils'

const props = withDefaults(
  defineProps<{ value?: number; class?: string; indicatorClass?: string }>(),
  { value: 0 },
)
// Values arrive as 0..1 from the API; the bar works in percent.
const pct = computed(() => Math.min(100, Math.max(0, (props.value ?? 0) * 100)))
</script>
<template>
  <div :class="cn('bg-muted relative h-1.5 w-full overflow-hidden rounded-full', props.class)">
    <div
      :class="cn('bg-primary h-full rounded-full transition-all duration-300', props.indicatorClass)"
      :style="{ width: `${pct}%` }"
    />
  </div>
</template>
