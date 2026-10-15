import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it } from 'vitest'
import { App } from './App'
import {
  account,
  apiError,
  following,
  json,
  mockApi,
  registration,
  requests,
  settings,
  status,
} from './test/helpers'

const LIST = 'GET /api/accounts'
const CREATE = 'POST /api/accounts'

const quietAccount = (id: string) => ({
  [`GET /api/accounts/${id}/settings`]: () => json(settings()),
  [`GET /api/accounts/${id}/registration`]: () => json(registration()),
  [`GET /api/accounts/${id}/following-notifications`]: () => json(following()),
  [`GET /api/accounts/${id}/status`]: () => json(status()),
})

const openAdd = () => userEvent.click(screen.getByRole('button', { name: 'アカウントを追加' }))
const dialog = () => screen.findByRole('dialog')

describe('adding an account', () => {
  it('creates the account, selects it and lands on its settings', async () => {
    window.location.hash = '#/default/status'
    const bodies: unknown[] = []
    mockApi({
      [LIST]: () => json({ accounts: [account('default')] }),
      ...quietAccount('default'),
      ...quietAccount('work-2'),
      [CREATE]: (init) => {
        bodies.push(JSON.parse(String(init.body)))
        return json(account('work-2', { registered: false, receiver_state: 'unconfigured' }), 201)
      },
    })
    render(<App />)
    await screen.findByRole('heading', { name: '受信状況' })
    await openAdd()
    const box = within(await dialog())
    await userEvent.type(box.getByLabelText('アカウント ID'), 'work-2')
    await userEvent.click(box.getByRole('button', { name: '追加' }))
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument())
    expect(bodies).toEqual([{ id: 'work-2' }])
    expect(window.location.hash).toBe('#/work-2/settings')
    expect(await screen.findByRole('heading', { name: '設定' })).toBeInTheDocument()
    expect(screen.getByRole('combobox', { name: 'アカウント' })).toHaveTextContent('work-2')
  })

  it('shows the id rule and rejects an invalid id without calling the server', async () => {
    const fetchMock = mockApi({
      [LIST]: () => json({ accounts: [account('default')] }),
      ...quietAccount('default'),
    })
    render(<App />)
    await screen.findByRole('heading', { name: '設定' })
    await openAdd()
    const box = within(await dialog())
    expect(box.getByText('/^[a-z0-9][a-z0-9_-]{0,31}$/')).toBeInTheDocument()
    for (const bad of ['Upper', '-lead', 'has space', 'a'.repeat(33), '']) {
      const input = box.getByLabelText('アカウント ID')
      await userEvent.clear(input)
      if (bad !== '') await userEvent.type(input, bad)
      await userEvent.click(box.getByRole('button', { name: '追加' }))
      expect(await box.findByRole('alert')).toHaveTextContent('英小文字')
      expect(input).toHaveAttribute('aria-invalid', 'true')
    }
    expect(requests(fetchMock, CREATE)).toBe(0)
    expect(screen.getByRole('dialog')).toBeInTheDocument()
  })

  it('reports a duplicate id from the server and keeps the dialog open', async () => {
    mockApi({
      [LIST]: () => json({ accounts: [account('default')] }),
      ...quietAccount('default'),
      [CREATE]: () => apiError(409, 'account_exists', 'account already exists'),
    })
    render(<App />)
    await screen.findByRole('heading', { name: '設定' })
    await openAdd()
    const box = within(await dialog())
    await userEvent.type(box.getByLabelText('アカウント ID'), 'default')
    await userEvent.click(box.getByRole('button', { name: '追加' }))
    expect(await box.findByRole('alert')).toHaveTextContent('既に存在')
    expect(box.getByLabelText('アカウント ID')).toHaveValue('default')
    expect(box.getByRole('button', { name: '追加' })).toBeEnabled()
    expect(window.location.hash).toBe('')
  })

  it('shows a server-side invalid id error with the rule', async () => {
    mockApi({
      [LIST]: () => json({ accounts: [account('default')] }),
      ...quietAccount('default'),
      [CREATE]: () => apiError(400, 'invalid_account_id', 'bad id'),
    })
    render(<App />)
    await screen.findByRole('heading', { name: '設定' })
    await openAdd()
    const box = within(await dialog())
    await userEvent.type(box.getByLabelText('アカウント ID'), 'ok')
    await userEvent.click(box.getByRole('button', { name: '追加' }))
    expect(await box.findByRole('alert')).toHaveTextContent('英小文字')
  })

  it('shows other server errors with the generic error display', async () => {
    mockApi({
      [LIST]: () => json({ accounts: [account('default')] }),
      ...quietAccount('default'),
      [CREATE]: () => apiError(500, 'internal', '保存できませんでした'),
    })
    render(<App />)
    await screen.findByRole('heading', { name: '設定' })
    await openAdd()
    const box = within(await dialog())
    await userEvent.type(box.getByLabelText('アカウント ID'), 'ok')
    await userEvent.click(box.getByRole('button', { name: '追加' }))
    expect(await box.findByRole('alert')).toHaveTextContent('保存できませんでした')
  })

  it('discards what was typed when the dialog is cancelled', async () => {
    mockApi({
      [LIST]: () => json({ accounts: [account('default')] }),
      ...quietAccount('default'),
    })
    render(<App />)
    await screen.findByRole('heading', { name: '設定' })
    await openAdd()
    await userEvent.type(within(await dialog()).getByLabelText('アカウント ID'), 'draft')
    await userEvent.click(
      within(screen.getByRole('dialog')).getByRole('button', { name: 'キャンセル' }),
    )
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument())
    await openAdd()
    expect(within(await dialog()).getByLabelText('アカウント ID')).toHaveValue('')
  })

  it('adds the first account from the empty state', async () => {
    mockApi({
      [LIST]: () => json({ accounts: [] }),
      ...quietAccount('default'),
      [CREATE]: () =>
        json(account('default', { registered: false, receiver_state: 'unconfigured' }), 201),
    })
    render(<App />)
    await screen.findByRole('heading', { name: 'アカウントがありません' })
    await userEvent.click(
      within(screen.getByRole('main')).getByRole('button', { name: 'アカウントを追加' }),
    )
    const box = within(await dialog())
    await userEvent.type(box.getByLabelText('アカウント ID'), 'default')
    await userEvent.click(box.getByRole('button', { name: '追加' }))
    expect(await screen.findByRole('heading', { name: '設定' })).toBeInTheDocument()
    expect(window.location.hash).toBe('#/default/settings')
  })
})

