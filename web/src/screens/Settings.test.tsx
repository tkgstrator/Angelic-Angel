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

const PUT = 'PUT /api/accounts/default/settings'
const VALIDATE = 'POST /api/accounts/default/settings/validate'
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
    render(<Settings accountId="default" />)
    expect(await screen.findByLabelText(/auth_token/)).toHaveValue('')
    expect(screen.getByLabelText(/ct0/)).toHaveValue('')
    expect(screen.getByLabelText(/Webhook secret/)).toHaveValue('')
    expect(screen.getAllByText('設定済み')).toHaveLength(3)
    expect(screen.getAllByText(/空欄のまま保存すると/).length).toBeGreaterThanOrEqual(1)
  })

  it('titles the section Cookie and puts registration and bulk notifications below it', async () => {
    mockApi(settingsRoutes())
    render(<Settings accountId="default" />)
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
      'Machine API',
      '受信先の登録',
      'フォロー中の全員の投稿通知をオン',
    ])
    expect(screen.queryByText(/X の Cookie/)).not.toBeInTheDocument()
  })

  it('ends the cookie and webhook form with save, apart from registration and bulk actions', async () => {
    mockApi(settingsRoutes())
    const { container } = render(<Settings accountId="default" />)
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
    render(<Settings accountId="default" />)
    await userEvent.type(await screen.findByLabelText(/ct0/), 'abc')
    await save()
    const message = await screen.findByText('保存しました。')
    expect(message.closest('div')).toBe(button('保存').closest('div'))
  })

  it('requires both cookies on first setup', async () => {
    const fetchMock = mockApi({
      ...settingsRoutes(),
      'GET /api/accounts/default/settings': () => json(unsaved),
    })
    render(<Settings accountId="default" />)
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
    render(<Settings accountId="default" />)
    const url = await screen.findByLabelText('Webhook URL')
    await userEvent.clear(url)
    await userEvent.type(url, 'https://new.example/h')
    await save()
    expect(await screen.findByText('保存しました。')).toBeInTheDocument()
    expect(body).toEqual({
      machine_post_enabled: false,
      machine_like_enabled: false,
      machine_follow_enabled: false,
      auth_token: null,
      ct0: null,
      webhook_secret: null,
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
    render(<Settings accountId="default" />)
    const auth = await screen.findByLabelText(/auth_token/)
    await userEvent.type(auth, 'abc123')
    await save()
    await screen.findByText('保存しました。')
    expect(body.auth_token).toBe('abc123')
    expect(body.ct0).toBeNull()
    expect(auth).toHaveValue('')
  })

  it('keeps a saved webhook secret when left blank and sends a typed one once', async () => {
    const bodies: Record<string, unknown>[] = []
    mockApi({
      ...settingsRoutes(),
      [PUT]: (init) => {
        bodies.push(JSON.parse(String(init.body)))
        return json(settings())
      },
    })
    render(<Settings accountId="default" />)
    const secret = await screen.findByLabelText(/Webhook secret/)
    expect(secret).toHaveAttribute('type', 'password')
    await save()
    await screen.findByText('保存しました。')
    await userEvent.type(secret, '  s3cret  ')
    await save()
    await waitFor(() => expect(bodies).toHaveLength(2))
    expect(bodies[0]?.webhook_secret).toBeNull()
    expect(bodies[1]?.webhook_secret).toBe('s3cret')
    await waitFor(() => expect(secret).toHaveValue(''))
  })

  it('shows the webhook secret as unset and never echoes a value back', async () => {
    mockApi({
      ...settingsRoutes(),
      'GET /api/accounts/default/settings': () => json(settings({ webhook_secret_set: false })),
    })
    const { container } = render(<Settings accountId="default" />)
    const secret = await screen.findByLabelText(/Webhook secret/)
    expect(screen.getByText('Webhook secret').parentElement).toHaveTextContent('未設定')
    expect(secret).toHaveValue('')
    expect(container.textContent).not.toContain('s3cret')
  })

  it('rejects a webhook secret with control characters without sending', async () => {
    const fetchMock = mockApi(settingsRoutes())
    render(<Settings accountId="default" />)
    const secret = await screen.findByLabelText(/Webhook secret/)
    await userEvent.type(secret, 'a')
    await userEvent.paste('\u0007')
    await save()
    expect(await screen.findByText(/Webhook secret に使用できない文字/)).toBeInTheDocument()
    expect(requests(fetchMock, PUT)).toBe(0)
  })

  it('rejects an invalid webhook URL without sending', async () => {
    const fetchMock = mockApi(settingsRoutes())
    render(<Settings accountId="default" />)
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
      'GET /api/accounts/default/settings': () =>
        json(settings({ webhook_source: 'environment', webhook_endpoint: 'https://env.example/' })),
    })
    render(<Settings accountId="default" />)
    expect(await screen.findByLabelText('Webhook URL')).toBeDisabled()
    expect(screen.getByText(/環境変数 WEBHOOK_ENDPOINT/)).toBeInTheDocument()
  })

  it('locks cookie inputs while registered', async () => {
    mockApi({
      ...settingsRoutes(),
      'GET /api/accounts/default/settings': () => json(settings({ credentials_locked: true })),
      'GET /api/accounts/default/registration': () => json(registration({ registered: true })),
    })
    render(<Settings accountId="default" />)
    expect(await screen.findByLabelText(/auth_token/)).toBeDisabled()
    expect(screen.getByLabelText(/ct0/)).toBeDisabled()
    expect(screen.getByLabelText('Webhook URL')).toBeEnabled()
  })

  it('shows the sanitized server error on save failure, keeps typed values and does not check', async () => {
    const fetchMock = mockApi({
      ...settingsRoutes(),
      [PUT]: () => apiError(409, 'credentials_locked', '登録を解除してください。'),
    })
    render(<Settings accountId="default" />)
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
      'GET /api/accounts/default/settings': () =>
        ++calls === 1 ? apiError(500, 'x', '失敗') : json(settings()),
    })
    render(<Settings accountId="default" />)
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
    render(<Settings accountId="default" />)
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
      'GET /api/accounts/default/settings': () =>
        json(settings({ notification_filter: filterOf({ xchat_message_received: false }) })),
    })
    render(<Settings accountId="default" />)
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
      'GET /api/accounts/default/settings': () =>
        json(settings({ notification_filter: filterOf({ quote: false }) })),
      [PUT]: (init) => {
        body.current = JSON.parse(String(init.body))
        return json(settings({ notification_filter: filterOf({ reply: false }) }))
      },
    })
    render(<Settings accountId="default" />)
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
      'GET /api/accounts/default/settings': () =>
        json(settings({ notification_filter: filterOf({ retweet: false }) })),
      [PUT]: (init) => {
        body.current = JSON.parse(String(init.body))
        return json(settings({ notification_filter: filterOf({ retweet: false }) }))
      },
    })
    render(<Settings accountId="default" />)
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
      'GET /api/accounts/default/settings': () => json(settings({ notification_filter: stored })),
      [PUT]: (init) => {
        body = JSON.parse(String(init.body))
        stored = body.notification_filter ?? stored
        return json(settings({ notification_filter: stored }))
      },
    })
    const first = render(<Settings accountId="default" />)
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
    render(<Settings accountId="default" />)
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
    render(<Settings accountId="default" />)
    await screen.findByLabelText(/auth_token/)
    await save()
    await screen.findByText('保存しました。')
    expect(checkbox(KIND.other)).not.toBeChecked()
    expect(checkbox(KIND.tweet)).toBeChecked()
  })

  it('keeps the edit when saving fails', async () => {
    mockApi({ ...settingsRoutes(), [PUT]: () => apiError(500, 'x', '保存失敗') })
    render(<Settings accountId="default" />)
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
      'GET /api/accounts/default/settings': () =>
        json(
          settings({
            credentials_locked: true,
            webhook_source: 'environment',
            webhook_endpoint: 'https://env.example/',
          }),
        ),
      'GET /api/accounts/default/registration': () => json(registration({ registered: true })),
      [PUT]: (init) => {
        body = JSON.parse(String(init.body))
        return json(settings({ notification_filter: filterOf({ tweet: false }) }))
      },
    })
    render(<Settings accountId="default" />)
    expect(await screen.findByLabelText(/auth_token/)).toBeDisabled()
    expect(screen.getByLabelText('Webhook URL')).toBeDisabled()
    for (const name of FILTER_NAMES) expect(checkbox(name)).toBeEnabled()
    await userEvent.click(checkbox(KIND.tweet))
    await save()
    await screen.findByText('保存しました。')
    expect(body).toEqual({
      machine_post_enabled: false,
      machine_like_enabled: false,
      machine_follow_enabled: false,
      auth_token: null,
      ct0: null,
      webhook_secret: null,
      webhook_endpoint: 'https://env.example/',
      notification_filter: filterOf({ tweet: false }),
    })
  })

  it('is disabled together with the other actions while busy', async () => {
    let release: (r: Response) => void = () => {}
    mockApi({ ...settingsRoutes(), [PUT]: () => new Promise<Response>((r) => (release = r)) })
    render(<Settings accountId="default" />)
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
      'GET /api/accounts/default/settings': () => json(settings({ notification_filter })),
      [PUT]: (init) => {
        body.current = JSON.parse(String(init.body))
        return json(settings({ notification_filter: body.current.notification_filter }))
      },
    })
    render(<Settings accountId="default" />)
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
    render(<Settings accountId="default" />)
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
    mockApi({ ...settingsRoutes(), 'GET /api/accounts/default/settings': () => json(unsaved) })
    render(<Settings accountId="default" />)
    await screen.findByLabelText(/auth_token/)
    expect(button('確認')).toBeDisabled()
  })

  it('explains that unsaved input is not what gets checked', async () => {
    mockApi(settingsRoutes())
    render(<Settings accountId="default" />)
    expect(await screen.findByText(/保存済みの Cookie を X に問い合わせます/)).toBeInTheDocument()
    expect(screen.queryByText(/入力中の値は未保存です/)).not.toBeInTheDocument()
    await userEvent.type(screen.getByLabelText(/ct0/), 'abc')
    expect(screen.getByText(/入力中の値は未保存です。保存後に確認します。/)).toBeInTheDocument()
  })

  it('checks automatically after a successful save, separately from the save message', async () => {
    const fetchMock = mockApi({ ...settingsRoutes(), [PUT]: () => json(settings()) })
    render(<Settings accountId="default" />)
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
    render(<Settings accountId="default" />)
    await userEvent.type(await screen.findByLabelText(/ct0/), 'abc')
    await save()
    const alert = await screen.findByRole('alert')
    expect(alert).toHaveTextContent('無効')
    expect(alert).toHaveTextContent('期限切れ')
    expect(screen.getByText('保存しました。')).toBeInTheDocument()
    expect(screen.getAllByText('設定済み')).toHaveLength(3)
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
    render(<Settings accountId="default" />)
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
    render(<Settings accountId="default" />)
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
    render(<Settings accountId="default" />)
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
    const { container } = render(<Settings accountId="default" />)
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
    render(<Settings accountId="default" />)
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
    render(<Settings accountId="default" />)
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
      'POST /api/accounts/default/register': () => new Promise<Response>((r) => (release = r)),
    })
    render(<Settings accountId="default" />)
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
      'POST /api/accounts/default/following-notifications': () =>
        new Promise<Response>((r) => (release = r)),
    })
    render(<Settings accountId="default" />)
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
      'GET /api/accounts/default/settings': () => json(configured ? settings() : unsaved),
      [PUT]: () => {
        configured = true
        return json(settings())
      },
    })
    render(<Settings accountId="default" />)
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
      'GET /api/accounts/default/settings': () =>
        json(settings({ credentials_locked: registeredNow })),
      'POST /api/accounts/default/register': () => {
        registeredNow = true
        return json(registration({ registered: true, uaid: 'u', channel_id: 'c' }))
      },
    })
    render(<Settings accountId="default" />)
    await userEvent.click(await screen.findByRole('button', { name: '登録' }))
    await screen.findByText('登録済み')
    await waitFor(() => expect(screen.getByLabelText(/auth_token/)).toBeDisabled())
    expect(screen.getByText(/下の「受信先の登録」で登録を解除/)).toBeInTheDocument()
    expect(requests(fetchMock, 'GET /api/accounts/default/settings')).toBe(2)
  })

  it('says so and offers a refetch when the settings cannot be refreshed after registering', async () => {
    let registeredNow = false
    let failRefresh = true
    mockApi({
      ...settingsRoutes(),
      'GET /api/accounts/default/settings': () => {
        if (!registeredNow) return json(settings())
        return failRefresh
          ? apiError(500, 'x', '更新に失敗')
          : json(settings({ credentials_locked: true }))
      },
      'POST /api/accounts/default/register': () => {
        registeredNow = true
        return json(registration({ registered: true }))
      },
    })
    render(<Settings accountId="default" />)
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
    render(<Settings accountId="default" />)
    expect(await screen.findByText('未登録')).toBeInTheDocument()
    expect(await screen.findByRole('button', { name: 'オンにする' })).toBeEnabled()
  })

  it('does not wait for the registration to load before showing the cookie form', async () => {
    mockApi({
      ...settingsRoutes(),
      'GET /api/accounts/default/registration': () => apiError(500, 'x', '登録の取得に失敗'),
    })
    render(<Settings accountId="default" />)
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
    render(<Settings accountId="default" />)
    await vi.advanceTimersByTimeAsync(10_000)
    expect(requests(fetchMock, VALIDATE)).toBe(0)
  })
})

