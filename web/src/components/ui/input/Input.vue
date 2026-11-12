<script setup lang="ts">
import type { HTMLAttributes } from 'vue'
import { computed } from 'vue'
import { cn } from '@/lib/utils'

/**
 * Declaring the native input attributes we forward explicitly (rather than
 * relying on `$attrs` fall-through) keeps vue-tsc's template type-checking
 * honest: with fall-through only, binding a `placeholder` string that looks like
 * a URL makes the compiler re-infer the *parent's* reactive object as this
 * component's props, which produces bogus "Property 'value' does not exist"
 * errors at completely unrelated lines.
 */
const props = defineProps<{
  modelValue?: string | number | null
  placeholder?: string
  type?: string
  id?: string
  autocomplete?: string
  disabled?: boolean
  min?: string | number
  max?: string | number
  step?: string | number
  class?: HTMLAttributes['class']
}>()
const emits = defineEmits<{ 'update:modelValue': [value: string | number | null] }>()

const model = computed({
  get: () => props.modelValue ?? '',
  set: (v: string | number | null) => emits('update:modelValue', v),
})
</script>
<template>
  <input
    v-model="model"
    :type="props.type ?? 'text'"
    :id="props.id"
    :placeholder="props.placeholder"
    :autocomplete="props.autocomplete"
    :disabled="props.disabled"
    :min="props.min"
    :max="props.max"
    :step="props.step"
    :class="cn(
      'border-input placeholder:text-muted-foreground flex h-9 w-full min-w-0 rounded-md border bg-transparent px-3 py-1 text-sm shadow-xs transition-[color,box-shadow] outline-none',
      'focus-visible:border-ring focus-visible:ring-ring/50 focus-visible:ring-[3px]',
      'disabled:pointer-events-none disabled:cursor-not-allowed disabled:opacity-50',
      props.class,
    )"
  />
</template>
