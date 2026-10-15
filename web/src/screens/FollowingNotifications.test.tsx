import { act, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'
import type { FollowingNotificationsState } from '../api'
import { renderLocked } from '../test/harness'
import { apiError, following, json, mockApi } from '../test/helpers'
import { FollowingNotifications } from './FollowingNotifications'

const GET = 'GET /api/accounts/default/following-notifications'
const POST = 'POST /api/accounts/default/following-notifications'
const DELETE = 'DELETE /api/accounts/default/following-notifications'
const running = following({
  state: 'running',
  total: 10,
  success_count: 3,
  failed_count: 1,
  remaining_count: 6,
})

async function tick(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms)
  })
}

const startButton = () => screen.findByRole('button', { name: 'オンにする' })

describe('FollowingNotifications', () => {
  it('asks for inline confirmation and explains the scope before starting', async () => {
    const post = vi.fn(() => json(running, 202))
    mockApi({ [GET]: () => json(following()), [POST]: post })
    renderLocked((lock) => <FollowingNotifications accountId="default" configured lock={lock} />)
    await userEvent.click(await startButton())
    expect(post).not.toHaveBeenCalled()
    const confirm = screen.getByRole('group', { name: '全員の通知オンの確認' })
    expect(confirm).toHaveTextContent('フォロー中の全アカウントの通知設定が変更されます')
    expect(confirm).toHaveTextContent('成功した分は元に戻りません')
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument()
  })

  it('does nothing when the confirmation is cancelled', async () => {
    const post = vi.fn(() => json(running, 202))
    mockApi({ [GET]: () => json(following()), [POST]: post })
    renderLocked((lock) => <FollowingNotifications accountId="default" configured lock={lock} />)
    await userEvent.click(await startButton())
    await userEvent.click(screen.getByRole('button', { name: 'キャンセル' }))
    expect(screen.queryByRole('group', { name: '全員の通知オンの確認' })).not.toBeInTheDocument()
    expect(post).not.toHaveBeenCalled()
  })

  it('starts once after confirmation and shows the progress counts', async () => {
    let release: (r: Response) => void = () => {}
    const post = vi.fn(() => new Promise<Response>((r) => (release = r)))
    mockApi({ [GET]: () => json(following()), [POST]: post })
    renderLocked((lock) => <FollowingNotifications accountId="default" configured lock={lock} />)
    await userEvent.click(await startButton())
    await userEvent.click(screen.getByRole('button', { name: '実行する' }))
    expect(await screen.findByRole('button', { name: '開始中…' })).toBeDisabled()
    expect(post).toHaveBeenCalledTimes(1)
    release(json(running, 202))
    expect(await screen.findByText('実行中')).toBeInTheDocument()
    expect(screen.getByRole('row', { name: '対象 10' })).toBeInTheDocument()
    expect(screen.getByRole('row', { name: '成功 3' })).toBeInTheDocument()
    expect(screen.getByRole('row', { name: '失敗 1' })).toBeInTheDocument()
    expect(screen.getByRole('row', { name: '残り 6' })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'オンにする' })).toBeDisabled()
    expect(post).toHaveBeenCalledTimes(1)
  })

  it('shows unknown totals as a dash while the list is loading', async () => {
    mockApi({ [GET]: () => json(following({ state: 'loading' })) })
    renderLocked((lock) => <FollowingNotifications accountId="default" configured lock={lock} />)
    expect(await screen.findByText('対象を取得中')).toBeInTheDocument()
    expect(screen.getByRole('row', { name: '対象 —' })).toBeInTheDocument()
    expect(screen.getByRole('row', { name: '残り —' })).toBeInTheDocument()
  })

  it('requests cancellation while running and reports that it is stopping', async () => {
    const del = vi.fn(() => json({ ...running, state: 'cancelling' }, 202))
    mockApi({ [GET]: () => json(running), [DELETE]: del })
    renderLocked((lock) => <FollowingNotifications accountId="default" configured lock={lock} />)
    await userEvent.click(await screen.findByRole('button', { name: '中止' }))
    expect(del).toHaveBeenCalledTimes(1)
    expect(await screen.findByText('中止処理中')).toBeInTheDocument()
    expect(screen.getByText(/中止を要求しました/)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '中止中…' })).toBeDisabled()
    expect(screen.getByRole('button', { name: 'オンにする' })).toBeDisabled()
  })

  it.each<FollowingNotificationsState>(['idle', 'completed', 'failed', 'cancelled'])(
    'cannot cancel when %s',
    async (state) => {
      mockApi({ [GET]: () => json(following({ state })) })
      renderLocked((lock) => <FollowingNotifications accountId="default" configured lock={lock} />)
      await startButton()
      expect(screen.getByRole('button', { name: '中止' })).toBeDisabled()
      expect(screen.getByRole('button', { name: 'オンにする' })).toBeEnabled()
    },
  )

  it('reports a cancelled run as partial: succeeded accounts stay on', async () => {
    mockApi({
      [GET]: () =>
        json(following({ state: 'cancelled', total: 10, success_count: 4, remaining_count: 6 })),
    })
    renderLocked((lock) => <FollowingNotifications accountId="default" configured lock={lock} />)
    expect(await screen.findByText('中止済み')).toBeInTheDocument()
    expect(screen.getByText(/成功済みの 4 件はオンのままです/)).toBeInTheDocument()
  })

  it('reports a failed run honestly, with the error and no automatic retry', async () => {
    vi.useFakeTimers()
    const post = vi.fn(() => json(running, 202))
    mockApi({
      [GET]: () =>
        json(
          following({
            state: 'failed',
            total: 10,
            success_count: 4,
            failed_count: 1,
            remaining_count: 5,
            last_error: 'Rate limit exceeded (429)',
          }),
        ),
      [POST]: post,
    })
    renderLocked((lock) => <FollowingNotifications accountId="default" configured lock={lock} />)
    await tick(0)
    expect(screen.getAllByText('失敗')).toHaveLength(2)
    expect(screen.getByRole('alert')).toHaveTextContent('Rate limit exceeded (429)')
    expect(screen.getByRole('alert')).toHaveTextContent('成功済みの 4 件はオンのままで')
    await tick(30_000)
    expect(post).not.toHaveBeenCalled()
    expect(screen.queryByRole('group', { name: '全員の通知オンの確認' })).not.toBeInTheDocument()
  })

  it('requires a fresh confirmation to run again after a failure', async () => {
    const post = vi.fn(() => json(running, 202))
    mockApi({
      [GET]: () => json(following({ state: 'failed', success_count: 1, last_error: 'x' })),
      [POST]: post,
    })
    renderLocked((lock) => <FollowingNotifications accountId="default" configured lock={lock} />)
    await userEvent.click(await startButton())
    expect(post).not.toHaveBeenCalled()
    await userEvent.click(screen.getByRole('button', { name: '実行する' }))
    expect(post).toHaveBeenCalledTimes(1)
  })

  it('does not call a completed run with failures a clean success', async () => {
    mockApi({
      [GET]: () =>
        json(following({ state: 'completed', total: 5, success_count: 3, failed_count: 2 })),
    })
    renderLocked((lock) => <FollowingNotifications accountId="default" configured lock={lock} />)
    expect(await screen.findByText(/2 件で失敗しました/)).toBeInTheDocument()
    expect(screen.queryByText(/件をオンにしました/)).not.toBeInTheDocument()
  })

  it('reports a clean completion', async () => {
    mockApi({
      [GET]: () => json(following({ state: 'completed', total: 5, success_count: 5 })),
    })
    renderLocked((lock) => <FollowingNotifications accountId="default" configured lock={lock} />)
    expect(await screen.findByText('完了しました。5 件をオンにしました。')).toBeInTheDocument()
  })

  it('shows a start conflict from the server and refreshes the state', async () => {
    let gets = 0
    mockApi({
      [GET]: () => (++gets === 1 ? json(following()) : json(running)),
      [POST]: () => apiError(409, 'following_job_active', '一括設定は実行中です。'),
    })
    renderLocked((lock) => <FollowingNotifications accountId="default" configured lock={lock} />)
    await userEvent.click(await startButton())
    await userEvent.click(screen.getByRole('button', { name: '実行する' }))
    expect(await screen.findByText('一括設定は実行中です。')).toBeInTheDocument()
    expect(await screen.findByText('実行中')).toBeInTheDocument()
  })

  it('disables start without credentials and explains why', async () => {
    mockApi({ [GET]: () => json(following()) })
    renderLocked((lock) => (
      <FollowingNotifications accountId="default" configured={false} lock={lock} />
    ))
    expect(await screen.findByText(/先に上の Cookie を保存/)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: 'オンにする' })).toBeDisabled()
  })

  it('shows a load error with retry', async () => {
    let gets = 0
    mockApi({ [GET]: () => (++gets === 1 ? apiError(500, 'x', '取得失敗') : json(following())) })
    renderLocked((lock) => <FollowingNotifications accountId="default" configured lock={lock} />)
    expect(await screen.findByRole('alert')).toHaveTextContent('取得失敗')
    await userEvent.click(screen.getByRole('button', { name: '再試行' }))
    expect(await startButton()).toBeEnabled()
  })

  it('polls every 2s without overlapping and follows the progress', async () => {
    vi.useFakeTimers()
    let release: (r: Response) => void = () => {}
    let inflight = 0
    let maxInflight = 0
    const fetchMock = mockApi({
      [GET]: () => {
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
    renderLocked((lock) => <FollowingNotifications accountId="default" configured lock={lock} />)
    await tick(10_000)
    expect(fetchMock).toHaveBeenCalledTimes(1)
    release(json(running))
    await tick(0)
    expect(screen.getByRole('row', { name: '成功 3' })).toBeInTheDocument()
    await tick(2000)
    expect(fetchMock).toHaveBeenCalledTimes(2)
    release(json({ ...running, success_count: 8, remaining_count: 1 }))
    await tick(0)
    expect(screen.getByRole('row', { name: '成功 8' })).toBeInTheDocument()
    expect(maxInflight).toBe(1)
  })
})
