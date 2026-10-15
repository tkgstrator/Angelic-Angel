import { act, render, screen } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'
import type { ReceiverState } from '../api'
import { apiError, json, mockApi, status } from '../test/helpers'
import { Status } from './Status'

async function flush() {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(0)
  })
}

async function tick(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms)
  })
}

describe('Status', () => {
  it.each<[ReceiverState, string]>([
    ['unconfigured', '未設定'],
    ['unregistered', '未登録'],
    ['stopped', '停止中'],
    ['connecting', '接続中'],
    ['connected', '接続済み'],
    ['retrying', '再接続待ち'],
    ['registering', '登録処理中'],
    ['unregistering', '解除処理中'],
    ['error', 'エラー'],
  ])('renders state %s', async (state, label) => {
    mockApi({ 'GET /api/accounts/default/status': () => json(status({ state })) })
    render(<Status accountId="default" />)
    expect(await screen.findAllByText(label)).not.toHaveLength(0)
  })

  it('shows zero counters, empty timestamps and no error', async () => {
    mockApi({
      'GET /api/accounts/default/status': () =>
        json(status({ state: 'unregistered', connected_since: null, registered: false })),
    })
    render(<Status accountId="default" />)
    expect(await screen.findByText(/登録画面で登録すると/)).toBeInTheDocument()
    expect(screen.getByRole('row', { name: '受信 0' })).toBeInTheDocument()
    expect(screen.getAllByText('—')).toHaveLength(2)
    expect(screen.getByText('なし')).toBeInTheDocument()
  })

  it('shows counters, local time, retry count and last error', async () => {
    mockApi({
      'GET /api/accounts/default/status': () =>
        json(
          status({
            state: 'retrying',
            received_count: 1234,
            forwarded_count: 1200,
            failed_count: 34,
            retry_count: 5,
            last_notification_at: 1_700_000_100,
            last_error: '接続が切れました',
          }),
        ),
    })
    render(<Status accountId="default" />)
    expect(await screen.findByRole('row', { name: '受信 1,234' })).toBeInTheDocument()
    expect(screen.getByRole('row', { name: '転送失敗 34' })).toBeInTheDocument()
    expect(screen.getByRole('row', { name: '再試行回数 5' })).toBeInTheDocument()
    expect(screen.getByText('接続が切れました')).toBeInTheDocument()
    expect(
      screen.getByText(new Date(1_700_000_100_000).toLocaleString('ja-JP')),
    ).toBeInTheDocument()
  })

  it('shows an error when the first load fails', async () => {
    mockApi({ 'GET /api/accounts/default/status': () => apiError(500, 'x', '取得に失敗') })
    render(<Status accountId="default" />)
    expect(await screen.findByRole('alert')).toHaveTextContent('取得に失敗')
  })

  it('polls every 2s and never overlaps requests', async () => {
    vi.useFakeTimers()
    let release: (r: Response) => void = () => {}
    let inflight = 0
    let maxInflight = 0
    const fetchMock = mockApi({
      'GET /api/accounts/default/status': () => {
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
    render(<Status accountId="default" />)
    await flush()
    expect(fetchMock).toHaveBeenCalledTimes(1)

    // A slow response: no new request starts while it is pending.
    await tick(10_000)
    expect(fetchMock).toHaveBeenCalledTimes(1)

    release(json(status()))
    await flush()
    await tick(1999)
    expect(fetchMock).toHaveBeenCalledTimes(1)
    await tick(1)
    expect(fetchMock).toHaveBeenCalledTimes(2)
    expect(maxInflight).toBe(1)
    release(json(status()))
    await flush()
  })

  it('pauses while hidden and refreshes when visible again', async () => {
    vi.useFakeTimers()
    const fetchMock = mockApi({ 'GET /api/accounts/default/status': () => json(status()) })
    render(<Status accountId="default" />)
    await flush()
    expect(fetchMock).toHaveBeenCalledTimes(1)

    const hidden = vi.spyOn(document, 'hidden', 'get').mockReturnValue(true)
    document.dispatchEvent(new Event('visibilitychange'))
    await tick(10_000)
    expect(fetchMock).toHaveBeenCalledTimes(1)

    hidden.mockReturnValue(false)
    document.dispatchEvent(new Event('visibilitychange'))
    await flush()
    expect(fetchMock).toHaveBeenCalledTimes(2)
    hidden.mockRestore()
  })

  it('keeps the last data and warns when a later poll fails', async () => {
    vi.useFakeTimers()
    let calls = 0
    mockApi({
      'GET /api/accounts/default/status': () =>
        ++calls === 1 ? json(status({ received_count: 7 })) : apiError(502, 'x', '一時的な障害'),
    })
    render(<Status accountId="default" />)
    await flush()
    await tick(2000)
    expect(screen.getByRole('alert')).toHaveTextContent('一時的な障害')
    expect(screen.getByRole('row', { name: '受信 7' })).toBeInTheDocument()
  })
})
