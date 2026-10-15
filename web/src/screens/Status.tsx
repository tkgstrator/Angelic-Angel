import { Message } from '@/components/message'
import { Loading, Screen } from '@/components/screen'
import { type Tone, ToneBadge } from '@/components/tone-badge'
import { Separator } from '@/components/ui/separator'
import {
  Table,
  TableBody,
  TableCaption,
  TableCell,
  TableHead,
  TableRow,
} from '@/components/ui/table'
import { api, errorText, type ReceiverState, type Status as StatusDto } from '../api'
import { formatCount, formatTime } from '../format'
import { usePoll } from '../hooks'

const POLL_MS = 2000
const TITLE = '受信状況'
const DESCRIPTION = '受信の状態とカウンターを 2 秒ごとに更新します。'

const STATE_LABEL: Record<ReceiverState, string> = {
  unconfigured: '未設定',
  unregistered: '未登録',
  stopped: '停止中',
  connecting: '接続中',
  connected: '接続済み',
  retrying: '再接続待ち',
  registering: '登録処理中',
  unregistering: '解除処理中',
  error: 'エラー',
}

const STATE_NOTE: Partial<Record<ReceiverState, string>> = {
  unconfigured: '設定画面で Cookie を保存すると利用できます。',
  unregistered: '登録画面で登録すると受信を開始します。',
  stopped: '受信は停止しています。',
  retrying: '接続に失敗したため再試行しています。',
}

function tone(state: ReceiverState): Tone {
  if (state === 'connected') return 'ok'
  if (state === 'error') return 'danger'
  if (state === 'retrying') return 'warn'
  return 'neutral'
}

export function Status({ accountId }: { accountId: string }) {
  const { data, error, loaded } = usePoll(() => api.getStatus(accountId), POLL_MS)

  if (!data) {
    return (
      <Screen title={TITLE} description={DESCRIPTION}>
        {loaded && error ? (
          <Message tone="error" role="alert">
            {errorText(error)}
          </Message>
        ) : (
          <Loading />
        )}
      </Screen>
    )
  }
  return <StatusView status={data} stale={error !== null} error={error} />
}

function StatusView({
  status,
  stale,
  error,
}: {
  status: StatusDto
  stale: boolean
  error: unknown
}) {
  const s = status
  const note = STATE_NOTE[s.state]
  const counters = [
    ['受信', s.received_count],
    ['転送成功', s.forwarded_count],
    ['転送失敗', s.failed_count],
    ['再試行回数', s.retry_count],
  ] as const
  return (
    <Screen title={TITLE} description={DESCRIPTION}>
      {stale && (
        <Message tone="error" role="alert">
          最新の状態を取得できません: {errorText(error)}
        </Message>
      )}
      <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
        <ToneBadge tone={tone(s.state)} className="h-7 px-3 text-sm">
          {STATE_LABEL[s.state]}
        </ToneBadge>
        {note && <p className="text-sm text-muted-foreground">{note}</p>}
      </div>
      <Table>
        <TableCaption className="sr-only">カウンター</TableCaption>
        <TableBody>
          {counters.map(([label, count]) => (
            <TableRow key={label}>
              <TableHead scope="row" className="h-12 font-normal text-muted-foreground">
                {label}
              </TableHead>
              <TableCell className="text-right font-mono text-base tabular-nums">
                {formatCount(count)}
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
      <Separator />
      <dl className="grid gap-x-12 gap-y-4 text-sm sm:grid-cols-[8rem_minmax(0,1fr)]">
        <dt className="text-muted-foreground">接続開始</dt>
        <dd className="font-mono">{formatTime(s.connected_since)}</dd>
        <dt className="text-muted-foreground">最終受信</dt>
        <dd className="font-mono">{formatTime(s.last_notification_at)}</dd>
        <dt className="text-muted-foreground">直近のエラー</dt>
        <dd className="break-words">{s.last_error ?? 'なし'}</dd>
      </dl>
    </Screen>
  )
}
