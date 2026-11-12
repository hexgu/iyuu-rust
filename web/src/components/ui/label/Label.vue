<script setup lang="ts">
import type { HTMLAttributes } from 'vue'
import { Label as LabelPrimitive } from 'reka-ui'
import { cn } from '@/lib/utils'
const props = defineProps<{ for?: string; class?: HTMLAttributes['class'] }>()
</script>
<template>
  <LabelPrimitive
    :for="props.for"
    :class="cn('text-sm font-medium leading-none select-none', props.class)"
  >
    <slot />
  </LabelPrimitive>
</template>
