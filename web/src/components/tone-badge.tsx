import type { ComponentProps } from 'react'
import { Badge } from '@/components/ui/badge'
import { cn } from '@/lib/utils'

const TONES = {
  ok: 'border-emerald-500/30 bg-emerald-500/10 text-emerald-400',
  warn: 'border-amber-500/30 bg-amber-500/10 text-amber-400',
  danger: 'border-destructive/30 bg-destructive/10 text-destructive',
  neutral: 'border-border text-muted-foreground',
} as const

export type Tone = keyof typeof TONES

export function ToneBadge({
  tone,
  className,
  children,
  ...props
}: Omit<ComponentProps<typeof Badge>, 'variant'> & { tone: Tone }) {
  return (
    <Badge variant="outline" className={cn(TONES[tone], className)} {...props}>
      <span aria-hidden className="size-1.5 rounded-full bg-current" />
      {children}
    </Badge>
  )
}
