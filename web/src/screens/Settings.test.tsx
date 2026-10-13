import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'
import type { NotificationFilter } from '../api'
import {
  apiError,
  filterOf,
  json,
  mockApi,
  registration,
  requests,
  settings,
  settingsRoutes,
  validation,
} from '../test/helpers'
import { Settings } from './Settings'

const PUT = 'PUT /api/settings'
const VALIDATE = 'POST /api/settings/validate'
const unsaved = settings({
  configured: false,
  auth_token_set: false,
  ct0_set: false,
  webhook_endpoint: null,
  webhook_source: 'none',
})

const save = () => userEvent.click(screen.getByRole('button', { name: '保存' }))
const button = (name: string) => screen.getByRole('button', { name })

describe('Settings', () => {
  it('never prefills cookies and shows set indicators', async () => {
    mockApi(settingsRoutes())
    render(<Settings />)
    expect(await screen.findByLabelText(/auth_token/)).toHaveValue('')
    expect(screen.getByLabelText(/ct0/)).toHaveValue('')
    expect(screen.getAllByText('設定済み')).toHaveLength(2)
    expect(screen.getByText(/空欄のまま保存すると/)).toBeInTheDocument()
  })

  it('titles the section Cookie and puts registration and bulk notifications below it', async () => {
    mockApi(settingsRoutes())
    render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    const names = (await screen.findAllByRole('group')).map((g) =>
      g.getAttribute('aria-labelledby'),
    )
    expect(names.length).toBeGreaterThanOrEqual(4)
    const headings = screen.getAllByRole('heading', { level: 3 }).map((h) => h.textContent)
    expect(headings).toEqual([
      'Cookie',
      '転送先',
      '通知フィルター',
      '受信先の登録',
      'フォロー中の全員の投稿通知をオン',
    ])
    expect(screen.queryByText(/X の Cookie/)).not.toBeInTheDocument()
  })

  it('ends the cookie and webhook form with save, apart from registration and bulk actions', async () => {
    mockApi(settingsRoutes())
    const { container } = render(<Settings />)
    await screen.findByText('未登録')
    const form = container.querySelector('form')
    if (!form) throw new Error('form missing')
    const labels = within(form)
      .getAllByRole('button')
      .map((b) => b.textContent)
    expect(labels).toEqual(['確認', '保存'])
    expect(within(form).getByRole('button', { name: '保存' })).toHaveAttribute('type', 'submit')
    expect(form.contains(button('登録'))).toBe(false)
    expect(form.contains(button('オンにする'))).toBe(false)
    const order = [...container.querySelectorAll('button')].map((b) => b.textContent)
    expect(order.indexOf('保存')).toBeLessThan(order.indexOf('登録'))
    expect(order.indexOf('確認')).toBeLessThan(order.indexOf('保存'))
  })

  it('keeps the save result next to the save button', async () => {
    mockApi({ ...settingsRoutes(), [PUT]: () => json(settings()) })
    render(<Settings />)
    await userEvent.type(await screen.findByLabelText(/ct0/), 'abc')
    await save()
    const message = await screen.findByText('保存しました。')
    expect(message.closest('div')).toBe(button('保存').closest('div'))
  })

  it('requires both cookies on first setup', async () => {
    const fetchMock = mockApi({ ...settingsRoutes(), 'GET /api/settings': () => json(unsaved) })
    render(<Settings />)
    await userEvent.click(await screen.findByRole('button', { name: '保存' }))
    expect(screen.getByText('初回設定では auth_token が必須です。')).toBeInTheDocument()
    expect(screen.getByText('初回設定では ct0 が必須です。')).toBeInTheDocument()
    expect(requests(fetchMock, PUT)).toBe(0)
    expect(requests(fetchMock, VALIDATE)).toBe(0)
  })

  it('sends null for blank cookies (retention) and clears inputs after save', async () => {
    let body: unknown
    mockApi({
      ...settingsRoutes(),
      [PUT]: (init) => {
        body = JSON.parse(String(init.body))
        return json(settings({ webhook_endpoint: 'https://new.example/h' }))
      },
    })
    render(<Settings />)
    const url = await screen.findByLabelText('Webhook URL')
    await userEvent.clear(url)
    await userEvent.type(url, 'https://new.example/h')
    await save()
    expect(await screen.findByText('保存しました。')).toBeInTheDocument()
    expect(body).toEqual({
      auth_token: null,
      ct0: null,
      webhook_endpoint: 'https://new.example/h',
      notification_filter: filterOf(),
    })
  })

  it('submits typed cookies and wipes them from the form after saving', async () => {
    let body: Record<string, unknown> = {}
    mockApi({
      ...settingsRoutes(),
      [PUT]: (init) => {
        body = JSON.parse(String(init.body))
        return json(settings())
      },
    })
    render(<Settings />)
    const auth = await screen.findByLabelText(/auth_token/)
    await userEvent.type(auth, 'abc123')
    await save()
    await screen.findByText('保存しました。')
    expect(body.auth_token).toBe('abc123')
    expect(body.ct0).toBeNull()
    expect(auth).toHaveValue('')
  })

  it('rejects an invalid webhook URL without sending', async () => {
    const fetchMock = mockApi(settingsRoutes())
    render(<Settings />)
    const url = await screen.findByLabelText('Webhook URL')
    await userEvent.clear(url)
    await userEvent.type(url, 'ftp://example.com')
    await save()
    expect(screen.getByText(/http:\/\/ または https:\/\//)).toBeInTheDocument()
    expect(requests(fetchMock, PUT)).toBe(0)
  })

  it('disables the webhook URL when the environment overrides it', async () => {
    mockApi({
      ...settingsRoutes(),
      'GET /api/settings': () =>
        json(settings({ webhook_source: 'environment', webhook_endpoint: 'https://env.example/' })),
    })
    render(<Settings />)
    expect(await screen.findByLabelText('Webhook URL')).toBeDisabled()
    expect(screen.getByText(/環境変数 WEBHOOK_ENDPOINT/)).toBeInTheDocument()
  })

  it('locks cookie inputs while registered', async () => {
    mockApi({
      ...settingsRoutes(),
      'GET /api/settings': () => json(settings({ credentials_locked: true })),
      'GET /api/registration': () => json(registration({ registered: true })),
    })
    render(<Settings />)
    expect(await screen.findByLabelText(/auth_token/)).toBeDisabled()
    expect(screen.getByLabelText(/ct0/)).toBeDisabled()
    expect(screen.getByLabelText('Webhook URL')).toBeEnabled()
  })

  it('shows the sanitized server error on save failure, keeps typed values and does not check', async () => {
    const fetchMock = mockApi({
      ...settingsRoutes(),
      [PUT]: () => apiError(409, 'credentials_locked', '登録を解除してください。'),
    })
    render(<Settings />)
    const auth = await screen.findByLabelText(/auth_token/)
    await userEvent.type(auth, 'abc')
    await save()
    expect(await screen.findByRole('alert')).toHaveTextContent('登録を解除してください。')
    expect(auth).toHaveValue('abc')
    expect(requests(fetchMock, VALIDATE)).toBe(0)
  })

  it('offers retry when loading fails', async () => {
    let calls = 0
    mockApi({
      ...settingsRoutes(),
      'GET /api/settings': () => (++calls === 1 ? apiError(500, 'x', '失敗') : json(settings())),
    })
    render(<Settings />)
    await userEvent.click(await screen.findByRole('button', { name: '再試行' }))
    expect(await screen.findByLabelText('Webhook URL')).toBeInTheDocument()
  })
})

const checkbox = (name: string | RegExp) => screen.getByRole('checkbox', { name })
const KIND = {
  tweet: /投稿（tweet）/,
  retweet: /リツイート（RT）/,
  quote: /引用RT/,
  quoteSelf: /自分自身の引用（quote_self）/,
  quoteOther: /他アカウントの引用（quote）/,
  reply: /リプライ/,
  chat: /チャット（xchat_message_received）/,
  other: /その他・未指定/,
} as const
const FILTER_NAMES = Object.values(KIND)

describe('Settings notification filter', () => {
  it('lists the kinds with quotes nested, all enabled by default, with the explanation', async () => {
    mockApi(settingsRoutes())
    render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    const group = screen.getByRole('group', { name: '通知フィルター' })
    expect(within(group).getAllByRole('checkbox')).toHaveLength(8)
    for (const name of FILTER_NAMES) expect(checkbox(name)).toBeChecked()
    expect(
      within(group).getByText(
        '無効にした種類は一覧とWebhook転送から除外します。受信履歴は保存されます。',
      ),
    ).toBeInTheDocument()
  })

  it('reflects the saved filter in the checkboxes', async () => {
    mockApi({
      ...settingsRoutes(),
      'GET /api/settings': () =>
        json(settings({ notification_filter: filterOf({ xchat_message_received: false }) })),
    })
    render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    for (const name of FILTER_NAMES.filter((n) => n !== KIND.chat)) {
      expect(checkbox(name)).toBeChecked()
    }
    expect(checkbox(KIND.chat)).not.toBeChecked()
  })

  it('toggles quotes and replies independently of each other and of retweets', async () => {
    const body = { current: {} as { notification_filter?: NotificationFilter } }
    mockApi({
      ...settingsRoutes(),
      'GET /api/settings': () =>
        json(settings({ notification_filter: filterOf({ quote: false }) })),
      [PUT]: (init) => {
        body.current = JSON.parse(String(init.body))
        return json(settings({ notification_filter: filterOf({ reply: false }) }))
      },
    })
    render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    expect(checkbox(/引用RT/)).not.toBeChecked()
    expect(checkbox(/リプライ/)).toBeChecked()
    expect(checkbox(/リツイート（RT）/)).toBeChecked()

    await userEvent.click(checkbox(/リプライ/))
    expect(checkbox(/引用RT/)).not.toBeChecked()
    expect(checkbox(/リプライ/)).not.toBeChecked()
    expect(checkbox(/リツイート（RT）/)).toBeChecked()
    expect(checkbox(/投稿（tweet）/)).toBeChecked()

    // 片方だけ無効の親は indeterminate。操作すると子が両方有効になる
    await userEvent.click(checkbox(/引用RT/))
    await save()
    await screen.findByText('保存しました。')
    expect(body.current.notification_filter).toEqual(filterOf({ reply: false }))
  })

  it('toggles the retweet kind independently of tweets', async () => {
    const body = { current: {} as { notification_filter?: NotificationFilter } }
    mockApi({
      ...settingsRoutes(),
      'GET /api/settings': () =>
        json(settings({ notification_filter: filterOf({ retweet: false }) })),
      [PUT]: (init) => {
        body.current = JSON.parse(String(init.body))
        return json(settings({ notification_filter: filterOf({ retweet: false }) }))
      },
    })
    render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    expect(checkbox(/リツイート（RT）/)).not.toBeChecked()
    expect(checkbox(/投稿（tweet）/)).toBeChecked()
    await userEvent.click(checkbox(/投稿（tweet）/))
    await userEvent.click(checkbox(/リツイート（RT）/))
    await save()
    await screen.findByText('保存しました。')
    expect(body.current.notification_filter).toEqual(filterOf({ tweet: false }))
  })

  it('saves all six kinds together with the form and keeps them after a reload', async () => {
    let stored = filterOf()
    let body: { notification_filter?: NotificationFilter } = {}
    mockApi({
      ...settingsRoutes(),
      'GET /api/settings': () => json(settings({ notification_filter: stored })),
      [PUT]: (init) => {
        body = JSON.parse(String(init.body))
        stored = body.notification_filter ?? stored
        return json(settings({ notification_filter: stored }))
      },
    })
    const first = render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    await userEvent.click(checkbox(KIND.tweet))
    await userEvent.click(checkbox(KIND.retweet))
    await userEvent.click(checkbox(KIND.quote))
    await userEvent.click(checkbox(KIND.reply))
    await userEvent.click(checkbox(KIND.chat))
    await userEvent.click(checkbox(KIND.other))
    await save()
    await screen.findByText('保存しました。')
    expect(body.notification_filter).toEqual({
      tweet: false,
      retweet: false,
      quote: false,
      quote_self: false,
      reply: false,
      xchat_message_received: false,
      other: false,
    })
    for (const name of FILTER_NAMES) expect(checkbox(name)).not.toBeChecked()

    first.unmount()
    render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    for (const name of FILTER_NAMES) expect(checkbox(name)).not.toBeChecked()
    await userEvent.click(checkbox(KIND.chat))
    await save()
    await waitFor(() =>
      expect(body.notification_filter).toEqual(
        filterOf({
          tweet: false,
          retweet: false,
          quote: false,
          quote_self: false,
          reply: false,
          other: false,
        }),
      ),
    )
  })

  it('shows the filter the server returned, not what was typed', async () => {
    mockApi({
      ...settingsRoutes(),
      [PUT]: () => json(settings({ notification_filter: filterOf({ other: false }) })),
    })
    render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    await save()
    await screen.findByText('保存しました。')
    expect(checkbox(KIND.other)).not.toBeChecked()
    expect(checkbox(KIND.tweet)).toBeChecked()
  })

  it('keeps the edit when saving fails', async () => {
    mockApi({ ...settingsRoutes(), [PUT]: () => apiError(500, 'x', '保存失敗') })
    render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    await userEvent.click(checkbox(KIND.tweet))
    await save()
    expect(await screen.findByRole('alert')).toHaveTextContent('保存失敗')
    expect(checkbox(KIND.tweet)).not.toBeChecked()
  })

  it('stays editable when registered or when the webhook comes from the environment', async () => {
    let body: Record<string, unknown> = {}
    mockApi({
      ...settingsRoutes(),
      'GET /api/settings': () =>
        json(
          settings({
            credentials_locked: true,
            webhook_source: 'environment',
            webhook_endpoint: 'https://env.example/',
          }),
        ),
      'GET /api/registration': () => json(registration({ registered: true })),
      [PUT]: (init) => {
        body = JSON.parse(String(init.body))
        return json(settings({ notification_filter: filterOf({ tweet: false }) }))
      },
    })
    render(<Settings />)
    expect(await screen.findByLabelText(/auth_token/)).toBeDisabled()
    expect(screen.getByLabelText('Webhook URL')).toBeDisabled()
    for (const name of FILTER_NAMES) expect(checkbox(name)).toBeEnabled()
    await userEvent.click(checkbox(KIND.tweet))
    await save()
    await screen.findByText('保存しました。')
    expect(body).toEqual({
      auth_token: null,
      ct0: null,
      webhook_endpoint: 'https://env.example/',
      notification_filter: filterOf({ tweet: false }),
    })
  })

  it('is disabled together with the other actions while busy', async () => {
    let release: (r: Response) => void = () => {}
    mockApi({ ...settingsRoutes(), [PUT]: () => new Promise<Response>((r) => (release = r)) })
    render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    await save()
    await screen.findByRole('button', { name: '保存中…' })
    for (const name of FILTER_NAMES) expect(checkbox(name)).toBeDisabled()
    release(json(settings()))
    await screen.findByText('Cookie は有効です（@alice）。')
    for (const name of FILTER_NAMES) expect(checkbox(name)).toBeEnabled()
  })
})

const state = (name: string | RegExp) => checkbox(name).getAttribute('aria-checked')

describe('Settings quote filter', () => {
  async function renderWith(notification_filter: NotificationFilter) {
    const body = { current: {} as { notification_filter?: NotificationFilter } }
    mockApi({
      ...settingsRoutes(),
      'GET /api/settings': () => json(settings({ notification_filter })),
      [PUT]: (init) => {
        body.current = JSON.parse(String(init.body))
        return json(settings({ notification_filter: body.current.notification_filter }))
      },
    })
    render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    return body
  }

  it('nests the two quote kinds under the parent and never hides them', async () => {
    await renderWith(filterOf())
    const children = screen.getByRole('group', { name: '引用RT' })
    expect(within(children).getAllByRole('checkbox')).toEqual([
      checkbox(KIND.quoteSelf),
      checkbox(KIND.quoteOther),
    ])
    expect(within(children).queryByRole('checkbox', { name: KIND.quote })).not.toBeInTheDocument()

    await userEvent.click(checkbox(KIND.quote))
    expect(state(KIND.quote)).toBe('false')
    expect(checkbox(KIND.quoteSelf)).toBeVisible()
    expect(checkbox(KIND.quoteOther)).toBeVisible()
  })

  it.each([
    [true, true, 'true'],
    [true, false, 'mixed'],
    [false, true, 'mixed'],
    [false, false, 'false'],
  ])('shows the parent for quote_self=%s and quote=%s as %s', async (quoteSelf, quote, parent) => {
    await renderWith(filterOf({ quote_self: quoteSelf, quote }))
    expect(state(KIND.quote)).toBe(parent)
    expect(state(KIND.quoteSelf)).toBe(String(quoteSelf))
    expect(state(KIND.quoteOther)).toBe(String(quote))
    expect(checkbox(KIND.quote).dataset.state).toBe(
      { true: 'checked', mixed: 'indeterminate', false: 'unchecked' }[parent],
    )
  })

  it('switches both children together from the parent', async () => {
    await renderWith(filterOf())
    await userEvent.click(checkbox(KIND.quote))
    expect([KIND.quote, KIND.quoteSelf, KIND.quoteOther].map(state)).toEqual([
      'false',
      'false',
      'false',
    ])
    await userEvent.click(checkbox(KIND.quote))
    expect([KIND.quote, KIND.quoteSelf, KIND.quoteOther].map(state)).toEqual([
      'true',
      'true',
      'true',
    ])
  })

  it.each([
    ['quote_self', filterOf({ quote_self: false })],
    ['quote', filterOf({ quote: false })],
  ])('enables both children when the parent is partly off (%s off)', async (_, partial) => {
    const body = await renderWith(partial)
    expect(state(KIND.quote)).toBe('mixed')
    await userEvent.click(checkbox(KIND.quote))
    expect([KIND.quote, KIND.quoteSelf, KIND.quoteOther].map(state)).toEqual([
      'true',
      'true',
      'true',
    ])
    await save()
    await screen.findByText('保存しました。')
    expect(body.current.notification_filter).toEqual(filterOf())
  })

  it('follows the children one by one and leaves the other kinds alone', async () => {
    await renderWith(filterOf())
    await userEvent.click(checkbox(KIND.quoteSelf))
    expect(state(KIND.quote)).toBe('mixed')
    expect(state(KIND.quoteOther)).toBe('true')
    await userEvent.click(checkbox(KIND.quoteOther))
    expect(state(KIND.quote)).toBe('false')
    await userEvent.click(checkbox(KIND.quoteSelf))
    expect(state(KIND.quote)).toBe('mixed')
    expect(state(KIND.quoteSelf)).toBe('true')
    expect(state(KIND.quoteOther)).toBe('false')
    await userEvent.click(checkbox(KIND.quoteOther))
    expect(state(KIND.quote)).toBe('true')
    for (const name of [KIND.tweet, KIND.retweet, KIND.reply, KIND.chat, KIND.other]) {
      expect(checkbox(name)).toBeChecked()
    }
  })

  it('saves quote and quote_self independently of each other', async () => {
    const body = await renderWith(filterOf())
    await userEvent.click(checkbox(KIND.quoteOther))
    await save()
    await screen.findByText('保存しました。')
    expect(body.current.notification_filter).toEqual(filterOf({ quote: false }))
    expect(state(KIND.quoteSelf)).toBe('true')
    expect(state(KIND.quoteOther)).toBe('false')

    await userEvent.click(checkbox(KIND.quoteOther))
    await userEvent.click(checkbox(KIND.quoteSelf))
    await save()
    await waitFor(() =>
      expect(body.current.notification_filter).toEqual(filterOf({ quote_self: false })),
    )
  })
})

describe('Settings cookie check', () => {
  it('starts unchecked and checks on demand with no body, naming the account', async () => {
    let init: RequestInit = {}
    const fetchMock = mockApi({
      ...settingsRoutes(),
      [VALIDATE]: (i) => {
        init = i
        return json(validation({ screen_name: 'alice' }))
      },
    })
    render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    expect(screen.getByText('未確認')).toBeInTheDocument()
    expect(requests(fetchMock, VALIDATE)).toBe(0)
    await userEvent.click(button('確認'))
    expect(await screen.findByText('Cookie は有効です（@alice）。')).toBeInTheDocument()
    expect(screen.getByText('有効')).toBeInTheDocument()
    expect(init.body).toBeUndefined()
    expect(requests(fetchMock, VALIDATE)).toBe(1)
  })

  it('is disabled until a cookie is saved', async () => {
    mockApi({ ...settingsRoutes(), 'GET /api/settings': () => json(unsaved) })
    render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    expect(button('確認')).toBeDisabled()
  })

  it('explains that unsaved input is not what gets checked', async () => {
    mockApi(settingsRoutes())
    render(<Settings />)
    expect(await screen.findByText(/保存済みの Cookie を X に問い合わせます/)).toBeInTheDocument()
    expect(screen.queryByText(/入力中の値は未保存です/)).not.toBeInTheDocument()
    await userEvent.type(screen.getByLabelText(/ct0/), 'abc')
    expect(screen.getByText(/入力中の値は未保存です。保存後に確認します。/)).toBeInTheDocument()
  })

  it('checks automatically after a successful save, separately from the save message', async () => {
    const fetchMock = mockApi({ ...settingsRoutes(), [PUT]: () => json(settings()) })
    render(<Settings />)
    await userEvent.type(await screen.findByLabelText(/auth_token/), 'abc')
    await save()
    expect(await screen.findByText('保存しました。')).toBeInTheDocument()
    expect(await screen.findByText('Cookie は有効です（@alice）。')).toBeInTheDocument()
    expect(requests(fetchMock, VALIDATE)).toBe(1)
    expect(screen.getByText('保存しました。')).not.toBe(screen.getByText(/Cookie は有効です/))
  })

  it('reports an invalid cookie without claiming the save failed or clearing saved state', async () => {
    mockApi({
      ...settingsRoutes(),
      [PUT]: () => json(settings()),
      [VALIDATE]: () => json(validation({ valid: false, screen_name: null, message: 'expired' })),
    })
    render(<Settings />)
    await userEvent.type(await screen.findByLabelText(/ct0/), 'abc')
    await save()
    const alert = await screen.findByRole('alert')
    expect(alert).toHaveTextContent('無効')
    expect(alert).toHaveTextContent('期限切れ')
    expect(screen.getByText('保存しました。')).toBeInTheDocument()
    expect(screen.getAllByText('設定済み')).toHaveLength(2)
    expect(screen.getByLabelText(/ct0/)).toHaveValue('')
  })

  it.each([
    [502, 'upstream'],
    [429, 'rate_limited'],
  ])('shows HTTP %s as "could not check", not as invalid', async (status, code) => {
    mockApi({
      ...settingsRoutes(),
      [VALIDATE]: () => apiError(status, code, '外部サービスに接続できませんでした。'),
    })
    render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    await userEvent.click(button('確認'))
    const result = await screen.findByText(/確認できませんでした/)
    expect(result).toHaveTextContent('Cookie が無効とは限りません')
    expect(result).toHaveTextContent('外部サービスに接続できませんでした。')
    expect(screen.getByText('確認不可')).toBeInTheDocument()
    expect(screen.queryByText('無効')).not.toBeInTheDocument()
    expect(screen.queryByRole('alert')).not.toBeInTheDocument()
  })

  it('treats a network failure as "could not check"', async () => {
    mockApi({
      ...settingsRoutes(),
      [VALIDATE]: () => {
        throw new TypeError('Failed to fetch')
      },
    })
    render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    await userEvent.click(button('確認'))
    expect(await screen.findByText(/確認できませんでした/)).toHaveTextContent(
      'サーバーに接続できません',
    )
    expect(screen.queryByText('無効')).not.toBeInTheDocument()
  })

  it('lets the user check again after it could not be checked', async () => {
    let calls = 0
    mockApi({
      ...settingsRoutes(),
      [VALIDATE]: () => (++calls === 1 ? apiError(502, 'x', '一時的な障害') : json(validation())),
    })
    render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    await userEvent.click(button('確認'))
    await screen.findByText('確認不可')
    await userEvent.click(button('確認'))
    expect(await screen.findByText('有効')).toBeInTheDocument()
  })

  it('ignores an unsafe screen name from the server', async () => {
    mockApi({
      ...settingsRoutes(),
      [VALIDATE]: () => json(validation({ screen_name: '<b>x</b>' })),
    })
    const { container } = render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    await userEvent.click(button('確認'))
    expect(await screen.findByText('Cookie は有効です。')).toBeInTheDocument()
    expect(container.querySelector('b')).toBeNull()
  })
})

describe('Settings exclusivity', () => {
  it('blocks every other action while a save is pending, and keeps the button position', async () => {
    let release: (r: Response) => void = () => {}
    const fetchMock = mockApi({
      ...settingsRoutes(),
      [PUT]: () => new Promise<Response>((r) => (release = r)),
    })
    render(<Settings />)
    await userEvent.type(await screen.findByLabelText(/auth_token/), 'abc')
    await save()
    expect(await screen.findByRole('button', { name: '保存中…' })).toBeDisabled()
    for (const name of ['確認', '登録', 'オンにする']) expect(button(name)).toBeDisabled()
    release(json(settings()))
    await screen.findByText('Cookie は有効です（@alice）。')
    expect(requests(fetchMock, PUT)).toBe(1)
    for (const name of ['保存', '確認', 'オンにする']) expect(button(name)).toBeEnabled()
  })

  it('keeps save disabled and sends one request while the check runs', async () => {
    let release: (r: Response) => void = () => {}
    const fetchMock = mockApi({
      ...settingsRoutes(),
      [VALIDATE]: () => new Promise<Response>((r) => (release = r)),
    })
    render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    await userEvent.click(button('確認'))
    expect(await screen.findByRole('button', { name: '確認中…' })).toBeDisabled()
    expect(button('保存')).toBeDisabled()
    expect(button('登録')).toBeDisabled()
    expect(button('オンにする')).toBeDisabled()
    await userEvent.click(button('保存'))
    expect(requests(fetchMock, PUT)).toBe(0)
    release(json(validation()))
    expect(await screen.findByText('有効')).toBeInTheDocument()
    expect(requests(fetchMock, VALIDATE)).toBe(1)
  })

  it('blocks save and check while registering', async () => {
    let release: (r: Response) => void = () => {}
    mockApi({
      ...settingsRoutes(),
      'POST /api/register': () => new Promise<Response>((r) => (release = r)),
    })
    render(<Settings />)
    await screen.findByText('未登録')
    await userEvent.click(button('登録'))
    expect(await screen.findByRole('button', { name: '登録中…' })).toBeDisabled()
    expect(button('保存')).toBeDisabled()
    expect(button('確認')).toBeDisabled()
    expect(button('オンにする')).toBeDisabled()
    release(json(registration({ registered: true })))
    await screen.findByText('登録済み')
  })

  it('blocks the other actions while the bulk job is being started', async () => {
    let release: (r: Response) => void = () => {}
    mockApi({
      ...settingsRoutes(),
      'POST /api/following-notifications': () => new Promise<Response>((r) => (release = r)),
    })
    render(<Settings />)
    await userEvent.click(await screen.findByRole('button', { name: 'オンにする' }))
    await userEvent.click(button('実行する'))
    expect(await screen.findByRole('button', { name: '開始中…' })).toBeDisabled()
    expect(button('保存')).toBeDisabled()
    expect(button('確認')).toBeDisabled()
    expect(button('登録')).toBeDisabled()
    release(
      json(
        {
          state: 'running',
          total: 1,
          success_count: 0,
          failed_count: 0,
          remaining_count: 1,
          last_error: null,
        },
        202,
      ),
    )
    await waitFor(() => expect(button('保存')).toBeEnabled())
  })
})

describe('Settings registration and bulk notifications', () => {
  it('enables register and bulk start after the first cookies are saved', async () => {
    let configured = false
    mockApi({
      ...settingsRoutes(),
      'GET /api/settings': () => json(configured ? settings() : unsaved),
      [PUT]: () => {
        configured = true
        return json(settings())
      },
    })
    render(<Settings />)
    await screen.findByLabelText(/auth_token/)
    expect(button('登録')).toBeDisabled()
    expect(button('オンにする')).toBeDisabled()
    expect(screen.getAllByText(/先に上の Cookie を保存/)).toHaveLength(2)
    await userEvent.type(screen.getByLabelText(/auth_token/), 'a')
    await userEvent.type(screen.getByLabelText(/ct0/), 'b')
    await save()
    await screen.findByText('有効')
    expect(button('登録')).toBeEnabled()
    expect(button('オンにする')).toBeEnabled()
    expect(screen.queryByText(/先に上の Cookie を保存/)).not.toBeInTheDocument()
  })

  it('reloads the settings after registering so the cookie lock shows up', async () => {
    let registeredNow = false
    const fetchMock = mockApi({
      ...settingsRoutes(),
      'GET /api/settings': () => json(settings({ credentials_locked: registeredNow })),
      'POST /api/register': () => {
        registeredNow = true
        return json(registration({ registered: true, uaid: 'u', channel_id: 'c' }))
      },
    })
    render(<Settings />)
    await userEvent.click(await screen.findByRole('button', { name: '登録' }))
    await screen.findByText('登録済み')
    await waitFor(() => expect(screen.getByLabelText(/auth_token/)).toBeDisabled())
    expect(screen.getByText(/下の「受信先の登録」で登録を解除/)).toBeInTheDocument()
    expect(requests(fetchMock, 'GET /api/settings')).toBe(2)
  })

  it('says so and offers a refetch when the settings cannot be refreshed after registering', async () => {
    let registeredNow = false
    let failRefresh = true
    mockApi({
      ...settingsRoutes(),
      'GET /api/settings': () => {
        if (!registeredNow) return json(settings())
        return failRefresh
          ? apiError(500, 'x', '更新に失敗')
          : json(settings({ credentials_locked: true }))
      },
      'POST /api/register': () => {
        registeredNow = true
        return json(registration({ registered: true }))
      },
    })
    render(<Settings />)
    await userEvent.click(await screen.findByRole('button', { name: '登録' }))
    const alert = await screen.findByRole('alert')
    expect(alert).toHaveTextContent('設定の状態を更新できません')
    expect(alert).toHaveTextContent('更新に失敗')
    expect(screen.getByLabelText(/auth_token/)).toBeEnabled()
    failRefresh = false
    await userEvent.click(screen.getByRole('button', { name: '再取得' }))
    await waitFor(() => expect(screen.getByLabelText(/auth_token/)).toBeDisabled())
    expect(screen.queryByRole('alert')).not.toBeInTheDocument()
  })

  it('keeps bulk notifications usable without a push registration', async () => {
    mockApi(settingsRoutes())
    render(<Settings />)
    expect(await screen.findByText('未登録')).toBeInTheDocument()
    expect(await screen.findByRole('button', { name: 'オンにする' })).toBeEnabled()
  })

  it('does not wait for the registration to load before showing the cookie form', async () => {
    mockApi({
      ...settingsRoutes(),
      'GET /api/registration': () => apiError(500, 'x', '登録の取得に失敗'),
    })
    render(<Settings />)
    expect(await screen.findByLabelText(/auth_token/)).toBeInTheDocument()
    const section = await screen.findByText('登録の取得に失敗')
    expect(section).toBeInTheDocument()
    expect(await screen.findByRole('button', { name: 'オンにする' })).toBeEnabled()
    const group = screen.getByRole('group', { name: '受信先の登録' })
    expect(within(group).getByRole('button', { name: '再試行' })).toBeInTheDocument()
  })
})

describe('Settings polling side effects', () => {
  it('does not poll the cookie check', async () => {
    vi.useFakeTimers()
    const fetchMock = mockApi(settingsRoutes())
    render(<Settings />)
    await vi.advanceTimersByTimeAsync(10_000)
    expect(requests(fetchMock, VALIDATE)).toBe(0)
  })
})

describe('Settings with a server that omits the filter', () => {
  it('fails to load through the existing retry path', async () => {
    const { notification_filter: _, ...legacy } = settings()
    mockApi({ ...settingsRoutes(), 'GET /api/settings': () => json(legacy) })
    render(<Settings />)
    expect(await screen.findByRole('alert')).toHaveTextContent('サーバーでエラーが発生しました。')
    expect(screen.getByRole('button', { name: '再試行' })).toBeInTheDocument()
    expect(screen.queryByRole('checkbox')).not.toBeInTheDocument()
  })
})
