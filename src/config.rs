use crate::error::{AngelicAngelError, Result};
use crate::notifications::NotificationKind;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Serializes read-modify-write cycles of the config file inside one process.
static CONFIG_RMW: Mutex<()> = Mutex::new(());

/// 環境変数の認証情報・Webhook 設定が効く唯一のアカウント。旧形式の設定もこの ID になる。
pub const DEFAULT_ACCOUNT_ID: &str = "default";

const MAX_ACCOUNT_ID_LEN: usize = 32;

/// 旧形式の設定がアカウントを持たずにトップレベルへ直接書いていたキー。
const LEGACY_KEYS: [&str; 4] = [
    "twitter",
    "registration",
    "webhook_endpoint",
    "notification_filter",
];

const LEGACY_BACKUP_SUFFIX: &str = ".legacy.bak";

/// アカウント ID は `^[a-z0-9][a-z0-9_-]{0,31}$`。ファイル名と URL にそのまま使う。
pub fn validate_account_id(id: &str) -> Result<()> {
    let valid = id.len() <= MAX_ACCOUNT_ID_LEN
        && matches!(
            id.as_bytes(),
            [first, rest @ ..]
                if (first.is_ascii_lowercase() || first.is_ascii_digit())
                    && rest.iter().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-')
                    })
        );
    if valid {
        Ok(())
    } else {
        Err(AngelicAngelError::Config(format!(
            "invalid account id {id:?}: use 1-{MAX_ACCOUNT_ID_LEN} characters of a-z, 0-9, '_' or \
             '-', starting with a letter or digit"
        )))
    }
}

fn validate_accounts(accounts: &[Account]) -> Result<()> {
    for (index, account) in accounts.iter().enumerate() {
        validate_account_id(&account.id)?;
        if accounts[..index].iter().any(|a| a.id == account.id) {
            return Err(AngelicAngelError::Config(format!(
                "duplicate account id {:?}",
                account.id
            )));
        }
    }
    Ok(())
}

/// 設定ファイル全体。`[[accounts]]` の並びで、アカウントごとに独立した認証情報と転送先を持つ。
///
/// 旧形式（トップレベルの `[twitter]` など）は読み込み時に `default` の 1 アカウントとして扱う。
/// 読むだけではファイルを書き換えず、最初の保存で `[[accounts]]` 形式に変わる（[`Config::save`]）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(try_from = "RawConfig")]
pub struct Config {
    pub accounts: Vec<Account>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    /// Opaque incarnation/revision, never derived from a credential in public responses.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub credential_revision: String,
    /// 環境変数で認証情報を渡す運用では `[accounts.twitter]` を持たない設定ファイルになる。
    #[serde(default, skip_serializing_if = "TwitterConfig::is_empty")]
    pub twitter: TwitterConfig,
    pub registration: Option<Registration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_secret: Option<String>,
    #[serde(default)]
    pub notification_filter: NotificationFilter,
}

impl Account {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            credential_revision: uuid::Uuid::new_v4().to_string(),
            twitter: TwitterConfig::default(),
            registration: None,
            webhook_endpoint: None,
            webhook_secret: None,
            notification_filter: NotificationFilter::default(),
        }
    }
}

/// 設定ファイルに書かれたままの形。`accounts` と旧形式のキーのどちらで書かれたかを区別する。
#[derive(Deserialize)]
struct RawConfig {
    accounts: Option<Vec<Account>>,
    twitter: Option<TwitterConfig>,
    registration: Option<Registration>,
    webhook_endpoint: Option<String>,
    notification_filter: Option<NotificationFilter>,
}

impl TryFrom<RawConfig> for Config {
    type Error = AngelicAngelError;

    fn try_from(raw: RawConfig) -> Result<Self> {
        let legacy = raw.twitter.is_some()
            || raw.registration.is_some()
            || raw.webhook_endpoint.is_some()
            || raw.notification_filter.is_some();
        let accounts = match (raw.accounts, legacy) {
            (Some(_), true) => {
                return Err(AngelicAngelError::Config(format!(
                    "`accounts` cannot be combined with the top-level keys {}; move them into an \
                     [[accounts]] entry",
                    LEGACY_KEYS.join(", ")
                )));
            },
            (Some(accounts), false) => accounts,
            (None, true) => vec![Account {
                id: DEFAULT_ACCOUNT_ID.to_string(),
                credential_revision: String::new(),
                twitter: raw.twitter.unwrap_or_default(),
                registration: raw.registration,
                webhook_endpoint: raw.webhook_endpoint,
                webhook_secret: None,
                notification_filter: raw.notification_filter.unwrap_or_default(),
            }],
            (None, false) => Vec::new(),
        };
        validate_accounts(&accounts)?;
        Ok(Self { accounts })
    }
}

/// 通知の区分（`data.type`、および投稿のうちリツイート・引用RT・リプライの別）ごとに、
/// 一覧表示と Webhook 転送を有効にするか。
///
/// 引用RT は `quote`（他のアカウントの投稿の引用）と `quote_self`（自分自身の投稿の引用）に分かれる。
/// 設定ファイルに `quote_self` が無いときは、同じファイルの `quote` の値に従う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RawNotificationFilter")]
pub struct NotificationFilter {
    pub tweet: bool,
    pub retweet: bool,
    pub quote: bool,
    pub quote_self: bool,
    pub reply: bool,
    pub xchat_message_received: bool,
    pub other: bool,
}

/// 設定ファイルに書かれたままの形。省略されたキーの既定値を `NotificationFilter` へ移す前に決める。
#[derive(Deserialize)]
struct RawNotificationFilter {
    #[serde(default = "enabled")]
    tweet: bool,
    #[serde(default = "enabled")]
    retweet: bool,
    #[serde(default = "enabled")]
    quote: bool,
    #[serde(default)]
    quote_self: Option<bool>,
    #[serde(default = "enabled")]
    reply: bool,
    #[serde(default = "enabled")]
    xchat_message_received: bool,
    #[serde(default = "enabled")]
    other: bool,
}

impl From<RawNotificationFilter> for NotificationFilter {
    fn from(raw: RawNotificationFilter) -> Self {
        Self {
            tweet: raw.tweet,
            retweet: raw.retweet,
            quote: raw.quote,
            // `quote_self` の導入前に保存された設定は、引用RT の扱いを変えない。
            quote_self: raw.quote_self.unwrap_or(raw.quote),
            reply: raw.reply,
            xchat_message_received: raw.xchat_message_received,
            other: raw.other,
        }
    }
}

fn enabled() -> bool {
    true
}

impl Default for NotificationFilter {
    fn default() -> Self {
        Self {
            tweet: true,
            retweet: true,
            quote: true,
            quote_self: true,
            reply: true,
            xchat_message_received: true,
            other: true,
        }
    }
}

