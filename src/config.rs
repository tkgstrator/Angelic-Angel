use crate::error::{AngelicAngelError, Result};
use crate::notifications::NotificationKind;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Serializes read-modify-write cycles of the config file inside one process.
static CONFIG_RMW: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub twitter: TwitterConfig,
    pub registration: Option<Registration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_endpoint: Option<String>,
    #[serde(default)]
    pub notification_filter: NotificationFilter,
}

/// `data.type` の区分ごとに、一覧表示と Webhook 転送を有効にするか。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationFilter {
    #[serde(default = "enabled")]
    pub tweet: bool,
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
            xchat_message_received: true,
            other: true,
        }
    }
}

impl NotificationFilter {
    pub fn allows(&self, kind: NotificationKind) -> bool {
        match kind {
            NotificationKind::Tweet => self.tweet,
            NotificationKind::XchatMessageReceived => self.xchat_message_received,
            NotificationKind::Other => self.other,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwitterConfig {
    pub auth_token: String,
    pub ct0: String,
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
            .open(&lock_path)?;
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
                && config.notification_filter.xchat_message_received
                && config.notification_filter.other
        );

        let partial = format!("{old}[notification_filter]\ntweet = false\n");
        let config: Config = toml::from_str(&partial).unwrap();
        assert!(!config.notification_filter.tweet);
        assert!(config.notification_filter.xchat_message_received);
        assert!(config.notification_filter.other);
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
            xchat_message_received: true,
            other: false,
        };
        assert!(!filter.allows(NotificationKind::Tweet));
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
