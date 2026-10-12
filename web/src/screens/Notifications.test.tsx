import { act, render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'
import {
  apiError,
  filterOf,
  json,
  mockApi,
  notification,
  page,
  pageOf,
  requests,
} from '../test/helpers'
import { Notifications } from './Notifications'

const FIRST = 'GET /api/notifications?limit=50'
const before = (cursor: string) =>
  `GET /api/notifications?before=${encodeURIComponent(cursor)}&limit=50`

async function tick(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms)
  })
}

const rows = () => within(screen.getByRole('list', { name: '通知の一覧' })).getAllByRole('listitem')

describe('Notifications', () => {
  it('explains an empty history', async () => {
    mockApi({ [FIRST]: () => json(page([])) })
    render(<Notifications />)
    expect(await screen.findByText('表示対象の通知はありません。')).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'もっと見る' })).not.toBeInTheDocument()
  })

  it('keeps the server order (newest first) and formats the time', async () => {
    mockApi({ [FIRST]: () => json(page(['c', 'b', 'a'])) })
    render(<Notifications />)
    await screen.findByText('title c')
    expect(rows().map((r) => r.querySelector('p')?.textContent)).toEqual([
      'title c',
      'title b',
      'title a',
    ])
    expect(screen.getAllByText(new Date(1_700_000_000_000).toLocaleString('ja-JP'))).toHaveLength(3)
  })

  it('renders title and body as text, never as HTML', async () => {
    mockApi({
      [FIRST]: () =>
        json(
          pageOf([
            notification('a', {
              title: '<img src=x onerror=alert(1)>',
              body: '1行目\n<b>2行目</b>',
              payload: { html: '<script>alert(1)</script>', n: 1 },
            }),
          ]),
        ),
    })
    const { container } = render(<Notifications />)
    expect(await screen.findByText('<img src=x onerror=alert(1)>')).toBeInTheDocument()
    expect(screen.getByText(/1行目/).textContent).toBe('1行目\n<b>2行目</b>')
    expect(container.querySelector('b, script')).toBeNull()
    expect(container.querySelector('img')).toBeNull()
  })

  it('shows the original payload as pretty JSON text in a collapsible', async () => {
    mockApi({
      [FIRST]: () =>
        json(
          pageOf([
            notification('a', { payload: { html: '<script>x</script>', nested: { n: 1 } } }),
          ]),
        ),
    })
    const { container } = render(<Notifications />)
    await screen.findByText('title a')
    const details = container.querySelector('details')
    if (!details) throw new Error('details missing')
    expect(details).not.toHaveAttribute('open')
    expect(within(details).getByText('元データ').tagName).toBe('SUMMARY')
    const pre = container.querySelector('pre')
    expect(pre?.textContent).toBe(
      JSON.stringify({ html: '<script>x</script>', nested: { n: 1 } }, null, 2),
    )
    expect(container.querySelector('script')).toBeNull()
  })

  it('does not break on a payload that is not an object', async () => {
    mockApi({
      [FIRST]: () =>
        json(
          pageOf([notification('a', { payload: null }), notification('b', { payload: 'plain' })]),
        ),
    })
    const { container } = render(<Notifications />)
    await screen.findByText('title a')
    expect([...container.querySelectorAll('pre')].map((p) => p.textContent)).toEqual([
      'null',
      '"plain"',
    ])
  })

  it.each([
    ['https://x.com/a/status/1', true],
    ['https://www.x.com/a/status/1', true],
    ['https://twitter.com/a/status/1', true],
    ['https://www.twitter.com/a/status/1', true],
    ['http://x.com/a/status/1', false],
    ['javascript:alert(1)', false],
    ['data:text/html,<script>alert(1)</script>', false],
    ['https://evil.example/a', false],
    ['https://x.com.evil.example/a', false],
    ['https://evil.example/https://x.com/', false],
    ['https://user:pw@x.com/a', false],
    ['https://x.com:8443/a', false],
    ['//x.com/a', false],
    ['not a url', false],
    [null, false],
  ])('post link %s -> link=%s', async (url, linked) => {
    mockApi({ [FIRST]: () => json(pageOf([notification('a', { url })])) })
    render(<Notifications />)
    await screen.findByText('title a')
    const link = screen.queryByRole('link', { name: /投稿を開く/ })
    if (!linked) return expect(link).toBeNull()
    expect(link).toHaveAttribute('href', new URL(url ?? '').href)
    expect(link).toHaveAttribute('target', '_blank')
    expect(link?.getAttribute('rel')?.split(' ').sort()).toEqual(['noopener', 'noreferrer'])
    expect(link).toHaveAttribute('referrerpolicy', 'no-referrer')
  })

  it.each([
    ['https://pbs.twimg.com/profile_images/1/a.jpg', true],
    ['https://abs.twimg.com/icons/a.png', true],
    ['http://pbs.twimg.com/a.jpg', false],
    ['https://evil.example/a.jpg', false],
    ['https://pbs.twimg.com.evil.example/a.jpg', false],
    ['https://x.com/a.jpg', false],
    ['data:image/png;base64,AAAA', false],
    [null, false],
  ])('icon %s -> image=%s', async (icon, shown) => {
    mockApi({ [FIRST]: () => json(pageOf([notification('a', { icon })])) })
    const { container } = render(<Notifications />)
    await screen.findByText('title a')
    const img = container.querySelector('img')
    if (!shown) return expect(img).toBeNull()
    expect(img).toHaveAttribute('src', icon)
    expect(img).toHaveAttribute('referrerpolicy', 'no-referrer')
    expect(img).toHaveAttribute('alt', '')
  })

  it('falls back to the placeholder when the icon fails to load', async () => {
    mockApi({
      [FIRST]: () => json(pageOf([notification('a', { icon: 'https://pbs.twimg.com/a.jpg' })])),
    })
    const { container } = render(<Notifications />)
    await screen.findByText('title a')
    act(() => container.querySelector('img')?.dispatchEvent(new Event('error')))
    expect(container.querySelector('img')).toBeNull()
  })

  it('loads older pages on demand, appends them and hides the button at the end', async () => {
    const older = vi.fn(() => json(page(['b2', 'b1'])))
    mockApi({ [FIRST]: () => json(page(['a2', 'a1'], 'a1')), [before('a1')]: older })
    render(<Notifications />)
    await screen.findByText('title a2')
    expect(older).not.toHaveBeenCalled()
    await userEvent.click(screen.getByRole('button', { name: 'もっと見る' }))
    await screen.findByText('title b1')
    expect(older).toHaveBeenCalledTimes(1)
    expect(rows().map((r) => r.querySelector('p')?.textContent)).toEqual([
      'title a2',
      'title a1',
      'title b2',
      'title b1',
    ])
    expect(screen.queryByRole('button', { name: 'もっと見る' })).not.toBeInTheDocument()
  })

  it('keeps the list and offers a retry when an older page fails', async () => {
    let calls = 0
    mockApi({
      [FIRST]: () => json(page(['a'], 'a')),
      [before('a')]: () => (++calls === 1 ? apiError(500, 'x', '続き失敗') : json(page(['z']))),
    })
    render(<Notifications />)
    await userEvent.click(await screen.findByRole('button', { name: 'もっと見る' }))
    expect(await screen.findByRole('alert')).toHaveTextContent('続き失敗')
    expect(screen.getByText('title a')).toBeInTheDocument()
    await userEvent.click(screen.getByRole('button', { name: '続きを再試行' }))
    expect(await screen.findByText('title z')).toBeInTheDocument()
    expect(screen.queryByRole('alert')).not.toBeInTheDocument()
  })

  it('disables the more button while an older page is loading', async () => {
    let release: (r: Response) => void = () => {}
    const older = vi.fn(() => new Promise<Response>((r) => (release = r)))
    mockApi({ [FIRST]: () => json(page(['a'], 'a')), [before('a')]: older })
    render(<Notifications />)
    await userEvent.click(await screen.findByRole('button', { name: 'もっと見る' }))
    expect(await screen.findByRole('button', { name: '読み込み中…' })).toBeDisabled()
    expect(older).toHaveBeenCalledTimes(1)
    release(json(page(['b'])))
    await screen.findByText('title b')
  })

  it('shows an error with retry when the first load fails', async () => {
    let calls = 0
    mockApi({ [FIRST]: () => (++calls === 1 ? apiError(500, 'x', '取得失敗') : json(page(['a']))) })
    render(<Notifications />)
    expect(await screen.findByRole('alert')).toHaveTextContent('取得失敗')
    expect(screen.queryByText('表示対象の通知はありません。')).not.toBeInTheDocument()
    await userEvent.click(screen.getByRole('button', { name: '再試行' }))
    expect(await screen.findByText('title a')).toBeInTheDocument()
    expect(screen.queryByRole('alert')).not.toBeInTheDocument()
  })
})

