import { LoaderCircle } from 'lucide-react'
import { type FormEvent, useId, useState } from 'react'
import { Message } from '@/components/message'
import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { ACCOUNT_ID_PATTERN, type Account, ApiError, api, errorText } from '../api'

const RULE_TEXT =
  'ID は半角の英小文字・数字・ハイフン・アンダースコアで、1〜32 文字、先頭は英小文字か数字にしてください。'

function createErrorText(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.code === 'account_exists') return 'この ID のアカウントは既に存在します。'
    if (e.code === 'invalid_account_id') return RULE_TEXT
  }
  return errorText(e)
}

/** The form lives inside the dialog content, so closing the dialog discards what was typed. */
function AddAccountForm({ onCreated }: { onCreated: (account: Account) => void }) {
  const id = useId()
  const [value, setValue] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [pending, setPending] = useState(false)

  async function submit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault()
    if (pending) return
    const accountId = value.trim()
    if (!ACCOUNT_ID_PATTERN.test(accountId)) {
      setError(RULE_TEXT)
      return
    }
    setError(null)
    setPending(true)
    try {
      onCreated(await api.createAccount(accountId))
    } catch (err) {
      setError(createErrorText(err))
      setPending(false)
    }
  }

  return (
    <form onSubmit={submit} noValidate className="grid gap-4">
      <div className="space-y-2">
        <Label htmlFor={`${id}-id`}>アカウント ID</Label>
        <Input
          id={`${id}-id`}
          autoComplete="off"
          autoCapitalize="none"
          spellCheck={false}
          maxLength={64}
          className="h-10 font-mono"
          value={value}
          disabled={pending}
          onChange={(e) => setValue(e.currentTarget.value)}
          aria-invalid={error ? true : undefined}
          aria-describedby={`${id}-rule${error ? ` ${id}-error` : ''}`}
        />
        <p id={`${id}-rule`} className="text-sm text-muted-foreground">
          使用できる形式: <code className="font-mono">{String(ACCOUNT_ID_PATTERN)}</code>
        </p>
        {error && (
          <Message tone="error" id={`${id}-error`} role="alert">
            {error}
          </Message>
        )}
      </div>
      <DialogFooter>
        <DialogClose asChild>
          <Button type="button" variant="outline" className="h-10 px-5" disabled={pending}>
            キャンセル
          </Button>
        </DialogClose>
        <Button type="submit" className="h-10 px-5" disabled={pending}>
          {pending && <LoaderCircle data-icon="inline-start" className="animate-spin" />}
          {pending ? '追加中…' : '追加'}
        </Button>
      </DialogFooter>
    </form>
  )
}

export function AddAccountDialog({
  open,
  onOpenChange,
  onCreated,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  onCreated: (account: Account) => void
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>アカウントを追加</DialogTitle>
          <DialogDescription>
            追加するアカウントの ID を入力してください。Cookie や転送先は追加後に設定します。
          </DialogDescription>
        </DialogHeader>
        <AddAccountForm onCreated={onCreated} />
      </DialogContent>
    </Dialog>
  )
}
