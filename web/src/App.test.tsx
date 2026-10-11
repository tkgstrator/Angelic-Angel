import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it } from 'vitest'
import { App } from './App'
import { apiError, json, mockApi, page, settings, settingsRoutes, status } from './test/helpers'

const heading = (name: string) =>
  waitFor(() => expect(screen.getByRole('heading', { name })).toBeInTheDocument())

const routes = {
  ...settingsRoutes(),
  'GET /api/status': () => json(status()),
  'GET /api/notifications?limit=50': () => json(page(['a'])),
}

describe('App', () => {
  it('shows the settings screen immediately without any login step', async () => {
    const fetchMock = mockApi(routes)
    render(<App />)
    await heading('設定')
    expect(screen.queryByLabelText('管理トークン')).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'ログイン' })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'ログアウト' })).not.toBeInTheDocument()
    const init = fetchMock.mock.calls[0]?.[1] as RequestInit
    expect(init.headers as Record<string, string>).not.toHaveProperty('Authorization')
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
      'GET /api/settings': () =>
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
      'GET /api/settings': () => apiError(500, 'internal', 'x'.repeat(500)),
    })
    render(<App />)
    expect(await screen.findByRole('alert')).toHaveTextContent('サーバーでエラーが発生しました')
  })
})
