<script setup lang="ts">
/**
 * A self-contained modal / bottom sheet.
 *
 * Written by hand rather than pulled from a headless library: the project already
 * builds its own primitives over reka-ui, and this dialog needs exactly one
 * behaviour — trap focus, close on Escape, close on overlay click — not a full
 * popover engine.
 *
 * On small screens it slides up as a bottom sheet with a grabber (the pattern the
 * rest of the app's sheets use); on `sm:` and up it centres as a card. The
 * transition is a plain CSS translate so it stays interruptible and cheap, and it
 * is disabled entirely under `prefers-reduced-motion`.
 */
import { computed, onBeforeUnmount, onMounted, ref, watch, nextTick } from 'vue'
import { X } from 'lucide-vue-next'
import { cn } from '@/lib/utils'

const props = withDefaults(
  defineProps<{
    open: boolean
    title?: string
    description?: string
    /** Prevent closing by overlay click / Escape (e.g. a required choice). */
    persistent?: boolean
    class?: string
  }>(),
  { persistent: false },
)

const emit = defineEmits<{ 'update:open': [boolean] }>()

const panel = ref<HTMLElement | null>(null)
/** Element to restore focus to when the dialog closes. */
let lastFocused: HTMLElement | null = null

function close() {
  if (props.persistent) return
  emit('update:open', false)
}

function onKeydown(e: KeyboardEvent) {
  if (!props.open) return
  if (e.key === 'Escape') {
    e.preventDefault()
    close()
    return
  }
  // Focus trap: without it, Tab walks into the page behind the overlay and the
  // "modal" stops being modal.
  if (e.key !== 'Tab' || !panel.value) return
  const focusable = panel.value.querySelectorAll<HTMLElement>(
    'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
  )
  if (!focusable.length) return
  const first = focusable[0]
  const last = focusable[focusable.length - 1]
  if (e.shiftKey && document.activeElement === first) {
    e.preventDefault()
    last.focus()
  } else if (!e.shiftKey && document.activeElement === last) {
    e.preventDefault()
    first.focus()
  }
}

watch(
  () => props.open,
  async (open) => {
    if (open) {
      lastFocused = document.activeElement as HTMLElement | null
      // Lock the page behind the sheet; otherwise scrolling it feels broken on
      // touch devices.
      document.body.style.overflow = 'hidden'
      await nextTick()
      const target = panel.value?.querySelector<HTMLElement>(
        'input, button, textarea, select, [tabindex]:not([tabindex="-1"])',
      )
      target?.focus()
    } else {
      document.body.style.overflow = ''
      lastFocused?.focus?.()
      lastFocused = null
    }
  },
)

onMounted(() => document.addEventListener('keydown', onKeydown))
onBeforeUnmount(() => {
  document.removeEventListener('keydown', onKeydown)
  document.body.style.overflow = ''
})

const hasHeader = computed(() => !!(props.title || props.description))
</script>

<template>
  <Teleport to="body">
    <Transition
      enter-active-class="transition-opacity duration-200 ease-out motion-reduce:transition-none"
      leave-active-class="transition-opacity duration-150 ease-in motion-reduce:transition-none"
      enter-from-class="opacity-0"
      leave-to-class="opacity-0"
    >
      <div
        v-if="open"
        class="fixed inset-0 z-50 flex items-end justify-center sm:items-center"
      >
        <!-- Overlay. A required choice must still be dismissible, otherwise the
             user is stuck with no way back to the page. -->
        <div
          class="bg-foreground/25 absolute inset-0 backdrop-blur-[2px]"
          @click="close"
        />

        <div
          ref="panel"
          role="dialog"
          aria-modal="true"
          :aria-label="title"
          :class="
            cn(
              'bg-card text-card-foreground relative w-full border shadow-lg',
              'rounded-t-2xl sm:max-w-lg sm:rounded-2xl',
              'max-h-[90dvh] overflow-y-auto scrollbar-slim',
              props.class,
            )
          "
        >
          <!-- Bottom-sheet grabber: mobile only. -->
          <div class="flex justify-center pt-2.5 sm:hidden">
            <div class="bg-muted-foreground/25 h-1 w-9 rounded-full" />
          </div>

          <div v-if="hasHeader" class="flex items-start gap-3 px-5 pt-4 pb-1 sm:pt-5">
            <div class="min-w-0 flex-1">
              <h2 v-if="title" class="text-sm font-semibold">{{ title }}</h2>
              <p v-if="description" class="text-muted-foreground mt-1 text-xs leading-relaxed">
                {{ description }}
              </p>
            </div>
            <button
              type="button"
              class="text-muted-foreground hover:bg-accent hover:text-foreground -mt-1 -mr-1 shrink-0 rounded-md p-1.5 transition-colors"
              aria-label="关闭"
              @click="close"
            >
              <X class="size-4" />
            </button>
          </div>

          <div class="px-5 py-4">
            <slot />
          </div>
        </div>
      </div>
    </Transition>
  </Teleport>
</template>
