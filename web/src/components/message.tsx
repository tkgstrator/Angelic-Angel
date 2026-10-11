import { Check, CircleAlert, Info } from 'lucide-react'
import type { ComponentProps } from 'react'
import { cn } from '@/lib/utils'

const TONES = {
  error: { icon: CircleAlert, className: 'text-destructive' },
  ok: { icon: Check, className: 'text-emerald-400' },
  info: { icon: Info, className: 'text-muted-foreground' },
} as const

export function Message({
  tone,
  className,
  children,
  ...props
}: ComponentProps<'p'> & { tone: keyof typeof TONES }) {
  const { icon: Icon, className: toneClass } = TONES[tone]
  return (
    <p className={cn('flex items-start gap-2 text-sm', toneClass, className)} {...props}>
      <Icon className="mt-0.5 size-4 shrink-0" />
      <span>{children}</span>
    </p>
  )
}
