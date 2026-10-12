import { BellRing } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { cn } from '@/lib/utils'
import { SCREENS, type Screen, useScreen } from './hooks'
import { Notifications } from './screens/Notifications'
import { Settings } from './screens/Settings'
import { Status } from './screens/Status'

const LABELS: Record<Screen, string> = {
  notifications: '通知',
  settings: '設定',
  status: '受信状況',
}

export function App() {
  const screen = useScreen()

  return (
    <div className="min-h-svh">
      <header className="border-b">
        <div className="mx-auto flex w-full max-w-[56rem] flex-wrap items-center gap-x-4 gap-y-1 px-4 py-2 sm:h-14 sm:flex-nowrap sm:py-0 sm:px-6">
          <h1 className="flex items-center gap-2 text-sm font-semibold">
            <BellRing className="size-5" />
            <span>Angelic Angel</span>
          </h1>
          <nav
            aria-label="メイン"
            className="order-last flex w-full items-center gap-1 sm:order-none sm:w-auto sm:flex-1"
          >
            {SCREENS.map((s) => (
              <Button
                key={s}
                asChild
                variant="ghost"
                size="sm"
                className={cn(
                  'h-9 px-3 text-sm text-muted-foreground',
                  'aria-[current=page]:bg-muted aria-[current=page]:text-foreground',
                )}
              >
                <a href={`#/${s}`} aria-current={s === screen ? 'page' : undefined}>
                  {LABELS[s]}
                </a>
              </Button>
            ))}
          </nav>
        </div>
      </header>
      <main className="mx-auto w-full max-w-[56rem] px-4 py-8 sm:px-6">
        {screen === 'settings' && <Settings />}
        {screen === 'notifications' && <Notifications />}
        {screen === 'status' && <Status />}
      </main>
    </div>
  )
}
