<script setup lang="ts">
defineProps<{ colspan?: number; text?: string }>()
</script>
<template>
  <tr>
    <td :colspan="colspan ?? 8" class="text-muted-foreground px-2 py-12 text-center text-sm">
      <slot>{{ text ?? '暂无数据' }}</slot>
    </td>
  </tr>
</template>
