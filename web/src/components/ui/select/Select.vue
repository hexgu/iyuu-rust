<script setup lang="ts">
import { computed } from 'vue'
import { cn } from '@/lib/utils'

const props = defineProps<{
  modelValue?: string
  options: { value: string; label: string }[]
  class?: string
  disabled?: boolean
}>()
const emits = defineEmits<{ 'update:modelValue': [value: string] }>()

const model = computed({
  get: () => props.modelValue ?? '',
  set: (v: string) => emits('update:modelValue', v),
})
</script>
<template>
  <select
    v-model="model"
    :disabled="props.disabled"
    :class="cn(
      'border-input flex h-9 w-full rounded-md border bg-transparent px-3 py-1 text-sm shadow-xs outline-none cursor-pointer',
      'focus-visible:border-ring focus-visible:ring-ring/50 focus-visible:ring-[3px]',
      'disabled:cursor-not-allowed disabled:opacity-50',
      props.class,
    )"
  >
    <option v-for="o in options" :key="o.value" :value="o.value">{{ o.label }}</option>
  </select>
</template>