describe('Notifications polling', () => {
  it('polls every 2s without overlapping requests', async () => {
    vi.useFakeTimers()
    let release: (r: Response) => void = () => {}
    let inflight = 0
    let maxInflight = 0
    const fetchMock = mockApi({
      [FIRST]: () => {
        inflight++
        maxInflight = Math.max(maxInflight, inflight)
        return new Promise<Response>((r) => {
          release = (res) => {
            inflight--
            r(res)
          }
        })
      },
    })
    render(<Notifications />)
    await tick(0)
    expect(fetchMock).toHaveBeenCalledTimes(1)
    await tick(10_000)
    expect(fetchMock).toHaveBeenCalledTimes(1)
    release(json(page(['a'])))
    await tick(1999)
    expect(fetchMock).toHaveBeenCalledTimes(1)
    await tick(1)
    expect(fetchMock).toHaveBeenCalledTimes(2)
    expect(maxInflight).toBe(1)
    release(json(page(['a'])))
    await tick(0)
  })

  it('prepends new notifications and keeps the older pages that were loaded', async () => {
    vi.useFakeTimers()
    let latest = page(['a2', 'a1'], 'a1')
    mockApi({ [FIRST]: () => json(latest), [before('a1')]: () => json(page(['b1'])) })
    render(<Notifications />)
    await tick(0)
    await act(async () => screen.getByRole('button', { name: 'もっと見る' }).click())
    await tick(0)
    expect(screen.getByText('title b1')).toBeInTheDocument()

    latest = page(['a3', 'a2', 'a1'], 'a1')
    await tick(2000)
    expect(rows().map((r) => r.querySelector('p')?.textContent)).toEqual([
      'title a3',
      'title a2',
      'title a1',
      'title b1',
    ])
    expect(screen.queryByRole('button', { name: 'もっと見る' })).not.toBeInTheDocument()
  })

  it('restarts from the latest page when it no longer overlaps what is shown', async () => {
    vi.useFakeTimers()
    let latest = page(['a2', 'a1'], 'a1')
    mockApi({ [FIRST]: () => json(latest) })
    render(<Notifications />)
    await tick(0)
    latest = page(['z2', 'z1'], 'z1')
    await tick(2000)
    expect(rows().map((r) => r.querySelector('p')?.textContent)).toEqual(['title z2', 'title z1'])
    expect(screen.getByRole('button', { name: 'もっと見る' })).toBeEnabled()
  })

  it('keeps the old rows and warns when a later poll fails, then recovers', async () => {
    vi.useFakeTimers()
    let calls = 0
    mockApi({
      [FIRST]: () => {
        calls++
        return calls === 2 ? apiError(502, 'x', '一時的な障害') : json(page(['a']))
      },
    })
    render(<Notifications />)
    await tick(0)
    await tick(2000)
    expect(screen.getByRole('alert')).toHaveTextContent('一時的な障害')
    expect(screen.getByText('title a')).toBeInTheDocument()
    await tick(2000)
    expect(screen.queryByRole('alert')).not.toBeInTheDocument()
    expect(screen.getByText('title a')).toBeInTheDocument()
  })

  it('pauses while hidden and refreshes when visible again', async () => {
    vi.useFakeTimers()
    const fetchMock = mockApi({ [FIRST]: () => json(page(['a'])) })
    render(<Notifications />)
    await tick(0)
    const hidden = vi.spyOn(document, 'hidden', 'get').mockReturnValue(true)
    document.dispatchEvent(new Event('visibilitychange'))
    await tick(10_000)
    expect(fetchMock).toHaveBeenCalledTimes(1)
    hidden.mockReturnValue(false)
    document.dispatchEvent(new Event('visibilitychange'))
    await tick(0)
    expect(fetchMock).toHaveBeenCalledTimes(2)
    hidden.mockRestore()
  })
})