impl NotificationFilter {
    pub fn allows(&self, kind: NotificationKind) -> bool {
        match kind {
            NotificationKind::Tweet => self.tweet,
            NotificationKind::Retweet => self.retweet,
            NotificationKind::Quote => self.quote,
            NotificationKind::SelfQuote => self.quote_self,
            NotificationKind::Reply => self.reply,
            NotificationKind::XchatMessageReceived => self.xchat_message_received,
            NotificationKind::Other => self.other,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TwitterConfig {
    pub auth_token: String,
    pub ct0: String,
}

impl TwitterConfig {
    /// 空文字は未設定として扱う（WebUI の `auth_token_set` / `ct0_set` と同じ規則）。
    pub fn is_empty(&self) -> bool {
        self.auth_token.is_empty() && self.ct0.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebPushKeys {
    pub public_key: Vec<u8>,
    pub private_key: Vec<u8>,
    pub auth_secret: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoPushSession {
    pub uaid: String,
    pub channel_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    pub endpoint: String,
    pub autopush: AutoPushSession,
    pub keys: WebPushKeys,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            AngelicAngelError::Config(format!("failed to read config ({}): {}", path.display(), e))
        })?;
        toml::from_str(&content).map_err(Into::into)
    }

    /// Loads the config, returning `None` when the file does not exist yet.
    pub fn load_optional(path: &Path) -> Result<Option<Self>> {
        match std::fs::read_to_string(path) {
            Ok(content) => Ok(Some(toml::from_str(&content)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn account(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn account_mut(&mut self, id: &str) -> Option<&mut Account> {
        self.accounts.iter_mut().find(|a| a.id == id)
    }

    /// Replaces the account with the same id in place, or appends it.
    pub fn upsert_account(&mut self, account: Account) {
        match self.account_mut(&account.id) {
            Some(slot) => *slot = account,
            None => self.accounts.push(account),
        }
    }

    /// Atomically replaces the config file (temp file + rename, mode 0600 on Unix).
    ///
    /// A file still in the legacy layout is first copied to `<config>.legacy.bak`, because an older
    /// version cannot read the converted file. An existing backup is never overwritten, and a failed
    /// backup fails the save.
    pub fn save(&self, path: &Path) -> Result<()> {
        validate_accounts(&self.accounts)?;
        let previous = Self::load_optional(path)?;
        let mut next = self.clone();
        for account in &mut next.accounts {
            let unchanged = previous.as_ref().and_then(|c| c.account(&account.id)).filter(|old| {
                old.twitter.auth_token == account.twitter.auth_token
                    && old.twitter.ct0 == account.twitter.ct0
                    && !old.credential_revision.is_empty()
            });
            account.credential_revision = unchanged
                .map(|old| old.credential_revision.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        }
        let content = toml::to_string_pretty(&next)
            .map_err(|e| AngelicAngelError::Config(format!("failed to serialize config: {}", e)))?;
        backup_legacy_file(path)?;
        write_private_atomic(path, content.as_bytes())
    }

    /// Runs a load-modify-save cycle for the account `id` only; the other accounts are written back
    /// as loaded.
    ///
    /// The closure gets the current account (`None` when it does not exist) and returns the account
    /// to persist (or `None` to leave the file as is) with a caller-defined value. Returning an
    /// account for an id that does not exist yet is an error here; use [`Config::modify_or_create_account`]
    /// to allow creating it.
    pub fn modify_account<T, E>(
        path: &Path,
        id: &str,
        f: impl FnOnce(Option<Account>) -> std::result::Result<(Option<Account>, T), E>,
    ) -> std::result::Result<T, E>
    where
        E: From<AngelicAngelError>,
    {
        Self::modify_account_inner(path, id, false, f)
    }

    /// Same as [`Config::modify_account`], but an account returned for an absent id is created.
    pub fn modify_or_create_account<T, E>(
        path: &Path,
        id: &str,
        f: impl FnOnce(Option<Account>) -> std::result::Result<(Option<Account>, T), E>,
    ) -> std::result::Result<T, E>
    where
        E: From<AngelicAngelError>,
    {
        Self::modify_account_inner(path, id, true, f)
    }

    fn modify_account_inner<T, E>(
        path: &Path,
        id: &str,
        create: bool,
        f: impl FnOnce(Option<Account>) -> std::result::Result<(Option<Account>, T), E>,
    ) -> std::result::Result<T, E>
    where
        E: From<AngelicAngelError>,
    {
        validate_account_id(id)?;
        Config::modify(path, |current| {
            let mut config = current.unwrap_or_default();
            let existing = config.account(id).cloned();
            let exists = existing.is_some();
            let (next, value) = f(existing)?;
            let Some(mut next) = next else {
                return Ok((None, value));
            };
            if !exists && !create {
                return Err(
                    AngelicAngelError::Config(format!("account {id:?} does not exist")).into(),
                );
            }
            next.id = id.to_string();
            config.upsert_account(next);
            Ok((Some(config), value))
        })
    }

    /// Runs a load-modify-save cycle while holding the in-process config mutex.
    ///
    /// The closure returns the config to persist (or `None` to leave the file as is)
    /// together with a caller-defined value.
    pub fn modify<T, E>(
        path: &Path,
        f: impl FnOnce(Option<Config>) -> std::result::Result<(Option<Config>, T), E>,
    ) -> std::result::Result<T, E>
    where
        E: From<AngelicAngelError>,
    {
        let _guard = CONFIG_RMW.lock().unwrap_or_else(|e| e.into_inner());
        let current = Config::load_optional(path)?;
        let (next, value) = f(current)?;
        if let Some(config) = next {
            config.save(path)?;
        }
        Ok(value)
    }
}

/// 旧形式のトップレベルキーを持ち `accounts` を持たない設定か。
fn is_legacy_layout(content: &str) -> bool {
    content.parse::<toml::Table>().is_ok_and(|table| {
        !table.contains_key("accounts") && LEGACY_KEYS.iter().any(|key| table.contains_key(*key))
    })
}

fn backup_legacy_file(path: &Path) -> Result<()> {
    let original = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if !std::str::from_utf8(&original).is_ok_and(is_legacy_layout) {
        return Ok(());
    }
    let backup = sibling(path, LEGACY_BACKUP_SUFFIX);
    match std::fs::symlink_metadata(&backup) {
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(e.into()),
    }
    write_private_atomic(&backup, &original).map_err(|e| {
        AngelicAngelError::Config(format!(
            "failed to back up the legacy config to {}: {}",
            backup.display(),
            e
        ))
    })
}

fn open_private(path: &Path, create_new: bool) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "angelic-angel.toml".into());
    name.push(suffix);
    path.with_file_name(name)
}

fn ensure_parent(path: &Path) -> std::io::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => std::fs::create_dir_all(parent),
        _ => Ok(()),
    }
}

/// Writes `content` to `path` via a same-directory temp file and rename, with mode 0600.
pub fn write_private_atomic(path: &Path, content: &[u8]) -> Result<()> {
    ensure_parent(path)?;
    let tmp = sibling(path, &format!(".tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let result = (|| -> std::io::Result<()> {
        let mut file = open_private(&tmp, true)?;
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    Ok(())
}

/// X へ登録する前に、結果を保存できることを確かめる。
///
/// 保存だけが失敗すると、再起動のたびに X 側の登録が積み重なる。設定ファイルが
/// 既にあれば書き込み可能か、無くてもディレクトリに一時ファイルを作れるかを調べる。
pub fn ensure_writable(path: &Path) -> Result<()> {
    let unwritable = |e: std::io::Error| {
        AngelicAngelError::Config(format!("config {} is not writable: {}", path.display(), e))
    };
    ensure_parent(path).map_err(unwritable)?;
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => {},
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(unwritable(e)),
    }
    let probe = sibling(path, &format!(".tmp-probe-{}", std::process::id()));
    let _ = std::fs::remove_file(&probe);
    open_private(&probe, true).map_err(unwritable)?;
    std::fs::remove_file(&probe).map_err(unwritable)?;
    Ok(())
}

/// Exclusive, process-lifetime ownership of a config file (advisory lock file).
#[derive(Debug)]
pub struct ConfigLock {
    _file: File,
}

impl ConfigLock {
    pub fn acquire(config_path: &Path) -> Result<Self> {
        ensure_parent(config_path)?;
        let lock_path = sibling(config_path, ".lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| {
                AngelicAngelError::Config(format!(
                    "cannot open lock file {} (is the directory writable?): {}",
                    lock_path.display(),
                    e
                ))
            })?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(AngelicAngelError::Config(format!(
                "config {} is in use by another angelic-angel process (serve/listen/...)",
                config_path.display()
            ))),
            Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }
}

/// Where the effective webhook endpoint comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WebhookSource {
    Environment,
    Config,
    None,
}

/// 環境変数が効くのは `default` アカウントだけ。ほかのアカウントでは、環境変数を設定していても
/// 無いものとして扱う（誤って別アカウントの Cookie や転送先で動かさないため）。
fn scoped<'a>(account_id: &str, env: Option<&'a str>) -> Option<&'a str> {
    if account_id == DEFAULT_ACCOUNT_ID {
        env
    } else {
        None
    }
}

fn nonempty(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|v| !v.is_empty())
}

pub const WEBHOOK_ENDPOINT_ENV: &str = "WEBHOOK_ENDPOINT";
pub const WEBHOOK_SECRET_ENV: &str = "WEBHOOK_SECRET_KEY";

/// Resolves the webhook endpoint: for `default`, a nonempty `WEBHOOK_ENDPOINT` wins over the config
/// value. Other accounts never read the environment.
pub fn resolve_webhook(account_id: &str, stored: Option<&str>) -> (Option<String>, WebhookSource) {
    resolve_webhook_with(
        account_id,
        std::env::var(WEBHOOK_ENDPOINT_ENV).ok().as_deref(),
        stored,
    )
}

pub fn resolve_webhook_with(
    account_id: &str,
    env: Option<&str>,
    stored: Option<&str>,
) -> (Option<String>, WebhookSource) {
    if let Some(v) = nonempty(scoped(account_id, env)) {
        (Some(v.to_string()), WebhookSource::Environment)
    } else if let Some(v) = nonempty(stored) {
        (Some(v.to_string()), WebhookSource::Config)
    } else {
        (None, WebhookSource::None)
    }
}

/// Resolves the webhook signing secret with the same precedence as the endpoint: for `default`, a
/// nonempty `WEBHOOK_SECRET_KEY` (passed in as `env`) wins over the stored value. The value is
/// returned as written, since the signature is computed over it; only blank values count as unset.
pub fn resolve_webhook_secret_with(
    account_id: &str,
    env: Option<&str>,
    stored: Option<&str>,
) -> (Option<String>, WebhookSource) {
    let present = |v: Option<&str>| v.filter(|v| !v.trim().is_empty()).map(String::from);
    if let Some(v) = present(scoped(account_id, env)) {
        (Some(v), WebhookSource::Environment)
    } else if let Some(v) = present(stored) {
        (Some(v), WebhookSource::Config)
    } else {
        (None, WebhookSource::None)
    }
}

/// Where the effective Twitter credentials come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwitterSource {
    Environment,
    Config,
    None,
}