describe('Settings with a server that omits the filter', () => {
  it('fails to load through the existing retry path', async () => {
    const { notification_filter: _, ...legacy } = settings()
    mockApi({ ...settingsRoutes(), 'GET /api/accounts/default/settings': () => json(legacy) })
    render(<Settings accountId="default" />)
    expect(await screen.findByRole('alert')).toHaveTextContent('サーバーでエラーが発生しました。')
    expect(screen.getByRole('button', { name: '再試行' })).toBeInTheDocument()
    expect(screen.queryByRole('checkbox')).not.toBeInTheDocument()
  })
})

const POST_LABEL = 'APIからの投稿・返信を許可'
const LIKE_LABEL = 'APIからのいいねを許可'
const FOLLOW_LABEL = 'APIからのフォローを許可'
const MACHINE_LABELS = [POST_LABEL, LIKE_LABEL, FOLLOW_LABEL] as const

type MachineBody = {
  machine_post_enabled?: boolean
  machine_like_enabled?: boolean
  machine_follow_enabled?: boolean
}

/** PUT の本文をそのまま返す保存先。保存済みの3値はサーバー側の状態として保持する。 */
function savingRoutes(bodies: MachineBody[]) {
  return {
    ...settingsRoutes(),
    [PUT]: (init: RequestInit) => {
      const body = JSON.parse(String(init.body)) as MachineBody
      bodies.push(body)
      return json(
        settings({
          machine_post_enabled: body.machine_post_enabled,
          machine_like_enabled: body.machine_like_enabled,
          machine_follow_enabled: body.machine_follow_enabled,
        }),
      )
    },
  }
}

