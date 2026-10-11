import { ExternalLink, LoaderCircle, UserRound } from 'lucide-react'
import { memo, useEffect, useReducer, useRef, useState } from 'react'
import { Message } from '@/components/message'
import { Loading, Screen } from '@/components/screen'
import { Button } from '@/components/ui/button'
import { api, errorText, type NotificationItem } from '../api'
import { formatTime } from '../format'
import { usePoll } from '../hooks'
import { emptyFeed, feedReducer, sameFilter } from '../notification-feed'
import { safeIconUrl, safePostUrl } from '../safe-url'

const POLL_MS = 2000
const TITLE = '通知'
const DESCRIPTION = '受信した通知を新しい順に表示します。2 秒ごとに更新します。'

function pretty(payload: unknown): string {
  try {
    return JSON.stringify(payload, null, 2) ?? 'null'
  } catch {
    return String(payload)
  }
}

function Avatar({ icon }: { icon: string | null }) {
  const [failed, setFailed] = useState(false)
  const src = safeIconUrl(icon)
  if (src === null || failed) {
    return (
      <span
        aria-hidden
        className="grid size-10 shrink-0 place-items-center rounded-full bg-muted text-muted-foreground"
      >
        <UserRound className="size-5" />
      </span>
    )
  }
  return (
    <img
      src={src}
      alt=""
      width={40}
      height={40}
      loading="lazy"
      decoding="async"
      referrerPolicy="no-referrer"
      onError={() => setFailed(true)}
      className="size-10 shrink-0 rounded-full bg-muted object-cover"
    />
  )
}

const Row = memo(function Row({ item }: { item: NotificationItem }) {
  const href = safePostUrl(item.url)
  return (
    <li className="flex gap-3 border-b py-4">
      <Avatar icon={item.icon} />
      <article className="min-w-0 flex-1 space-y-2">
        <div className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
          <p className="min-w-0 text-sm font-medium break-words whitespace-pre-wrap">
            {item.title}
          </p>
          <span className="shrink-0 font-mono text-xs text-muted-foreground tabular-nums">
            {formatTime(item.received_at)}
          </span>
        </div>
        {item.body !== '' && (
          <p className="text-sm break-words whitespace-pre-wrap text-muted-foreground">
            {item.body}
          </p>
        )}
        {href !== null && (
          <a
            href={href}
            target="_blank"
            rel="noopener noreferrer"
            referrerPolicy="no-referrer"
            className="inline-flex items-center gap-1 rounded-sm text-sm text-primary underline-offset-4 outline-none hover:underline focus-visible:ring-3 focus-visible:ring-ring/50"
          >
            <ExternalLink className="size-3.5" />
            投稿を開く
          </a>
        )}
        <details className="text-xs">
          <summary className="w-fit cursor-pointer rounded-sm text-muted-foreground outline-none select-none hover:text-foreground focus-visible:ring-3 focus-visible:ring-ring/50">
            元データ
          </summary>
          <pre className="mt-2 max-h-72 overflow-auto rounded-md bg-muted p-3 font-mono break-all whitespace-pre-wrap">
            {pretty(item.payload)}
          </pre>
        </details>
      </article>
    </li>
  )
})

type More = { generation: number; pending: boolean; error: string | null }

function useMounted() {
  const mounted = useRef(true)
  useEffect(() => {
    mounted.current = true
    return () => {
      mounted.current = false
    }
  }, [])
  return mounted
}

export function Notifications() {
  const [feed, dispatch] = useReducer(feedReducer, emptyFeed)
  const [moreState, setMore] = useState<More>({ generation: 0, pending: false, error: null })
  const mounted = useMounted()
  const poll = usePoll(async () => {
    const page = await api.getNotifications()
    if (mounted.current) dispatch({ type: 'latest', page })
  }, POLL_MS)

  // 一覧が置き換わったら、前の世代の追加読み込みの状態は持ち越さない
  const more =
    moreState.generation === feed.generation
      ? moreState
      : { generation: feed.generation, pending: false, error: null }

  async function loadMore() {
    const { cursor: from, filter, generation } = feed
    if (from === null || filter === null || more.pending) return
    setMore({ generation, pending: true, error: null })
    const settle = (error: string | null) => {
      if (!mounted.current) return
      setMore((prev) =>
        prev.generation === generation ? { generation, pending: false, error } : prev,
      )
    }
    try {
      const page = await api.getNotifications(from)
      if (!mounted.current) return
      if (sameFilter(page.notification_filter, filter)) {
        dispatch({ type: 'older', from, generation, page })
      } else {
        poll.refresh()
      }
      settle(null)
    } catch (e) {
      settle(errorText(e))
    }
  }

  if (!poll.loaded) {
    return (
      <Screen title={TITLE} description={DESCRIPTION}>
        <Loading />
      </Screen>
    )
  }

  const stale = poll.error !== null
  return (
    <Screen title={TITLE} description={DESCRIPTION}>
      {stale && (
        <div role="alert" className="space-y-3">
          <Message tone="error">
            {feed.items.length > 0 ? '最新の通知を取得できません' : '通知を取得できません'}:{' '}
            {errorText(poll.error)}
          </Message>
          <Button type="button" variant="outline" className="h-10" onClick={poll.refresh}>
            再試行
          </Button>
        </div>
      )}
      {feed.items.length === 0 ? (
        !stale && <p className="text-sm text-muted-foreground">表示対象の通知はありません。</p>
      ) : (
        <div className="space-y-4">
          <ol aria-label="通知の一覧" className="border-t">
            {feed.items.map((item) => (
              <Row key={item.id} item={item} />
            ))}
          </ol>
          {more.error && (
            <Message tone="error" role="alert">
              続きを取得できません: {more.error}
            </Message>
          )}
          {feed.cursor !== null && (
            <Button
              type="button"
              variant="outline"
              className="h-10 px-5"
              disabled={more.pending}
              onClick={loadMore}
            >
              {more.pending && <LoaderCircle data-icon="inline-start" className="animate-spin" />}
              {more.pending ? '読み込み中…' : more.error ? '続きを再試行' : 'もっと見る'}
            </Button>
          )}
        </div>
      )}
    </Screen>
  )
}
