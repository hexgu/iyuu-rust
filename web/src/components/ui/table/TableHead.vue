<script setup lang="ts">
import { cn } from '@/lib/utils'
const props = defineProps<{ class?: string }>()
</script>
<template>
  <th
    :class="cn('text-muted-foreground h-9 px-2 text-left align-middle text-xs font-medium whitespace-nowrap', props.class)"
  >
    <slot />
  </th>
</template>
