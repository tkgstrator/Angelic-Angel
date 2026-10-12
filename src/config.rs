use crate::error::{AngelicAngelError, Result};
use crate::notifications::NotificationKind;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Serializes read-modify-write cycles of the config file inside one process.
static CONFIG_RMW: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// 環境変数で認証情報を渡す運用では `[twitter]` を持たない設定ファイルになる。
    #[serde(default, skip_serializing_if = "TwitterConfig::is_empty")]
    pub twitter: TwitterConfig,
    pub registration: Option<Registration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_endpoint: Option<String>,
    #[serde(default)]
    pub notification_filter: NotificationFilter,
}

/// 通知の区分（`data.type`、および投稿のうちリツイート・引用RT・リプライの別）ごとに、
/// 一覧表示と Webhook 転送を有効にするか。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationFilter {
    #[serde(default = "enabled")]
    pub tweet: bool,
    #[serde(default = "enabled")]
    pub retweet: bool,
    #[serde(default = "enabled")]
    pub quote: bool,
    #[serde(default = "enabled")]
    pub reply: bool,
    #[serde(default = "enabled")]
    pub xchat_message_received: bool,
    #[serde(default = "enabled")]
    pub other: bool,
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

    /// Atomically replaces the config file (temp file + rename, mode 0600 on Unix).
    pub fn save(&self, path: &Path) -> Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| AngelicAngelError::Config(format!("failed to serialize config: {}", e)))?;
        write_private_atomic(path, content.as_bytes())
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

/// Resolves the webhook endpoint: a nonempty `WEBHOOK_ENDPOINT` wins over the config value.
pub fn resolve_webhook(stored: Option<&str>) -> (Option<String>, WebhookSource) {
    resolve_webhook_with(std::env::var("WEBHOOK_ENDPOINT").ok().as_deref(), stored)
}