pub const TWITTER_AUTH_TOKEN_ENV: &str = "TWITTER_AUTH_TOKEN";
pub const TWITTER_CSRF_TOKEN_ENV: &str = "TWITTER_CSRF_TOKEN";

/// Raw values of the credential variables. Non-Unicode values count as unset.
pub fn twitter_env_pair() -> (Option<String>, Option<String>) {
    (
        std::env::var(TWITTER_AUTH_TOKEN_ENV).ok(),
        std::env::var(TWITTER_CSRF_TOKEN_ENV).ok(),
    )
}

/// Credentials from the environment: both nonempty, or neither. Setting only one is an error
/// (never a silent fallback to the config file). Only `default` consults the environment, so the
/// error never fires for another account.
pub fn env_twitter_with(
    account_id: &str,
    env_auth: Option<&str>,
    env_csrf: Option<&str>,
) -> Result<Option<TwitterConfig>> {
    match (
        nonempty(scoped(account_id, env_auth)),
        nonempty(scoped(account_id, env_csrf)),
    ) {
        (Some(auth_token), Some(ct0)) => Ok(Some(TwitterConfig {
            auth_token: auth_token.to_string(),
            ct0: ct0.to_string(),
        })),
        (None, None) => Ok(None),
        (Some(_), None) => Err(AngelicAngelError::Config(format!(
            "{TWITTER_AUTH_TOKEN_ENV} is set but {TWITTER_CSRF_TOKEN_ENV} is not; set both or neither"
        ))),
        (None, Some(_)) => Err(AngelicAngelError::Config(format!(
            "{TWITTER_CSRF_TOKEN_ENV} is set but {TWITTER_AUTH_TOKEN_ENV} is not; set both or neither"
        ))),
    }
}

/// Resolves the credentials: for `default`, the environment pair wins over the config file.
pub fn resolve_twitter_with(
    account_id: &str,
    env_auth: Option<&str>,
    env_csrf: Option<&str>,
    stored: &TwitterConfig,
) -> Result<(TwitterConfig, TwitterSource)> {
    Ok(match env_twitter_with(account_id, env_auth, env_csrf)? {
        Some(twitter) => (twitter, TwitterSource::Environment),
        None if stored.is_empty() => (stored.clone(), TwitterSource::None),
        None => (stored.clone(), TwitterSource::Config),
    })
}

/// Credentials to hand to X: the environment pair (`default` only) or the config file, otherwise an
/// error.
pub fn require_twitter_with(
    account_id: &str,
    env_auth: Option<&str>,
    env_csrf: Option<&str>,
    stored: &TwitterConfig,
) -> Result<TwitterConfig> {
    match resolve_twitter_with(account_id, env_auth, env_csrf, stored)? {
        (_, TwitterSource::None) if account_id == DEFAULT_ACCOUNT_ID => {
            Err(AngelicAngelError::Config(format!(
                "Twitter credentials are not set. Set {TWITTER_AUTH_TOKEN_ENV} and \
                 {TWITTER_CSRF_TOKEN_ENV}, or run `init`."
            )))
        },
        (_, TwitterSource::None) => Err(AngelicAngelError::Config(format!(
            "Twitter credentials are not set for account {account_id:?}."
        ))),
        (twitter, _) => Ok(twitter),
    }
}

