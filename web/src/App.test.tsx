import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it } from 'vitest'
import { App } from './App'
import {
  account,
  apiError,
  json,
  mockApi,
  page,
  requests,
  settings,
  settingsRoutes,
  status,
} from './test/helpers'

const heading = (name: string) =>
  waitFor(() => expect(screen.getByRole('heading', { name })).toBeInTheDocument())

const LIST = 'GET /api/accounts'

const routes = {
  [LIST]: () => json({ accounts: [account('default')] }),
  ...settingsRoutes(),
  'GET /api/accounts/default/status': () => json(status()),
  'GET /api/accounts/default/notifications?limit=50': () => json(page(['a'])),
}

describe('App', () => {
  it('shows the settings screen immediately without any login step', async () => {
    const fetchMock = mockApi(routes)
    render(<App />)
    await heading('設定')
    expect(screen.queryByLabelText('管理トークン')).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'ログイン' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'ログアウト' })).not.toBeInTheDocument()
    const init = fetchMock.mock.calls[0]?.[1] ?? {}
    expect(init.headers).not.toHaveProperty('Authorization')
  })

  it('shows the app name as "Angelic Angel" in the header', async () => {
    mockApi(routes)
    render(<App />)
    await heading('設定')
    expect(screen.getByRole('heading', { level: 1, name: 'Angelic Angel' })).toBeInTheDocument()
    expect(screen.queryByText('angelic-angel')).not.toBeInTheDocument()
  })

  it('has no registration screen: nav is notifications, settings, status', async () => {
    mockApi(routes)
    render(<App />)
    await heading('設定')
    const nav = screen.getByRole('navigation', { name: 'メイン' })
    expect(
      within(nav)
        .getAllByRole('link')
        .map((l) => l.textContent),
    ).toEqual(['通知', '設定', '受信状況'])
    expect(within(nav).queryByRole('link', { name: '登録' })).not.toBeInTheDocument()
    expect(screen.getByRole('heading', { name: '受信先の登録' })).toBeInTheDocument()
  })

  it('navigates between the three screens', async () => {
    mockApi(routes)
    render(<App />)
    await heading('設定')
    expect(screen.getByRole('link', { name: '設定' })).toHaveAttribute('aria-current', 'page')
    await userEvent.click(screen.getByRole('link', { name: '通知' }))
    await heading('通知')
    expect(window.location.hash).toBe('#/default/notifications')
    expect(await screen.findByText('title a')).toBeInTheDocument()
    expect(screen.getByRole('link', { name: '通知' })).toHaveAttribute('aria-current', 'page')
    await userEvent.click(screen.getByRole('link', { name: '受信状況' }))
    await heading('受信状況')
    await userEvent.click(screen.getByRole('link', { name: '設定' }))
    await heading('設定')
  })

  it('falls back to settings for the removed #/registration hash', async () => {
    window.location.hash = '#/registration'
    mockApi(routes)
    render(<App />)
    await heading('設定')
    expect(screen.getByRole('link', { name: '設定' })).toHaveAttribute('aria-current', 'page')
  })

  it('recovers from an API error with the retry button', async () => {
    let calls = 0
    mockApi({
      ...routes,
      'GET /api/accounts/default/settings': () =>
        ++calls === 1 ? apiError(500, 'internal', '一時的な障害') : json(settings()),
    })
    render(<App />)
    expect(await screen.findByRole('alert')).toHaveTextContent('一時的な障害')
    await userEvent.click(screen.getByRole('button', { name: '再試行' }))
    expect(await screen.findByLabelText('Webhook URL')).toBeInTheDocument()
  })

  it('shows a generic message for unsafe or oversized server errors', async () => {
    mockApi({
      ...routes,
      'GET /api/accounts/default/settings': () => apiError(500, 'internal', 'x'.repeat(500)),
    })
    render(<App />)
    expect(await screen.findByRole('alert')).toHaveTextContent('サーバーでエラーが発生しました')
  })
})