pub fn resolve_webhook_with(
    env: Option<&str>,
    stored: Option<&str>,
) -> (Option<String>, WebhookSource) {
    let nonempty = |v: Option<&str>| v.map(str::trim).filter(|v| !v.is_empty()).map(String::from);
    if let Some(v) = nonempty(env) {
        (Some(v), WebhookSource::Environment)
    } else if let Some(v) = nonempty(stored) {
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
/// (never a silent fallback to the config file).
pub fn env_twitter_with(
    env_auth: Option<&str>,
    env_csrf: Option<&str>,
) -> Result<Option<TwitterConfig>> {
    fn nonempty(v: Option<&str>) -> Option<&str> {
        v.map(str::trim).filter(|v| !v.is_empty())
    }
    match (nonempty(env_auth), nonempty(env_csrf)) {
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

/// Resolves the credentials: the environment pair wins over the config file.
pub fn resolve_twitter(stored: &TwitterConfig) -> Result<(TwitterConfig, TwitterSource)> {
    let (env_auth, env_csrf) = twitter_env_pair();
    resolve_twitter_with(env_auth.as_deref(), env_csrf.as_deref(), stored)
}

pub fn resolve_twitter_with(
    env_auth: Option<&str>,
    env_csrf: Option<&str>,
    stored: &TwitterConfig,
) -> Result<(TwitterConfig, TwitterSource)> {
    Ok(match env_twitter_with(env_auth, env_csrf)? {
        Some(twitter) => (twitter, TwitterSource::Environment),
        None if stored.is_empty() => (stored.clone(), TwitterSource::None),
        None => (stored.clone(), TwitterSource::Config),
    })
}

/// Credentials to hand to X: the environment pair or the config file, otherwise an error.
pub fn require_twitter_with(
    env_auth: Option<&str>,
    env_csrf: Option<&str>,
    stored: &TwitterConfig,
) -> Result<TwitterConfig> {
    match resolve_twitter_with(env_auth, env_csrf, stored)? {
        (_, TwitterSource::None) => Err(AngelicAngelError::Config(format!(
            "Twitter credentials are not set. Set {TWITTER_AUTH_TOKEN_ENV} and \
             {TWITTER_CSRF_TOKEN_ENV}, or run `init`."
        ))),
        (twitter, _) => Ok(twitter),
    }
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

    #[test]
    fn old_config_without_webhook_parses() {
        let old = r#"
[twitter]
auth_token = "a"
ct0 = "b"
"#;
        let config: Config = toml::from_str(old).unwrap();
        assert!(config.webhook_endpoint.is_none());
        assert!(config.registration.is_none());
        assert!(
            !toml::to_string_pretty(&config)
                .unwrap()
                .contains("webhook_endpoint")
        );
    }

    #[test]
    fn notification_filter_defaults_to_all_enabled() {
        let old = "[twitter]\nauth_token = \"a\"\nct0 = \"b\"\n";
        let config: Config = toml::from_str(old).unwrap();
        assert_eq!(config.notification_filter, NotificationFilter::default());
        assert!(
            config.notification_filter.tweet
                && config.notification_filter.retweet
                && config.notification_filter.quote
                && config.notification_filter.reply
                && config.notification_filter.xchat_message_received
                && config.notification_filter.other
        );

        let partial = format!("{old}[notification_filter]\ntweet = false\n");
        let config: Config = toml::from_str(&partial).unwrap();
        assert!(!config.notification_filter.tweet);
        assert!(config.notification_filter.retweet);
        assert!(config.notification_filter.quote);
        assert!(config.notification_filter.reply);
        assert!(config.notification_filter.xchat_message_received);
        assert!(config.notification_filter.other);

        // retweet・quote・reply が追加される前に保存された 3 キーの設定は、新しい区分が有効のまま読める。
        let legacy = format!(
            "{old}[notification_filter]\ntweet = false\nxchat_message_received = false\nother = false\n"
        );
        let config: Config = toml::from_str(&legacy).unwrap();
        assert!(config.notification_filter.retweet);
        assert!(config.notification_filter.quote);
        assert!(config.notification_filter.reply);
        assert!(!config.notification_filter.tweet);

        // quote と reply は、片方だけ無効にしても他の区分に影響しない。
        let split = format!("{old}[notification_filter]\nquote = false\n");
        let config: Config = toml::from_str(&split).unwrap();
        assert!(!config.notification_filter.quote);
        assert!(config.notification_filter.reply);
        assert!(config.notification_filter.retweet);
    }

    #[test]
    fn notification_filter_round_trips_next_to_other_settings() {
        let dir = temp_dir();
        let path = dir.join("c.toml");
        let config = Config {
            twitter: TwitterConfig {
                auth_token: "a".into(),
                ct0: "b".into(),
            },
            registration: Some(Registration {
                endpoint: "https://push.example/e".into(),
                autopush: AutoPushSession {
                    uaid: "u".into(),
                    channel_id: "c".into(),
                },
                keys: WebPushKeys {
                    public_key: vec![1],
                    private_key: vec![2],
                    auth_secret: vec![3],
                },
            }),
            webhook_endpoint: Some("https://example.com/h".into()),
            notification_filter: NotificationFilter {
                tweet: false,
                retweet: false,
                quote: false,
                reply: true,
                xchat_message_received: true,
                other: false,
            },
        };
        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.notification_filter, config.notification_filter);
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
            reply: true,
            xchat_message_received: true,
            other: false,
        };
        assert!(!filter.allows(NotificationKind::Tweet));
        assert!(filter.allows(NotificationKind::Retweet));
        assert!(!filter.allows(NotificationKind::Quote));
        assert!(filter.allows(NotificationKind::Reply));
        assert!(filter.allows(NotificationKind::XchatMessageReceived));
        assert!(!filter.allows(NotificationKind::Other));
    }

    #[test]
    fn save_is_atomic_and_private() {
        let dir = temp_dir();
        let path = dir.join("c.toml");
        let config = Config {
            twitter: TwitterConfig {
                auth_token: "a".into(),
                ct0: "b".into(),
            },
            registration: None,
            webhook_endpoint: Some("https://example.com/h".into()),
            notification_filter: NotificationFilter::default(),
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
        let loaded = Config::load(&path).unwrap();
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
        let (t, source) =
            resolve_twitter_with(Some(" env-auth "), Some("env-ct0"), &stored).unwrap();
        assert_eq!(source, TwitterSource::Environment);
        assert_eq!(
            (t.auth_token.as_str(), t.ct0.as_str()),
            ("env-auth", "env-ct0")
        );
        // 設定ファイルが空でも環境変数で足りる。
        let (t, source) =
            resolve_twitter_with(Some("env-auth"), Some("env-ct0"), &TwitterConfig::default())
                .unwrap();
        assert_eq!(source, TwitterSource::Environment);
        assert_eq!(t.auth_token, "env-auth");
    }

    #[test]
    fn twitter_falls_back_to_the_file_only_when_both_env_vars_are_blank() {
        let stored = twitter("file-auth", "file-ct0");
        for (auth, csrf) in [(None, None), (Some(""), Some("  ")), (Some(" "), None)] {
            let (t, source) = resolve_twitter_with(auth, csrf, &stored).unwrap();
            assert_eq!(source, TwitterSource::Config);
            assert_eq!(t.auth_token, "file-auth");
        }
        let (_, source) = resolve_twitter_with(None, None, &TwitterConfig::default()).unwrap();
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
                let err = resolve_twitter_with(auth, csrf, stored).unwrap_err();
                assert!(matches!(err, AngelicAngelError::Config(_)));
                assert!(err.to_string().contains(missing), "{err}");
                assert!(require_twitter_with(auth, csrf, stored).is_err());
            }
            assert!(env_twitter_with(auth, csrf).is_err());
        }
    }

    #[test]
    fn require_twitter_needs_env_or_file_credentials() {
        let err = require_twitter_with(None, None, &TwitterConfig::default()).unwrap_err();
        assert!(err.to_string().contains(TWITTER_AUTH_TOKEN_ENV), "{err}");
        let t = require_twitter_with(None, None, &twitter("file-auth", "file-ct0")).unwrap();
        assert_eq!(t.ct0, "file-ct0");
        let t = require_twitter_with(Some("e"), Some("f"), &TwitterConfig::default()).unwrap();
        assert_eq!((t.auth_token.as_str(), t.ct0.as_str()), ("e", "f"));
    }

    #[test]
    fn config_without_twitter_table_parses_and_never_writes_one() {
        let config: Config =
            toml::from_str("webhook_endpoint = \"https://example.com/h\"\n").unwrap();
        assert!(config.twitter.is_empty());
        assert!(config.registration.is_none());

        // 環境変数の認証情報だけで登録した設定は、保存しても [twitter] を含まない。
        let dir = temp_dir();
        let path = dir.join("c.toml");
        let config = Config {
            registration: Some(Registration {
                endpoint: "https://push.example/e".into(),
                autopush: AutoPushSession {
                    uaid: "u".into(),
                    channel_id: "c".into(),
                },
                keys: WebPushKeys {
                    public_key: vec![1],
                    private_key: vec![2],
                    auth_secret: vec![3],
                },
            }),
            ..Config::default()
        };
        config.save(&path).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("twitter") && !written.contains("auth_token"));
        let loaded = Config::load(&path).unwrap();
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
        let r = resolve_webhook_with(Some("https://env"), Some("https://cfg"));
        assert_eq!(r, (Some("https://env".into()), WebhookSource::Environment));
        let r = resolve_webhook_with(Some("  "), Some("https://cfg"));
        assert_eq!(r, (Some("https://cfg".into()), WebhookSource::Config));
        let r = resolve_webhook_with(None, None);
        assert_eq!(r, (None, WebhookSource::None));
    }
}