#[cfg(test)]
pub fn test_default_account(path: &Path) -> Account {
    Config::load(path)
        .unwrap()
        .account(DEFAULT_ACCOUNT_ID)
        .cloned()
        .unwrap()
}

#[cfg(test)]
pub fn test_temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aa-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::test_temp_dir as temp_dir;

    const LEGACY: &str = "[twitter]\nauth_token = \"a\"\nct0 = \"b\"\n";

    /// 旧形式（またはそのまま `[[accounts]]`）の本文を読み、`default` アカウントを返す。
    fn default_of(content: &str) -> Account {
        let config: Config = toml::from_str(content).unwrap();
        config.account(DEFAULT_ACCOUNT_ID).cloned().unwrap()
    }

    fn registration(uaid: &str) -> Registration {
        Registration {
            endpoint: "https://push.example/e".into(),
            autopush: AutoPushSession {
                uaid: uaid.into(),
                channel_id: "c".into(),
            },
            keys: WebPushKeys {
                public_key: vec![1],
                private_key: vec![2],
                auth_secret: vec![3],
            },
        }
    }

    fn account(id: &str, auth: &str) -> Account {
        let mut account = Account::new(id);
        account.twitter = TwitterConfig {
            auth_token: auth.into(),
            ct0: format!("ct0-{id}"),
        };
        account
    }

    #[test]
    fn old_config_without_webhook_parses() {
        let account = default_of(LEGACY);
        assert!(account.webhook_endpoint.is_none());
        assert!(account.registration.is_none());
        assert!(
            !toml::to_string_pretty(&Config {
                accounts: vec![account]
            })
            .unwrap()
            .contains("webhook_endpoint")
        );
    }

    #[test]
    fn notification_filter_defaults_to_all_enabled() {
        let old = LEGACY;
        let filter = default_of(old).notification_filter;
        assert_eq!(filter, NotificationFilter::default());
        assert!(
            filter.tweet
                && filter.retweet
                && filter.quote
                && filter.quote_self
                && filter.reply
                && filter.xchat_message_received
                && filter.other
        );

        let partial = format!("{old}[notification_filter]\ntweet = false\n");
        let filter = default_of(&partial).notification_filter;
        assert!(!filter.tweet);
        assert!(filter.retweet);
        assert!(filter.quote);
        assert!(filter.reply);
        assert!(filter.xchat_message_received);
        assert!(filter.other);

        // retweet・quote・reply が追加される前に保存された 3 キーの設定は、新しい区分が有効のまま読める。
        let legacy = format!(
            "{old}[notification_filter]\ntweet = false\nxchat_message_received = false\nother = false\n"
        );
        let filter = default_of(&legacy).notification_filter;
        assert!(filter.retweet);
        assert!(filter.quote);
        assert!(filter.reply);
        assert!(!filter.tweet);

        // quote と reply は、片方だけ無効にしても他の区分に影響しない。
        let split = format!("{old}[notification_filter]\nquote = false\n");
        let filter = default_of(&split).notification_filter;
        assert!(!filter.quote);
        assert!(filter.reply);
        assert!(filter.retweet);
    }

    #[test]
    fn notification_filter_round_trips_next_to_other_settings() {
        let dir = temp_dir();
        let path = dir.join("c.toml");
        let mut original = account(DEFAULT_ACCOUNT_ID, "a");
        original.registration = Some(registration("u"));
        original.webhook_endpoint = Some("https://example.com/h".into());
        original.notification_filter = NotificationFilter {
            tweet: false,
            retweet: false,
            quote: false,
            quote_self: true,
            reply: true,
            xchat_message_received: true,
            other: false,
        };
        let config = Config {
            accounts: vec![original.clone()],
        };
        config.save(&path).unwrap();
        let loaded = test_default_account(&path);
        assert_eq!(loaded.notification_filter, original.notification_filter);
        assert!(!loaded.notification_filter.quote && loaded.notification_filter.quote_self);
        assert!(loaded.registration.is_some());
        assert_eq!(
            loaded.webhook_endpoint.as_deref(),
            Some("https://example.com/h")
        );
    }

    #[test]
    fn filter_allows_each_kind_independently() {
        let filter = NotificationFilter {
            tweet: false,
            retweet: true,
            quote: false,
            quote_self: true,
            reply: true,
            xchat_message_received: true,
            other: false,
        };
        assert!(!filter.allows(NotificationKind::Tweet));
        assert!(filter.allows(NotificationKind::Retweet));
        assert!(!filter.allows(NotificationKind::Quote));
        assert!(filter.allows(NotificationKind::SelfQuote));
        assert!(filter.allows(NotificationKind::Reply));
        assert!(filter.allows(NotificationKind::XchatMessageReceived));
        assert!(!filter.allows(NotificationKind::Other));

        // quote_self と quote は互いに影響しない。
        let swapped = NotificationFilter {
            quote: true,
            quote_self: false,
            ..filter
        };
        assert!(swapped.allows(NotificationKind::Quote));
        assert!(!swapped.allows(NotificationKind::SelfQuote));
    }

    #[test]
    fn a_missing_quote_self_follows_quote_in_the_same_file() {
        let base = LEGACY;
        let load = |filter: &str| -> NotificationFilter {
            default_of(&format!("{base}{filter}")).notification_filter
        };

        // quote の値に従う。quote を無効にしていた既存の設定は、自己引用も転送しないまま。
        let off = load("[notification_filter]\nquote = false\n");
        assert!(!off.quote && !off.quote_self);
        let on = load("[notification_filter]\nquote = true\n");
        assert!(on.quote && on.quote_self);

        // quote も無いとき（または表自体が無いとき）は、既定の有効に従う。
        assert!(load("[notification_filter]\ntweet = false\n").quote_self);
        assert!(load("").quote_self);

        // quote_self があれば、quote とは独立にそれを使う。
        let only_self = load("[notification_filter]\nquote = false\nquote_self = true\n");
        assert!(!only_self.quote && only_self.quote_self);
        let only_other = load("[notification_filter]\nquote = true\nquote_self = false\n");
        assert!(only_other.quote && !only_other.quote_self);
        let self_alone = load("[notification_filter]\nquote_self = false\n");
        assert!(self_alone.quote && !self_alone.quote_self);
    }

    #[test]
    fn the_filter_is_written_with_both_quote_keys() {
        let dir = temp_dir();
        let path = dir.join("c.toml");
        let legacy =
            "[twitter]\nauth_token = \"a\"\nct0 = \"b\"\n[notification_filter]\nquote = false\n";
        std::fs::write(&path, legacy).unwrap();

        // 読み込んで保存し直すと、省略されていた quote_self も書き出される。
        let loaded = Config::load(&path).unwrap();
        loaded.save(&path).unwrap();
        let written: toml::Table =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let filter = written["accounts"][0]["notification_filter"]
            .as_table()
            .unwrap();
        assert_eq!(filter["quote"].as_bool(), Some(false));
        assert_eq!(filter["quote_self"].as_bool(), Some(false));

        // 書き出した値は、再び quote に引きずられず、そのまま読み戻せる。
        let mut changed = Config::load(&path).unwrap();
        changed
            .account_mut(DEFAULT_ACCOUNT_ID)
            .unwrap()
            .notification_filter
            .quote_self = true;
        changed.save(&path).unwrap();
        let reloaded = test_default_account(&path).notification_filter;
        assert!(!reloaded.quote && reloaded.quote_self);
        assert_eq!(
            reloaded,
            changed
                .account(DEFAULT_ACCOUNT_ID)
                .unwrap()
                .notification_filter
        );
    }

    #[test]
    fn save_is_atomic_and_private() {
        let dir = temp_dir();
        let path = dir.join("c.toml");
        let mut account = account(DEFAULT_ACCOUNT_ID, "a");
        account.webhook_endpoint = Some("https://example.com/h".into());
        let config = Config {
            accounts: vec![account],
        };
        config.save(&path).unwrap();
        config.save(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty());
        let loaded = test_default_account(&path);
        assert_eq!(
            loaded.webhook_endpoint.as_deref(),
            Some("https://example.com/h")
        );
    }

    #[test]
    fn lock_is_exclusive() {
        let dir = temp_dir();
        let path = dir.join("c.toml");
        let first = ConfigLock::acquire(&path).unwrap();
        let second = ConfigLock::acquire(&path);
        assert!(second.is_err());
        drop(first);
        assert!(ConfigLock::acquire(&path).is_ok());
    }

    fn twitter(auth_token: &str, ct0: &str) -> TwitterConfig {
        TwitterConfig {
            auth_token: auth_token.into(),
            ct0: ct0.into(),
        }
    }

    #[test]
    fn twitter_env_pair_takes_precedence_over_the_file() {
        let stored = twitter("file-auth", "file-ct0");
        let (t, source) = resolve_twitter_with(
            DEFAULT_ACCOUNT_ID,
            Some(" env-auth "),
            Some("env-ct0"),
            &stored,
        )
        .unwrap();
        assert_eq!(source, TwitterSource::Environment);
        assert_eq!(
            (t.auth_token.as_str(), t.ct0.as_str()),
            ("env-auth", "env-ct0")
        );
        // 設定ファイルが空でも環境変数で足りる。
        let (t, source) = resolve_twitter_with(
            DEFAULT_ACCOUNT_ID,
            Some("env-auth"),
            Some("env-ct0"),
            &TwitterConfig::default(),
        )
        .unwrap();
        assert_eq!(source, TwitterSource::Environment);
        assert_eq!(t.auth_token, "env-auth");
    }

    #[test]
    fn twitter_falls_back_to_the_file_only_when_both_env_vars_are_blank() {
        let stored = twitter("file-auth", "file-ct0");
        for (auth, csrf) in [(None, None), (Some(""), Some("  ")), (Some(" "), None)] {
            let (t, source) =
                resolve_twitter_with(DEFAULT_ACCOUNT_ID, auth, csrf, &stored).unwrap();
            assert_eq!(source, TwitterSource::Config);
            assert_eq!(t.auth_token, "file-auth");
        }
        let (_, source) =
            resolve_twitter_with(DEFAULT_ACCOUNT_ID, None, None, &TwitterConfig::default())
                .unwrap();
        assert_eq!(source, TwitterSource::None);
    }

    #[test]
    fn twitter_env_with_only_one_value_is_an_error() {
        let stored = twitter("file-auth", "file-ct0");
        for (auth, csrf, missing) in [
            (Some("a"), None, TWITTER_CSRF_TOKEN_ENV),
            (Some("a"), Some(" "), TWITTER_CSRF_TOKEN_ENV),
            (None, Some("c"), TWITTER_AUTH_TOKEN_ENV),
            (Some(""), Some("c"), TWITTER_AUTH_TOKEN_ENV),
        ] {
            // 設定ファイルに値があっても黙ってそちらへ戻さない。
            for stored in [&stored, &TwitterConfig::default()] {
                let err = resolve_twitter_with(DEFAULT_ACCOUNT_ID, auth, csrf, stored).unwrap_err();
                assert!(matches!(err, AngelicAngelError::Config(_)));
                assert!(err.to_string().contains(missing), "{err}");
                assert!(require_twitter_with(DEFAULT_ACCOUNT_ID, auth, csrf, stored).is_err());
            }
            assert!(env_twitter_with(DEFAULT_ACCOUNT_ID, auth, csrf).is_err());
        }
    }

    #[test]
    fn require_twitter_needs_env_or_file_credentials() {
        let err = require_twitter_with(DEFAULT_ACCOUNT_ID, None, None, &TwitterConfig::default())
            .unwrap_err();
        assert!(err.to_string().contains(TWITTER_AUTH_TOKEN_ENV), "{err}");
        let t = require_twitter_with(
            DEFAULT_ACCOUNT_ID,
            None,
            None,
            &twitter("file-auth", "file-ct0"),
        )
        .unwrap();
        assert_eq!(t.ct0, "file-ct0");
        let t = require_twitter_with(
            DEFAULT_ACCOUNT_ID,
            Some("e"),
            Some("f"),
            &TwitterConfig::default(),
        )
        .unwrap();
        assert_eq!((t.auth_token.as_str(), t.ct0.as_str()), ("e", "f"));
    }

    #[test]
    fn machine_revision_is_durable_and_rotates_on_credentials_or_recreation() {
        let path = temp_dir().join("revision.toml");
        let mut config = Config { accounts: vec![Account::new("default")] };
        config.accounts[0].twitter = TwitterConfig { auth_token: "fixture-auth".into(), ct0: "fixture-csrf".into() };
        config.save(&path).unwrap();
        let first = test_default_account(&path).credential_revision;
        config.save(&path).unwrap();
        assert_eq!(first, test_default_account(&path).credential_revision);
        config.accounts[0].twitter.ct0 = "rotated-fixture-csrf".into();
        config.save(&path).unwrap();
        let second = test_default_account(&path).credential_revision;
        assert_ne!(first, second);
        Config::default().save(&path).unwrap();
        config.save(&path).unwrap();
        assert_ne!(second, test_default_account(&path).credential_revision);
    }

    #[test]
    fn config_without_twitter_table_parses_and_never_writes_one() {
        let account = default_of("webhook_endpoint = \"https://example.com/h\"\n");
        assert!(account.twitter.is_empty());
        assert!(account.registration.is_none());

        // 環境変数の認証情報だけで登録した設定は、保存しても [twitter] を含まない。
        let dir = temp_dir();
        let path = dir.join("c.toml");
        let mut env_only = Account::new(DEFAULT_ACCOUNT_ID);
        env_only.registration = Some(registration("u"));
        let config = Config {
            accounts: vec![env_only],
        };
        config.save(&path).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("twitter") && !written.contains("auth_token"));
        let loaded = test_default_account(&path);
        assert!(loaded.twitter.is_empty() && loaded.registration.is_some());
    }

    #[test]
    fn ensure_writable_checks_file_and_directory_before_registering() {
        let dir = temp_dir();
        // 設定ファイルがまだ無くても、ディレクトリが書ければよい。プローブは残さない。
        let path = dir.join("new").join("c.toml");
        ensure_writable(&path).unwrap();
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(dir.join("new")).unwrap().count(), 0);

        // 既存の設定ファイルは内容を変えずに書き込み可能かだけ確かめる。
        std::fs::write(&path, "keep").unwrap();
        ensure_writable(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep");

        // 書き込めない設定ファイル（ここではディレクトリ）は登録前にエラーにする。
        let as_dir = dir.join("dir.toml");
        std::fs::create_dir(&as_dir).unwrap();
        let err = ensure_writable(&as_dir).unwrap_err();
        assert!(err.to_string().contains("not writable"), "{err}");

        // 親がディレクトリでなければ保存できない。
        let blocked = dir.join("new").join("c.toml").join("nested.toml");
        assert!(ensure_writable(&blocked).is_err());
    }

    #[test]
    fn webhook_env_takes_precedence_only_when_nonempty() {
        let r = resolve_webhook_with(DEFAULT_ACCOUNT_ID, Some("https://env"), Some("https://cfg"));
        assert_eq!(r, (Some("https://env".into()), WebhookSource::Environment));
        let r = resolve_webhook_with(DEFAULT_ACCOUNT_ID, Some("  "), Some("https://cfg"));
        assert_eq!(r, (Some("https://cfg".into()), WebhookSource::Config));
        let r = resolve_webhook_with(DEFAULT_ACCOUNT_ID, None, None);
        assert_eq!(r, (None, WebhookSource::None));
    }

    // ---- multiple accounts ----

    fn load_err(content: &str) -> String {
        toml::from_str::<Config>(content).unwrap_err().to_string()
    }

    const FULL_LEGACY: &str = r#"webhook_endpoint = "https://example.com/h"