const rowTitles = () => rows().map((r) => r.querySelector('p')?.textContent)
const allOff = filterOf({
  tweet: false,
  retweet: false,
  quote: false,
  reply: false,
  xchat_message_received: false,
  other: false,
})

describe('Notifications filter', () => {
  it('drops excluded rows when the filter changes, brings them back and handles all disabled', async () => {
    vi.useFakeTimers()
    let latest = page(['c', 'b', 'a'])
    mockApi({ [FIRST]: () => json(latest) })
    render(<Notifications />)
    await tick(0)
    expect(rowTitles()).toEqual(['title c', 'title b', 'title a'])

    latest = page(['c', 'a'], null, filterOf({ xchat_message_received: false }))
    await tick(2000)
    expect(rowTitles()).toEqual(['title c', 'title a'])

    latest = page(['c', 'b', 'a'])
    await tick(2000)
    expect(rowTitles()).toEqual(['title c', 'title b', 'title a'])

    latest = page([], null, allOff)
    await tick(2000)
    expect(screen.queryByRole('list', { name: '通知の一覧' })).not.toBeInTheDocument()
    expect(screen.getByText('表示対象の通知はありません。')).toBeInTheDocument()
    expect(screen.queryByRole('alert')).not.toBeInTheDocument()
  })

  it('does not keep an excluded row when the new latest page still overlaps the list', async () => {
    vi.useFakeTimers()
    let latest = page(['a3', 'a2', 'a1'], 'a1')
    mockApi({ [FIRST]: () => json(latest), [before('a1')]: () => json(page(['b1'])) })
    render(<Notifications />)
    await tick(0)
    await act(async () => screen.getByRole('button', { name: 'もっと見る' }).click())
    await tick(0)
    expect(rowTitles()).toEqual(['title a3', 'title a2', 'title a1', 'title b1'])

    latest = page(['a3', 'a1'], 'a1', filterOf({ tweet: false }))
    await tick(2000)
    expect(rowTitles()).toEqual(['title a3', 'title a1'])
    expect(screen.getByRole('button', { name: 'もっと見る' })).toBeEnabled()
  })

  it('treats a change of only the retweet flag as a filter change', async () => {
    vi.useFakeTimers()
    let latest = page(['a3', 'a2', 'a1'], 'a1')
    mockApi({ [FIRST]: () => json(latest), [before('a1')]: () => json(page(['b1'])) })
    render(<Notifications />)
    await tick(0)
    await act(async () => screen.getByRole('button', { name: 'もっと見る' }).click())
    await tick(0)
    expect(rowTitles()).toEqual(['title a3', 'title a2', 'title a1', 'title b1'])

    latest = page(['a3', 'a1'], 'a1', filterOf({ retweet: false }))
    await tick(2000)
    expect(rowTitles()).toEqual(['title a3', 'title a1'])
    expect(screen.getByRole('button', { name: 'もっと見る' })).toBeEnabled()
  })

  it.each([
    ['quote', filterOf({ quote: false })],
    ['reply', filterOf({ reply: false })],
  ])('treats a change of only the %s flag as a filter change', async (_, changed) => {
    vi.useFakeTimers()
    const server = { latest: page(['a3', 'a2', 'a1'], 'a1') }
    mockApi({ [FIRST]: () => json(server.latest), [before('a1')]: () => json(page(['b1'])) })
    render(<Notifications />)
    await tick(0)
    await act(async () => screen.getByRole('button', { name: 'もっと見る' }).click())
    await tick(0)
    expect(rowTitles()).toEqual(['title a3', 'title a2', 'title a1', 'title b1'])

    server.latest = page(['a3', 'a1'], 'a1', changed)
    await tick(2000)
    expect(rowTitles()).toEqual(['title a3', 'title a1'])
    expect(screen.getByRole('button', { name: 'もっと見る' })).toBeEnabled()
  })

  it('keeps merging in place while the filter stays the same', async () => {
    vi.useFakeTimers()
    const same = filterOf({ other: false })
    let latest = page(['a2', 'a1'], 'a1', same)
    mockApi({ [FIRST]: () => json(latest), [before('a1')]: () => json(page(['b1'], null, same)) })
    render(<Notifications />)
    await tick(0)
    await act(async () => screen.getByRole('button', { name: 'もっと見る' }).click())
    await tick(0)
    latest = page(['a3', 'a2', 'a1'], 'a1', same)
    await tick(2000)
    expect(rowTitles()).toEqual(['title a3', 'title a2', 'title a1', 'title b1'])
  })

  it('discards an older page that was requested before the filter changed', async () => {
    vi.useFakeTimers()
    let latest = page(['a2', 'a1'], 'a1')
    let release: (r: Response) => void = () => {}
    mockApi({
      [FIRST]: () => json(latest),
      [before('a1')]: () => new Promise<Response>((r) => (release = r)),
    })
    render(<Notifications />)
    await tick(0)
    await act(async () => screen.getByRole('button', { name: 'もっと見る' }).click())
    await tick(0)
    expect(screen.getByRole('button', { name: '読み込み中…' })).toBeDisabled()

    latest = page(['a2'], 'a2', filterOf({ tweet: false }))
    await tick(2000)
    expect(screen.getByRole('button', { name: 'もっと見る' })).toBeEnabled()

    release(json(page(['b1', 'b0'])))
    await tick(0)
    expect(rowTitles()).toEqual(['title a2'])
    expect(screen.queryByText('title b1')).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'もっと見る' })).toBeEnabled()
  })

  it('does not show a late older failure after the filter changed', async () => {
    vi.useFakeTimers()
    let latest = page(['a2', 'a1'], 'a1')
    let release: (r: Response) => void = () => {}
    mockApi({
      [FIRST]: () => json(latest),
      [before('a1')]: () => new Promise<Response>((r) => (release = r)),
    })
    render(<Notifications />)
    await tick(0)
    await act(async () => screen.getByRole('button', { name: 'もっと見る' }).click())
    await tick(0)

    latest = page(['a2'], 'a2', filterOf({ other: false }))
    await tick(2000)
    release(apiError(500, 'x', '続き失敗'))
    await tick(0)
    expect(screen.queryByRole('alert')).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'もっと見る' })).toBeEnabled()
    expect(rowTitles()).toEqual(['title a2'])
  })

  it('clears an older-page error when the list is replaced by a new filter', async () => {
    vi.useFakeTimers()
    let latest = page(['a'], 'a')
    mockApi({
      [FIRST]: () => json(latest),
      [before('a')]: () => apiError(500, 'x', '続き失敗'),
    })
    render(<Notifications />)
    await tick(0)
    await act(async () => screen.getByRole('button', { name: 'もっと見る' }).click())
    await tick(0)
    expect(screen.getByRole('alert')).toHaveTextContent('続き失敗')

    latest = page(['a'], 'a', filterOf({ xchat_message_received: false }))
    await tick(2000)
    expect(screen.queryByRole('alert')).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'もっと見る' })).toBeInTheDocument()
  })

  it('discards a late older page after the latest page restarted the list under the same filter', async () => {
    vi.useFakeTimers()
    let latest = page(['a2', 'a1'], 'a1')
    let release: (r: Response) => void = () => {}
    mockApi({
      [FIRST]: () => json(latest),
      [before('a1')]: () => new Promise<Response>((r) => (release = r)),
    })
    render(<Notifications />)
    await tick(0)
    await act(async () => screen.getByRole('button', { name: 'もっと見る' }).click())
    await tick(0)

    latest = page(['z2', 'z1'], 'a1')
    await tick(2000)
    expect(rowTitles()).toEqual(['title z2', 'title z1'])

    release(json(page(['b1'])))
    await tick(0)
    expect(rowTitles()).toEqual(['title z2', 'title z1'])
    expect(screen.getByRole('button', { name: 'もっと見る' })).toBeEnabled()
  })

  it('refreshes instead of merging an older page filtered differently from the request start', async () => {
    vi.useFakeTimers()
    let latest = page(['a2', 'a1'], 'a1')
    let release: (r: Response) => void = () => {}
    const fetchMock = mockApi({
      [FIRST]: () => json(latest),
      [before('a1')]: () => new Promise<Response>((r) => (release = r)),
    })
    render(<Notifications />)
    await tick(0)
    await act(async () => screen.getByRole('button', { name: 'もっと見る' }).click())
    await tick(0)
    const polls = requests(fetchMock, FIRST)

    latest = page(['a2'], null, filterOf({ tweet: false }))
    release(json(page(['b1'], null, filterOf({ tweet: false }))))
    await tick(0)
    expect(requests(fetchMock, FIRST)).toBe(polls + 1)
    expect(rowTitles()).toEqual(['title a2'])
    expect(screen.queryByText('title b1')).not.toBeInTheDocument()
  })

  it('does not dispatch or fail when an older page settles after unmount', async () => {
    vi.useFakeTimers()
    const error = vi.spyOn(console, 'error').mockImplementation(() => {})
    let release: (r: Response) => void = () => {}
    mockApi({
      [FIRST]: () => json(page(['a'], 'a')),
      [before('a')]: () => new Promise<Response>((r) => (release = r)),
    })
    const view = render(<Notifications />)
    await tick(0)
    await act(async () => screen.getByRole('button', { name: 'もっと見る' }).click())
    await tick(0)
    view.unmount()
    release(json(page(['b'])))
    await tick(0)
    expect(error).not.toHaveBeenCalled()
    error.mockRestore()
  })
})

describe('Notifications with a server that omits the filter', () => {
  it('shows the generic error instead of an empty or broken list', async () => {
    mockApi({ [FIRST]: () => json({ items: [notification('a')], next_cursor: null }) })
    render(<Notifications />)
    expect(await screen.findByRole('alert')).toHaveTextContent('サーバーでエラーが発生しました。')
    expect(screen.queryByText('title a')).not.toBeInTheDocument()
    expect(screen.queryByText('表示対象の通知はありません。')).not.toBeInTheDocument()
  })
})