describe('machine posting permission', () => {
  const label = POST_LABEL

  it('defaults off and only applies after save, keeping the returned value', async () => {
    const bodies: Record<string, unknown>[] = []
    mockApi({
      ...settingsRoutes(),
      [PUT]: (init) => {
        const body = JSON.parse(String(init.body))
        bodies.push(body)
        return json(settings({ machine_post_enabled: body.machine_post_enabled }))
      },
    })
    render(<Settings accountId="default" />)
    const toggle = await screen.findByRole('switch', { name: label })
    expect(toggle).not.toBeChecked()
    expect(screen.getByText(/変更は保存後に反映/)).toBeInTheDocument()
    await userEvent.click(toggle)
    expect(toggle).toBeChecked()
    expect(bodies).toHaveLength(0)
    await save()
    await screen.findByText('保存しました。')
    expect(bodies[0]?.machine_post_enabled).toBe(true)
    expect(toggle).toBeChecked()
    await userEvent.click(toggle)
    await save()
    await waitFor(() => expect(bodies).toHaveLength(2))
    expect(bodies[1]?.machine_post_enabled).toBe(false)
    expect(toggle).not.toBeChecked()
  })

  it('preserves the unsaved toggle after a busy save failure and can retry', async () => {
    const attempts: unknown[] = []
    mockApi({
      ...settingsRoutes(),
      [PUT]: (init) => {
        attempts.push(JSON.parse(String(init.body)))
        return attempts.length === 1
          ? apiError(409, 'busy', '送信処理中です。')
          : json(settings({ machine_post_enabled: false }))
      },
      'GET /api/accounts/default/settings': () => json(settings({ machine_post_enabled: true })),
    })
    render(<Settings accountId="default" />)
    const toggle = await screen.findByRole('switch', { name: label })
    expect(toggle).toBeChecked()
    await userEvent.click(toggle)
    await save()
    await screen.findByText(/送信処理中です。/)
    expect(toggle).not.toBeChecked()
    expect(screen.queryByText('保存しました。')).not.toBeInTheDocument()
    await save()
    await screen.findByText('保存しました。')
    expect(attempts).toHaveLength(2)
  })

  it('is editable even when registered and using environment credentials', async () => {
    mockApi({
      ...settingsRoutes(),
      'GET /api/accounts/default/settings': () =>
        json(settings({ credentials_locked: true, webhook_source: 'environment' })),
    })
    render(<Settings accountId="default" />)
    const toggle = await screen.findByRole('switch', { name: label })
    expect(toggle).toBeEnabled()
    expect(screen.getByLabelText(/auth_token/)).toBeDisabled()
    await userEvent.click(toggle)
    expect(toggle).toBeChecked()
  })

  it('disables the toggle while saving without hiding it', async () => {
    const pending: { resolve?: (response: Response) => void } = {}
    mockApi({
      ...settingsRoutes(),
      [PUT]: () =>
        new Promise<Response>((resolve) => {
          pending.resolve = resolve
        }),
    })
    render(<Settings accountId="default" />)
    const toggle = await screen.findByRole('switch', { name: label })
    await userEvent.click(toggle)
    await save()
    expect(toggle).toBeDisabled()
    pending.resolve?.(json(settings({ machine_post_enabled: true })))
    await screen.findByText('保存しました。')
    expect(toggle).toBeEnabled()
  })
})