describe('deleting an account', () => {
  const DELETE = 'DELETE /api/accounts/alpha'
  const twoAccounts = () => ({
    [LIST]: () => json({ accounts: [account('default'), account('alpha')] }),
    ...quietAccount('default'),
    ...quietAccount('alpha'),
  })
  const openDelete = () => userEvent.click(screen.getByRole('button', { name: 'アカウントを削除' }))

  it('explains that the registration on X and the notification history are removed', async () => {
    window.location.hash = '#/alpha/status'
    mockApi(twoAccounts())
    render(<App />)
    await screen.findByRole('heading', { name: '受信状況' })
    await openDelete()
    const box = within(await screen.findByRole('alertdialog'))
    expect(box.getByRole('heading', { name: /「alpha」/ })).toBeInTheDocument()
    expect(box.getByText(/X 上の受信先の登録を解除/)).toBeInTheDocument()
    expect(box.getByText(/通知履歴をすべて削除/)).toBeInTheDocument()
  })

  it('does nothing when cancelled', async () => {
    window.location.hash = '#/alpha/status'
    const fetchMock = mockApi(twoAccounts())
    render(<App />)
    await screen.findByRole('heading', { name: '受信状況' })
    await openDelete()
    await userEvent.click(
      within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'キャンセル' }),
    )
    await waitFor(() => expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument())
    expect(requests(fetchMock, DELETE)).toBe(0)
    expect(screen.getByRole('combobox', { name: 'アカウント' })).toHaveTextContent('alpha')
  })

  it('deletes the account and moves to the default one', async () => {
    window.location.hash = '#/alpha/status'
    mockApi({ ...twoAccounts(), [DELETE]: () => json({}) })
    render(<App />)
    await screen.findByRole('heading', { name: '受信状況' })
    await openDelete()
    await userEvent.click(
      within(await screen.findByRole('alertdialog')).getByRole('button', { name: '削除する' }),
    )
    await waitFor(() => expect(window.location.hash).toBe('#/default/status'))
    await waitFor(() =>
      expect(screen.getByRole('combobox', { name: 'アカウント' })).toHaveTextContent('default'),
    )
    expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument()
  })

  it('accepts an empty 204 response as success', async () => {
    window.location.hash = '#/alpha/status'
    mockApi({ ...twoAccounts(), [DELETE]: () => new Response(null, { status: 204 }) })
    render(<App />)
    await screen.findByRole('heading', { name: '受信状況' })
    await openDelete()
    await userEvent.click(
      within(await screen.findByRole('alertdialog')).getByRole('button', { name: '削除する' }),
    )
    await waitFor(() => expect(window.location.hash).toBe('#/default/status'))
  })

  it('falls to the empty state when the last account is deleted', async () => {
    window.location.hash = '#/default/status'
    mockApi({
      [LIST]: () => json({ accounts: [account('default')] }),
      ...quietAccount('default'),
      'DELETE /api/accounts/default': () => json({}),
    })
    render(<App />)
    await screen.findByRole('heading', { name: '受信状況' })
    await openDelete()
    await userEvent.click(
      within(await screen.findByRole('alertdialog')).getByRole('button', { name: '削除する' }),
    )
    expect(
      await screen.findByRole('heading', { name: 'アカウントがありません' }),
    ).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'アカウントを削除' })).toBeDisabled()
  })

  it('keeps the account and shows the error when unregistering on X fails, and allows a retry', async () => {
    window.location.hash = '#/alpha/status'
    const calls = { n: 0 }
    mockApi({
      ...twoAccounts(),
      [DELETE]: () =>
        ++calls.n === 1 ? apiError(502, 'upstream', 'X の登録解除に失敗しました') : json({}),
    })
    render(<App />)
    await screen.findByRole('heading', { name: '受信状況' })
    await openDelete()
    const box = within(await screen.findByRole('alertdialog'))
    await userEvent.click(box.getByRole('button', { name: '削除する' }))
    expect(await box.findByRole('alert')).toHaveTextContent('X の登録解除に失敗しました')
    expect(window.location.hash).toBe('#/alpha/status')
    expect(screen.getByRole('combobox', { name: 'アカウント', hidden: true })).toHaveTextContent(
      'alpha',
    )
    await userEvent.click(box.getByRole('button', { name: '削除を再試行' }))
    await waitFor(() => expect(window.location.hash).toBe('#/default/status'))
    expect(calls.n).toBe(2)
  })

  const confirmDelete = async () =>
    userEvent.click(
      within(await screen.findByRole('alertdialog')).getByRole('button', { name: '削除する' }),
    )
  const cleanupFailed = () =>
    apiError(500, 'history_cleanup_failed', 'The account was removed, but its history is left.')

  it('treats a failed history cleanup as deleted: closes the dialog, drops the account and keeps a warning', async () => {
    window.location.hash = '#/alpha/status'
    const fetchMock = mockApi({ ...twoAccounts(), [DELETE]: cleanupFailed })
    render(<App />)
    await screen.findByRole('heading', { name: '受信状況' })
    await openDelete()
    await confirmDelete()

    await waitFor(() => expect(window.location.hash).toBe('#/default/status'))
    await waitFor(() => expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument())
    expect(screen.getByRole('combobox', { name: 'アカウント' })).toHaveTextContent('default')
    const warning = await screen.findByRole('alert')
    expect(warning).toHaveTextContent('「alpha」')
    expect(warning).toHaveTextContent('履歴のファイルを削除できませんでした')
    expect(warning).toHaveTextContent('手動で削除')
    // The deleted account is gone from the list: its address no longer resolves.
    window.location.hash = '#/alpha/status'
    expect(
      await screen.findByRole('heading', { name: 'アカウントが見つかりません' }),
    ).toBeInTheDocument()
    // The warning outlives the dialog and the navigation, and no retry was sent.
    expect(screen.getByRole('alert')).toHaveTextContent('「alpha」')
    expect(requests(fetchMock, DELETE)).toBe(1)
  })

  it('keeps the history warning until it is dismissed', async () => {
    window.location.hash = '#/alpha/status'
    mockApi({ ...twoAccounts(), [DELETE]: cleanupFailed })
    render(<App />)
    await screen.findByRole('heading', { name: '受信状況' })
    await openDelete()
    await confirmDelete()
    expect(await screen.findByRole('alert')).toHaveTextContent('「alpha」')
    await userEvent.click(screen.getByRole('button', { name: '閉じる' }))
    expect(screen.queryByRole('alert')).not.toBeInTheDocument()
  })

  it('shows the warning in the empty state when the last account was deleted with its history kept', async () => {
    window.location.hash = '#/default/status'
    mockApi({
      [LIST]: () => json({ accounts: [account('default')] }),
      ...quietAccount('default'),
      'DELETE /api/accounts/default': cleanupFailed,
    })
    render(<App />)
    await screen.findByRole('heading', { name: '受信状況' })
    await openDelete()
    await confirmDelete()
    expect(
      await screen.findByRole('heading', { name: 'アカウントがありません' }),
    ).toBeInTheDocument()
    expect(screen.getByRole('alert')).toHaveTextContent('「default」')
    expect(screen.getByRole('button', { name: 'アカウントを削除' })).toBeDisabled()
  })

  it('drops the warning once an account with that id is created again', async () => {
    window.location.hash = '#/alpha/status'
    mockApi({
      ...twoAccounts(),
      ...quietAccount('alpha'),
      [DELETE]: cleanupFailed,
      [CREATE]: () =>
        json(account('alpha', { registered: false, receiver_state: 'unconfigured' }), 201),
    })
    render(<App />)
    await screen.findByRole('heading', { name: '受信状況' })
    await openDelete()
    await confirmDelete()
    expect(await screen.findByRole('alert')).toHaveTextContent('「alpha」')

    await openAdd()
    const box = within(await dialog())
    await userEvent.type(box.getByLabelText('アカウント ID'), 'alpha')
    await userEvent.click(box.getByRole('button', { name: '追加' }))
    await waitFor(() => expect(window.location.hash).toBe('#/alpha/settings'))
    await waitFor(() => expect(screen.queryByRole('alert')).not.toBeInTheDocument())
  })

  it('removes an account the server no longer knows (404) without a warning', async () => {
    window.location.hash = '#/alpha/status'
    const fetchMock = mockApi({
      ...twoAccounts(),
      [DELETE]: () => apiError(404, 'account_not_found', 'No such account.'),
    })
    render(<App />)
    await screen.findByRole('heading', { name: '受信状況' })
    await openDelete()
    await confirmDelete()

    await waitFor(() => expect(window.location.hash).toBe('#/default/status'))
    await waitFor(() => expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument())
    expect(screen.getByRole('combobox', { name: 'アカウント' })).toHaveTextContent('default')
    expect(screen.queryByRole('alert')).not.toBeInTheDocument()
    window.location.hash = '#/alpha/status'
    expect(
      await screen.findByRole('heading', { name: 'アカウントが見つかりません' }),
    ).toBeInTheDocument()
    expect(requests(fetchMock, DELETE)).toBe(1)
  })

  it.each([
    [500, 'persistence_failed', '設定を書き込めませんでした'],
    [409, 'busy', '別の操作が進行中です'],
    [404, 'not_found', '見つかりません'],
  ])(
    'keeps the account and the dialog when the server answers %i %s',
    async (status, code, message) => {
      window.location.hash = '#/alpha/status'
      mockApi({ ...twoAccounts(), [DELETE]: () => apiError(status, code, message) })
      render(<App />)
      await screen.findByRole('heading', { name: '受信状況' })
      await openDelete()
      const box = within(await screen.findByRole('alertdialog'))
      await userEvent.click(box.getByRole('button', { name: '削除する' }))
      expect(await box.findByRole('alert')).toHaveTextContent(message)
      expect(box.getByRole('button', { name: '削除を再試行' })).toBeEnabled()
      expect(window.location.hash).toBe('#/alpha/status')
      expect(screen.getByRole('combobox', { name: 'アカウント', hidden: true })).toHaveTextContent(
        'alpha',
      )
    },
  )

  it('blocks a second delete request while one is in flight', async () => {
    window.location.hash = '#/alpha/status'
    const gate = { release: (_: Response) => {} }
    const fetchMock = mockApi({
      ...twoAccounts(),
      [DELETE]: () =>
        new Promise<Response>((r) => {
          gate.release = r
        }),
    })
    render(<App />)
    await screen.findByRole('heading', { name: '受信状況' })
    await openDelete()
    const box = within(await screen.findByRole('alertdialog'))
    await userEvent.click(box.getByRole('button', { name: '削除する' }))
    expect(await box.findByRole('button', { name: '削除中…' })).toBeDisabled()
    expect(box.getByRole('button', { name: 'キャンセル' })).toBeDisabled()
    gate.release(json({}))
    await waitFor(() => expect(window.location.hash).toBe('#/default/status'))
    expect(requests(fetchMock, DELETE)).toBe(1)
  })
})
