import { LoaderCircle } from 'lucide-react'
import { type FormEvent, useId, useState } from 'react'
import { Message } from '@/components/message'
import { LoadFailure, Loading, Screen, Section, SectionActions } from '@/components/screen'
import { type Tone, ToneBadge } from '@/components/tone-badge'
import { Button } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Separator } from '@/components/ui/separator'
import { Switch } from '@/components/ui/switch'
import {
  api,
  cleanMessage,
  errorText,
  type NotificationFilter,
  type Settings as SettingsDto,
} from '../api'
import { type Exclusive, useExclusive, useLoad } from '../hooks'
import { FollowingNotifications } from './FollowingNotifications'
import { Registration } from './Registration'

const MAX_SECRET = 4096
const SOURCE_LABEL = {
  environment: '環境変数 WEBHOOK_ENDPOINT',
  config: '設定ファイル',
  none: '未設定',
} as const

type MachineKey = 'machine_post_enabled' | 'machine_like_enabled' | 'machine_follow_enabled'
type MachinePermissions = Record<MachineKey, boolean>

/** アカウント別に許可する Machine API の操作。3つは互いに独立して保存する。 */
const MACHINE_SWITCHES: readonly { key: MachineKey; id: string; label: string }[] = [
  { key: 'machine_post_enabled', id: 'machine-post', label: 'APIからの投稿・返信を許可' },
  { key: 'machine_like_enabled', id: 'machine-like', label: 'APIからのいいねを許可' },
  { key: 'machine_follow_enabled', id: 'machine-follow', label: 'APIからのフォローを許可' },
]

function machinePermissions(settings: SettingsDto): MachinePermissions {
  return {
    machine_post_enabled: settings.machine_post_enabled,
    machine_like_enabled: settings.machine_like_enabled,
    machine_follow_enabled: settings.machine_follow_enabled,
  }
}

type FilterKey = keyof NotificationFilter
type FilterLeaf = { key: FilterKey; label: string }
/** 子を持つ項目は、キーを持たず子の状態をまとめて表す。 */
type FilterNode = FilterLeaf | { id: string; label: string; children: readonly FilterLeaf[] }
type CheckedState = boolean | 'indeterminate'

const FILTER_OPTIONS: readonly FilterNode[] = [
  { key: 'tweet', label: '投稿（tweet）' },
  { key: 'retweet', label: 'リツイート（RT）' },
  {
    id: 'quote-group',
    label: '引用RT',
    children: [
      { key: 'quote_self', label: '自分自身の引用（quote_self）' },
      { key: 'quote', label: '他アカウントの引用（quote）' },
    ],
  },
  { key: 'reply', label: 'リプライ' },
  { key: 'xchat_message_received', label: 'チャット（xchat_message_received）' },
  { key: 'other', label: 'その他・未指定' },
]

/** すべて有効ならチェック、一部だけなら indeterminate、すべて無効なら未チェック。 */
function groupState(filter: NotificationFilter, children: readonly FilterLeaf[]): CheckedState {
  const enabled = children.filter(({ key }) => filter[key]).length
  if (enabled === children.length) return true
  return enabled === 0 ? false : 'indeterminate'
}

function FilterCheckbox({
  id,
  label,
  checked,
  onCheckedChange,
}: {
  id: string
  label: string
  checked: CheckedState
  onCheckedChange: (checked: CheckedState) => void
}) {
  return (
    <div className="flex items-center gap-3">
      <Checkbox id={id} checked={checked} onCheckedChange={onCheckedChange} />
      <Label id={`${id}-label`} htmlFor={id}>
        {label}
      </Label>
    </div>
  )
}

export interface SettingsInput {
  authToken: string
  ct0: string
  webhook: string
  webhookSecret: string
}

export type SettingsErrors = Partial<Record<keyof SettingsInput, string>>

function hasControl(value: string): boolean {
  for (const ch of value) {
    const c = ch.codePointAt(0) ?? 0
    if (c < 0x20 || c === 0x7f) return true
  }
  return false
}

export function validate(input: SettingsInput, settings: SettingsDto): SettingsErrors {
  const errors: SettingsErrors = {}
  const secrets = [
    ['authToken', 'auth_token', settings.auth_token_set],
    ['ct0', 'ct0', settings.ct0_set],
  ] as const
  for (const [key, name, isSet] of secrets) {
    const v = input[key].trim()
    if (v === '') {
      if (!settings.configured && !isSet) errors[key] = `初回設定では ${name} が必須です。`
    } else if (v.length > MAX_SECRET) {
      errors[key] = `${name} が長すぎます。`
    } else if (hasControl(v)) {
      errors[key] = `${name} に使用できない文字が含まれています。`
    }
  }
  const secret = input.webhookSecret.trim()
  if (secret.length > MAX_SECRET) {
    errors.webhookSecret = 'Webhook secret が長すぎます。'
  } else if (hasControl(secret)) {
    errors.webhookSecret = 'Webhook secret に使用できない文字が含まれています。'
  }
  const url = input.webhook.trim()
  if (url !== '' && settings.webhook_source !== 'environment') {
    try {
      const u = new URL(url)
      if (u.protocol !== 'http:' && u.protocol !== 'https:') throw new Error('scheme')
    } catch {
      errors.webhook = 'http:// または https:// で始まる URL を入力してください。'
    }
  }
  return errors
}