[twitter]
auth_token = "a"
ct0 = "b"

[registration]
endpoint = "https://push.example/e"

[registration.autopush]
uaid = "u"
channel_id = "c"

[registration.keys]
public_key = [1]
private_key = [2]
auth_secret = [3]

[notification_filter]
tweet = false
"#;

    #[test]
    fn a_legacy_file_loads_as_the_single_default_account() {
        let config: Config = toml::from_str(FULL_LEGACY).unwrap();
        assert_eq!(config.accounts.len(), 1);
        let account = &config.accounts[0];
        assert_eq!(account.id, DEFAULT_ACCOUNT_ID);
        assert_eq!(account.twitter.auth_token, "a");
        assert_eq!(account.twitter.ct0, "b");
        assert_eq!(
            account.webhook_endpoint.as_deref(),
            Some("https://example.com/h")
        );
        assert!(account.webhook_secret.is_none());
        assert_eq!(account.registration.as_ref().unwrap().autopush.uaid, "u");
        assert!(!account.notification_filter.tweet && account.notification_filter.retweet);

        // 一部のキーだけの旧形式（環境変数運用で registration だけを持つ設定など）も 1 アカウントになる。
        let only_webhook = default_of("webhook_endpoint = \"https://example.com/h\"\n");
        assert!(only_webhook.twitter.is_empty());
    }

    #[test]
    fn mixing_accounts_with_a_legacy_key_is_an_error() {
        let accounts = "[[accounts]]\nid = \"default\"\n";
        for legacy in [
            "[twitter]\nauth_token = \"a\"\nct0 = \"b\"\n",
            "webhook_endpoint = \"https://example.com/h\"\n",
            "[notification_filter]\ntweet = false\n",
            "[registration]\nendpoint = \"e\"\n[registration.autopush]\nuaid = \"u\"\nchannel_id = \"c\"\n[registration.keys]\npublic_key = []\nprivate_key = []\nauth_secret = []\n",
        ] {
            // 旧キーを先に書けばトップレベルのキーになるのでエラー（黙って合成しない）。
            let err = load_err(&format!("{legacy}{accounts}"));
            assert!(err.contains("cannot be combined"), "{err}");
            // 表の旧キーは `[[accounts]]` の後ろでもトップレベルの表なので同じ。スカラーのキーは
            // 直前の `[[accounts]]` 項目の中身になるため、この並びはエラーにならない。
            if legacy.starts_with('[') {
                let err = load_err(&format!("{accounts}{legacy}"));
                assert!(err.contains("cannot be combined"), "{err}");
            }
        }
        // `accounts = []` と旧キーの同居も同じ。
        assert!(load_err("accounts = []\nwebhook_endpoint = \"x\"\n").contains("combined"));
    }

    #[test]
    fn an_empty_file_loads_as_zero_accounts() {
        for content in ["", "\n", "# comment only\n", "accounts = []\n"] {
            let config: Config = toml::from_str(content).unwrap();
            assert!(config.accounts.is_empty(), "{content:?}");
        }
    }

    #[test]
    fn account_ids_are_validated_on_load_and_save() {
        for valid in [
            "default",
            "a",
            "0",
            "a-b_c",
            "x".repeat(32).as_str(),
            "9lives",
        ] {
            assert!(validate_account_id(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "A",
            "Work",
            "-a",
            "_a",
            "a b",
            "a.b",
            "a/b",
            "a..",
            "..",
            "é",
            "日本",
            "a\n",
            "x".repeat(33).as_str(),
        ] {
            assert!(validate_account_id(invalid).is_err(), "{invalid:?}");
            let content = format!("[[accounts]]\nid = {}\n", toml::Value::from(invalid));
            assert!(
                load_err(&content).contains("invalid account id"),
                "{invalid:?}"
            );
        }

        let duplicate =
            "[[accounts]]\nid = \"a\"\n[[accounts]]\nid = \"b\"\n[[accounts]]\nid = \"a\"\n";
        assert!(load_err(duplicate).contains("duplicate account id"));

        // 保存でも同じ検査をし、不正な設定は書かない。
        let dir = temp_dir();
        let path = dir.join("c.toml");
        let bad = Config {
            accounts: vec![Account::new("ok"), Account::new("ok")],
        };
        assert!(bad.save(&path).is_err());
        let bad = Config {
            accounts: vec![Account::new("Bad Id")],
        };
        assert!(bad.save(&path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn two_accounts_round_trip_and_omit_empty_secrets() {
        let dir = temp_dir();
        let path = dir.join("c.toml");
        let mut first = account(DEFAULT_ACCOUNT_ID, "auth-1");
        first.registration = Some(registration("u1"));
        first.webhook_endpoint = Some("https://example.com/one".into());
        first.webhook_secret = Some("secret-1".into());
        first.notification_filter.tweet = false;
        // 認証情報も secret も無いアカウントは、対応する表・キーを書かない。
        let mut second = Account::new("sub-2");
        second.registration = Some(registration("u2"));
        Config {
            accounts: vec![first.clone(), second.clone()],
        }
        .save(&path)
        .unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written.matches("[[accounts]]").count(), 2, "{written}");
        assert!(written.contains("[accounts.twitter]"), "{written}");
        assert!(written.contains("[accounts.registration]"), "{written}");
        assert!(
            written.contains("[accounts.notification_filter]"),
            "{written}"
        );
        assert_eq!(
            written.matches("[accounts.twitter]").count(),
            1,
            "{written}"
        );
        assert_eq!(written.matches("webhook_secret").count(), 1, "{written}");
        assert_eq!(written.matches("webhook_endpoint").count(), 1, "{written}");
        // 新形式には旧形式のトップレベルキーを書かない。
        assert!(!written.starts_with("[twitter]") && !written.contains("\n[twitter]"));

        let loaded = Config::load(&path).unwrap();
        assert_eq!(
            loaded
                .accounts
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>(),
            [DEFAULT_ACCOUNT_ID, "sub-2"]
        );
        let (a, b) = (&loaded.accounts[0], &loaded.accounts[1]);
        assert_eq!(a.twitter.auth_token, "auth-1");
        assert_eq!(a.webhook_secret.as_deref(), Some("secret-1"));
        assert_eq!(a.webhook_endpoint, first.webhook_endpoint);
        assert_eq!(a.notification_filter, first.notification_filter);
        assert_eq!(a.registration.as_ref().unwrap().autopush.uaid, "u1");
        assert!(b.twitter.is_empty() && b.webhook_secret.is_none());
        assert_eq!(b.registration.as_ref().unwrap().autopush.uaid, "u2");
        assert_eq!(b.notification_filter, NotificationFilter::default());
    }

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn dir_listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn loading_never_creates_or_rewrites_anything() {
        let dir = temp_dir();
        let path = dir.join("c.toml");
        std::fs::write(&path, FULL_LEGACY).unwrap();
        let before = dir_listing(&dir);

        Config::load(&path).unwrap();
        Config::load_optional(&path).unwrap().unwrap();
        // 何も変更しない modify も、旧形式のファイルを変換しない。
        Config::modify_account(&path, DEFAULT_ACCOUNT_ID, |current| {
            assert!(current.is_some());
            Ok::<_, AngelicAngelError>((None, ()))
        })
        .unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), FULL_LEGACY);
        assert_eq!(dir_listing(&dir), before);
        assert!(!sibling(&path, LEGACY_BACKUP_SUFFIX).exists());
    }

    #[test]
    fn the_first_save_of_a_legacy_file_backs_up_the_original_bytes() {
        let dir = temp_dir();
        let path = dir.join("c.toml");
        let backup = sibling(&path, LEGACY_BACKUP_SUFFIX);
        // 末尾の空白やコメントも含め、原本をバイト単位でそのまま残す。
        let original = format!("# my settings\n{FULL_LEGACY}\n\n");
        std::fs::write(&path, &original).unwrap();

        let mut config = Config::load(&path).unwrap();
        config
            .account_mut(DEFAULT_ACCOUNT_ID)
            .unwrap()
            .webhook_secret = Some("s".into());
        config.save(&path).unwrap();

        assert_eq!(std::fs::read(&backup).unwrap(), original.as_bytes());
        assert_eq!(mode_of(&backup), 0o600);
        assert_eq!(mode_of(&path), 0o600);
        // 保存後は新形式で、同じ内容を `default` として読める。
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("[[accounts]]") && !written.contains("\n[twitter]"));
        assert_eq!(test_default_account(&path).twitter.auth_token, "a");

        // 2 回目以降の保存は、新形式なので退避も上書きもしない。
        config
            .account_mut(DEFAULT_ACCOUNT_ID)
            .unwrap()
            .webhook_secret = Some("t".into());
        config.save(&path).unwrap();
        assert_eq!(std::fs::read(&backup).unwrap(), original.as_bytes());
        assert!(dir_listing(&dir).iter().all(|n| !n.contains(".tmp-")));
    }

    #[test]
    fn an_existing_backup_is_never_overwritten() {
        let dir = temp_dir();
        let path = dir.join("c.toml");
        let backup = sibling(&path, LEGACY_BACKUP_SUFFIX);
        std::fs::write(&backup, "older backup").unwrap();
        std::fs::write(&path, FULL_LEGACY).unwrap();

        Config::load(&path).unwrap().save(&path).unwrap();

        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "older backup");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("[[accounts]]")
        );
    }

    #[test]
    fn saving_a_new_format_or_missing_file_makes_no_backup() {
        let dir = temp_dir();
        let path = dir.join("c.toml");
        let backup = sibling(&path, LEGACY_BACKUP_SUFFIX);
        let config = Config {
            accounts: vec![Account::new(DEFAULT_ACCOUNT_ID)],
        };
        config.save(&path).unwrap();
        config.save(&path).unwrap();
        assert!(!backup.exists());

        // アカウントを持たない空の設定や、旧キーの無い設定を上書きしても退避しない。
        std::fs::write(&path, "").unwrap();
        config.save(&path).unwrap();
        assert!(!backup.exists());
    }

    #[test]
    fn a_legacy_file_is_converted_by_modify_account_with_a_backup() {
        let dir = temp_dir();
        let path = dir.join("c.toml");
        let backup = sibling(&path, LEGACY_BACKUP_SUFFIX);
        std::fs::write(&path, FULL_LEGACY).unwrap();

        Config::modify_account(&path, DEFAULT_ACCOUNT_ID, |current| {
            let mut account = current.unwrap();
            account.registration = None;
            Ok::<_, AngelicAngelError>((Some(account), ()))
        })
        .unwrap();

        assert_eq!(std::fs::read_to_string(&backup).unwrap(), FULL_LEGACY);
        let account = test_default_account(&path);
        assert!(account.registration.is_none());
        assert_eq!(account.twitter.auth_token, "a");
    }

    #[test]
    fn a_failed_backup_fails_the_save_and_keeps_the_legacy_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir();
        let path = dir.join("c.toml");
        std::fs::write(&path, FULL_LEGACY).unwrap();
        let config = Config::load(&path).unwrap();

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        // root は書き込み禁止のディレクトリにも書けるので、確かめられない。
        let blocked = std::fs::File::create(dir.join("probe")).is_err();
        let result = config.save(&path);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        if !blocked {
            return;
        }

        let err = result.unwrap_err().to_string();
        assert!(err.contains("legacy"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), FULL_LEGACY);
        assert!(!sibling(&path, LEGACY_BACKUP_SUFFIX).exists());
    }

    #[test]
    fn modify_account_changes_only_the_target_account() {
        let dir = temp_dir();
        let path = dir.join("c.toml");
        let mut first = account(DEFAULT_ACCOUNT_ID, "auth-1");
        first.webhook_secret = Some("secret-1".into());
        let second = account("sub-2", "auth-2");
        Config {
            accounts: vec![first, second],
        }
        .save(&path)
        .unwrap();

        let value = Config::modify_account(&path, "sub-2", |current| {
            let mut account = current.unwrap();
            account.registration = Some(registration("u2"));
            Ok::<_, AngelicAngelError>((Some(account), "done"))
        })
        .unwrap();
        assert_eq!(value, "done");

        let config = Config::load(&path).unwrap();
        assert_eq!(config.accounts.len(), 2);
        let first = config.account(DEFAULT_ACCOUNT_ID).unwrap();
        assert!(first.registration.is_none());
        assert_eq!(first.webhook_secret.as_deref(), Some("secret-1"));
        let second = config.account("sub-2").unwrap();
        assert_eq!(second.registration.as_ref().unwrap().autopush.uaid, "u2");
        // 並びは変わらない。
        assert_eq!(config.accounts[0].id, DEFAULT_ACCOUNT_ID);

        // 閉包が id を書き換えても、対象の id のまま保存する。
        Config::modify_account(&path, "sub-2", |current| {
            let mut account = current.unwrap();
            account.id = DEFAULT_ACCOUNT_ID.to_string();
            Ok::<_, AngelicAngelError>((Some(account), ()))
        })
        .unwrap();
        assert_eq!(Config::load(&path).unwrap().accounts.len(), 2);
    }

    #[test]
    fn modify_account_does_not_create_an_absent_account_unless_asked() {
        let dir = temp_dir();
        let path = dir.join("c.toml");
        Config {
            accounts: vec![account("a", "auth-a")],
        }
        .save(&path)
        .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        // 無いアカウントを閉包が返しても、`modify_account` は作らずエラーにする。
        let err = Config::modify_account(&path, "b", |current| {
            assert!(current.is_none());
            Ok::<_, AngelicAngelError>((Some(Account::new("b")), ()))
        })
        .unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
        // 何も返さなければ、無いアカウントでもエラーにならずファイルも変えない。
        Config::modify_account(&path, "b", |current| {
            Ok::<_, AngelicAngelError>((current, ()))
        })
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        // 不正な id は閉包を呼ぶ前に弾く。
        let err = Config::modify_account(
            &path,
            "Bad",
            |_| -> std::result::Result<(Option<Account>, ()), AngelicAngelError> {
                panic!("must not be called")
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid account id"), "{err}");

        // 作成を許す呼び出しは、新しいアカウントを末尾に足し、既存のものは変えない。
        Config::modify_or_create_account(&path, "b", |current| {
            assert!(current.is_none());
            Ok::<_, AngelicAngelError>((Some(Account::new("b")), ()))
        })
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(
            config
                .accounts
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(config.account("a").unwrap().twitter.auth_token, "auth-a");

        // 設定ファイルが無くても、作成を許せば最初のアカウントとして作れる。
        let fresh = dir.join("fresh.toml");
        Config::modify_or_create_account(&fresh, DEFAULT_ACCOUNT_ID, |current| {
            assert!(current.is_none());
            Ok::<_, AngelicAngelError>((Some(Account::new(DEFAULT_ACCOUNT_ID)), ()))
        })
        .unwrap();
        assert_eq!(Config::load(&fresh).unwrap().accounts.len(), 1);
    }

    #[test]
    fn the_environment_pair_applies_to_the_default_account_only() {
        let stored = twitter("file-auth", "file-ct0");
        let env = (Some("env-auth"), Some("env-ct0"));

        let (t, source) = resolve_twitter_with(DEFAULT_ACCOUNT_ID, env.0, env.1, &stored).unwrap();
        assert_eq!(
            (source, t.auth_token.as_str()),
            (TwitterSource::Environment, "env-auth")
        );

        // ほかのアカウントは環境変数を見ず、常に設定ファイルの値を使う。
        let (t, source) = resolve_twitter_with("work", env.0, env.1, &stored).unwrap();
        assert_eq!(
            (source, t.auth_token.as_str()),
            (TwitterSource::Config, "file-auth")
        );
        let (_, source) =
            resolve_twitter_with("work", env.0, env.1, &TwitterConfig::default()).unwrap();
        assert_eq!(source, TwitterSource::None);
        assert!(env_twitter_with("work", env.0, env.1).unwrap().is_none());

        // 認証情報が無いときのエラーは、default だけが環境変数に触れる。
        let err = require_twitter_with("work", env.0, env.1, &TwitterConfig::default())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("work") && !err.contains(TWITTER_AUTH_TOKEN_ENV),
            "{err}"
        );
        assert_eq!(
            require_twitter_with("work", env.0, env.1, &stored)
                .unwrap()
                .ct0,
            "file-ct0"
        );
        assert_eq!(
            require_twitter_with(DEFAULT_ACCOUNT_ID, env.0, env.1, &stored)
                .unwrap()
                .ct0,
            "env-ct0"
        );
    }

    #[test]
    fn a_half_set_environment_pair_is_an_error_for_default_only() {
        let stored = twitter("file-auth", "file-ct0");
        for (auth, csrf, missing) in [
            (Some("a"), None, TWITTER_CSRF_TOKEN_ENV),
            (None, Some("c"), TWITTER_AUTH_TOKEN_ENV),
        ] {
            let err = resolve_twitter_with(DEFAULT_ACCOUNT_ID, auth, csrf, &stored).unwrap_err();
            assert!(err.to_string().contains(missing), "{err}");
            assert!(env_twitter_with(DEFAULT_ACCOUNT_ID, auth, csrf).is_err());

            for id in ["work", "a", "default2"] {
                let (t, source) = resolve_twitter_with(id, auth, csrf, &stored).unwrap();
                assert_eq!(
                    (source, t.auth_token.as_str()),
                    (TwitterSource::Config, "file-auth")
                );
                assert!(env_twitter_with(id, auth, csrf).unwrap().is_none());
                assert!(require_twitter_with(id, auth, csrf, &stored).is_ok());
            }
        }
    }

    #[test]
    fn the_webhook_endpoint_environment_applies_to_the_default_account_only() {
        let r = resolve_webhook_with("work", Some("https://env"), Some("https://cfg"));
        assert_eq!(r, (Some("https://cfg".into()), WebhookSource::Config));
        let r = resolve_webhook_with("work", Some("https://env"), None);
        assert_eq!(r, (None, WebhookSource::None));
    }

    #[test]
    fn the_webhook_secret_follows_the_endpoint_precedence() {
        // default: 空白でない環境変数の値が設定ファイルの値に勝つ。値は加工しない。
        let r = resolve_webhook_secret_with(DEFAULT_ACCOUNT_ID, Some(" env "), Some("cfg"));
        assert_eq!(r, (Some(" env ".into()), WebhookSource::Environment));
        let r = resolve_webhook_secret_with(DEFAULT_ACCOUNT_ID, Some("  "), Some("cfg"));
        assert_eq!(r, (Some("cfg".into()), WebhookSource::Config));
        let r = resolve_webhook_secret_with(DEFAULT_ACCOUNT_ID, None, Some("cfg"));
        assert_eq!(r, (Some("cfg".into()), WebhookSource::Config));
        let r = resolve_webhook_secret_with(DEFAULT_ACCOUNT_ID, Some(""), Some("  "));
        assert_eq!(r, (None, WebhookSource::None));
        let r = resolve_webhook_secret_with(DEFAULT_ACCOUNT_ID, None, None);
        assert_eq!(r, (None, WebhookSource::None));

        // ほかのアカウントは環境変数を見ず、保存された値だけを使う。
        let r = resolve_webhook_secret_with("work", Some("env"), Some("cfg"));
        assert_eq!(r, (Some("cfg".into()), WebhookSource::Config));
        let r = resolve_webhook_secret_with("work", Some("env"), None);
        assert_eq!(r, (None, WebhookSource::None));
    }
}
