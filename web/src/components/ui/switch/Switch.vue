<script setup lang="ts">
import type { HTMLAttributes } from 'vue'
import { computed } from 'vue'
import { SwitchRoot, SwitchThumb } from 'reka-ui'
import { cn } from '@/lib/utils'

const props = defineProps<{
  modelValue?: boolean
  disabled?: boolean
  class?: HTMLAttributes['class']
}>()
const emits = defineEmits<{ 'update:modelValue': [value: boolean] }>()

// reka-ui's SwitchRoot emits `update:modelValue`, so a computed round-trips
// cleanly and keeps the type visible to the template compiler.
const model = computed({
  get: () => props.modelValue ?? false,
  set: (v: boolean) => emits('update:modelValue', v),
})
</script>
<template>
  <SwitchRoot
    v-model="model"
    :disabled="props.disabled"
    :class="cn(
      'peer inline-flex h-5 w-9 shrink-0 cursor-pointer items-center rounded-full border-2 border-transparent transition-colors',
      'data-[state=checked]:bg-primary data-[state=unchecked]:bg-input',
      'focus-visible:ring-ring/50 focus-visible:ring-[3px] outline-none disabled:cursor-not-allowed disabled:opacity-50',
      props.class,
    )"
  >
    <SwitchThumb
      class="bg-background pointer-events-none block size-4 rounded-full shadow-lg ring-0 transition-transform data-[state=checked]:translate-x-4 data-[state=unchecked]:translate-x-0"
    />
  </SwitchRoot>
</template>