function Indicator({ set }: { set: boolean }) {
  return <ToneBadge tone={set ? 'ok' : 'neutral'}>{set ? '設定済み' : '未設定'}</ToneBadge>
}

const TITLE = '設定'
const DESCRIPTION = 'Cookie、通知の転送先、Machine APIの許可、受信先の登録を設定します。'
const SCREEN_NAME = /^[A-Za-z0-9_]{1,15}$/

type Check =
  | { kind: 'none' }
  | { kind: 'checking' }
  | { kind: 'valid'; screenName: string | null }
  | { kind: 'invalid'; detail: string | null }
  | { kind: 'unavailable'; message: string }

const CHECK_BADGE: Record<Check['kind'], { tone: Tone; label: string }> = {
  none: { tone: 'neutral', label: '未確認' },
  checking: { tone: 'neutral', label: '確認中' },
  valid: { tone: 'ok', label: '有効' },
  invalid: { tone: 'danger', label: '無効' },
  unavailable: { tone: 'warn', label: '確認不可' },
}

function CheckResult({ check }: { check: Check }) {
  const { tone, label } = CHECK_BADGE[check.kind]
  return (
    <div
      role={check.kind === 'invalid' ? 'alert' : 'status'}
      className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1"
    >
      <ToneBadge tone={tone}>{label}</ToneBadge>
      {check.kind === 'valid' && (
        <span className="text-sm">
          Cookie は有効です{check.screenName !== null && `（@${check.screenName}）`}。
        </span>
      )}
      {check.kind === 'invalid' && (
        <span className="text-sm text-destructive">
          Cookie が無効、または期限切れです。auth_token と ct0 を取得し直して保存してください。
          {check.detail !== null && `（${check.detail}）`}
        </span>
      )}
      {check.kind === 'unavailable' && (
        <span className="text-sm text-muted-foreground">
          確認できませんでした: {check.message}（Cookie が無効とは限りません）
        </span>
      )}
    </div>
  )
}

export function Settings({ accountId }: { accountId: string }) {
  const { data, error, loading, reload, setData } = useLoad(() => api.getSettings(accountId))
  const lock = useExclusive()
  const [syncError, setSyncError] = useState<string | null>(null)

  async function refresh() {
    try {
      setData(await api.getSettings(accountId))
      setSyncError(null)
    } catch (e) {
      setSyncError(errorText(e))
    }
  }

  if (loading && !data) {
    return (
      <Screen title={TITLE} description={DESCRIPTION}>
        <Loading />
      </Screen>
    )
  }
  if (!data) {
    return (
      <Screen title={TITLE} description={DESCRIPTION}>
        <LoadFailure message={errorText(error)} onRetry={reload} />
      </Screen>
    )
  }
  return (
    <Screen title={TITLE} description={DESCRIPTION}>
      {syncError !== null && (
        <div role="alert" className="space-y-3">
          <Message tone="error">
            設定の状態を更新できません。表示が古い可能性があります: {syncError}
          </Message>
          <Button
            type="button"
            variant="outline"
            className="h-10 px-4"
            disabled={lock.active !== null}
            onClick={refresh}
          >
            再取得
          </Button>
        </div>
      )}
      <SettingsForm accountId={accountId} settings={data} lock={lock} onSaved={setData} />
      <Separator />
      <Registration
        accountId={accountId}
        configured={data.configured}
        lock={lock}
        onChanged={refresh}
      />
      <Separator />
      <FollowingNotifications accountId={accountId} configured={data.configured} lock={lock} />
    </Screen>
  )
}

