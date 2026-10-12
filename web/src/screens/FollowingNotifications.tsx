import { LoaderCircle } from 'lucide-react'
import { useState } from 'react'
import { Message } from '@/components/message'
import { Loading, Section } from '@/components/screen'
import { type Tone, ToneBadge } from '@/components/tone-badge'
import { Button } from '@/components/ui/button'
import {
  Table,
  TableBody,
  TableCaption,
  TableCell,
  TableHead,
  TableRow,
} from '@/components/ui/table'
import {
  api,
  type FollowingNotifications as Dto,
  errorText,
  type FollowingNotificationsState as State,
} from '../api'
import { formatCount } from '../format'
import { type Exclusive, usePoll } from '../hooks'

const POLL_MS = 2000

const STATE_LABEL: Record<State, string> = {
  idle: '未実行',
  loading: '対象を取得中',
  running: '実行中',
  cancelling: '中止処理中',
  completed: '完了',
  failed: '失敗',
  cancelled: '中止済み',
}

const STATE_TONE: Record<State, Tone> = {
  idle: 'neutral',
  loading: 'warn',
  running: 'warn',
  cancelling: 'warn',
  completed: 'ok',
  failed: 'danger',
  cancelled: 'neutral',
}

const isActive = (state: State) =>
  state === 'loading' || state === 'running' || state === 'cancelling'

function Outcome({ dto }: { dto: Dto }) {
  const done = formatCount(dto.success_count)
  const detail = dto.last_error ? `: ${dto.last_error}` : ''
  switch (dto.state) {
    case 'completed':
      return dto.failed_count > 0 ? (
        <Message tone="error">
          完了しましたが、{formatCount(dto.failed_count)} 件で失敗しました{detail}
        </Message>
      ) : (
        <Message tone="ok">完了しました。{done} 件をオンにしました。</Message>
      )
    case 'failed':
      return (
        <div role="alert" className="space-y-2">
          <Message tone="error">失敗しました{detail}</Message>
          <Message tone="info">成功済みの {done} 件はオンのままで、元には戻りません。</Message>
        </div>
      )
    case 'cancelled':
      return <Message tone="info">中止しました。成功済みの {done} 件はオンのままです。</Message>
    case 'cancelling':
      return <Message tone="info">中止を要求しました。停止するまでお待ちください。</Message>
    default:
      return null
  }
}

function Progress({ dto }: { dto: Dto }) {
  const rows = [
    ['対象', dto.total],
    ['成功', dto.success_count],
    ['失敗', dto.failed_count],
    ['残り', dto.remaining_count],
  ] as const
  return (
    <Table>
      <TableCaption className="sr-only">進捗</TableCaption>
      <TableBody>
        {rows.map(([label, count]) => (
          <TableRow key={label}>
            <TableHead scope="row" className="h-10 font-normal text-muted-foreground">
              {label}
            </TableHead>
            <TableCell className="text-right font-mono tabular-nums">
              {count === null ? '—' : formatCount(count)}
            </TableCell>
          </TableRow>
        ))}
      </TableBody>
    </Table>
  )
}

/** Push registration is deliberately not required: only the saved cookies are. */
export function FollowingNotifications({
  configured,
  lock,
}: {
  configured: boolean
  lock: Exclusive
}) {
  const poll = usePoll(api.getFollowingNotifications, POLL_MS)
  const [confirming, setConfirming] = useState(false)
  const [actionError, setActionError] = useState<string | null>(null)

  const dto = poll.data
  const active = dto !== null && isActive(dto.state)
  const cancellable = dto !== null && (dto.state === 'loading' || dto.state === 'running')
  const busy = lock.active !== null
  const starting = lock.active === 'bulk-start'
  const cancelling = lock.active === 'bulk-cancel' || dto?.state === 'cancelling'

  function run(kind: 'start' | 'cancel') {
    setActionError(null)
    setConfirming(false)
    return lock.run(`bulk-${kind}`, async () => {
      try {
        poll.setData(
          await (kind === 'start'
            ? api.startFollowingNotifications()
            : api.cancelFollowingNotifications()),
        )
      } catch (e) {
        setActionError(errorText(e))
        poll.refresh()
      }
    })
  }

  return (
    <Section
      title="フォロー中の全員の投稿通知をオン"
      description="X でフォロー中のすべてのアカウントについて、投稿の通知をオンにします。"
    >
      {!configured && (
        <Message tone="info">実行するには、先に上の Cookie を保存してください。</Message>
      )}
      {dto === null ? (
        poll.loaded && poll.error ? (
          <div className="space-y-3">
            <Message tone="error" role="alert">
              {errorText(poll.error)}
            </Message>
            <Button type="button" variant="outline" className="h-10" onClick={poll.refresh}>
              再試行
            </Button>
          </div>
        ) : (
          <Loading />
        )
      ) : (
        <div className="space-y-4">
          {poll.error !== null && (
            <Message tone="error" role="alert">
              最新の状態を取得できません: {errorText(poll.error)}
            </Message>
          )}
          <ToneBadge tone={STATE_TONE[dto.state]} className="h-7 px-3 text-sm">
            {STATE_LABEL[dto.state]}
          </ToneBadge>
          {dto.state !== 'idle' && <Progress dto={dto} />}
          <Outcome dto={dto} />
        </div>
      )}
      <div className="space-y-4">
        <div className="flex flex-wrap items-center gap-3">
          <Button
            type="button"
            className="h-10 min-w-28 px-5"
            disabled={busy || dto === null || active || !configured}
            aria-expanded={confirming}
            onClick={() => setConfirming(true)}
          >
            {starting && <LoaderCircle data-icon="inline-start" className="animate-spin" />}
            {starting ? '開始中…' : 'オンにする'}
          </Button>
          <Button
            type="button"
            variant="outline"
            className="h-10 min-w-28 px-5"
            disabled={busy || !cancellable}
            onClick={() => run('cancel')}
          >
            {cancelling && <LoaderCircle data-icon="inline-start" className="animate-spin" />}
            {cancelling ? '中止中…' : '中止'}
          </Button>
        </div>
        {confirming && !busy && !active && (
          <fieldset
            aria-label="全員の通知オンの確認"
            className="min-w-0 space-y-4 border-t pt-4 [&_button]:h-10"
          >
            <p className="text-sm">
              X
              側でフォロー中の全アカウントの通知設定が変更されます。途中で中止しても、成功した分は元に戻りません。実行しますか？
            </p>
            <div className="flex flex-wrap items-center gap-3">
              <Button type="button" onClick={() => run('start')}>
                実行する
              </Button>
              <Button type="button" variant="outline" onClick={() => setConfirming(false)}>
                キャンセル
              </Button>
            </div>
          </fieldset>
        )}
        {actionError && (
          <Message tone="error" role="alert">
            {actionError}
          </Message>
        )}
      </div>
    </Section>
  )
}
