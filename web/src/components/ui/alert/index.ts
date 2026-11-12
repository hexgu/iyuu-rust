import { cva, type VariantProps } from 'class-variance-authority'
export { default as Alert } from './Alert.vue'
export { default as AlertTitle } from './AlertTitle.vue'
export { default as AlertDescription } from './AlertDescription.vue'

export const alertVariants = cva(
  'relative w-full rounded-lg border px-4 py-3 text-sm grid gap-1 [&>svg]:size-4 [&>svg]:translate-y-0.5 grid-cols-[auto_1fr] [&>svg]:col-start-1 [&>div]:col-start-2',
  {
    variants: {
      variant: {
        default: 'bg-card text-card-foreground',
        destructive: 'border-destructive/50 text-destructive bg-destructive/10',
        warning: 'border-status-warn/40 text-status-warn bg-status-warn/10',
        success: 'border-status-success/40 text-status-success bg-status-success/10',
        info: 'border-status-active/40 text-status-active bg-status-active/10',
      },
    },
    defaultVariants: { variant: 'default' },
  },
)
export type AlertVariants = VariantProps<typeof alertVariants>
