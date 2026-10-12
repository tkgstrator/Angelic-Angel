import type { NotificationFilter, NotificationItem, NotificationPage } from './api'

export interface Feed {
  items: NotificationItem[]
  /** `before` cursor for the next older page; null once the oldest entry is loaded. */
  cursor: string | null
  filter: NotificationFilter | null
  generation: number
}

export type FeedAction =
  | { type: 'latest'; page: NotificationPage }
  | { type: 'older'; from: string; generation: number; page: NotificationPage }

export const emptyFeed: Feed = { items: [], cursor: null, filter: null, generation: 0 }

export function sameFilter(a: NotificationFilter, b: NotificationFilter): boolean {
  return (
    a.tweet === b.tweet &&
    a.retweet === b.retweet &&
    a.quote === b.quote &&
    a.reply === b.reply &&
    a.xchat_message_received === b.xchat_message_received &&
    a.other === b.other
  )
}

function fresh(items: NotificationItem[], known: Set<string>): NotificationItem[] {
  const out: NotificationItem[] = []
  for (const item of items) {
    if (known.has(item.id)) continue
    known.add(item.id)
    out.push(item)
  }
  return out
}

function restart(feed: Feed, page: NotificationPage): Feed {
  return {
    items: fresh(page.items, new Set()),
    cursor: page.next_cursor,
    filter: page.notification_filter,
    generation: feed.generation + 1,
  }
}

/**
 * Merges a newest-first page into what is already shown. Items are only ever prepended, so older
 * pages the user loaded stay in place and unchanged rows keep their identity. A page that shares
 * no entry with the list means more than a page of notifications arrived in between: start over
 * from that page rather than show a list with a hole in it.
 */
function mergeLatest(feed: Feed, page: NotificationPage): Feed {
  // 重なっていても filter が変わったら、除外された行を残さないよう作り直す
  if (feed.filter === null || !sameFilter(feed.filter, page.notification_filter)) {
    return restart(feed, page)
  }
  if (feed.items.length === 0) {
    return page.items.length === 0 && page.next_cursor === feed.cursor ? feed : restart(feed, page)
  }
  const known = new Set(feed.items.map((item) => item.id))
  if (!page.items.some((item) => known.has(item.id))) return restart(feed, page)
  const added = fresh(page.items, known)
  return added.length === 0 ? feed : { ...feed, items: [...added, ...feed.items] }
}

function appendOlder(feed: Feed, from: string, generation: number, page: NotificationPage): Feed {
  if (feed.generation !== generation || feed.cursor !== from) return feed
  if (feed.filter === null || !sameFilter(feed.filter, page.notification_filter)) return feed
  const known = new Set(feed.items.map((item) => item.id))
  return { ...feed, items: [...feed.items, ...fresh(page.items, known)], cursor: page.next_cursor }
}

export function feedReducer(feed: Feed, action: FeedAction): Feed {
  return action.type === 'latest'
    ? mergeLatest(feed, action.page)
    : appendOlder(feed, action.from, action.generation, action.page)
}
