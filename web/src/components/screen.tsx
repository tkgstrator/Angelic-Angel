import { LoaderCircle } from 'lucide-react'
import { type ComponentProps, type ReactNode, useId } from 'react'
import { Message } from '@/components/message'
import { Button } from '@/components/ui/button'
import { cn } from '@/lib/utils'

export function Screen({
  title,
  description,
  children,
}: {
  title: string
  description: string
  children?: ReactNode
}) {
  return (
    <section className="space-y-8">
      <div className="space-y-1.5">
        <h2 className="text-2xl font-semibold tracking-tight">{title}</h2>
        <p className="text-sm text-muted-foreground">{description}</p>
      </div>
      {children}
    </section>
  )
}

export function Section({
  title,
  description,
  className,
  children,
  ...props
}: {
  title: string
  description?: string
} & Omit<ComponentProps<'fieldset'>, 'title'>) {
  const id = useId()
  return (
    <fieldset
      aria-labelledby={id}
      className={cn(
        'grid min-w-0 gap-6 md:grid-cols-[minmax(0,1fr)_minmax(0,1.5fr)] md:gap-12',
        className,
      )}
      {...props}
    >
      <div className="space-y-1.5">
        <h3 id={id} className="text-base font-medium">
          {title}
        </h3>
        {description && <p className="text-sm text-muted-foreground">{description}</p>}
      </div>
      <div className="min-w-0 space-y-6">{children}</div>
    </fieldset>
  )
}

/** Actions that end a form: same two-column grid as `Section`, aligned to the end of the fields. */
export function SectionActions({ children }: { children: ReactNode }) {
  return (
    <div className="grid md:grid-cols-[minmax(0,1fr)_minmax(0,1.5fr)] md:gap-12">
      <div className="flex min-w-0 flex-col gap-3 md:col-start-2 md:flex-row md:items-center md:justify-end md:gap-4">
        {children}
      </div>
    </div>
  )
}

export function Loading() {
  return (
    <p role="status" className="flex items-center gap-2 text-sm text-muted-foreground">
      <LoaderCircle className="size-4 animate-spin" />
      読み込み中…
    </p>
  )
}

export function LoadFailure({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div className="space-y-4">
      <Message tone="error" role="alert">
        {message}
      </Message>
      <Button type="button" variant="outline" className="h-10 px-4" onClick={onRetry}>
        再試行
      </Button>
    </div>
  )
}
