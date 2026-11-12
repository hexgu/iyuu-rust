<script setup lang="ts">
import type { HTMLAttributes } from 'vue'
import { cn } from '@/lib/utils'
import { alertVariants, type AlertVariants } from '.'
const props = withDefaults(
  defineProps<{ variant?: AlertVariants['variant']; class?: HTMLAttributes['class'] }>(),
  { variant: 'default' },
)
</script>
<template>
  <div role="alert" :class="cn(alertVariants({ variant: props.variant }), props.class)">
    <slot />
  </div>
</template>