describe('machine like and follow permissions', () => {
  it('lists the three switches in the Machine API section, all off by default', async () => {
    mockApi(settingsRoutes())
    render(<Settings accountId="default" />)
    await screen.findByRole('switch', { name: POST_LABEL })
    for (const name of MACHINE_LABELS) {
      expect(screen.getByRole('switch', { name })).not.toBeChecked()
    }
    const section = screen.getByRole('group', { name: 'Machine API' })
    expect(within(section).getAllByRole('switch')).toHaveLength(3)
    expect(screen.getByText(/サービストークン側の許可も必要/)).toBeInTheDocument()
    expect(screen.getByText(/処理中は保存できません（409 busy）/)).toBeInTheDocument()
    expect(screen.getByText(/開始済みの処理は取り消せません/)).toBeInTheDocument()
  })

  it('shows the saved state of each switch independently', async () => {
    mockApi({
      ...settingsRoutes(),
      'GET /api/accounts/default/settings': () => json(settings({ machine_like_enabled: true })),
    })
    render(<Settings accountId="default" />)
    expect(await screen.findByRole('switch', { name: LIKE_LABEL })).toBeChecked()
    expect(screen.getByRole('switch', { name: POST_LABEL })).not.toBeChecked()
    expect(screen.getByRole('switch', { name: FOLLOW_LABEL })).not.toBeChecked()
  })

  it('saves like and follow independently of posting, and only after save', async () => {
    const bodies: MachineBody[] = []
    mockApi(savingRoutes(bodies))
    render(<Settings accountId="default" />)
    const like = await screen.findByRole('switch', { name: LIKE_LABEL })
    const follow = screen.getByRole('switch', { name: FOLLOW_LABEL })
    const post = screen.getByRole('switch', { name: POST_LABEL })

    await userEvent.click(like)
    expect(like).toBeChecked()
    expect(post).not.toBeChecked()
    expect(follow).not.toBeChecked()
    expect(bodies).toHaveLength(0)
    await save()
    await screen.findByText('保存しました。')
    expect(bodies[0]).toMatchObject({
      machine_post_enabled: false,
      machine_like_enabled: true,
      machine_follow_enabled: false,
    })

    await userEvent.click(follow)
    await save()
    await waitFor(() => expect(bodies).toHaveLength(2))
    expect(bodies[1]).toMatchObject({
      machine_post_enabled: false,
      machine_like_enabled: true,
      machine_follow_enabled: true,
    })

    await userEvent.click(like)
    await userEvent.click(post)
    await save()
    await waitFor(() => expect(bodies).toHaveLength(3))
    expect(bodies[2]).toMatchObject({
      machine_post_enabled: true,
      machine_like_enabled: false,
      machine_follow_enabled: true,
    })
    expect(like).not.toBeChecked()
    expect(follow).toBeChecked()
    expect(post).toBeChecked()
  })

  it('keeps unsaved like and follow changes after a failed save and retries them', async () => {
    const attempts: MachineBody[] = []
    mockApi({
      ...settingsRoutes(),
      [PUT]: (init) => {
        const body = JSON.parse(String(init.body)) as MachineBody
        attempts.push(body)
        return attempts.length === 1
          ? apiError(409, 'busy', '処理中です。')
          : json(
              settings({
                machine_post_enabled: body.machine_post_enabled,
                machine_like_enabled: body.machine_like_enabled,
                machine_follow_enabled: body.machine_follow_enabled,
              }),
            )
      },
      'GET /api/accounts/default/settings': () => json(settings({ machine_like_enabled: true })),
    })
    render(<Settings accountId="default" />)
    const like = await screen.findByRole('switch', { name: LIKE_LABEL })
    const follow = screen.getByRole('switch', { name: FOLLOW_LABEL })
    await userEvent.click(like)
    await userEvent.click(follow)
    expect(like).not.toBeChecked()
    expect(follow).toBeChecked()
    await save()
    await screen.findByText(/処理中です。/)
    expect(screen.queryByText('保存しました。')).not.toBeInTheDocument()
    expect(like).not.toBeChecked()
    expect(follow).toBeChecked()
    await save()
    await screen.findByText('保存しました。')
    expect(attempts).toHaveLength(2)
    expect(attempts[1]).toMatchObject({
      machine_like_enabled: false,
      machine_follow_enabled: true,
    })
    expect(attempts[1]).toEqual(attempts[0])
  })

  it('reflects the value the server returned, not the value that was sent', async () => {
    mockApi({
      ...settingsRoutes(),
      [PUT]: () => json(settings({ machine_follow_enabled: false, machine_like_enabled: true })),
    })
    render(<Settings accountId="default" />)
    const follow = await screen.findByRole('switch', { name: FOLLOW_LABEL })
    await userEvent.click(follow)
    expect(follow).toBeChecked()
    await save()
    await screen.findByText('保存しました。')
    expect(follow).not.toBeChecked()
    expect(screen.getByRole('switch', { name: LIKE_LABEL })).toBeChecked()
  })

  it('disables all three switches while saving without hiding them', async () => {
    const pending: { resolve?: (response: Response) => void } = {}
    mockApi({
      ...settingsRoutes(),
      [PUT]: () =>
        new Promise<Response>((resolve) => {
          pending.resolve = resolve
        }),
    })
    render(<Settings accountId="default" />)
    await userEvent.click(await screen.findByRole('switch', { name: LIKE_LABEL }))
    await save()
    for (const name of MACHINE_LABELS) {
      expect(screen.getByRole('switch', { name })).toBeDisabled()
    }
    pending.resolve?.(json(settings({ machine_like_enabled: true })))
    await screen.findByText('保存しました。')
    for (const name of MACHINE_LABELS) {
      expect(screen.getByRole('switch', { name })).toBeEnabled()
    }
    expect(screen.getByRole('switch', { name: LIKE_LABEL })).toBeChecked()
  })

  it('does not show a switch as on when the saved response is invalid', async () => {
    mockApi({
      ...settingsRoutes(),
      [PUT]: () =>
        json({ ...settings({ machine_follow_enabled: true }), machine_like_enabled: 'true' }),
    })
    render(<Settings accountId="default" />)
    const follow = await screen.findByRole('switch', { name: FOLLOW_LABEL })
    await userEvent.click(follow)
    await save()
    expect(await screen.findByRole('alert')).toBeInTheDocument()
    expect(screen.queryByText('保存しました。')).not.toBeInTheDocument()
    expect(screen.getByRole('switch', { name: LIKE_LABEL })).not.toBeChecked()
    expect(follow).toBeChecked()
  })
})
