import { BellRing } from 'lucide-react'
import { useState } from 'react'
import { AccountSwitcher } from '@/components/account-switcher'
import { AddAccountDialog } from '@/components/add-account-dialog'
import { Message } from '@/components/message'
import { LoadFailure, Loading, Screen as Page } from '@/components/screen'
import { Button } from '@/components/ui/button'
import { cn } from '@/lib/utils'
import { type Account, api, errorText } from './api'
import { useLoad } from './hooks'
import { resolveAccount, SCREENS, type Screen, screenHref, useRoute } from './route'
import { Notifications } from './screens/Notifications'
import { Settings } from './screens/Settings'
import { Status } from './screens/Status'

const LABELS: Record<Screen, string> = {
  notifications: '通知',
  settings: '設定',
  status: '受信状況',
}

const ACCOUNTS_TITLE = 'アカウント'

function AccountScreen({ accountId, screen }: { accountId: string; screen: Screen }) {
  return (
    <>
      {screen === 'settings' && <Settings accountId={accountId} />}
      {screen === 'notifications' && <Notifications accountId={accountId} />}
      {screen === 'status' && <Status accountId={accountId} />}
    </>
  )
}

export function App() {
  const route = useRoute()
  const accounts = useLoad(api.listAccounts)
  const [adding, setAdding] = useState(false)
  // Accounts that were deleted while their history files stayed on disk. Kept until dismissed.
  const [historyKept, setHistoryKept] = useState<readonly string[]>([])

  const list = accounts.data
  const resolved = list === null ? null : resolveAccount(list, route.account)
  const active = resolved?.kind === 'active' ? resolved.account : null
  // 画面の遷移先に使うアカウント。旧形式のハッシュでも、解決済みのアカウントへ向ける。
  const linkAccount = active?.id ?? null

  function created(account: Account) {
    accounts.setData([...(list ?? []), account])
    // The server only creates an account once no history files of its id are left.
    setHistoryKept((ids) => ids.filter((id) => id !== account.id))
    setAdding(false)
    window.location.hash = screenHref(account.id, 'settings')
  }

  function deleted(id: string, kept: boolean) {
    const rest = (list ?? []).filter((a) => a.id !== id)
    accounts.setData(rest)
    if (kept) setHistoryKept((ids) => [...ids.filter((x) => x !== id), id])
    const next = resolveAccount(rest, null)
    window.location.hash = screenHref(next.kind === 'active' ? next.account.id : null, route.screen)
  }

  return (
    <div className="min-h-svh">
      <header className="border-b">
        <div className="mx-auto flex w-full max-w-[56rem] flex-wrap items-center gap-x-4 gap-y-1 px-4 py-2 sm:h-14 sm:flex-nowrap sm:py-0 sm:px-6">
          <h1 className="flex items-center gap-2 text-sm font-semibold">
            <BellRing className="size-5" />
            <span>Angelic Angel</span>
          </h1>
          <nav
            aria-label="メイン"
            className="order-last flex w-full items-center gap-1 sm:order-none sm:w-auto sm:flex-1"
          >
            {SCREENS.map((s) => (
              <Button
                key={s}
                asChild
                variant="ghost"
                size="sm"
                className={cn(
                  'h-9 px-3 text-sm text-muted-foreground',
                  'aria-[current=page]:bg-muted aria-[current=page]:text-foreground',
                )}
              >
                <a
                  href={screenHref(linkAccount, s)}
                  aria-current={s === route.screen ? 'page' : undefined}
                >
                  {LABELS[s]}
                </a>
              </Button>
            ))}
          </nav>
          <AccountSwitcher
            accounts={list}
            active={active}
            screen={route.screen}
            onAdd={() => setAdding(true)}
            onDeleted={deleted}
          />
        </div>
      </header>
      <main className="mx-auto w-full max-w-[56rem] px-4 py-8 sm:px-6">
        {historyKept.length > 0 && (
          <div className="mb-6 flex items-start justify-between gap-4">
            <Message tone="error" role="alert">
              アカウント{historyKept.map((id) => `「${id}」`).join('')}
              は削除されましたが、通知履歴のファイルを削除できませんでした。同じ ID
              でアカウントを作り直すには、設定ファイルと同じ場所にある履歴ファイル（.notifications.sqlite3
              で終わるもの）を手動で削除してください。
            </Message>
            <Button type="button" variant="ghost" size="sm" onClick={() => setHistoryKept([])}>
              閉じる
            </Button>
          </div>
        )}
        {resolved === null ? (
          accounts.loading ? (
            <Page title={ACCOUNTS_TITLE} description="アカウントの一覧を読み込みます。">
              <Loading />
            </Page>
          ) : (
            <Page title={ACCOUNTS_TITLE} description="アカウントの一覧を取得できませんでした。">
              <LoadFailure message={errorText(accounts.error)} onRetry={accounts.reload} />
            </Page>
          )
        ) : resolved.kind === 'active' ? (
          <AccountScreen
            key={resolved.account.id}
            accountId={resolved.account.id}
            screen={route.screen}
          />
        ) : resolved.kind === 'missing' ? (
          <Page
            title="アカウントが見つかりません"
            description={`アカウント「${resolved.id}」はありません。削除されたか、ID が間違っている可能性があります。`}
          >
            <p className="text-sm text-muted-foreground">
              右上のメニューからアカウントを選び直してください。
            </p>
          </Page>
        ) : (
          <Page
            title="アカウントがありません"
            description="通知を受け取るアカウントを追加してください。"
          >
            <Button type="button" className="h-10 px-5" onClick={() => setAdding(true)}>
              アカウントを追加
            </Button>
          </Page>
        )}
      </main>
      <AddAccountDialog open={adding} onOpenChange={setAdding} onCreated={created} />
    </div>
  )
}
