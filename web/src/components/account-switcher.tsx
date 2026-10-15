import { Plus } from 'lucide-react'
import { DeleteAccountButton } from '@/components/delete-account-dialog'
import { Button } from '@/components/ui/button'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import type { Account } from '../api'
import { type Screen, screenHref } from '../route'

/** Select, add and delete stay in place whatever the state; they only become disabled. */
export function AccountSwitcher({
  accounts,
  active,
  screen,
  onAdd,
  onDeleted,
}: {
  accounts: readonly Account[] | null
  active: Account | null
  screen: Screen
  onAdd: () => void
  onDeleted: (id: string, historyKept: boolean) => void
}) {
  return (
    <div role="group" aria-label="アカウントの切り替え" className="flex items-center gap-1">
      <Select
        value={active?.id ?? ''}
        disabled={accounts === null || accounts.length === 0}
        onValueChange={(id) => {
          window.location.hash = screenHref(id, screen)
        }}
      >
        <SelectTrigger aria-label="アカウント" className="h-9 min-w-40 font-mono">
          <SelectValue placeholder={accounts === null ? '読み込み中…' : 'アカウントなし'} />
        </SelectTrigger>
        <SelectContent>
          {(accounts ?? []).map((a) => (
            <SelectItem key={a.id} value={a.id} className="font-mono">
              {a.id}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      <Button
        type="button"
        variant="ghost"
        size="icon"
        aria-label="アカウントを追加"
        disabled={accounts === null}
        onClick={onAdd}
      >
        <Plus />
      </Button>
      <DeleteAccountButton account={active} onDeleted={onDeleted} />
    </div>
  )
}