describe('App routing', () => {
  it.each([
    ['#/settings', '設定'],
    ['#/notifications', '通知'],
    ['#/status', '受信状況'],
    ['', '設定'],
    ['#/nonsense', '設定'],
  ])(
    'resolves the old hash %j to the default account without rewriting the URL',
    async (hash, title) => {
      window.location.hash = hash
      const before = window.location.href
      mockApi({
        [LIST]: () => json({ accounts: [account('alpha'), account('default')] }),
        'GET /api/accounts/default/settings': () => json(settings()),
        'GET /api/accounts/default/registration': () =>
          json({ registered: false, uaid: null, channel_id: null }),
        'GET /api/accounts/default/following-notifications': () =>
          json({
            state: 'idle',
            total: null,
            success_count: 0,
            failed_count: 0,
            remaining_count: null,
            last_error: null,
          }),
        'GET /api/accounts/default/status': () => json(status()),
        'GET /api/accounts/default/notifications?limit=50': () => json(page([])),
      })
      render(<App />)
      await heading(title)
      expect(window.location.href).toBe(before)
      expect(screen.getByRole('combobox', { name: 'アカウント' })).toHaveTextContent('default')
    },
  )

  it('falls back to the first account when there is no default', async () => {
    window.location.hash = '#/status'
    mockApi({
      [LIST]: () => json({ accounts: [account('beta'), account('alpha')] }),
      'GET /api/accounts/beta/status': () => json(status()),
    })
    render(<App />)
    await heading('受信状況')
    expect(window.location.hash).toBe('#/status')
    expect(screen.getByRole('combobox', { name: 'アカウント' })).toHaveTextContent('beta')
  })

  it('shows the account named in the hash', async () => {
    window.location.hash = '#/alpha/status'
    const fetchMock = mockApi({
      [LIST]: () => json({ accounts: [account('default'), account('alpha')] }),
      'GET /api/accounts/alpha/status': () => json(status()),
    })
    render(<App />)
    await heading('受信状況')
    expect(screen.getByRole('combobox', { name: 'アカウント' })).toHaveTextContent('alpha')
    expect(requests(fetchMock, 'GET /api/accounts/alpha/status')).toBe(1)
    expect(screen.getByRole('link', { name: '設定' })).toHaveAttribute('href', '#/alpha/settings')
  })

  it('says so when the named account does not exist and does not guess another one', async () => {
    window.location.hash = '#/gone/status'
    const fetchMock = mockApi({ [LIST]: () => json({ accounts: [account('default')] }) })
    render(<App />)
    await heading('アカウントが見つかりません')
    expect(screen.getByText(/「gone」はありません/)).toBeInTheDocument()
    expect(fetchMock.mock.calls.map(([url]) => url)).toEqual(['/api/accounts'])
    expect(window.location.hash).toBe('#/gone/status')
  })

  it('shows an empty state with an add action when there are no accounts', async () => {
    const fetchMock = mockApi({ [LIST]: () => json({ accounts: [] }) })
    render(<App />)
    await heading('アカウントがありません')
    expect(
      within(screen.getByRole('main')).getByRole('button', { name: 'アカウントを追加' }),
    ).toBeEnabled()
    expect(screen.getByRole('combobox', { name: 'アカウント' })).toBeDisabled()
    expect(screen.getByRole('button', { name: 'アカウントを削除' })).toBeDisabled()
    expect(requests(fetchMock, LIST)).toBe(1)
  })

  it('offers a retry when the account list cannot be loaded', async () => {
    const calls = { n: 0 }
    mockApi({
      ...routes,
      [LIST]: () =>
        ++calls.n === 1
          ? apiError(500, 'internal', '一覧の障害')
          : json({ accounts: [account('default')] }),
    })
    render(<App />)
    expect(await screen.findByRole('alert')).toHaveTextContent('一覧の障害')
    await userEvent.click(screen.getByRole('button', { name: '再試行' }))
    await heading('設定')
  })

  it('lists the accounts and switches the whole screen when another one is picked', async () => {
    window.location.hash = '#/default/status'
    const fetchMock = mockApi({
      [LIST]: () => json({ accounts: [account('default'), account('alpha')] }),
      'GET /api/accounts/default/status': () => json(status({ received_count: 11 })),
      'GET /api/accounts/alpha/status': () => json(status({ received_count: 22 })),
    })
    render(<App />)
    expect(await screen.findByText('11')).toBeInTheDocument()
    await userEvent.click(screen.getByRole('combobox', { name: 'アカウント' }))
    const options = screen.getAllByRole('option').map((o) => o.textContent)
    expect(options).toEqual(['default', 'alpha'])
    await userEvent.click(screen.getByRole('option', { name: 'alpha' }))
    expect(await screen.findByText('22')).toBeInTheDocument()
    expect(screen.queryByText('11')).not.toBeInTheDocument()
    expect(window.location.hash).toBe('#/alpha/status')
    expect(requests(fetchMock, 'GET /api/accounts/alpha/status')).toBe(1)
    expect(requests(fetchMock, 'GET /api/accounts/default/status')).toBe(1)
  })

  it('refetches every account-scoped screen from scratch on switch (key remount)', async () => {
    window.location.hash = '#/default/settings'
    const fetchMock = mockApi({
      [LIST]: () => json({ accounts: [account('default'), account('alpha')] }),
      ...settingsRoutes(),
      'GET /api/accounts/alpha/settings': () =>
        json(settings({ webhook_endpoint: 'https://alpha.example/h' })),
      'GET /api/accounts/alpha/registration': () =>
        json({ registered: false, uaid: null, channel_id: null }),
      'GET /api/accounts/alpha/following-notifications': () =>
        json({
          state: 'idle',
          total: null,
          success_count: 0,
          failed_count: 0,
          remaining_count: null,
          last_error: null,
        }),
    })
    render(<App />)
    expect(await screen.findByLabelText('Webhook URL')).toHaveValue('https://example.com/hook')
    await userEvent.click(screen.getByRole('combobox', { name: 'アカウント' }))
    await userEvent.click(screen.getByRole('option', { name: 'alpha' }))
    await waitFor(() =>
      expect(screen.getByLabelText('Webhook URL')).toHaveValue('https://alpha.example/h'),
    )
    for (const path of ['settings', 'registration', 'following-notifications']) {
      expect(requests(fetchMock, `GET /api/accounts/alpha/${path}`)).toBe(1)
    }
  })
})
