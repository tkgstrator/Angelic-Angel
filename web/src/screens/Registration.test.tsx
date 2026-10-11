import { screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'
import { renderLocked } from '../test/harness'
import { apiError, json, mockApi, registration } from '../test/helpers'
import { Registration } from './Registration'

const registered = registration({ registered: true, uaid: 'uaid-1', channel_id: 'chan-1' })

function renderSection(configured = true, onChanged = async () => {}) {
  return renderLocked((lock) => (
    <Registration configured={configured} lock={lock} onChanged={onChanged} />
  ))
}

describe('Registration', () => {
  it('shows the unregistered state with unregister disabled', async () => {
    mockApi({ 'GET /api/registration': () => json(registration()) })
    renderSection()
    expect(await screen.findByText('未登録')).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '登録解除' })).toBeDisabled()
    expect(screen.getByRole('button', { name: '登録' })).toBeEnabled()
  })

  it('disables register when cookies are not saved and explains why', async () => {
    mockApi({ 'GET /api/registration': () => json(registration()) })
    renderSection(false)
    expect(await screen.findByText(/先に上の Cookie を保存/)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '登録' })).toBeDisabled()
  })

  it('registers, shows the IDs and tells the parent', async () => {
    let release: (r: Response) => void = () => {}
    const onChanged = vi.fn(async () => {})
    mockApi({
      'GET /api/registration': () => json(registration()),
      'POST /api/register': () => new Promise<Response>((r) => (release = r)),
    })
    renderSection(true, onChanged)
    await screen.findByText('未登録')
    await userEvent.click(screen.getByRole('button', { name: '登録' }))
    expect(await screen.findByRole('button', { name: '登録中…' })).toBeDisabled()
    expect(screen.getByRole('button', { name: '登録解除' })).toBeDisabled()
    release(json(registered))
    expect(await screen.findByText('登録済み')).toBeInTheDocument()
    expect(screen.getByText('uaid-1')).toBeInTheDocument()
    expect(screen.getByText('chan-1')).toBeInTheDocument()
    expect(onChanged).toHaveBeenCalledTimes(1)
  })

  describe('unregister confirmation dialog', () => {
    function mockUnregister(onDelete: () => Response | Promise<Response>) {
      const calls = { deleted: 0 }
      mockApi({
        'GET /api/registration': () => json(registered),
        'DELETE /api/registration': () => {
          calls.deleted++
          return onDelete()
        },
      })
      return calls
    }

    it('asks first, focusing cancel, and sends nothing until confirmed', async () => {
      const calls = mockUnregister(() => json(registration()))
      renderSection()
      await userEvent.click(await screen.findByRole('button', { name: '登録解除' }))
      const dialog = await screen.findByRole('alertdialog', { name: '登録解除の確認' })
      expect(dialog).toHaveAccessibleDescription(
        '登録を解除すると、通知の受信が止まります。解除しますか？',
      )
      expect(within(dialog).getByRole('button', { name: 'キャンセル' })).toHaveFocus()
      expect(calls.deleted).toBe(0)
      await userEvent.click(within(dialog).getByRole('button', { name: '解除する' }))
      expect(await screen.findByText('未登録')).toBeInTheDocument()
      expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument()
      expect(calls.deleted).toBe(1)
    })

    it('cancels without a request and returns focus to the trigger', async () => {
      const calls = mockUnregister(() => json(registration()))
      renderSection()
      const trigger = await screen.findByRole('button', { name: '登録解除' })
      await userEvent.click(trigger)
      await userEvent.click(await screen.findByRole('button', { name: 'キャンセル' }))
      await waitFor(() => expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument())
      await waitFor(() => expect(trigger).toHaveFocus())
      expect(calls.deleted).toBe(0)
      expect(screen.getByText('登録済み')).toBeInTheDocument()
    })

    it('cancels on Escape without a request and returns focus to the trigger', async () => {
      const calls = mockUnregister(() => json(registration()))
      renderSection()
      const trigger = await screen.findByRole('button', { name: '登録解除' })
      await userEvent.click(trigger)
      await screen.findByRole('alertdialog')
      await userEvent.keyboard('{Escape}')
      await waitFor(() => expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument())
      await waitFor(() => expect(trigger).toHaveFocus())
      expect(calls.deleted).toBe(0)
    })

    it('locks the screen while the request runs and keeps the dialog closed', async () => {
      let release: (r: Response) => void = () => {}
      const calls = mockUnregister(() => new Promise<Response>((r) => (release = r)))
      renderSection()
      await userEvent.click(await screen.findByRole('button', { name: '登録解除' }))
      await userEvent.click(await screen.findByRole('button', { name: '解除する' }))
      expect(await screen.findByRole('button', { name: '解除中…' })).toBeDisabled()
      expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument()
      expect(screen.getByRole('button', { name: '登録' })).toBeDisabled()
      release(json(registration()))
      expect(await screen.findByText('未登録')).toBeInTheDocument()
      expect(calls.deleted).toBe(1)
    })

    it('confirms again before retrying a failed unregister', async () => {
      const calls = mockUnregister(() =>
        calls.deleted === 1
          ? apiError(502, 'provider', '外部サービスに失敗しました。')
          : json(registration()),
      )
      renderSection()
      await userEvent.click(await screen.findByRole('button', { name: '登録解除' }))
      await userEvent.click(await screen.findByRole('button', { name: '解除する' }))
      expect(await screen.findByRole('alert')).toHaveTextContent('外部サービスに失敗しました。')
      expect(calls.deleted).toBe(1)

      const retry = screen.getByRole('button', { name: '再試行' })
      await userEvent.click(retry)
      const dialog = await screen.findByRole('alertdialog', { name: '登録解除の確認' })
      expect(within(dialog).getByRole('button', { name: 'キャンセル' })).toHaveFocus()
      expect(calls.deleted).toBe(1)

      await userEvent.click(within(dialog).getByRole('button', { name: 'キャンセル' }))
      await waitFor(() => expect(screen.queryByRole('alertdialog')).not.toBeInTheDocument())
      await waitFor(() => expect(retry).toHaveFocus())
      expect(calls.deleted).toBe(1)

      await userEvent.click(retry)
      await userEvent.click(await screen.findByRole('button', { name: '解除する' }))
      expect(await screen.findByText('未登録')).toBeInTheDocument()
      expect(calls.deleted).toBe(2)
      expect(screen.queryByRole('alert')).not.toBeInTheDocument()
    })
  })

  it('shows remote failures and retries the failed action', async () => {
    let calls = 0
    const onChanged = vi.fn(async () => {})
    mockApi({
      'GET /api/registration': () => json(registration()),
      'POST /api/register': () =>
        ++calls === 1
          ? apiError(502, 'provider', '外部サービスに失敗しました。')
          : json(registered),
    })
    renderSection(true, onChanged)
    await screen.findByText('未登録')
    await userEvent.click(screen.getByRole('button', { name: '登録' }))
    expect(await screen.findByRole('alert')).toHaveTextContent('外部サービスに失敗しました。')
    expect(onChanged).not.toHaveBeenCalled()
    await userEvent.click(screen.getByRole('button', { name: '再試行' }))
    expect(await screen.findByText('登録済み')).toBeInTheDocument()
    expect(calls).toBe(2)
    expect(onChanged).toHaveBeenCalledTimes(1)
  })

  it('offers retry when loading fails', async () => {
    mockApi({ 'GET /api/registration': () => apiError(500, 'x', '読み込み失敗') })
    renderSection()
    expect(await screen.findByRole('alert')).toHaveTextContent('読み込み失敗')
    expect(screen.getByRole('button', { name: '再試行' })).toBeInTheDocument()
  })
})