function SettingsForm({
  accountId,
  settings,
  lock,
  onSaved,
}: {
  accountId: string
  settings: SettingsDto
  lock: Exclusive
  onSaved: (s: SettingsDto) => void
}) {
  const id = useId()
  const [authToken, setAuthToken] = useState('')
  const [ct0, setCt0] = useState('')
  const [webhook, setWebhook] = useState(settings.webhook_endpoint ?? '')
  const [webhookSecret, setWebhookSecret] = useState('')
  const [filter, setFilter] = useState(settings.notification_filter)
  const [machine, setMachine] = useState(() => machinePermissions(settings))
  const [errors, setErrors] = useState<SettingsErrors>({})
  const [result, setResult] = useState<{ ok: boolean; text: string } | null>(null)
  const [check, setCheck] = useState<Check>({ kind: 'none' })

  const envLocked = settings.webhook_source === 'environment'
  const locked = settings.credentials_locked
  const busy = lock.active !== null
  const checking = check.kind === 'checking'
  const saving = lock.active === 'save' && !checking
  const dirty = authToken.trim() !== '' || ct0.trim() !== ''

  function setFilterKeys(keys: readonly FilterKey[], enabled: boolean) {
    setFilter((prev) => keys.reduce((next, key) => ({ ...next, [key]: enabled }), prev))
  }

  async function runCheck() {
    setCheck({ kind: 'checking' })
    try {
      const r = await api.validateSettings(accountId)
      setCheck(
        r.valid === true
          ? {
              kind: 'valid',
              screenName:
                typeof r.screen_name === 'string' && SCREEN_NAME.test(r.screen_name)
                  ? r.screen_name
                  : null,
            }
          : { kind: 'invalid', detail: cleanMessage(r.message) },
      )
    } catch (err) {
      setCheck({ kind: 'unavailable', message: errorText(err) })
    }
  }

  function submit(e: FormEvent<HTMLFormElement>) {
    e.preventDefault()
    setResult(null)
    const found = validate({ authToken, ct0, webhook, webhookSecret }, settings)
    setErrors(found)
    if (Object.keys(found).length > 0) return
    return lock.run('save', async () => {
      let saved: SettingsDto
      try {
        saved = await api.putSettings(accountId, {
          auth_token: authToken.trim() || null,
          ct0: ct0.trim() || null,
          webhook_secret: webhookSecret.trim() || null,
          webhook_endpoint: envLocked ? (settings.webhook_endpoint ?? '') : webhook.trim(),
          notification_filter: filter,
          ...machine,
        })
      } catch (err) {
        setResult({ ok: false, text: errorText(err) })
        return
      }
      setAuthToken('')
      setCt0('')
      setWebhookSecret('')
      setWebhook(saved.webhook_endpoint ?? '')
      setFilter(saved.notification_filter)
      setMachine(machinePermissions(saved))
      onSaved(saved)
      setResult({ ok: true, text: '保存しました。' })
      if (saved.configured) await runCheck()
    })
  }

  const fieldProps = (key: keyof SettingsInput) => ({
    'aria-invalid': errors[key] ? (true as const) : undefined,
    'aria-describedby': errors[key] ? `${id}-${key}-e` : undefined,
  })
  const fieldError = (key: keyof SettingsInput) =>
    errors[key] && (
      <Message tone="error" id={`${id}-${key}-e`}>
        {errors[key]}
      </Message>
    )

  return (
    <>
      {!settings.configured && (
        <Message tone="info">初回設定です。auth_token と ct0 の両方を入力してください。</Message>
      )}
      <form onSubmit={submit} noValidate className="space-y-8">
        <Section
          title="Cookie"
          description="保存済みの値は表示されません。空欄のまま保存すると、保存済みの値は変更されません。"
          disabled={busy}
        >
          {locked && (
            <Message tone="info">
              登録中は Cookie を変更できません。先に下の「受信先の登録」で登録を解除してください。
            </Message>
          )}
          <div className="space-y-2">
            <Label htmlFor={`${id}-auth`}>
              auth_token <Indicator set={settings.auth_token_set} />
            </Label>
            <Input
              id={`${id}-auth`}
              type="password"
              autoComplete="off"
              spellCheck={false}
              className="h-10"
              value={authToken}
              disabled={locked}
              onChange={(e) => setAuthToken(e.currentTarget.value)}
              {...fieldProps('authToken')}
            />
            {fieldError('authToken')}
          </div>
          <div className="space-y-2">
            <Label htmlFor={`${id}-ct0`}>
              ct0 <Indicator set={settings.ct0_set} />
            </Label>
            <Input
              id={`${id}-ct0`}
              type="password"
              autoComplete="off"
              spellCheck={false}
              className="h-10"
              value={ct0}
              disabled={locked}
              onChange={(e) => setCt0(e.currentTarget.value)}
              {...fieldProps('ct0')}
            />
            {fieldError('ct0')}
          </div>
          <div className="space-y-3">
            <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
              <Button
                type="button"
                variant="outline"
                className="h-10 min-w-28 px-5"
                disabled={busy || !settings.configured}
                onClick={() => lock.run('check', runCheck)}
              >
                {checking && <LoaderCircle data-icon="inline-start" className="animate-spin" />}
                {checking ? '確認中…' : '確認'}
              </Button>
              <CheckResult check={check} />
            </div>
            {dirty && <Message tone="info">入力中の値は未保存です。保存後に確認します。</Message>}
            <p className="text-sm text-muted-foreground">
              「確認」は保存済みの Cookie を X に問い合わせます。保存すると自動で確認します。
            </p>
          </div>
        </Section>
        <Separator />
        <Section
          title="転送先"
          description="受信した通知を転送する Webhook の URL と、署名に使う secret です。"
          disabled={busy}
        >
          <div className="space-y-2">
            <Label htmlFor={`${id}-url`}>Webhook URL</Label>
            <Input
              id={`${id}-url`}
              type="url"
              inputMode="url"
              autoComplete="off"
              spellCheck={false}
              className="h-10"
              value={webhook}
              disabled={envLocked}
              onChange={(e) => setWebhook(e.currentTarget.value)}
              {...fieldProps('webhook')}
            />
            {fieldError('webhook')}
            <p className="text-sm text-muted-foreground">
              設定元: {SOURCE_LABEL[settings.webhook_source]}
              {envLocked
                ? '。環境変数が優先されるため、ここでは変更できません。'
                : '。空欄で保存すると設定を削除します。'}
            </p>
          </div>
          <div className="space-y-2">
            <Label htmlFor={`${id}-secret`}>
              Webhook secret <Indicator set={settings.webhook_secret_set} />
            </Label>
            <Input
              id={`${id}-secret`}
              type="password"
              autoComplete="off"
              spellCheck={false}
              className="h-10"
              value={webhookSecret}
              onChange={(e) => setWebhookSecret(e.currentTarget.value)}
              {...fieldProps('webhookSecret')}
            />
            {fieldError('webhookSecret')}
            <p className="text-sm text-muted-foreground">
              転送リクエストの署名に使います。保存済みの値は表示されません。空欄のまま保存すると、保存済みの値は変更されません。
            </p>
          </div>
        </Section>
        <Separator />
        <Section
          title="通知フィルター"
          description="無効にした種類は一覧とWebhook転送から除外します。受信履歴は保存されます。"
          disabled={busy}
        >
          <div className="space-y-4">
            {FILTER_OPTIONS.map((node) => {
              if (!('children' in node)) {
                return (
                  <FilterCheckbox
                    key={node.key}
                    id={`${id}-filter-${node.key}`}
                    label={node.label}
                    checked={filter[node.key]}
                    onCheckedChange={(checked) => setFilterKeys([node.key], checked === true)}
                  />
                )
              }
              const groupId = `${id}-filter-${node.id}`
              return (
                <div key={groupId} className="space-y-4">
                  <FilterCheckbox
                    id={groupId}
                    label={node.label}
                    checked={groupState(filter, node.children)}
                    onCheckedChange={(checked) =>
                      setFilterKeys(
                        node.children.map(({ key }) => key),
                        checked === true,
                      )
                    }
                  />
                  <div role="group" aria-labelledby={`${groupId}-label`} className="ml-7 space-y-4">
                    {node.children.map((child) => (
                      <FilterCheckbox
                        key={child.key}
                        id={`${id}-filter-${child.key}`}
                        label={child.label}
                        checked={filter[child.key]}
                        onCheckedChange={(checked) => setFilterKeys([child.key], checked === true)}
                      />
                    ))}
                  </div>
                </div>
              )
            })}
          </div>
        </Section>
        <Separator />
        <Section
          title="Machine API"
          description="このアカウントに対するAPIからの投稿・返信、いいね、フォローを、操作ごとに許可します。サービストークン側の許可も必要です。変更は保存後に反映されます。"
          disabled={busy}
        >
          {MACHINE_SWITCHES.map(({ key, id: switchId, label }) => (
            <div key={key} className="flex items-center gap-3">
              <Switch
                id={`${id}-${switchId}`}
                checked={machine[key]}
                onCheckedChange={(checked) => setMachine((prev) => ({ ...prev, [key]: checked }))}
              />
              <Label htmlFor={`${id}-${switchId}`}>{label}</Label>
            </div>
          ))}
          <p className="text-sm text-muted-foreground">
            送信やいいね・フォローの処理中は保存できません（409
            busy）。無効化しても、開始済みの処理は取り消せません。
          </p>
        </Section>
        <SectionActions>
          {result && (
            <Message tone={result.ok ? 'ok' : 'error'} role={result.ok ? 'status' : 'alert'}>
              {result.text}
            </Message>
          )}
          <Button type="submit" className="h-10 w-full px-6 md:w-auto" disabled={busy}>
            {saving && <LoaderCircle data-icon="inline-start" className="animate-spin" />}
            {saving ? '保存中…' : '保存'}
          </Button>
        </SectionActions>
      </form>
    </>
  )
}
