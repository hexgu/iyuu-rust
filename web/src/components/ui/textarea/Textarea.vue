<script setup lang="ts">
import type { HTMLAttributes } from 'vue'
import { computed } from 'vue'
import { cn } from '@/lib/utils'

const props = defineProps<{
  modelValue?: string
  placeholder?: string
  rows?: number
  class?: HTMLAttributes['class']
}>()
const emits = defineEmits<{ 'update:modelValue': [value: string] }>()

const model = computed({
  get: () => props.modelValue ?? '',
  set: (v: string) => emits('update:modelValue', v),
})
</script>
<template>
  <textarea
    v-model="model"
    :rows="props.rows ?? 3"
    :placeholder="props.placeholder"
    :class="cn(
      'border-input placeholder:text-muted-foreground flex min-h-16 w-full rounded-md border bg-transparent px-3 py-2 text-sm shadow-xs outline-none',
      'focus-visible:border-ring focus-visible:ring-ring/50 focus-visible:ring-[3px]',
      'font-mono',
      props.class,
    )"
  />
</template>
