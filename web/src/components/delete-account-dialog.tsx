import { LoaderCircle, Trash2 } from 'lucide-react'
import { type MouseEvent, useState } from 'react'
import { Message } from '@/components/message'
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from '@/components/ui/alert-dialog'
import { Button } from '@/components/ui/button'
import { type Account, ApiError, api, errorText } from '../api'

/**
 * The server answers 500 `history_cleanup_failed` after it has already removed the account: only
 * the notification history files are left on disk. A 404 means the account was already gone.
 * Both leave nothing to retry, so the list must follow the server.
 */
function removedDespite(e: unknown): { historyKept: boolean } | null {
  if (!(e instanceof ApiError)) return null
  if (e.code === 'history_cleanup_failed') return { historyKept: true }
  if (e.status === 404 && e.code === 'account_not_found') return { historyKept: false }
  return null
}

/** The trigger is always rendered; without a selected account it is only disabled. */
export function DeleteAccountButton({
  account,
  onDeleted,
}: {
  account: Account | null
  /** `historyKept`: the account is gone, but its history files could not be deleted. */
  onDeleted: (id: string, historyKept: boolean) => void
}) {
  const [open, setOpen] = useState(false)
  const [pending, setPending] = useState(false)
  const [error, setError] = useState<string | null>(null)

  function changeOpen(next: boolean) {
    if (pending) return
    if (next) setError(null)
    setOpen(next)
  }

  async function remove(e: MouseEvent<HTMLButtonElement>) {
    // Keep the dialog open while the request runs, so a failure can be shown and retried here.
    e.preventDefault()
    if (account === null || pending) return
    setError(null)
    setPending(true)
    try {
      await api.deleteAccount(account.id)
      setOpen(false)
      onDeleted(account.id, false)
    } catch (err) {
      const removed = removedDespite(err)
      if (removed === null) {
        setError(errorText(err))
      } else {
        setOpen(false)
        onDeleted(account.id, removed.historyKept)
      }
    } finally {
      setPending(false)
    }
  }

  return (
    <AlertDialog open={open} onOpenChange={changeOpen}>
      <AlertDialogTrigger asChild>
        <Button
          type="button"
          variant="ghost"
          size="icon"
          aria-label="アカウントを削除"
          disabled={account === null}
        >
          <Trash2 />
        </Button>
      </AlertDialogTrigger>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>アカウント「{account?.id}」の削除の確認</AlertDialogTitle>
          <AlertDialogDescription>
            登録済みの場合は X
            上の受信先の登録を解除し、このアカウントの通知履歴をすべて削除します。この操作は元に戻せません。
            登録の解除に失敗したときは、アカウントは削除されません。
          </AlertDialogDescription>
        </AlertDialogHeader>
        {error && (
          <Message tone="error" role="alert">
            {error}
          </Message>
        )}
        <AlertDialogFooter>
          <AlertDialogCancel className="h-10 px-5" disabled={pending}>
            キャンセル
          </AlertDialogCancel>
          <Button
            type="button"
            variant="destructive"
            className="h-10 px-5"
            disabled={pending}
            onClick={remove}
          >
            {pending && <LoaderCircle data-icon="inline-start" className="animate-spin" />}
            {pending ? '削除中…' : error ? '削除を再試行' : '削除する'}
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  )
}
