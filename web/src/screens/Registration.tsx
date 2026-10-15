import { LoaderCircle } from 'lucide-react'
import { useRef, useState } from 'react'
import { Message } from '@/components/message'
import { LoadFailure, Loading, Section } from '@/components/screen'
import { ToneBadge } from '@/components/tone-badge'
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from '@/components/ui/alert-dialog'
import { Button } from '@/components/ui/button'
import { api, errorText } from '../api'
import { type Exclusive, useLoad } from '../hooks'

type Kind = 'register' | 'unregister'

const TITLE = '受信先の登録'
const DESCRIPTION = 'X の通知を受け取るための Web Push 登録を管理します。'

/** `onChanged` runs after a successful change so the parent can refresh what depends on it. */
export function Registration({
  accountId,
  configured,
  lock,
  onChanged,
}: {
  accountId: string
  configured: boolean
  lock: Exclusive
  onChanged: () => Promise<void>
}) {
  const reg = useLoad(() => api.getRegistration(accountId))
  const [confirming, setConfirming] = useState(false)
  const [actionError, setActionError] = useState<string | null>(null)
  const [lastAction, setLastAction] = useState<Kind | null>(null)
  const retryRef = useRef<HTMLButtonElement>(null)
  const openedFromRetry = useRef(false)

  const busy = lock.active !== null

  function changeConfirming(open: boolean) {
    if (open) openedFromRetry.current = false
    setConfirming(open)
  }

  function retry() {
    if (lastAction === 'unregister') {
      openedFromRetry.current = true
      setConfirming(true)
    } else if (lastAction) {
      void run(lastAction)
    }
  }

  function run(kind: Kind) {
    setLastAction(kind)
    setActionError(null)
    setConfirming(false)
    return lock.run(kind, async () => {
      try {
        reg.setData(
          await (kind === 'register' ? api.register(accountId) : api.unregister(accountId)),
        )
        await onChanged()
      } catch (e) {
        setActionError(errorText(e))
      }
    })
  }

  if (reg.loading && !reg.data) {
    return (
      <Section title={TITLE} description={DESCRIPTION}>
        <Loading />
      </Section>
    )
  }
  if (!reg.data) {
    return (
      <Section title={TITLE} description={DESCRIPTION}>
        <LoadFailure message={errorText(reg.error)} onRetry={reg.reload} />
      </Section>
    )
  }

  const { registered, uaid, channel_id } = reg.data

  return (
    <Section title={TITLE} description={DESCRIPTION}>
      <div className="space-y-4">
        <ToneBadge tone={registered ? 'ok' : 'neutral'}>
          {registered ? '登録済み' : '未登録'}
        </ToneBadge>
        {!registered && !configured && (
          <Message tone="info">登録するには、先に上の Cookie を保存してください。</Message>
        )}
      </div>
      <dl className="grid gap-x-12 gap-y-4 text-sm sm:grid-cols-[8rem_minmax(0,1fr)]">
        <dt className="text-muted-foreground">UAID</dt>
        <dd className="font-mono break-all">{uaid ?? '—'}</dd>
        <dt className="text-muted-foreground">チャンネル ID</dt>
        <dd className="font-mono break-all">{channel_id ?? '—'}</dd>
      </dl>
      <div className="space-y-4">
        <div className="flex flex-wrap items-center gap-3">
          <Button
            type="button"
            className="h-10 min-w-28 px-5"
            disabled={busy || registered || !configured}
            onClick={() => run('register')}
          >
            {lock.active === 'register' && (
              <LoaderCircle data-icon="inline-start" className="animate-spin" />
            )}
            {lock.active === 'register' ? '登録中…' : '登録'}
          </Button>
          <AlertDialog open={confirming} onOpenChange={changeConfirming}>
            <AlertDialogTrigger asChild>
              <Button
                type="button"
                variant="destructive"
                className="h-10 min-w-28 px-5"
                disabled={busy || !registered}
              >
                {lock.active === 'unregister' && (
                  <LoaderCircle data-icon="inline-start" className="animate-spin" />
                )}
                {lock.active === 'unregister' ? '解除中…' : '登録解除'}
              </Button>
            </AlertDialogTrigger>
            <AlertDialogContent
              onCloseAutoFocus={(e) => {
                if (openedFromRetry.current && retryRef.current) {
                  e.preventDefault()
                  retryRef.current.focus()
                }
              }}
            >
              <AlertDialogHeader>
                <AlertDialogTitle>登録解除の確認</AlertDialogTitle>
                <AlertDialogDescription>
                  登録を解除すると、通知の受信が止まります。解除しますか？
                </AlertDialogDescription>
              </AlertDialogHeader>
              <AlertDialogFooter>
                <AlertDialogCancel className="h-10 px-5">キャンセル</AlertDialogCancel>
                <AlertDialogAction
                  variant="destructive"
                  className="h-10 px-5"
                  disabled={busy}
                  onClick={() => run('unregister')}
                >
                  解除する
                </AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
        </div>
        {actionError && (
          <div role="alert" className="space-y-3">
            <Message tone="error">{actionError}</Message>
            <Button
              ref={retryRef}
              type="button"
              variant="outline"
              className="h-10"
              disabled={busy}
              onClick={retry}
            >
              再試行
            </Button>
          </div>
        )}
      </div>
    </Section>
  )
}
