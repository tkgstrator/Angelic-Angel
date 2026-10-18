//! Per-account registration/settings services, and the registry that owns them.

use super::error::ApiError;
use super::following::{FollowingJobs, FollowingNotificationsDto, Timings};
use crate::config::{
    self, Account, AutoPushSession, Config, DEFAULT_ACCOUNT_ID, NotificationFilter, Registration,
    TwitterConfig, WebhookSource,
};
use crate::error::{AngelicAngelError, Result};
use crate::notifications::{self, NotificationPage};
use crate::push::PushSubscription;
use crate::status::{Counters, Operation, ReceiverState, Telemetry};
use crate::twitter::{FollowingPage, XApi, XError};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::task::{JoinHandle, JoinSet};

const REMOTE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CREDENTIAL_LEN: usize = 1024;
const MAX_WEBHOOK_SECRET_LEN: usize = 4096;
const MAX_URL_LEN: usize = 2048;

/// Remote operations and the receiver loop, abstracted so tests can use a fake.
pub trait Backend: Send + Sync + 'static {
    fn subscribe(&self) -> impl Future<Output = Result<PushSubscription>> + Send;
    fn twitter_register(
        &self,
        twitter: &TwitterConfig,
        subscription: &PushSubscription,
    ) -> impl Future<Output = Result<()>> + Send;
    fn autopush_unregister(
        &self,
        session: &AutoPushSession,
    ) -> impl Future<Output = Result<()>> + Send;
    fn run_receiver(
        &self,
        account_id: String,
        registration: Registration,
        config_path: PathBuf,
        telemetry: Arc<Telemetry>,
    ) -> impl Future<Output = Result<()>> + Send;
    /// Screen name of the account the saved credentials belong to.
    fn screen_name(
        &self,
        twitter: &TwitterConfig,
    ) -> impl Future<Output = std::result::Result<String, XError>> + Send;
    /// One page of the accounts followed by `screen_name`, starting at `cursor`.
    fn following_page(
        &self,
        twitter: &TwitterConfig,
        screen_name: &str,
        cursor: i64,
    ) -> impl Future<Output = std::result::Result<FollowingPage, XError>> + Send;
    /// Turns on device notifications for one followed account.
    fn enable_device_notifications(
        &self,
        twitter: &TwitterConfig,
        user_id: &str,
    ) -> impl Future<Output = std::result::Result<(), XError>> + Send;
}

/// Production backend talking to AutoPush, X and the real listener. One instance serves every
/// account: each call receives the credentials of the account it acts for.
pub struct LiveBackend {
    x: XApi,
}

impl Default for LiveBackend {
    fn default() -> Self {
        Self { x: XApi::new() }
    }
}

impl Backend for LiveBackend {
    async fn subscribe(&self) -> Result<PushSubscription> {
        crate::push::subscribe().await
    }

    async fn twitter_register(
        &self,
        twitter: &TwitterConfig,
        subscription: &PushSubscription,
    ) -> Result<()> {
        crate::twitter::register(twitter, subscription).await
    }

    async fn autopush_unregister(&self, session: &AutoPushSession) -> Result<()> {
        crate::autopush::unregister(session).await
    }

    async fn run_receiver(
        &self,
        account_id: String,
        registration: Registration,
        config_path: PathBuf,
        telemetry: Arc<Telemetry>,
    ) -> Result<()> {
        crate::listener::listen_managed(&account_id, registration, &config_path, &telemetry).await
    }

    async fn screen_name(&self, twitter: &TwitterConfig) -> std::result::Result<String, XError> {
        self.x.screen_name(twitter).await
    }

    async fn following_page(
        &self,
        twitter: &TwitterConfig,
        screen_name: &str,
        cursor: i64,
    ) -> std::result::Result<FollowingPage, XError> {
        self.x.following_page(twitter, screen_name, cursor).await
    }

    async fn enable_device_notifications(
        &self,
        twitter: &TwitterConfig,
        user_id: &str,
    ) -> std::result::Result<(), XError> {
        self.x.enable_device_notifications(twitter, user_id).await
    }
}

// ---- environment ------------------------------------------------------------

/// The cookie and webhook environment variables, read once at startup. They apply to the
/// `default` account only, and their values are never written to the config file.
///
/// A cookie pair in the environment also makes `default` exist without a config entry (the
/// environment-only Docker setup); that account can be used and registered, but not removed.
#[derive(Default)]
pub struct Environment {
    twitter: Option<TwitterConfig>,
    webhook_endpoint: Option<String>,
    webhook_secret: Option<String>,
}

impl Environment {
    pub fn from_process() -> Result<Self> {
        let (auth, csrf) = config::twitter_env_pair();
        Self::new(
            auth.as_deref(),
            csrf.as_deref(),
            std::env::var(config::WEBHOOK_ENDPOINT_ENV).ok().as_deref(),
            std::env::var(config::WEBHOOK_SECRET_ENV).ok().as_deref(),
        )
    }

    /// A half-set cookie pair is an error, as it is for `listen`; it never falls back silently.
    pub fn new(
        auth: Option<&str>,
        csrf: Option<&str>,
        webhook_endpoint: Option<&str>,
        webhook_secret: Option<&str>,
    ) -> Result<Self> {
        Ok(Self {
            twitter: config::env_twitter_with(DEFAULT_ACCOUNT_ID, auth, csrf)?,
            webhook_endpoint: webhook_endpoint.map(String::from),
            webhook_secret: webhook_secret.map(String::from),
        })
    }

    fn twitter_for(&self, id: &str) -> Option<&TwitterConfig> {
        self.twitter.as_ref().filter(|_| id == DEFAULT_ACCOUNT_ID)
    }

    /// Whether the cookies of `id` come from the environment, which also makes the account exist.
    fn defines_account(&self, id: &str) -> bool {
        self.twitter_for(id).is_some()
    }

    /// The cookies in effect: the environment's for `default`, otherwise the stored ones.
    fn twitter(&self, id: &str, stored: &TwitterConfig) -> TwitterConfig {
        self.twitter_for(id).unwrap_or(stored).clone()
    }

    fn webhook(&self, id: &str, stored: Option<&str>) -> (Option<String>, WebhookSource) {
        config::resolve_webhook_with(id, self.webhook_endpoint.as_deref(), stored)
    }

    fn webhook_secret(&self, id: &str, stored: Option<&str>) -> (Option<String>, WebhookSource) {
        config::resolve_webhook_secret_with(id, self.webhook_secret.as_deref(), stored)
    }
}

// ---- DTOs -----------------------------------------------------------------

#[derive(Debug, Serialize, PartialEq)]
pub struct SettingsDto {
    pub configured: bool,
    pub auth_token_set: bool,
    pub ct0_set: bool,
    /// The secret is write-only: only whether one is in effect is ever reported.
    pub webhook_secret_set: bool,
    pub webhook_endpoint: Option<String>,
    pub webhook_source: WebhookSource,
    pub credentials_locked: bool,
    pub notification_filter: NotificationFilter,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct RegistrationDto {
    pub registered: bool,
    pub uaid: Option<String>,
    pub channel_id: Option<String>,
}

/// Outcome of checking the saved cookies against X; `message` is always a fixed string.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct CredentialCheckDto {
    pub valid: bool,
    pub screen_name: Option<String>,
    pub message: &'static str,
}

const MESSAGE_CREDENTIALS_VALID: &str = "Cookieを確認しました。";
const MESSAGE_CREDENTIALS_INVALID: &str = "Cookieが無効か期限切れです。";

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StateName {
    Unconfigured,
    Unregistered,
    Stopped,
    Connecting,
    Connected,
    Retrying,
    Registering,
    Unregistering,
    Error,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct StatusDto {
    pub state: StateName,
    pub registered: bool,
    #[serde(flatten)]
    pub counters: Counters,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct AccountDto {
    pub id: String,
    pub registered: bool,
    pub receiver_state: StateName,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct AccountListDto {
    pub accounts: Vec<AccountDto>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsUpdate {
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default)]
    pub ct0: Option<String>,
    /// Absent, `null` and blank all keep the saved secret.
    #[serde(default)]
    pub webhook_secret: Option<String>,
    pub webhook_endpoint: String,
    /// 省略時は保存済みの設定を保持する。
    #[serde(default)]
    pub notification_filter: Option<NotificationFilterInput>,
}

/// 更新では全区分の指定を必須にし、部分指定で他の区分が黙って有効に戻るのを防ぐ。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationFilterInput {
    pub tweet: bool,
    pub retweet: bool,
    pub quote: bool,
    pub quote_self: bool,
    pub reply: bool,
    pub xchat_message_received: bool,
    pub other: bool,
}

impl From<NotificationFilterInput> for NotificationFilter {
    fn from(input: NotificationFilterInput) -> Self {
        Self {
            tweet: input.tweet,
            retweet: input.retweet,
            quote: input.quote,
            quote_self: input.quote_self,
            reply: input.reply,
            xchat_message_received: input.xchat_message_received,
            other: input.other,
        }
    }
}

fn has_credentials(twitter: &TwitterConfig) -> bool {
    !twitter.auth_token.is_empty() && !twitter.ct0.is_empty()
}

fn settings_dto(env: &Environment, account: &Account) -> SettingsDto {
    let twitter = env.twitter(&account.id, &account.twitter);
    let (webhook_endpoint, webhook_source) =
        env.webhook(&account.id, account.webhook_endpoint.as_deref());
    let (secret, _) = env.webhook_secret(&account.id, account.webhook_secret.as_deref());
    SettingsDto {
        configured: has_credentials(&twitter),
        auth_token_set: !twitter.auth_token.is_empty(),
        ct0_set: !twitter.ct0.is_empty(),
        webhook_secret_set: secret.is_some(),
        webhook_endpoint,
        webhook_source,
        credentials_locked: account.registration.is_some(),
        notification_filter: account.notification_filter,
    }
}

fn no_registration() -> RegistrationDto {
    RegistrationDto {
        registered: false,
        uaid: None,
        channel_id: None,
    }
}

fn registration_dto(account: &Account) -> RegistrationDto {
    let registration = account.registration.as_ref();
    RegistrationDto {
        registered: registration.is_some(),
        uaid: registration.map(|r| r.autopush.uaid.clone()),
        channel_id: registration.map(|r| r.autopush.channel_id.clone()),
    }
}

/// The stored account, or the empty account an environment-defined `default` stands for.
fn load_account(
    path: &Path,
    id: &str,
    env: &Environment,
) -> std::result::Result<Account, ApiError> {
    match Config::load_optional(path)?.and_then(|c| c.account(id).cloned()) {
        Some(account) => Ok(account),
        None if env.defines_account(id) => Ok(Account::new(id)),
        None => Err(ApiError::account_not_found()),
    }
}

// ---- validation -------------------------------------------------------------

/// Blank/absent credentials mean "retain the saved value".
fn validate_credential(
    name: &str,
    value: Option<String>,
) -> std::result::Result<Option<String>, ApiError> {
    let Some(value) = value else { return Ok(None) };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > MAX_CREDENTIAL_LEN
        || value
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == ';')
    {
        return Err(ApiError::invalid(
            "invalid_credential",
            format!("{name} has an invalid format."),
        ));
    }
    Ok(Some(value.to_string()))
}

/// Blank/absent keeps the saved secret. A real value is kept exactly as written, because the
/// webhook signature is computed over it.
fn validate_webhook_secret(value: Option<String>) -> std::result::Result<Option<String>, ApiError> {
    let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
        return Ok(None);
    };
    if value.len() > MAX_WEBHOOK_SECRET_LEN || value.chars().any(|c| c.is_control()) {
        return Err(ApiError::invalid(
            "invalid_webhook_secret",
            "The webhook secret has an invalid format.",
        ));
    }
    Ok(Some(value))
}

fn validate_webhook(value: &str) -> std::result::Result<Option<String>, ApiError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let invalid = || {
        ApiError::invalid(
            "invalid_webhook_endpoint",
            "The webhook URL must be a valid http(s) URL.",
        )
    };
    if value.len() > MAX_URL_LEN || value.chars().any(|c| c.is_control()) {
        return Err(invalid());
    }
    let url = reqwest::Url::parse(value).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(invalid());
    }
    Ok(Some(value.to_string()))
}

fn validate_id(id: &str) -> std::result::Result<(), ApiError> {
    config::validate_account_id(id).map_err(|_| ApiError::invalid_account_id())
}

fn history_error(error: AngelicAngelError) -> ApiError {
    match &error {
        AngelicAngelError::Config(message) if message.starts_with("unknown notification") => {
            ApiError::invalid("invalid_cursor", "The cursor does not match any entry.")
        },
        _ => {
            tracing::warn!(error = %error, "notification history could not be read");
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "history_unavailable",
                "The notification history could not be read.",
            )
        },
    }
}

/// The history database of one account and the SQLite side files that belong to it.
fn history_files(db: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    ["", "-journal", "-wal", "-shm"].into_iter().map(|suffix| {
        let mut name = db.as_os_str().to_os_string();
        name.push(suffix);
        PathBuf::from(name)
    })
}

/// Deletes one history database and the SQLite side files that belong to it. Every file is
/// attempted and a missing file is fine; the first real failure is returned.
fn remove_history_files(db: &Path) -> std::io::Result<()> {
    history_files(db)
        .map(|file| match std::fs::remove_file(file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect()
}

/// Whether any file of the history of `db` exists (a dangling symlink or a directory counts). The
/// files are only looked at, never removed: they may be the only copy of someone's history.
fn history_files_remain(db: &Path) -> std::io::Result<bool> {
    for file in history_files(db) {
        match std::fs::symlink_metadata(&file) {
            Ok(_) => return Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

// ---- service ----------------------------------------------------------------

struct OperationGuard<'a>(&'a Telemetry);

impl<'a> OperationGuard<'a> {
    fn new(telemetry: &'a Telemetry, operation: Operation) -> Self {
        telemetry.set_operation(Some(operation));
        Self(telemetry)
    }
}

impl Drop for OperationGuard<'_> {
    fn drop(&mut self) {
        self.0.set_operation(None);
    }
}

/// One account: its receiver, telemetry, bulk job and the lock that serializes its operations.
/// Nothing is shared with another account except the backend.
pub struct Service<B: Backend> {
    id: String,
    backend: Arc<B>,
    config_path: PathBuf,
    env: Arc<Environment>,
    telemetry: Arc<Telemetry>,
    remote_timeout: Duration,
    /// Serializes mutations and owns the receiver task handle.
    receiver: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    /// Bulk notification job; independent of `receiver`, so it never stops the receiver.
    following: FollowingJobs,
    /// Readers of the history hold it shared; a deletion takes it exclusively before removing the
    /// files, so no read can open (and so re-create) the database after it was deleted.
    history: tokio::sync::RwLock<()>,
    /// Set by a deletion once the config entry is gone. A handle resolved earlier then answers
    /// 404 instead of writing the account or its history back.
    removed: AtomicBool,
    /// Lets a test hold a deletion between "marked removed" and "history files deleted".
    #[cfg(test)]
    cleanup_gate: tokio::sync::Mutex<()>,
}

impl<B: Backend> Service<B> {
    fn new(id: &str, accounts: &Accounts<B>) -> Self {
        Self {
            id: id.to_string(),
            backend: Arc::clone(&accounts.backend),
            config_path: accounts.config_path.clone(),
            env: Arc::clone(&accounts.env),
            telemetry: Arc::new(Telemetry::default()),
            remote_timeout: accounts.remote_timeout,
            receiver: tokio::sync::Mutex::new(None),
            following: FollowingJobs::new(accounts.following_timings),
            history: tokio::sync::RwLock::new(()),
            removed: AtomicBool::new(false),
            #[cfg(test)]
            cleanup_gate: tokio::sync::Mutex::new(()),
        }
    }

    fn is_removed(&self) -> bool {
        self.removed.load(Ordering::SeqCst)
    }

    fn ensure_live(&self) -> std::result::Result<(), ApiError> {
        if self.is_removed() {
            return Err(ApiError::account_not_found());
        }
        Ok(())
    }

    /// Machine operations hold the same lock as credential updates and deletion until dispatch
    /// and result persistence finish. A previously resolved, deleted handle is checked again.
    pub(crate) async fn machine_lock(
        &self,
    ) -> std::result::Result<tokio::sync::MutexGuard<'_, Option<JoinHandle<()>>>, ApiError> {
        let guard = self.receiver.lock().await;
        self.ensure_live()?;
        Ok(guard)
    }

    pub(crate) fn machine_credentials(
        &self,
    ) -> std::result::Result<(TwitterConfig, String), ApiError> {
        let mut account = self.load()?;
        if self.env.defines_account(&self.id)
            && Config::load_optional(&self.config_path)?
                .is_none_or(|c| c.account(&self.id).is_none())
        {
            account.credential_revision.clear();
        }
        Ok((
            self.env.twitter(&self.id, &account.twitter),
            account.credential_revision,
        ))
    }

    fn load(&self) -> std::result::Result<Account, ApiError> {
        self.ensure_live()?;
        load_account(&self.config_path, &self.id, &self.env)
    }

    /// Starts the receiver when a registration is already present.
    async fn startup(&self) {
        let mut slot = self.receiver.lock().await;
        match self.load() {
            Ok(Account {
                registration: Some(registration),
                ..
            }) => self.start_receiver(&mut slot, registration).await,
            Ok(_) => {},
            Err(error) if error.is_account_not_found() => {},
            Err(_) => {
                tracing::warn!(account = %self.id, "configuration could not be read at startup");
            },
        }
    }

    /// Stops and joins the bulk job and the owned receiver.
    pub async fn shutdown(&self) {
        self.following.shutdown().await;
        let mut slot = self.receiver.lock().await;
        self.stop_receiver(&mut slot).await;
    }

    async fn stop_receiver(&self, slot: &mut Option<JoinHandle<()>>) {
        if let Some(handle) = slot.take() {
            handle.abort();
            let _ = handle.await;
        }
        self.telemetry.set_stopped();
    }

    async fn start_receiver(&self, slot: &mut Option<JoinHandle<()>>, registration: Registration) {
        self.stop_receiver(slot).await;
        self.telemetry.set_connecting();
        let backend = Arc::clone(&self.backend);
        let telemetry = Arc::clone(&self.telemetry);
        let config_path = self.config_path.clone();
        let account_id = self.id.clone();
        *slot = Some(tokio::spawn(async move {
            let result = backend
                .run_receiver(
                    account_id,
                    registration,
                    config_path,
                    Arc::clone(&telemetry),
                )
                .await;
            match result {
                Ok(()) => telemetry.set_stopped(),
                Err(_) => {
                    if telemetry.snapshot().0 != ReceiverState::Error {
                        telemetry.set_error("receiver stopped unexpectedly");
                    }
                },
            }
        }));
    }

    async fn remote<T>(
        &self,
        what: &str,
        fut: impl Future<Output = Result<T>>,
    ) -> std::result::Result<T, ApiError> {
        match tokio::time::timeout(self.remote_timeout, fut).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => {
                tracing::warn!(account = %self.id, error = %error, "{what} failed");
                Err(ApiError::provider(format!("{what} failed.")))
            },
            Err(_) => {
                tracing::warn!(account = %self.id, "{what} timed out");
                Err(ApiError::provider(format!("{what} timed out.")))
            },
        }
    }

    // ---- reads ----

    pub fn settings(&self) -> std::result::Result<SettingsDto, ApiError> {
        Ok(settings_dto(&self.env, &self.load()?))
    }

    pub fn registration(&self) -> std::result::Result<RegistrationDto, ApiError> {
        Ok(registration_dto(&self.load()?))
    }

    pub fn status(&self) -> std::result::Result<StatusDto, ApiError> {
        match self.load() {
            Ok(account) => Ok(self.status_of(&account)),
            Err(error) if error.is_account_not_found() => Err(error),
            Err(_) => {
                let (_, _, mut counters) = self.telemetry.snapshot();
                counters.last_error = Some("configuration is invalid".to_string());
                Ok(StatusDto {
                    state: StateName::Error,
                    registered: false,
                    counters,
                })
            },
        }
    }

    fn status_of(&self, account: &Account) -> StatusDto {
        let (receiver, operation, counters) = self.telemetry.snapshot();
        let registered = account.registration.is_some();
        let state = match operation {
            Some(Operation::Registering) => StateName::Registering,
            Some(Operation::Unregistering) => StateName::Unregistering,
            None if !registered => {
                if has_credentials(&self.env.twitter(&self.id, &account.twitter)) {
                    StateName::Unregistered
                } else {
                    StateName::Unconfigured
                }
            },
            None => match receiver {
                ReceiverState::Stopped => StateName::Stopped,
                ReceiverState::Connecting => StateName::Connecting,
                ReceiverState::Connected => StateName::Connected,
                ReceiverState::Retrying => StateName::Retrying,
                ReceiverState::Error => StateName::Error,
            },
        };
        StatusDto {
            state,
            registered,
            counters,
        }
    }

    fn summary(&self, account: &Account) -> AccountDto {
        let status = self.status_of(account);
        AccountDto {
            id: self.id.clone(),
            registered: status.registered,
            receiver_state: status.state,
        }
    }

    pub fn following_notifications(&self) -> FollowingNotificationsDto {
        self.following.snapshot()
    }

    /// One page of the local notification history; file access stays off the async threads.
    /// The saved filter is read inside the same blocking task as the history query.
    pub async fn notifications(
        &self,
        before: Option<String>,
        limit: usize,
    ) -> std::result::Result<NotificationPage, ApiError> {
        let _history = self.history.read().await;
        self.ensure_live()?;
        let path = self.config_path.clone();
        let id = self.id.clone();
        let env = Arc::clone(&self.env);
        tokio::task::spawn_blocking(move || {
            let account = load_account(&path, &id, &env)?;
            notifications::list_filtered_for(
                &path,
                &id,
                before.as_deref(),
                limit,
                &account.notification_filter,
            )
            .map_err(history_error)
        })
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "The notification history could not be read.",
            )
        })?
    }

    // ---- mutations ----

    /// Starts the bulk job. Holds the mutation lock only while validating and spawning.
    pub fn start_following_notifications(
        &self,
    ) -> std::result::Result<FollowingNotificationsDto, ApiError> {
        let _slot = self.receiver.try_lock().map_err(|_| ApiError::busy())?;
        let twitter = self.saved_credentials()?;
        self.following.start(Arc::clone(&self.backend), twitter)
    }

    /// The X cookies in effect (the environment's for `default`, else the saved ones), or a 400
    /// when either one is missing.
    fn saved_credentials(&self) -> std::result::Result<TwitterConfig, ApiError> {
        let account = self.load()?;
        let twitter = self.env.twitter(&self.id, &account.twitter);
        if has_credentials(&twitter) {
            Ok(twitter)
        } else {
            Err(ApiError::invalid(
                "not_configured",
                "Save the credentials in settings first.",
            ))
        }
    }

    /// Mutations other than a webhook edit wait for the bulk job so they never overlap it.
    fn ensure_no_following_job(&self) -> std::result::Result<(), ApiError> {
        if self.following.is_active() {
            return Err(ApiError::conflict(
                "following_job_active",
                "Wait for the bulk notification job to finish or cancel it first.",
            ));
        }
        Ok(())
    }

    /// Asks X who the saved cookies belong to (read-only). Only 401/403 mean "invalid";
    /// every other failure is reported as a provider error so it is never mistaken for one.
    pub async fn validate_credentials(&self) -> std::result::Result<CredentialCheckDto, ApiError> {
        let _slot = self.receiver.try_lock().map_err(|_| ApiError::busy())?;
        self.ensure_no_following_job()?;
        let twitter = self.saved_credentials()?;

        let outcome =
            tokio::time::timeout(self.remote_timeout, self.backend.screen_name(&twitter)).await;
        match outcome {
            Ok(Ok(screen_name)) => Ok(CredentialCheckDto {
                valid: true,
                screen_name: Some(screen_name),
                message: MESSAGE_CREDENTIALS_VALID,
            }),
            Ok(Err(XError::Unauthorized | XError::Forbidden)) => Ok(CredentialCheckDto {
                valid: false,
                screen_name: None,
                message: MESSAGE_CREDENTIALS_INVALID,
            }),
            Ok(Err(error)) => {
                tracing::warn!(account = %self.id, error = %error, "cookie check failed");
                Err(ApiError::provider(format!(
                    "The cookies could not be checked. {error}"
                )))
            },
            Err(_) => {
                tracing::warn!(account = %self.id, "cookie check timed out");
                Err(ApiError::provider(
                    "The cookies could not be checked. X did not respond in time.",
                ))
            },
        }
    }

    /// Returns whether a cancellation was requested, and the resulting snapshot.
    pub fn cancel_following_notifications(&self) -> (bool, FollowingNotificationsDto) {
        self.following.cancel()
    }

    pub async fn update_settings(
        &self,
        update: SettingsUpdate,
    ) -> std::result::Result<SettingsDto, ApiError> {
        let endpoint_from_env = self.env.webhook(&self.id, None).1 == WebhookSource::Environment;
        let secret_from_env =
            self.env.webhook_secret(&self.id, None).1 == WebhookSource::Environment;
        let credentials_from_env = self.env.defines_account(&self.id);
        let auth_token = validate_credential("auth_token", update.auth_token)?;
        let ct0 = validate_credential("ct0", update.ct0)?;
        let webhook_secret = validate_webhook_secret(update.webhook_secret)?;
        let webhook = if endpoint_from_env {
            None
        } else {
            validate_webhook(&update.webhook_endpoint)?
        };
        if credentials_from_env && (auth_token.is_some() || ct0.is_some()) {
            return Err(ApiError::conflict(
                "credentials_from_environment",
                "The cookies come from environment variables and cannot be changed here.",
            ));
        }
        if secret_from_env && webhook_secret.is_some() {
            return Err(ApiError::conflict(
                "webhook_secret_from_environment",
                "The webhook secret comes from an environment variable and cannot be changed here.",
            ));
        }

        let _slot = self.receiver.try_lock().map_err(|_| ApiError::busy())?;
        self.ensure_live()?;

        let saved = Config::modify_or_create_account(&self.config_path, &self.id, |current| {
            let mut account = match current {
                None if credentials_from_env => Account::new(&self.id),
                None => return Err(ApiError::account_not_found()),
                Some(account) => account,
            };
            let twitter = TwitterConfig {
                auth_token: auth_token.unwrap_or_else(|| account.twitter.auth_token.clone()),
                ct0: ct0.unwrap_or_else(|| account.twitter.ct0.clone()),
            };
            if !credentials_from_env && !has_credentials(&twitter) {
                return Err(ApiError::invalid(
                    "credentials_required",
                    "Both auth_token and ct0 are required for the first setup.",
                ));
            }
            let changed = twitter.auth_token != account.twitter.auth_token
                || twitter.ct0 != account.twitter.ct0;
            if changed && account.registration.is_some() {
                return Err(ApiError::conflict(
                    "credentials_locked",
                    "Unregister before changing the saved credentials.",
                ));
            }
            if changed && self.following.is_active() {
                return Err(ApiError::conflict(
                    "following_job_active",
                    "Wait for the bulk notification job to finish before changing the credentials.",
                ));
            }
            account.twitter = twitter;
            if !endpoint_from_env {
                account.webhook_endpoint = webhook;
            }
            if let Some(secret) = webhook_secret {
                account.webhook_secret = Some(secret);
            }
            if let Some(filter) = update.notification_filter {
                account.notification_filter = filter.into();
            }
            Ok((Some(account.clone()), account))
        })?;
        Ok(settings_dto(&self.env, &saved))
    }

    pub async fn register(&self) -> std::result::Result<RegistrationDto, ApiError> {
        let mut slot = self.receiver.try_lock().map_err(|_| ApiError::busy())?;
        self.ensure_no_following_job()?;
        let account = self.load()?;

        if let Some(registration) = account.registration.clone() {
            let running = slot.as_ref().is_some_and(|h| !h.is_finished());
            if !running {
                self.start_receiver(&mut slot, registration).await;
            }
            return Ok(registration_dto(&account));
        }

        let twitter = self.env.twitter(&self.id, &account.twitter);
        if !has_credentials(&twitter) {
            return Err(ApiError::conflict(
                "not_configured",
                "Save the credentials in settings first.",
            ));
        }

        let _operation = OperationGuard::new(&self.telemetry, Operation::Registering);
        self.stop_receiver(&mut slot).await;

        let subscription = self
            .remote("AutoPush registration", self.backend.subscribe())
            .await?;
        if let Err(error) = self
            .remote(
                "X push registration",
                self.backend.twitter_register(&twitter, &subscription),
            )
            .await
        {
            self.cleanup_subscription(&subscription.autopush).await;
            return Err(error);
        }

        let registration = Registration {
            endpoint: subscription.endpoint.clone(),
            autopush: subscription.autopush.clone(),
            keys: subscription.keys.clone(),
        };
        let implicit = self.env.defines_account(&self.id);
        let saved = Config::modify_or_create_account(&self.config_path, &self.id, |current| {
            let mut account = match current {
                None if implicit => Account::new(&self.id),
                None => return Err(ApiError::account_not_found()),
                Some(account) => account,
            };
            account.registration = Some(registration.clone());
            Ok((Some(account.clone()), account))
        });
        let saved = match saved {
            Ok(saved) => saved,
            Err(error) => {
                self.cleanup_subscription(&subscription.autopush).await;
                return Err(error);
            },
        };

        self.start_receiver(&mut slot, registration).await;
        Ok(registration_dto(&saved))
    }

    /// Best-effort removal of a subscription created by a failed registration.
    async fn cleanup_subscription(&self, session: &AutoPushSession) {
        let _ = self
            .remote(
                "AutoPush cleanup",
                self.backend.autopush_unregister(session),
            )
            .await;
    }

    pub async fn unregister(&self) -> std::result::Result<RegistrationDto, ApiError> {
        let mut slot = self.receiver.try_lock().map_err(|_| ApiError::busy())?;
        self.ensure_no_following_job()?;
        let account = self.load()?;
        let Some(registration) = account.registration.clone() else {
            self.stop_receiver(&mut slot).await;
            return Ok(registration_dto(&account));
        };

        let _operation = OperationGuard::new(&self.telemetry, Operation::Unregistering);
        self.stop_receiver(&mut slot).await;

        if let Err(error) = self
            .remote(
                "AutoPush unregistration",
                self.backend.autopush_unregister(&registration.autopush),
            )
            .await
        {
            self.start_receiver(&mut slot, registration).await;
            return Err(error);
        }

        let saved = Config::modify_account(&self.config_path, &self.id, |current| {
            Ok::<_, ApiError>(match current {
                Some(mut account) => {
                    account.registration = None;
                    (Some(account.clone()), Some(account))
                },
                None => (None, None),
            })
        });
        match saved {
            Ok(saved) => Ok(saved
                .as_ref()
                .map_or_else(no_registration, registration_dto)),
            Err(error) => {
                self.start_receiver(&mut slot, registration).await;
                Err(error)
            },
        }
    }

    /// Removes the account: cancels its bulk job, unregisters it when registered (a failure keeps
    /// the account and restores its receiver), stops the receiver, drops its config entry and
    /// deletes its history database. Other accounts are untouched.
    ///
    /// The mutation lock is taken without waiting and held until the history files are gone, so a
    /// deletion excludes every other operation on this account (including re-creating the id), and
    /// `removed` makes any handle resolved earlier fail afterwards.
    async fn delete(&self) -> std::result::Result<(), ApiError> {
        let mut slot = self.receiver.try_lock().map_err(|_| ApiError::busy())?;
        self.ensure_live()?;
        if self.env.defines_account(&self.id) {
            return Err(ApiError::conflict(
                "account_from_environment",
                "This account is defined by environment variables; unset them to remove it.",
            ));
        }
        let account = self.load()?;

        self.following.shutdown().await;

        let registration = account.registration;
        let _operation = registration
            .as_ref()
            .map(|_| OperationGuard::new(&self.telemetry, Operation::Unregistering));
        self.stop_receiver(&mut slot).await;
        if let Some(registration) = &registration
            && let Err(error) = self
                .remote(
                    "AutoPush unregistration",
                    self.backend.autopush_unregister(&registration.autopush),
                )
                .await
        {
            self.start_receiver(&mut slot, registration.clone()).await;
            return Err(error);
        }

        let id = self.id.as_str();
        let removal = Config::modify(&self.config_path, |current: Option<Config>| {
            Ok::<_, ApiError>(match current {
                Some(mut config) if config.account(id).is_some() => {
                    config.accounts.retain(|a| a.id != id);
                    (Some(config), ())
                },
                _ => (None, ()),
            })
        });
        if let Err(error) = removal {
            if let Some(registration) = registration {
                self.start_receiver(&mut slot, registration).await;
            }
            return Err(error);
        }

        let _history = self.history.write().await;
        self.removed.store(true, Ordering::SeqCst);
        #[cfg(test)]
        drop(self.cleanup_gate.lock().await);
        let db = notifications::history_path_for(&self.config_path, &self.id);
        let cleaned = tokio::task::spawn_blocking(move || remove_history_files(&db)).await;
        match cleaned {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                tracing::warn!(account = %self.id, error = %error, "history cleanup failed");
                Err(history_cleanup_failed())
            },
            Err(error) => {
                tracing::warn!(account = %self.id, error = %error, "history cleanup failed");
                Err(history_cleanup_failed())
            },
        }
    }
}

fn history_cleanup_failed() -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "history_cleanup_failed",
        "The account was removed, but its notification history could not be deleted.",
    )
}

// ---- registry ---------------------------------------------------------------

/// All accounts of one config file. A `Service` is created the first time an account is used and
/// kept in a map that is only locked briefly: never across file or network I/O.
pub struct Accounts<B: Backend> {
    backend: Arc<B>,
    config_path: PathBuf,
    env: Arc<Environment>,
    remote_timeout: Duration,
    following_timings: Timings,
    services: RwLock<HashMap<String, Arc<Service<B>>>>,
}

impl<B: Backend> Accounts<B> {
    pub fn new(backend: B, config_path: PathBuf, env: Environment) -> Self {
        Self {
            backend: Arc::new(backend),
            config_path,
            env: Arc::new(env),
            remote_timeout: REMOTE_TIMEOUT,
            following_timings: Timings::default(),
            services: RwLock::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.remote_timeout = timeout;
        self
    }

    #[cfg(test)]
    pub fn with_following_timings(mut self, timings: Timings) -> Self {
        self.following_timings = timings;
        self
    }

    fn live(&self, id: &str) -> Option<Arc<Service<B>>> {
        let services = self.services.read().unwrap_or_else(|e| e.into_inner());
        services.get(id).filter(|s| !s.is_removed()).cloned()
    }

    /// The service of `id`, created on first use; a deleted one is replaced.
    fn service_for(&self, id: &str) -> Arc<Service<B>> {
        let mut services = self.services.write().unwrap_or_else(|e| e.into_inner());
        if let Some(service) = services.get(id).filter(|s| !s.is_removed()) {
            return Arc::clone(service);
        }
        let service = Arc::new(Service::new(id, self));
        services.insert(id.to_string(), Arc::clone(&service));
        service
    }

    /// The service for `id`: 400 for a malformed id, 404 for an unknown one.
    pub fn get(&self, id: &str) -> std::result::Result<Arc<Service<B>>, ApiError> {
        validate_id(id)?;
        if let Some(service) = self.live(id) {
            return Ok(service);
        }
        let stored = Config::load_optional(&self.config_path)?
            .is_some_and(|config| config.account(id).is_some());
        if stored || self.env.defines_account(id) {
            Ok(self.service_for(id))
        } else {
            Err(ApiError::account_not_found())
        }
    }

    pub fn list(&self) -> std::result::Result<AccountListDto, ApiError> {
        let config = Config::load_optional(&self.config_path)?.unwrap_or_default();
        let implicit = (config.account(DEFAULT_ACCOUNT_ID).is_none()
            && self.env.defines_account(DEFAULT_ACCOUNT_ID))
        .then(|| Account::new(DEFAULT_ACCOUNT_ID));
        let accounts = config
            .accounts
            .iter()
            .chain(&implicit)
            .map(|account| self.service_for(&account.id).summary(account))
            .collect();
        Ok(AccountListDto { accounts })
    }

    /// Creates an account with no cookies yet: 400 for a malformed id, 409 for an existing one or
    /// when history files of that id are still on disk.
    pub fn create(&self, id: &str) -> std::result::Result<AccountDto, ApiError> {
        validate_id(id)?;
        if self.env.defines_account(id) {
            return Err(ApiError::account_exists());
        }
        // The service of this id, deleted or not, is still registered while a deletion removes its
        // history files, and it keeps its lock until they are gone. Taking that lock (whatever the
        // `removed` flag says) turns a re-creation during the deletion into a 409 instead of letting
        // the old cleanup delete the new account's files.
        let current = self
            .services
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned();
        let _idle = current
            .as_ref()
            .map(|s| s.receiver.try_lock().map_err(|_| ApiError::busy()))
            .transpose()?;
        let db = notifications::history_path_for(&self.config_path, id);
        let account = Config::modify_or_create_account(&self.config_path, id, |existing| {
            if existing.is_some() {
                return Err(ApiError::account_exists());
            }
            // A history left behind (a failed cleanup after a deletion, or the file of an older
            // single-account setup) would be shown as this new account's own. Refuse instead of
            // deleting files nobody asked to delete. Checked under the config lock, with nothing
            // written yet, so a refusal leaves the config as it was.
            if history_files_remain(&db).map_err(AngelicAngelError::from)? {
                tracing::warn!(
                    account = %id,
                    history = %db.display(),
                    "refusing to create an account over existing notification history files"
                );
                return Err(ApiError::history_cleanup_required());
            }
            let account = Account::new(id);
            Ok((Some(account.clone()), account))
        })?;
        Ok(self.service_for(id).summary(&account))
    }

    /// Deletes the account and its history; see [`Service::delete`]. The service leaves the
    /// registry once the deletion has taken effect, even when only the history cleanup failed.
    pub async fn delete(&self, id: &str) -> std::result::Result<(), ApiError> {
        let service = self.get(id)?;
        let outcome = service.delete().await;
        self.forget_removed(id, &service);
        outcome
    }

    /// Drops a deleted service from the registry, unless the id has been created again since: the
    /// registry then already holds the new service, which must stay.
    fn forget_removed(&self, id: &str, service: &Arc<Service<B>>) {
        if !service.is_removed() {
            return;
        }
        let mut services = self.services.write().unwrap_or_else(|e| e.into_inner());
        if services.get(id).is_some_and(|s| Arc::ptr_eq(s, service)) {
            services.remove(id);
        }
    }

    /// Starts the receiver of every registered account.
    pub async fn startup(&self) {
        let config = match Config::load_optional(&self.config_path) {
            Ok(config) => config.unwrap_or_default(),
            Err(_) => {
                tracing::warn!("configuration could not be read at startup");
                return;
            },
        };
        for account in &config.accounts {
            self.service_for(&account.id).startup().await;
        }
    }

    /// Stops the bulk jobs and receivers of all accounts, in parallel.
    pub async fn shutdown(&self) {
        let services = self
            .services
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut tasks = JoinSet::new();
        for service in services {
            tasks.spawn(async move { service.shutdown().await });
        }
        while tasks.join_next().await.is_some() {}
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    pub struct FakeBackend {
        /// Fails `screen_name` with this error.
        pub screen_error: Mutex<Option<XError>>,
        /// Scripted `following_page` results by cursor; a missing cursor is a malformed reply.
        pub pages: Mutex<HashMap<i64, std::result::Result<FollowingPage, XError>>>,
        /// Scripted per-account results; accounts not listed succeed.
        pub update_results: Mutex<HashMap<String, XError>>,
        /// Accounts that were updated, in call order.
        pub updated: Mutex<Vec<String>>,
        pub page_calls: Mutex<Vec<i64>>,
        pub hang_pages: AtomicBool,
        pub hang_screen_name: AtomicBool,
        /// auth_token values `screen_name` was called with.
        pub screen_name_creds: Mutex<Vec<String>>,
        /// Accounts whose update never completes.
        pub hang_updates: Mutex<Vec<String>>,
        /// Concurrent update calls in flight (must never exceed one).
        pub updates_in_flight: Arc<AtomicUsize>,
        pub max_updates_in_flight: AtomicUsize,
        pub fail_subscribe: AtomicBool,
        pub fail_twitter: AtomicBool,
        pub fail_unregister: AtomicBool,
        /// Milliseconds `autopush_unregister` takes before it answers.
        pub unregister_delay_ms: AtomicUsize,
        pub hang_twitter: AtomicBool,
        /// auth_token values `twitter_register` was called with, in call order.
        pub registered_with: Mutex<Vec<String>>,
        pub subscribes: AtomicUsize,
        pub unregisters: AtomicUsize,
        /// Receivers currently alive (must never exceed one).
        pub active_receivers: Arc<AtomicUsize>,
        pub max_active: Arc<AtomicUsize>,
        pub started: AtomicUsize,
        /// Receivers currently alive, by account id.
        pub active_by_account: Arc<Mutex<HashMap<String, usize>>>,
        /// Account ids a receiver was started for, in start order.
        pub started_accounts: Mutex<Vec<String>>,
        pub events: Mutex<Vec<String>>,
    }

    struct ActiveGuard(Arc<AtomicUsize>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    struct AccountGuard(Arc<Mutex<HashMap<String, usize>>>, String);
    impl Drop for AccountGuard {
        fn drop(&mut self) {
            if let Some(n) = self.0.lock().unwrap().get_mut(&self.1) {
                *n -= 1;
            }
        }
    }

    impl FakeBackend {
        pub fn active_for(&self, account_id: &str) -> usize {
            self.active_by_account
                .lock()
                .unwrap()
                .get(account_id)
                .copied()
                .unwrap_or(0)
        }
    }

    fn fail(msg: &str) -> crate::error::AngelicAngelError {
        crate::error::AngelicAngelError::TwitterApi(format!("SECRET-DETAIL {msg}"))
    }

    impl Backend for Arc<FakeBackend> {
        async fn subscribe(&self) -> Result<PushSubscription> {
            let n = self.subscribes.fetch_add(1, Ordering::SeqCst);
            if self.fail_subscribe.load(Ordering::SeqCst) {
                return Err(fail("subscribe"));
            }
            Ok(PushSubscription {
                endpoint: format!("https://push.example/{n}"),
                autopush: AutoPushSession {
                    uaid: format!("uaid-{n}"),
                    channel_id: format!("chan-{n}"),
                },
                keys: crate::push::generate_keys(),
            })
        }

        async fn twitter_register(
            &self,
            twitter: &TwitterConfig,
            _: &PushSubscription,
        ) -> Result<()> {
            self.registered_with
                .lock()
                .unwrap()
                .push(twitter.auth_token.clone());
            if self.hang_twitter.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if self.fail_twitter.load(Ordering::SeqCst) {
                return Err(fail("twitter"));
            }
            Ok(())
        }

        async fn autopush_unregister(&self, session: &AutoPushSession) -> Result<()> {
            self.unregisters.fetch_add(1, Ordering::SeqCst);
            self.events
                .lock()
                .unwrap()
                .push(format!("unregister:{}", session.uaid));
            let delay = self.unregister_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay as u64)).await;
            }
            if self.fail_unregister.load(Ordering::SeqCst) {
                return Err(fail("unregister"));
            }
            Ok(())
        }

        async fn run_receiver(
            &self,
            account_id: String,
            _: Registration,
            _: PathBuf,
            telemetry: Arc<Telemetry>,
        ) -> Result<()> {
            self.started.fetch_add(1, Ordering::SeqCst);
            self.started_accounts
                .lock()
                .unwrap()
                .push(account_id.clone());
            *self
                .active_by_account
                .lock()
                .unwrap()
                .entry(account_id.clone())
                .or_default() += 1;
            let _per_account = AccountGuard(Arc::clone(&self.active_by_account), account_id);
            let now = self.active_receivers.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(now, Ordering::SeqCst);
            let _guard = ActiveGuard(Arc::clone(&self.active_receivers));
            telemetry.set_connected();
            std::future::pending::<()>().await;
            Ok(())
        }

        async fn screen_name(
            &self,
            twitter: &TwitterConfig,
        ) -> std::result::Result<String, XError> {
            self.screen_name_creds
                .lock()
                .unwrap()
                .push(twitter.auth_token.clone());
            if self.hang_screen_name.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            match *self.screen_error.lock().unwrap() {
                Some(error) => Err(error),
                None => Ok("me".to_string()),
            }
        }

        async fn following_page(
            &self,
            _: &TwitterConfig,
            screen_name: &str,
            cursor: i64,
        ) -> std::result::Result<FollowingPage, XError> {
            assert_eq!(screen_name, "me");
            self.page_calls.lock().unwrap().push(cursor);
            if self.hang_pages.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            self.pages
                .lock()
                .unwrap()
                .get(&cursor)
                .cloned()
                .unwrap_or(Err(XError::Malformed))
        }

        async fn enable_device_notifications(
            &self,
            _: &TwitterConfig,
            user_id: &str,
        ) -> std::result::Result<(), XError> {
            let now = self.updates_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_updates_in_flight.fetch_max(now, Ordering::SeqCst);
            let _guard = ActiveGuard(Arc::clone(&self.updates_in_flight));
            self.updated.lock().unwrap().push(user_id.to_string());
            let hang = self
                .hang_updates
                .lock()
                .unwrap()
                .iter()
                .any(|id| id == user_id);
            if hang {
                std::future::pending::<()>().await;
            }
            match self.update_results.lock().unwrap().get(user_id) {
                Some(error) => Err(*error),
                None => Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::following;
    use super::fake::FakeBackend;
    use super::*;

    /// Minimal timings so bulk-job tests finish quickly.
    fn fast_following() -> Timings {
        Timings {
            request_timeout: Duration::from_millis(150),
            interval: Duration::from_millis(1),
            max_duration: Duration::from_secs(30),
        }
    }

    /// Job requests never time out on their own, so a hung update stays in flight until cancelled.
    fn slow_following() -> Timings {
        Timings {
            request_timeout: Duration::from_secs(30),
            ..fast_following()
        }
    }

    fn rig_with(
        env: Environment,
        timings: Timings,
    ) -> (Arc<FakeBackend>, Accounts<Arc<FakeBackend>>, PathBuf) {
        let dir = config::test_temp_dir();
        let path = dir.join("c.toml");
        let fake = Arc::new(FakeBackend::default());
        let accounts = Accounts::new(Arc::clone(&fake), path.clone(), env)
            .with_timeout(Duration::from_millis(200))
            .with_following_timings(timings);
        (fake, accounts, path)
    }

    fn rig() -> (Arc<FakeBackend>, Accounts<Arc<FakeBackend>>, PathBuf) {
        rig_with(Environment::default(), fast_following())
    }

    /// The service of `id`, creating the (empty) account first when it does not exist yet.
    fn account(accounts: &Accounts<Arc<FakeBackend>>, id: &str) -> Arc<Service<Arc<FakeBackend>>> {
        if accounts.get(id).is_err() {
            accounts.create(id).unwrap();
        }
        accounts.get(id).unwrap()
    }

    /// A rig with one empty `default` account.
    fn setup() -> (Arc<FakeBackend>, Arc<Service<Arc<FakeBackend>>>, PathBuf) {
        let (fake, accounts, path) = rig();
        let service = account(&accounts, DEFAULT_ACCOUNT_ID);
        (fake, service, path)
    }

    fn first_setup() -> SettingsUpdate {
        SettingsUpdate {
            auth_token: Some("auth-secret".into()),
            ct0: Some("ct0-secret".into()),
            webhook_secret: None,
            webhook_endpoint: "https://example.com/hook".into(),
            notification_filter: None,
        }
    }

    #[tokio::test]
    async fn first_setup_requires_both_credentials() {
        let (_, service, _) = setup();
        let mut update = first_setup();
        update.ct0 = Some("  ".into());
        let err = service.update_settings(update).await.unwrap_err();
        assert_eq!(err.code, "credentials_required");
        assert_eq!(service.status().unwrap().state, StateName::Unconfigured);

        let dto = service.update_settings(first_setup()).await.unwrap();
        assert!(dto.configured && dto.auth_token_set && dto.ct0_set);
        assert_eq!(service.status().unwrap().state, StateName::Unregistered);
    }

    #[tokio::test]
    async fn blank_credentials_retain_and_url_is_validated() {
        let (_, service, path) = setup();
        service.update_settings(first_setup()).await.unwrap();
        service
            .update_settings(SettingsUpdate {
                auth_token: None,
                ct0: Some(String::new()),
                webhook_secret: None,
                webhook_endpoint: String::new(),
                notification_filter: None,
            })
            .await
            .unwrap();
        let stored = config::test_default_account(&path);
        assert_eq!(stored.twitter.auth_token, "auth-secret");
        assert_eq!(stored.twitter.ct0, "ct0-secret");
        assert!(stored.webhook_endpoint.is_none());

        for bad in ["ftp://x/y", "not a url", "javascript:alert(1)"] {
            let err = service
                .update_settings(SettingsUpdate {
                    auth_token: None,
                    ct0: None,
                    webhook_secret: None,
                    webhook_endpoint: bad.into(),
                    notification_filter: None,
                })
                .await
                .unwrap_err();
            assert_eq!(err.code, "invalid_webhook_endpoint", "{bad}");
        }
        let err = service
            .update_settings(SettingsUpdate {
                auth_token: Some("a b".into()),
                ct0: None,
                webhook_secret: None,
                webhook_endpoint: String::new(),
                notification_filter: None,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code, "invalid_credential");
    }

    #[tokio::test]
    async fn register_unregister_lifecycle_is_idempotent_with_single_receiver() {
        let (fake, service, path) = setup();
        service.update_settings(first_setup()).await.unwrap();

        let reg = service.register().await.unwrap();
        assert!(reg.registered);
        let again = service.register().await.unwrap();
        assert_eq!(again, reg);
        assert_eq!(fake.subscribes.load(Ordering::SeqCst), 1);
        tokio::task::yield_now().await;
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 1);

        // Credential changes are locked while registered; webhook-only is fine.
        let err = service
            .update_settings(SettingsUpdate {
                auth_token: Some("other".into()),
                ct0: None,
                webhook_secret: None,
                webhook_endpoint: "https://example.com/hook".into(),
                notification_filter: None,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code, "credentials_locked");
        service
            .update_settings(SettingsUpdate {
                auth_token: Some("auth-secret".into()),
                ct0: None,
                webhook_secret: None,
                webhook_endpoint: "https://example.com/new".into(),
                notification_filter: None,
            })
            .await
            .unwrap();
        assert_eq!(
            config::test_default_account(&path)
                .webhook_endpoint
                .as_deref(),
            Some("https://example.com/new")
        );
        assert_eq!(fake.started.load(Ordering::SeqCst), 1);

        let gone = service.unregister().await.unwrap();
        assert!(!gone.registered);
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 0);
        let gone_again = service.unregister().await.unwrap();
        assert!(!gone_again.registered);
        assert_eq!(fake.unregisters.load(Ordering::SeqCst), 1);
        assert_eq!(fake.max_active.load(Ordering::SeqCst), 1);
        assert_eq!(service.status().unwrap().state, StateName::Unregistered);
    }

    #[tokio::test]
    async fn remote_failures_are_redacted_and_do_not_leave_registration() {
        let (fake, service, path) = setup();
        service.update_settings(first_setup()).await.unwrap();

        fake.fail_twitter.store(true, Ordering::SeqCst);
        let err = service.register().await.unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_GATEWAY);
        assert!(!err.message.contains("SECRET-DETAIL"));
        assert!(config::test_default_account(&path).registration.is_none());
        // Partial success (AutoPush subscribed) is cleaned up.
        assert_eq!(fake.unregisters.load(Ordering::SeqCst), 1);

        fake.fail_twitter.store(false, Ordering::SeqCst);
        fake.hang_twitter.store(true, Ordering::SeqCst);
        let err = service.register().await.unwrap_err();
        assert!(err.message.contains("timed out"));
        assert_eq!(fake.unregisters.load(Ordering::SeqCst), 2);
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failed_unregister_keeps_registration_and_restores_receiver() {
        let (fake, service, path) = setup();
        service.update_settings(first_setup()).await.unwrap();
        service.register().await.unwrap();

        fake.fail_unregister.store(true, Ordering::SeqCst);
        let err = service.unregister().await.unwrap_err();
        assert_eq!(err.code, "provider_failure");
        assert!(config::test_default_account(&path).registration.is_some());
        tokio::task::yield_now().await;
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 1);
        assert_eq!(fake.max_active.load(Ordering::SeqCst), 1);
        assert!(service.status().unwrap().registered);
    }

    #[tokio::test]
    async fn startup_starts_receiver_and_shutdown_joins_it() {
        let (fake, service, _) = setup();
        service.update_settings(first_setup()).await.unwrap();
        service.register().await.unwrap();
        service.shutdown().await;
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 0);
        assert_eq!(service.status().unwrap().state, StateName::Stopped);

        service.startup().await;
        tokio::task::yield_now().await;
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 1);
        service.shutdown().await;
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn concurrent_mutation_is_rejected_as_busy() {
        let (fake, service, _) = setup();
        service.update_settings(first_setup()).await.unwrap();
        fake.hang_twitter.store(true, Ordering::SeqCst);
        let slow = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.register().await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(service.status().unwrap().state, StateName::Registering);
        let err = service.unregister().await.unwrap_err();
        assert_eq!(err.code, "busy");
        let _ = slow.await;
    }

    fn setup_following() -> (Arc<FakeBackend>, Arc<Service<Arc<FakeBackend>>>, PathBuf) {
        let (fake, service, path) = setup();
        fake.pages.lock().unwrap().insert(
            -1,
            Ok(FollowingPage {
                ids: vec!["1".into(), "2".into(), "3".into()],
                next_cursor: 0,
            }),
        );
        (fake, service, path)
    }

    fn two_page_fake(fake: &FakeBackend) {
        fake.pages.lock().unwrap().insert(
            -1,
            Ok(FollowingPage {
                ids: vec!["1".into(), "2".into()],
                next_cursor: 0,
            }),
        );
        fake.hang_updates.lock().unwrap().push("1".to_string());
    }

    /// A job that hangs on its first update, until cancelled.
    fn setup_slow_following() -> (Arc<FakeBackend>, Arc<Service<Arc<FakeBackend>>>, PathBuf) {
        let (fake, accounts, path) = rig_with(Environment::default(), slow_following());
        two_page_fake(&fake);
        let service = account(&accounts, DEFAULT_ACCOUNT_ID);
        (fake, service, path)
    }

    /// Waits for an observable condition instead of guessing how long the scheduler needs.
    async fn wait_for(what: &str, cond: impl Fn() -> bool) {
        for _ in 0..500 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    async fn wait_settled(service: &Service<Arc<FakeBackend>>) -> FollowingNotificationsDto {
        for _ in 0..500 {
            let snapshot = service.following_notifications();
            if !matches!(
                snapshot.state,
                following::JobState::Loading
                    | following::JobState::Running
                    | following::JobState::Cancelling
            ) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the job did not settle");
    }

    #[tokio::test]
    async fn bulk_job_requires_saved_credentials() {
        let (_, service, _) = setup_following();
        let err = service.start_following_notifications().unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.code, "not_configured");
        assert_eq!(
            service.following_notifications().state,
            following::JobState::Idle
        );
    }

    #[tokio::test]
    async fn bulk_job_leaves_registration_and_receiver_untouched() {
        let (fake, service, path) = setup_following();
        service.update_settings(first_setup()).await.unwrap();
        service.register().await.unwrap();
        wait_for("the receiver", || {
            fake.active_receivers.load(Ordering::SeqCst) == 1
        })
        .await;
        let before = std::fs::read_to_string(&path).unwrap();

        service.start_following_notifications().unwrap();
        let done = wait_settled(&service).await;

        assert_eq!(done.success_count, 3);
        assert_eq!(*fake.updated.lock().unwrap(), ["1", "2", "3"]);
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 1);
        assert_eq!(fake.started.load(Ordering::SeqCst), 1);
        assert_eq!(fake.subscribes.load(Ordering::SeqCst), 1);
        assert_eq!(fake.unregisters.load(Ordering::SeqCst), 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(service.status().unwrap().registered);
        assert_eq!(service.status().unwrap().state, StateName::Connected);
    }

    #[tokio::test]
    async fn bulk_job_failure_does_not_affect_the_receiver() {
        let (fake, service, _) = setup_following();
        service.update_settings(first_setup()).await.unwrap();
        service.register().await.unwrap();
        fake.update_results
            .lock()
            .unwrap()
            .insert("1".to_string(), XError::RateLimited);

        service.start_following_notifications().unwrap();
        let done = wait_settled(&service).await;
        assert_eq!(done.state, following::JobState::Failed);
        assert_eq!(done.remaining_count, Some(3));
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 1);
        assert_eq!(service.status().unwrap().state, StateName::Connected);
    }

    #[tokio::test]
    async fn credentials_cannot_change_while_the_bulk_job_runs() {
        let (_, service, path) = setup_slow_following();
        service.update_settings(first_setup()).await.unwrap();
        service.start_following_notifications().unwrap();
        let err = service.start_following_notifications().unwrap_err();
        assert_eq!(err.code, "following_job_active");

        let err = service
            .update_settings(SettingsUpdate {
                auth_token: Some("other".into()),
                ct0: None,
                webhook_secret: None,
                webhook_endpoint: String::new(),
                notification_filter: None,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code, "following_job_active");
        assert_eq!(
            config::test_default_account(&path).twitter.auth_token,
            "auth-secret"
        );
        service
            .update_settings(SettingsUpdate {
                auth_token: None,
                ct0: None,
                webhook_secret: None,
                webhook_endpoint: "https://example.com/other".into(),
                notification_filter: None,
            })
            .await
            .unwrap();

        let (requested, _) = service.cancel_following_notifications();
        assert!(requested);
        wait_settled(&service).await;
        service
            .update_settings(SettingsUpdate {
                auth_token: Some("other".into()),
                ct0: None,
                webhook_secret: None,
                webhook_endpoint: String::new(),
                notification_filter: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn bulk_job_start_is_rejected_while_another_mutation_runs() {
        let (fake, service, _) = setup_following();
        service.update_settings(first_setup()).await.unwrap();
        fake.hang_twitter.store(true, Ordering::SeqCst);
        let slow = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.register().await })
        };
        wait_for("the registration to start", || {
            service.status().unwrap().state == StateName::Registering
        })
        .await;
        let err = service.start_following_notifications().unwrap_err();
        assert_eq!(err.code, "busy");
        let _ = slow.await;
        assert_eq!(
            service.following_notifications().state,
            following::JobState::Idle
        );
    }

    #[tokio::test]
    async fn shutdown_cancels_the_bulk_job_and_stops_the_receiver() {
        let (fake, service, _) = setup_slow_following();
        service.update_settings(first_setup()).await.unwrap();
        service.register().await.unwrap();
        service.start_following_notifications().unwrap();
        for _ in 0..200 {
            if fake.updates_in_flight.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(fake.updates_in_flight.load(Ordering::SeqCst), 1);
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 1);

        service.shutdown().await;
        assert_eq!(fake.updates_in_flight.load(Ordering::SeqCst), 0);
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 0);
        assert_eq!(
            service.following_notifications().state,
            following::JobState::Cancelled
        );
    }

    #[tokio::test]
    async fn notification_history_pages_and_rejects_unknown_cursors() {
        let (_, service, path) = setup();
        let empty = service.notifications(None, 50).await.unwrap();
        assert!(empty.items.is_empty() && empty.next_cursor.is_none());

        for n in 0..3 {
            notifications::append(&path, &format!("id-{n}"), &serde_json::json!({"title": n}))
                .unwrap();
        }
        let first = service.notifications(None, 2).await.unwrap();
        assert_eq!(first.items.len(), 2);
        let cursor = first.next_cursor.unwrap();
        let rest = service.notifications(Some(cursor), 2).await.unwrap();
        assert_eq!(rest.items.len(), 1);
        assert!(rest.next_cursor.is_none());

        let err = service
            .notifications(Some("missing".into()), 2)
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.code, "invalid_cursor");
    }

    /// `retweet`・`quote`・`quote_self`・`reply` は `tweet` に揃える。個別に変える場合は構造体を直接作る。
    fn filter_input(tweet: bool, chat: bool, other: bool) -> Option<NotificationFilterInput> {
        Some(NotificationFilterInput {
            tweet,
            retweet: tweet,
            quote: tweet,
            quote_self: tweet,
            reply: tweet,
            xchat_message_received: chat,
            other,
        })
    }

    #[tokio::test]
    async fn notification_filter_defaults_saves_and_survives_omitted_updates() {
        let (_, service, path) = setup();
        assert_eq!(
            service.settings().unwrap().notification_filter,
            NotificationFilter::default()
        );

        let mut first = first_setup();
        first.notification_filter = filter_input(false, true, true);
        let dto = service.update_settings(first).await.unwrap();
        assert!(!dto.notification_filter.tweet);
        assert!(
            config::test_default_account(&path)
                .webhook_endpoint
                .is_some()
        );
        assert!(
            !config::test_default_account(&path)
                .notification_filter
                .tweet
        );

        // 省略した更新は保存済みのフィルターを変えない。
        let dto = service
            .update_settings(SettingsUpdate {
                auth_token: None,
                ct0: None,
                webhook_secret: None,
                webhook_endpoint: String::new(),
                notification_filter: None,
            })
            .await
            .unwrap();
        assert_eq!(
            dto.notification_filter,
            filter_input(false, true, true).unwrap().into()
        );
        assert_eq!(
            service.settings().unwrap().notification_filter,
            dto.notification_filter
        );
        assert!(
            config::test_default_account(&path)
                .webhook_endpoint
                .is_none()
        );

        let dto = service
            .update_settings(SettingsUpdate {
                auth_token: None,
                ct0: None,
                webhook_secret: None,
                webhook_endpoint: String::new(),
                notification_filter: filter_input(true, false, false),
            })
            .await
            .unwrap();
        assert_eq!(
            config::test_default_account(&path).notification_filter,
            dto.notification_filter
        );
        assert!(dto.notification_filter.tweet && !dto.notification_filter.other);
    }

    #[tokio::test]
    async fn notification_filter_is_editable_while_registered_without_restarting() {
        let (fake, service, _) = setup();
        service.update_settings(first_setup()).await.unwrap();
        service.register().await.unwrap();
        tokio::task::yield_now().await;
        let update = |filter| SettingsUpdate {
            auth_token: None,
            ct0: None,
            webhook_secret: None,
            webhook_endpoint: "https://example.com/hook".into(),
            notification_filter: filter,
        };
        let dto = service
            .update_settings(update(filter_input(false, false, true)))
            .await
            .unwrap();
        assert!(dto.credentials_locked && !dto.notification_filter.xchat_message_received);
        assert_eq!(fake.started.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn notifications_apply_the_saved_filter_and_report_it() {
        let (_, service, path) = setup();
        let empty = service.notifications(None, 10).await.unwrap();
        assert_eq!(empty.notification_filter, NotificationFilter::default());

        service.update_settings(first_setup()).await.unwrap();
        for (id, kind) in [("a", "tweet"), ("b", "xchat_message_received"), ("c", "x")] {
            notifications::append(&path, id, &serde_json::json!({"data": {"type": kind}})).unwrap();
        }
        assert_eq!(
            service.notifications(None, 10).await.unwrap().items.len(),
            3
        );

        service
            .update_settings(SettingsUpdate {
                auth_token: None,
                ct0: None,
                webhook_secret: None,
                webhook_endpoint: String::new(),
                notification_filter: filter_input(false, true, false),
            })
            .await
            .unwrap();
        let page = service.notifications(None, 10).await.unwrap();
        assert_eq!(
            page.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["b"]
        );
        assert!(!page.notification_filter.tweet && page.notification_filter.xchat_message_received);

        // 非表示の行を cursor にしても有効で、全件除外は空ページになる。
        let rest = service.notifications(Some("c".into()), 10).await.unwrap();
        assert_eq!(rest.items.len(), 1);
        service
            .update_settings(SettingsUpdate {
                auth_token: None,
                ct0: None,
                webhook_secret: None,
                webhook_endpoint: String::new(),
                notification_filter: filter_input(false, false, false),
            })
            .await
            .unwrap();
        let none = service.notifications(None, 10).await.unwrap();
        assert!(none.items.is_empty() && none.next_cursor.is_none());

        std::fs::write(&path, "not = [valid toml").unwrap();
        let err = service.notifications(None, 10).await.unwrap_err();
        assert_eq!(err.code, "config_invalid");
    }

    #[tokio::test]
    async fn retweets_are_filtered_independently_of_tweets() {
        let (_, service, path) = setup();
        service.update_settings(first_setup()).await.unwrap();
        let retweet = serde_json::json!({"data": {
            "type": "tweet", "uri": "/a/status/1", "tag": "tweet-2"
        }});
        let tweet = serde_json::json!({"data": {
            "type": "tweet", "uri": "/a/status/3", "tag": "tweet-3"
        }});
        notifications::append(&path, "tweet", &tweet).unwrap();
        notifications::append(&path, "retweet", &retweet).unwrap();
        let ids = |page: NotificationPage| -> Vec<String> {
            page.items.into_iter().map(|item| item.id).collect()
        };
        assert_eq!(
            ids(service.notifications(None, 10).await.unwrap()),
            ["retweet", "tweet"]
        );

        let dto = service
            .update_settings(SettingsUpdate {
                auth_token: None,
                ct0: None,
                webhook_secret: None,
                webhook_endpoint: String::new(),
                notification_filter: Some(NotificationFilterInput {
                    tweet: true,
                    retweet: false,
                    quote: true,
                    quote_self: true,
                    reply: true,
                    xchat_message_received: true,
                    other: true,
                }),
            })
            .await
            .unwrap();
        assert!(dto.notification_filter.tweet && !dto.notification_filter.retweet);
        assert!(
            !config::test_default_account(&path)
                .notification_filter
                .retweet
        );
        let page = service.notifications(None, 10).await.unwrap();
        assert!(!page.notification_filter.retweet);
        assert_eq!(ids(page), ["tweet"]);
    }

    #[tokio::test]
    async fn quotes_self_quotes_and_replies_are_filtered_independently() {
        use crate::notifications::NotificationKind::{Quote, Reply, SelfQuote, Tweet};
        let (_, service, path) = setup();
        service.update_settings(first_setup()).await.unwrap();
        let payload = serde_json::json!({"data": {"type": "tweet"}});
        for (id, kind) in [
            ("tweet", Tweet),
            ("quote", Quote),
            ("self_quote", SelfQuote),
            ("reply", Reply),
        ] {
            notifications::append_as(&path, id, &payload, kind).unwrap();
        }
        let ids = |page: NotificationPage| -> Vec<String> {
            page.items.into_iter().map(|item| item.id).collect()
        };
        let update = |quote, quote_self, reply| SettingsUpdate {
            auth_token: None,
            ct0: None,
            webhook_secret: None,
            webhook_endpoint: String::new(),
            notification_filter: Some(NotificationFilterInput {
                tweet: true,
                retweet: true,
                quote,
                quote_self,
                reply,
                xchat_message_received: true,
                other: true,
            }),
        };
        assert_eq!(
            ids(service.notifications(None, 10).await.unwrap()),
            ["reply", "self_quote", "quote", "tweet"]
        );

        let dto = service
            .update_settings(update(false, true, true))
            .await
            .unwrap();
        assert!(!dto.notification_filter.quote && dto.notification_filter.quote_self);
        assert!(dto.notification_filter.reply);
        let saved = config::test_default_account(&path).notification_filter;
        assert!(!saved.quote && saved.quote_self && saved.reply);
        assert_eq!(
            ids(service.notifications(None, 10).await.unwrap()),
            ["reply", "self_quote", "tweet"]
        );

        service
            .update_settings(update(true, false, true))
            .await
            .unwrap();
        let saved = config::test_default_account(&path).notification_filter;
        assert!(saved.quote && !saved.quote_self);
        assert_eq!(
            ids(service.notifications(None, 10).await.unwrap()),
            ["reply", "quote", "tweet"]
        );

        service
            .update_settings(update(true, true, false))
            .await
            .unwrap();
        assert_eq!(
            ids(service.notifications(None, 10).await.unwrap()),
            ["self_quote", "quote", "tweet"]
        );

        service
            .update_settings(update(false, false, false))
            .await
            .unwrap();
        assert_eq!(
            ids(service.notifications(None, 10).await.unwrap()),
            ["tweet"]
        );
    }

    #[tokio::test]
    async fn credential_check_reports_valid_for_the_saved_cookies() {
        let (fake, service, _) = setup();
        let err = service.validate_credentials().await.unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.code, "not_configured");
        assert!(fake.screen_name_creds.lock().unwrap().is_empty());

        service.update_settings(first_setup()).await.unwrap();
        let ok = service.validate_credentials().await.unwrap();
        assert_eq!(
            ok,
            CredentialCheckDto {
                valid: true,
                screen_name: Some("me".into()),
                message: "Cookieを確認しました。",
            }
        );
        assert_eq!(*fake.screen_name_creds.lock().unwrap(), ["auth-secret"]);
        let json = serde_json::to_string(&ok).unwrap();
        assert!(!json.contains("secret"));
    }

    #[tokio::test]
    async fn credential_check_treats_only_401_and_403_as_invalid() {
        let (fake, service, _) = setup();
        service.update_settings(first_setup()).await.unwrap();

        for error in [XError::Unauthorized, XError::Forbidden] {
            *fake.screen_error.lock().unwrap() = Some(error);
            let dto = service.validate_credentials().await.unwrap();
            assert_eq!(
                dto,
                CredentialCheckDto {
                    valid: false,
                    screen_name: None,
                    message: "Cookieが無効か期限切れです。",
                }
            );
        }

        for error in [
            XError::RateLimited,
            XError::Status(401),
            XError::Status(403),
            XError::Status(404),
            XError::Status(429),
            XError::Status(500),
            XError::Status(503),
            XError::Transport,
            XError::Malformed,
        ] {
            *fake.screen_error.lock().unwrap() = Some(error);
            let err = service.validate_credentials().await.unwrap_err();
            assert_eq!(err.status, StatusCode::BAD_GATEWAY, "{error:?}");
            assert_eq!(err.code, "provider_failure");
            assert!(!err.message.contains("secret"));
            assert!(err.message.len() < 200);
        }
    }

    #[tokio::test]
    async fn credential_check_times_out_as_a_provider_error() {
        let (fake, service, _) = setup();
        service.update_settings(first_setup()).await.unwrap();
        fake.hang_screen_name.store(true, Ordering::SeqCst);
        let err = service.validate_credentials().await.unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_GATEWAY);
        assert!(err.message.contains("in time"));
        // The lock is released afterwards.
        fake.hang_screen_name.store(false, Ordering::SeqCst);
        assert!(service.validate_credentials().await.unwrap().valid);
    }

    #[tokio::test]
    async fn credential_check_conflicts_with_other_mutations_and_keeps_the_receiver() {
        let (fake, service, _) = setup_following();
        service.update_settings(first_setup()).await.unwrap();
        service.register().await.unwrap();
        wait_for("the receiver", || {
            fake.active_receivers.load(Ordering::SeqCst) == 1
        })
        .await;

        fake.hang_screen_name.store(true, Ordering::SeqCst);
        let check = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.validate_credentials().await })
        };
        wait_for("the cookie check to reach X", || {
            !fake.screen_name_creds.lock().unwrap().is_empty()
        })
        .await;
        for err in [
            service.register().await.unwrap_err(),
            service.unregister().await.unwrap_err(),
            service.start_following_notifications().unwrap_err(),
            service.validate_credentials().await.unwrap_err(),
        ] {
            assert_eq!(err.status, StatusCode::CONFLICT);
            assert_eq!(err.code, "busy");
        }
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 1);
        let _ = check.await;
        fake.hang_screen_name.store(false, Ordering::SeqCst);

        service.shutdown().await;

        let (_, service, _) = setup_slow_following();
        service.update_settings(first_setup()).await.unwrap();
        service.start_following_notifications().unwrap();
        let err = service.validate_credentials().await.unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);
        assert_eq!(err.code, "following_job_active");
        service.shutdown().await;
    }

    #[tokio::test]
    async fn bulk_job_blocks_register_and_unregister_but_not_webhook_edits() {
        let (fake, service, path) = setup_slow_following();
        service.update_settings(first_setup()).await.unwrap();
        service.register().await.unwrap();
        service.start_following_notifications().unwrap();
        wait_for("the receiver and the hung update", || {
            fake.active_receivers.load(Ordering::SeqCst) == 1
                && fake.updates_in_flight.load(Ordering::SeqCst) == 1
        })
        .await;

        for err in [
            service.register().await.unwrap_err(),
            service.unregister().await.unwrap_err(),
        ] {
            assert_eq!(err.status, StatusCode::CONFLICT);
            assert_eq!(err.code, "following_job_active");
        }
        assert!(config::test_default_account(&path).registration.is_some());
        assert_eq!(fake.unregisters.load(Ordering::SeqCst), 0);
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 1);
        service
            .update_settings(SettingsUpdate {
                auth_token: None,
                ct0: None,
                webhook_secret: None,
                webhook_endpoint: "https://example.com/while-running".into(),
                notification_filter: None,
            })
            .await
            .unwrap();

        service.cancel_following_notifications();
        wait_settled(&service).await;
        assert!(!service.unregister().await.unwrap().registered);
        service.shutdown().await;
    }

    // ---- multiple accounts ----

    fn with_cookies(auth: &str, webhook: &str) -> SettingsUpdate {
        SettingsUpdate {
            auth_token: Some(auth.into()),
            ct0: Some(format!("ct0-of-{auth}")),
            webhook_secret: None,
            webhook_endpoint: webhook.into(),
            notification_filter: None,
        }
    }

    fn keep_everything() -> SettingsUpdate {
        SettingsUpdate {
            auth_token: None,
            ct0: None,
            webhook_secret: None,
            webhook_endpoint: "https://example.com/hook".into(),
            notification_filter: None,
        }
    }

    fn stored_account(path: &Path, id: &str) -> Option<Account> {
        Config::load(path).ok()?.account(id).cloned()
    }

    fn ids(accounts: &Accounts<Arc<FakeBackend>>) -> Vec<String> {
        accounts
            .list()
            .unwrap()
            .accounts
            .into_iter()
            .map(|a| a.id)
            .collect()
    }

    #[tokio::test]
    async fn account_ids_are_validated_before_anything_else() {
        let (_, accounts, path) = rig();
        for bad in [
            "",
            "Upper",
            "-lead",
            "_lead",
            "has space",
            "a.b",
            "a/b",
            "é",
            &"a".repeat(33),
        ] {
            let err = accounts.get(bad).err().unwrap();
            assert_eq!(err.status, StatusCode::BAD_REQUEST, "{bad:?}");
            assert_eq!(err.code, "invalid_account_id", "{bad:?}");
            let err = accounts.create(bad).unwrap_err();
            assert_eq!(err.code, "invalid_account_id", "{bad:?}");
            let err = accounts.delete(bad).await.unwrap_err();
            assert_eq!(err.code, "invalid_account_id", "{bad:?}");
        }
        assert!(!path.exists());

        accounts.create(&"a".repeat(32)).unwrap();
        accounts.create("0-a_b").unwrap();

        let err = accounts.get("ghost").err().unwrap();
        assert_eq!(err.status, StatusCode::NOT_FOUND);
        assert_eq!(err.code, "account_not_found");
        let err = accounts.delete("ghost").await.unwrap_err();
        assert_eq!(err.code, "account_not_found");
        assert_eq!(ids(&accounts), ["a".repeat(32), "0-a_b".to_string()]);
    }

    #[tokio::test]
    async fn creating_an_account_twice_is_a_conflict_and_keeps_the_first() {
        let (_, accounts, path) = rig();
        let created = accounts.create("work").unwrap();
        assert_eq!(
            created,
            AccountDto {
                id: "work".into(),
                registered: false,
                receiver_state: StateName::Unconfigured,
            }
        );
        accounts
            .get("work")
            .unwrap()
            .update_settings(with_cookies("work-auth", "https://example.com/w"))
            .await
            .unwrap();

        let err = accounts.create("work").unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);
        assert_eq!(err.code, "account_exists");
        assert_eq!(
            stored_account(&path, "work").unwrap().twitter.auth_token,
            "work-auth"
        );
        assert_eq!(ids(&accounts), ["work"]);
    }

    #[tokio::test]
    async fn two_accounts_keep_separate_settings_secrets_and_filters() {
        let (_, accounts, path) = rig();
        let one = account(&accounts, "one");
        let two = account(&accounts, "two");

        let mut first = with_cookies("auth-one", "https://example.com/one");
        first.webhook_secret = Some("secret-one".into());
        first.notification_filter = filter_input(false, true, true);
        one.update_settings(first).await.unwrap();
        two.update_settings(with_cookies("auth-two", "https://example.com/two"))
            .await
            .unwrap();

        let (a, b) = (
            stored_account(&path, "one").unwrap(),
            stored_account(&path, "two").unwrap(),
        );
        assert_eq!(a.twitter.auth_token, "auth-one");
        assert_eq!(b.twitter.auth_token, "auth-two");
        assert_eq!(
            a.webhook_endpoint.as_deref(),
            Some("https://example.com/one")
        );
        assert_eq!(
            b.webhook_endpoint.as_deref(),
            Some("https://example.com/two")
        );
        assert_eq!(a.webhook_secret.as_deref(), Some("secret-one"));
        assert_eq!(b.webhook_secret, None);
        assert!(!a.notification_filter.tweet);
        assert_eq!(b.notification_filter, NotificationFilter::default());

        let (sa, sb) = (one.settings().unwrap(), two.settings().unwrap());
        assert!(sa.webhook_secret_set && !sb.webhook_secret_set);
        assert!(!sa.notification_filter.tweet && sb.notification_filter.tweet);
        assert_eq!(
            sb.webhook_endpoint.as_deref(),
            Some("https://example.com/two")
        );

        // Editing one account leaves the other exactly as it was.
        two.update_settings(keep_everything()).await.unwrap();
        assert_eq!(
            stored_account(&path, "one").unwrap().webhook_secret,
            a.webhook_secret
        );
        assert_eq!(
            stored_account(&path, "two")
                .unwrap()
                .webhook_endpoint
                .as_deref(),
            Some("https://example.com/hook")
        );
        assert_eq!(
            stored_account(&path, "one")
                .unwrap()
                .webhook_endpoint
                .as_deref(),
            Some("https://example.com/one")
        );
    }

    #[tokio::test]
    async fn webhook_secret_is_write_only_and_kept_unless_replaced() {
        let (_, service, path) = setup();
        let dto = service.update_settings(first_setup()).await.unwrap();
        assert!(!dto.webhook_secret_set);

        let mut set = keep_everything();
        set.webhook_secret = Some("  s3cr3t value ".into());
        let dto = service.update_settings(set).await.unwrap();
        assert!(dto.webhook_secret_set);
        // Kept as written: the signature is computed over exactly this string.
        assert_eq!(
            config::test_default_account(&path)
                .webhook_secret
                .as_deref(),
            Some("  s3cr3t value ")
        );

        for keep in [None, Some(String::new()), Some("   ".to_string())] {
            let mut update = keep_everything();
            update.webhook_secret = keep;
            let dto = service.update_settings(update).await.unwrap();
            assert!(dto.webhook_secret_set);
            assert_eq!(
                config::test_default_account(&path)
                    .webhook_secret
                    .as_deref(),
                Some("  s3cr3t value ")
            );
        }

        let mut replace = keep_everything();
        replace.webhook_secret = Some("another".into());
        service.update_settings(replace).await.unwrap();
        assert_eq!(
            config::test_default_account(&path)
                .webhook_secret
                .as_deref(),
            Some("another")
        );

        for bad in [
            "bad\nsecret".to_string(),
            "x".repeat(MAX_WEBHOOK_SECRET_LEN + 1),
        ] {
            let mut update = keep_everything();
            update.webhook_secret = Some(bad);
            let err = service.update_settings(update).await.unwrap_err();
            assert_eq!(err.code, "invalid_webhook_secret");
            assert!(!err.message.contains("another"));
        }

        let json = serde_json::to_string(&service.settings().unwrap()).unwrap();
        assert!(json.contains("\"webhook_secret_set\":true"));
        assert!(!json.contains("another") && !json.contains("s3cr3t"));
    }

    #[derive(Clone, Default)]
    struct LogCapture(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = LogCapture;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Everything the test process logs, from here on. The subscriber is global because a
    /// thread-scoped one races with tracing's per-callsite cache when tests run in parallel.
    fn captured_logs() -> LogCapture {
        static CAPTURE: std::sync::OnceLock<LogCapture> = std::sync::OnceLock::new();
        CAPTURE
            .get_or_init(|| {
                let capture = LogCapture::default();
                let subscriber = tracing_subscriber::fmt()
                    .with_writer(capture.clone())
                    .with_max_level(tracing::Level::TRACE)
                    .finish();
                tracing::subscriber::set_global_default(subscriber)
                    .expect("no other test installs a global subscriber");
                capture
            })
            .clone()
    }

    #[tokio::test]
    async fn webhook_secret_never_reaches_responses_errors_or_logs() {
        let capture = captured_logs();

        let (fake, service, _) = setup();
        let mut update = first_setup();
        update.webhook_secret = Some("HOOK-SECRET-VALUE".into());
        let mut responses = vec![
            serde_json::to_string(&service.update_settings(update).await.unwrap()).unwrap(),
            serde_json::to_string(&service.settings().unwrap()).unwrap(),
        ];
        fake.fail_twitter.store(true, Ordering::SeqCst);
        let err = service.register().await.unwrap_err();
        responses.push(err.message);
        fake.fail_twitter.store(false, Ordering::SeqCst);
        responses.push(serde_json::to_string(&service.register().await.unwrap()).unwrap());
        let mut bad = keep_everything();
        bad.webhook_endpoint = "ftp://nope".into();
        bad.webhook_secret = Some("HOOK-SECRET-VALUE".into());
        responses.push(service.update_settings(bad).await.unwrap_err().message);

        for text in responses {
            assert!(!text.contains("HOOK-SECRET-VALUE"), "{text}");
        }
        let logs = String::from_utf8_lossy(&capture.0.lock().unwrap()).into_owned();
        // The failed registration above logs the provider's raw error, so a capture that saw
        // nothing would make this test pass for the wrong reason.
        assert!(
            logs.contains("SECRET-DETAIL"),
            "the capture saw no log output"
        );
        assert!(!logs.contains("HOOK-SECRET-VALUE"), "{logs}");
    }

    #[tokio::test]
    async fn registering_one_account_does_not_touch_another() {
        let (fake, accounts, path) = rig();
        let one = account(&accounts, "one");
        let two = account(&accounts, "two");
        one.update_settings(with_cookies("auth-one", ""))
            .await
            .unwrap();
        two.update_settings(with_cookies("auth-two", ""))
            .await
            .unwrap();

        one.register().await.unwrap();
        wait_for("the receiver of one", || fake.active_for("one") == 1).await;
        assert_eq!(fake.active_for("two"), 0);
        assert_eq!(*fake.registered_with.lock().unwrap(), ["auth-one"]);
        assert!(stored_account(&path, "one").unwrap().registration.is_some());
        assert!(stored_account(&path, "two").unwrap().registration.is_none());
        assert_eq!(one.status().unwrap().state, StateName::Connected);
        assert_eq!(two.status().unwrap().state, StateName::Unregistered);

        two.register().await.unwrap();
        wait_for("the receiver of two", || fake.active_for("two") == 1).await;
        assert_eq!(
            *fake.registered_with.lock().unwrap(),
            ["auth-one", "auth-two"]
        );

        one.unregister().await.unwrap();
        assert_eq!(fake.active_for("one"), 0);
        assert_eq!(fake.active_for("two"), 1);
        assert!(stored_account(&path, "two").unwrap().registration.is_some());
        assert!(!one.registration().unwrap().registered);
        assert!(two.registration().unwrap().registered);
        // One receiver per account, never a second one for the same account.
        assert_eq!(fake.max_active.load(Ordering::SeqCst), 2);
        assert_eq!(*fake.started_accounts.lock().unwrap(), ["one", "two"]);
    }

    #[tokio::test]
    async fn operation_locks_are_independent_per_account() {
        let (fake, accounts, _) = rig();
        let one = account(&accounts, "one");
        let two = account(&accounts, "two");
        one.update_settings(with_cookies("auth-one", ""))
            .await
            .unwrap();
        two.update_settings(with_cookies("auth-two", ""))
            .await
            .unwrap();

        fake.hang_twitter.store(true, Ordering::SeqCst);
        let slow = {
            let one = Arc::clone(&one);
            tokio::spawn(async move { one.register().await })
        };
        wait_for("the registration of one", || {
            one.status().unwrap().state == StateName::Registering
        })
        .await;

        // The busy account rejects further operations ...
        assert_eq!(one.unregister().await.unwrap_err().code, "busy");
        assert_eq!(
            one.update_settings(keep_everything())
                .await
                .unwrap_err()
                .code,
            "busy"
        );
        assert_eq!(one.validate_credentials().await.unwrap_err().code, "busy");
        // ... while the other account carries on.
        assert_eq!(two.status().unwrap().state, StateName::Unregistered);
        two.update_settings(keep_everything()).await.unwrap();
        assert!(two.validate_credentials().await.unwrap().valid);
        fake.hang_twitter.store(false, Ordering::SeqCst);
        two.register().await.unwrap();
        assert!(two.status().unwrap().registered);
        assert_eq!(one.status().unwrap().state, StateName::Registering);

        let _ = slow.await;
    }

    #[tokio::test]
    async fn a_bulk_job_blocks_only_its_own_account() {
        let (fake, accounts, _) = rig_with(Environment::default(), slow_following());
        two_page_fake(&fake);
        let one = account(&accounts, "one");
        let two = account(&accounts, "two");
        one.update_settings(with_cookies("auth-one", ""))
            .await
            .unwrap();
        two.update_settings(with_cookies("auth-two", ""))
            .await
            .unwrap();
        one.start_following_notifications().unwrap();
        wait_for("the hung update", || {
            fake.updates_in_flight.load(Ordering::SeqCst) == 1
        })
        .await;

        assert_eq!(
            one.register().await.unwrap_err().code,
            "following_job_active"
        );
        assert_eq!(
            one.update_settings(with_cookies("changed", ""))
                .await
                .unwrap_err()
                .code,
            "following_job_active"
        );
        assert_eq!(
            two.following_notifications().state,
            following::JobState::Idle
        );
        two.register().await.unwrap();
        two.update_settings(with_cookies("auth-two-b", ""))
            .await
            .unwrap_err();
        assert!(two.registration().unwrap().registered);

        one.cancel_following_notifications();
        wait_settled(&one).await;
        accounts.shutdown().await;
    }

    #[tokio::test]
    async fn notification_history_and_filter_are_per_account() {
        let (_, accounts, path) = rig();
        let default = account(&accounts, DEFAULT_ACCOUNT_ID);
        let work = account(&accounts, "work");
        default.update_settings(first_setup()).await.unwrap();
        work.update_settings(with_cookies("auth-work", ""))
            .await
            .unwrap();

        let tweet = serde_json::json!({"data": {"type": "tweet"}});
        let chat = serde_json::json!({"data": {"type": "xchat_message_received"}});
        notifications::append_as_for(
            &path,
            DEFAULT_ACCOUNT_ID,
            "d-1",
            &tweet,
            notifications::NotificationKind::Tweet,
        )
        .unwrap();
        for (id, payload, kind) in [
            ("w-1", &tweet, notifications::NotificationKind::Tweet),
            (
                "w-2",
                &chat,
                notifications::NotificationKind::XchatMessageReceived,
            ),
        ] {
            notifications::append_as_for(&path, "work", id, payload, kind).unwrap();
        }
        let titles = |page: NotificationPage| -> Vec<String> {
            page.items.into_iter().map(|i| i.id).collect()
        };
        assert_eq!(
            titles(default.notifications(None, 10).await.unwrap()),
            ["d-1"]
        );
        assert_eq!(
            titles(work.notifications(None, 10).await.unwrap()),
            ["w-2", "w-1"]
        );
        assert!(notifications::history_path_for(&path, "work").is_file());

        // The filter of one account does not hide anything from the other.
        let mut only_chat = keep_everything();
        only_chat.notification_filter = filter_input(false, true, false);
        work.update_settings(only_chat).await.unwrap();
        assert_eq!(titles(work.notifications(None, 10).await.unwrap()), ["w-2"]);
        assert_eq!(
            titles(default.notifications(None, 10).await.unwrap()),
            ["d-1"]
        );
        // A cursor from one account's history does not exist in the other's.
        let err = default
            .notifications(Some("w-1".into()), 10)
            .await
            .unwrap_err();
        assert_eq!(err.code, "invalid_cursor");
    }

    #[tokio::test]
    async fn deleting_an_account_removes_only_that_account() {
        let (fake, accounts, path) = rig();
        let keep = account(&accounts, "keep");
        let gone = account(&accounts, "gone");
        keep.update_settings(with_cookies("auth-keep", "https://example.com/k"))
            .await
            .unwrap();
        gone.update_settings(with_cookies("auth-gone", ""))
            .await
            .unwrap();
        keep.register().await.unwrap();
        gone.register().await.unwrap();
        wait_for("both receivers", || {
            fake.active_for("keep") == 1 && fake.active_for("gone") == 1
        })
        .await;
        let payload = serde_json::json!({"data": {"type": "tweet"}});
        for id in ["keep", "gone"] {
            notifications::append_as_for(
                &path,
                id,
                "n-1",
                &payload,
                notifications::NotificationKind::Tweet,
            )
            .unwrap();
        }
        let gone_db = notifications::history_path_for(&path, "gone");
        let keep_db = notifications::history_path_for(&path, "keep");
        let journal = {
            let mut name = gone_db.as_os_str().to_os_string();
            name.push("-journal");
            PathBuf::from(name)
        };
        std::fs::write(&journal, b"left over").unwrap();
        assert!(gone_db.is_file() && keep_db.is_file());
        let uaid = stored_account(&path, "gone")
            .unwrap()
            .registration
            .unwrap()
            .autopush
            .uaid;

        accounts.delete("gone").await.unwrap();

        assert_eq!(*fake.events.lock().unwrap(), [format!("unregister:{uaid}")]);
        assert_eq!(fake.active_for("gone"), 0);
        assert_eq!(fake.active_for("keep"), 1);
        assert!(stored_account(&path, "gone").is_none());
        assert!(
            stored_account(&path, "keep")
                .unwrap()
                .registration
                .is_some()
        );
        assert!(!gone_db.exists() && !journal.exists());
        assert!(keep_db.is_file());
        assert_eq!(ids(&accounts), ["keep"]);
        assert_eq!(
            accounts.get("gone").err().unwrap().code,
            "account_not_found"
        );
        let err = accounts.delete("gone").await.unwrap_err();
        assert_eq!(err.code, "account_not_found");

        // A handle taken before the deletion fails everywhere and writes nothing back.
        assert_eq!(gone.settings().unwrap_err().code, "account_not_found");
        assert_eq!(gone.status().unwrap_err().code, "account_not_found");
        assert_eq!(gone.register().await.unwrap_err().code, "account_not_found");
        assert_eq!(
            gone.update_settings(keep_everything())
                .await
                .unwrap_err()
                .code,
            "account_not_found"
        );
        assert_eq!(
            gone.notifications(None, 10).await.unwrap_err().code,
            "account_not_found"
        );
        assert_eq!(
            gone.start_following_notifications().unwrap_err().code,
            "account_not_found"
        );
        assert!(stored_account(&path, "gone").is_none());
        assert!(!gone_db.exists());

        // The id can be created again, as a fresh empty account.
        let again = accounts.create("gone").unwrap();
        assert_eq!(again.receiver_state, StateName::Unconfigured);
        assert!(!accounts.get("gone").unwrap().settings().unwrap().configured);
        assert!(stored_account(&path, "gone").unwrap().twitter.is_empty());
    }

    #[tokio::test]
    async fn deleting_an_unregistered_account_needs_no_unregistration() {
        let (fake, accounts, path) = rig();
        account(&accounts, "idle");
        accounts.delete("idle").await.unwrap();
        assert_eq!(fake.unregisters.load(Ordering::SeqCst), 0);
        assert!(stored_account(&path, "idle").is_none());
        assert!(ids(&accounts).is_empty());
    }

    #[tokio::test]
    async fn failed_unregistration_keeps_the_account_and_restores_its_receiver() {
        let (fake, accounts, path) = rig();
        let other = account(&accounts, "other");
        let work = account(&accounts, "work");
        other
            .update_settings(with_cookies("auth-other", ""))
            .await
            .unwrap();
        work.update_settings(with_cookies("auth-work", ""))
            .await
            .unwrap();
        other.register().await.unwrap();
        work.register().await.unwrap();
        let payload = serde_json::json!({"data": {"type": "tweet"}});
        notifications::append_as_for(
            &path,
            "work",
            "n-1",
            &payload,
            notifications::NotificationKind::Tweet,
        )
        .unwrap();
        wait_for("both receivers", || {
            fake.active_receivers.load(Ordering::SeqCst) == 2
        })
        .await;

        fake.fail_unregister.store(true, Ordering::SeqCst);
        let err = accounts.delete("work").await.unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_GATEWAY);
        assert_eq!(err.code, "provider_failure");
        assert!(!err.message.contains("SECRET-DETAIL"));

        let kept = stored_account(&path, "work").unwrap();
        assert!(kept.registration.is_some());
        assert_eq!(kept.twitter.auth_token, "auth-work");
        assert!(notifications::history_path_for(&path, "work").is_file());
        wait_for("the receiver of work again", || {
            fake.active_for("work") == 1
        })
        .await;
        assert_eq!(fake.active_for("other"), 1);
        assert_eq!(work.status().unwrap().state, StateName::Connected);
        assert!(accounts.get("work").is_ok());
        assert_eq!(ids(&accounts), ["other", "work"]);

        // The account is fully usable afterwards, and a retry succeeds once X agrees.
        work.update_settings(keep_everything()).await.unwrap();
        fake.fail_unregister.store(false, Ordering::SeqCst);
        accounts.delete("work").await.unwrap();
        assert!(stored_account(&path, "work").is_none());
        assert!(!notifications::history_path_for(&path, "work").exists());
        assert_eq!(fake.active_for("other"), 1);
    }

    #[tokio::test]
    async fn deletion_cancels_a_running_bulk_job() {
        let (fake, accounts, path) = rig_with(Environment::default(), slow_following());
        two_page_fake(&fake);
        let work = account(&accounts, "work");
        work.update_settings(with_cookies("auth-work", ""))
            .await
            .unwrap();
        work.start_following_notifications().unwrap();
        wait_for("the hung update", || {
            fake.updates_in_flight.load(Ordering::SeqCst) == 1
        })
        .await;

        accounts.delete("work").await.unwrap();
        assert_eq!(fake.updates_in_flight.load(Ordering::SeqCst), 0);
        assert_eq!(
            work.following_notifications().state,
            following::JobState::Cancelled
        );
        assert!(stored_account(&path, "work").is_none());
    }

    #[tokio::test]
    async fn concurrent_deletes_and_operations_do_not_resurrect_the_account() {
        let (fake, accounts, path) = rig();
        let accounts = Arc::new(accounts);
        let work = account(&accounts, "work");
        work.update_settings(with_cookies("auth-work", ""))
            .await
            .unwrap();
        work.register().await.unwrap();
        let payload = serde_json::json!({"data": {"type": "tweet"}});
        notifications::append_as_for(
            &path,
            "work",
            "n-1",
            &payload,
            notifications::NotificationKind::Tweet,
        )
        .unwrap();
        // The unregistration takes long enough for the other calls to arrive during it.
        fake.unregister_delay_ms.store(150, Ordering::SeqCst);

        let first = {
            let accounts = Arc::clone(&accounts);
            tokio::spawn(async move { accounts.delete("work").await })
        };
        wait_for("the unregistration", || {
            work.status()
                .is_ok_and(|s| s.state == StateName::Unregistering)
        })
        .await;

        let second = accounts.delete("work").await.unwrap_err();
        assert_eq!(second.code, "busy");
        assert_eq!(work.register().await.unwrap_err().code, "busy");
        assert_eq!(
            work.update_settings(keep_everything())
                .await
                .unwrap_err()
                .code,
            "busy"
        );
        assert_eq!(work.unregister().await.unwrap_err().code, "busy");
        // The id cannot be re-created while it is still being removed.
        assert_eq!(accounts.create("work").unwrap_err().code, "busy");

        first.await.unwrap().unwrap();
        assert_eq!(fake.unregisters.load(Ordering::SeqCst), 1);
        assert!(stored_account(&path, "work").is_none());
        assert!(!notifications::history_path_for(&path, "work").exists());
        // Calls that arrive after the deletion see nothing to resurrect.
        assert_eq!(work.register().await.unwrap_err().code, "account_not_found");
        assert_eq!(
            work.update_settings(with_cookies("again", ""))
                .await
                .unwrap_err()
                .code,
            "account_not_found"
        );
        assert_eq!(
            accounts.delete("work").await.unwrap_err().code,
            "account_not_found"
        );
        assert!(stored_account(&path, "work").is_none());
        assert_eq!(fake.active_for("work"), 0);
    }

    fn tweet() -> serde_json::Value {
        serde_json::json!({"data": {"type": "tweet"}})
    }

    fn record(path: &Path, id: &str, entry: &str) {
        notifications::append_as_for(
            path,
            id,
            entry,
            &tweet(),
            notifications::NotificationKind::Tweet,
        )
        .unwrap();
    }

    /// Everything a stale handle could still try, and what must come of it.
    async fn assert_dead_handle(old: &Service<Arc<FakeBackend>>) {
        assert_eq!(old.settings().unwrap_err().code, "account_not_found");
        assert_eq!(old.status().unwrap_err().code, "account_not_found");
        assert_eq!(old.registration().unwrap_err().code, "account_not_found");
        assert_eq!(
            old.update_settings(with_cookies("stale", "https://stale.example/"))
                .await
                .unwrap_err()
                .code,
            "account_not_found"
        );
        assert_eq!(old.register().await.unwrap_err().code, "account_not_found");
        assert_eq!(
            old.unregister().await.unwrap_err().code,
            "account_not_found"
        );
        assert_eq!(
            old.notifications(None, 10).await.unwrap_err().code,
            "account_not_found"
        );
        assert_eq!(
            old.start_following_notifications().unwrap_err().code,
            "account_not_found"
        );
        assert_eq!(
            old.validate_credentials().await.unwrap_err().code,
            "account_not_found"
        );
    }

    #[tokio::test]
    async fn an_id_cannot_be_recreated_while_its_old_history_is_being_deleted() {
        let (fake, accounts, path) = rig();
        let accounts = Arc::new(accounts);
        let old = account(&accounts, "work");
        old.update_settings(with_cookies("auth-old", "https://old.example/"))
            .await
            .unwrap();
        record(&path, "work", "old-1");
        let db = notifications::history_path_for(&path, "work");
        assert!(db.is_file());

        // Hold the deletion after the account is marked removed and before its files are deleted.
        let gate = old.cleanup_gate.lock().await;
        let deleting = {
            let accounts = Arc::clone(&accounts);
            tokio::spawn(async move { accounts.delete("work").await })
        };
        wait_for("the deletion to mark the account removed", || {
            old.is_removed()
        })
        .await;
        assert!(stored_account(&path, "work").is_none());
        assert!(db.is_file(), "the files are still there at this point");

        // Re-creating the id is refused, and nothing is written for it.
        assert_eq!(accounts.create("work").unwrap_err().code, "busy");
        assert!(stored_account(&path, "work").is_none());
        assert!(Config::load(&path).unwrap().accounts.is_empty());
        // Nobody can find the account being deleted, nor use the handle taken earlier.
        assert_eq!(
            accounts.get("work").err().unwrap().code,
            "account_not_found"
        );
        assert_eq!(
            accounts.delete("work").await.unwrap_err().code,
            "account_not_found"
        );
        assert!(ids(&accounts).is_empty());
        assert_eq!(
            old.update_settings(with_cookies("sneaky", "https://sneaky.example/"))
                .await
                .unwrap_err()
                .code,
            "busy"
        );
        assert_eq!(old.register().await.unwrap_err().code, "busy");
        assert_eq!(old.unregister().await.unwrap_err().code, "busy");
        // A history read waits for the deletion instead of reopening the database being removed.
        let read = {
            let old = Arc::clone(&old);
            tokio::spawn(async move { old.notifications(None, 10).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!read.is_finished());
        assert!(stored_account(&path, "work").is_none());

        drop(gate);
        deleting.await.unwrap().unwrap();
        assert_eq!(read.await.unwrap().unwrap_err().code, "account_not_found");
        assert!(!db.exists());
        assert!(stored_account(&path, "work").is_none());
        assert_eq!(fake.unregisters.load(Ordering::SeqCst), 0);

        // Now the id is free again, and the finished cleanup cannot reach the new account.
        accounts.create("work").unwrap();
        record(&path, "work", "new-1");
        let new = accounts.get("work").unwrap();
        assert!(!Arc::ptr_eq(&old, &new));
        assert_dead_handle(&old).await;
        assert!(db.is_file());
        assert_eq!(
            new.notifications(None, 10)
                .await
                .unwrap()
                .items
                .iter()
                .map(|i| i.id.as_str())
                .collect::<Vec<_>>(),
            ["new-1"]
        );
        let kept = stored_account(&path, "work").unwrap();
        assert!(kept.twitter.is_empty() && kept.webhook_endpoint.is_none());
    }

    #[tokio::test]
    async fn an_id_cannot_be_recreated_between_the_config_removal_and_the_removed_mark() {
        let (_, accounts, path) = rig();
        let accounts = Arc::new(accounts);
        let old = account(&accounts, "work");
        old.update_settings(with_cookies("auth-old", ""))
            .await
            .unwrap();
        record(&path, "work", "old-1");

        // A running history read keeps the deletion waiting for the exclusive history lock, which
        // it needs only after the config entry is gone.
        let reader = old.history.read().await;
        let deleting = {
            let accounts = Arc::clone(&accounts);
            tokio::spawn(async move { accounts.delete("work").await })
        };
        wait_for("the config entry to be removed", || {
            stored_account(&path, "work").is_none()
        })
        .await;
        assert!(!old.is_removed());

        assert_eq!(accounts.create("work").unwrap_err().code, "busy");
        assert!(stored_account(&path, "work").is_none());
        // The registry may still hand out the handle here, but it finds no account behind it.
        assert_eq!(old.settings().unwrap_err().code, "account_not_found");
        assert!(ids(&accounts).is_empty());
        assert_eq!(
            old.update_settings(with_cookies("sneaky", ""))
                .await
                .unwrap_err()
                .code,
            "busy"
        );
        assert!(stored_account(&path, "work").is_none());

        drop(reader);
        deleting.await.unwrap().unwrap();
        assert!(!notifications::history_path_for(&path, "work").exists());
        accounts.create("work").unwrap();
        assert_dead_handle(&old).await;
    }

    #[tokio::test]
    async fn a_late_registry_cleanup_of_a_deleted_service_keeps_the_recreated_one() {
        let (_, accounts, path) = rig();
        let old = account(&accounts, "work");
        record(&path, "work", "old-1");

        // `Accounts::delete` removes the tombstone only after `Service::delete` returned; run the
        // two halves apart so the id can be created again in between.
        old.delete().await.unwrap();
        assert!(old.is_removed());
        accounts.create("work").unwrap();
        let new = accounts.get("work").unwrap();
        assert!(!Arc::ptr_eq(&old, &new));
        new.update_settings(with_cookies("auth-new", "https://new.example/"))
            .await
            .unwrap();
        record(&path, "work", "new-1");

        accounts.forget_removed("work", &old);

        let still = accounts.get("work").unwrap();
        assert!(Arc::ptr_eq(&new, &still));
        assert!(still.settings().unwrap().configured);
        assert_eq!(
            stored_account(&path, "work").unwrap().twitter.auth_token,
            "auth-new"
        );
        assert!(notifications::history_path_for(&path, "work").is_file());
        assert_dead_handle(&old).await;
        assert_eq!(ids(&accounts), ["work"]);

        // Without a new service in its place, the same call does remove the tombstone.
        let other = account(&accounts, "other");
        other.delete().await.unwrap();
        accounts.forget_removed("other", &other);
        assert!(accounts.services.read().unwrap().get("other").is_none());
        assert_eq!(
            accounts.get("other").err().unwrap().code,
            "account_not_found"
        );
    }

    #[tokio::test]
    async fn deleting_one_account_leaves_a_concurrent_edit_of_another_intact() {
        let (_, accounts, path) = rig();
        let accounts = Arc::new(accounts);
        let a = account(&accounts, "aaa");
        let b = account(&accounts, "bbb");
        a.update_settings(with_cookies("auth-a", "")).await.unwrap();
        b.update_settings(with_cookies("auth-b", "")).await.unwrap();

        let (deleted, edited) = tokio::join!(
            accounts.delete("aaa"),
            b.update_settings(with_cookies("auth-b2", "https://example.com/b2")),
        );
        deleted.unwrap();
        edited.unwrap();
        assert!(stored_account(&path, "aaa").is_none());
        let kept = stored_account(&path, "bbb").unwrap();
        assert_eq!(kept.twitter.auth_token, "auth-b2");
        assert_eq!(
            kept.webhook_endpoint.as_deref(),
            Some("https://example.com/b2")
        );
    }

    #[tokio::test]
    async fn a_history_cleanup_failure_is_reported_after_the_account_is_gone() {
        let (_, accounts, path) = rig();
        let work = account(&accounts, "work");
        // A directory where the database should be makes the removal fail.
        let db = notifications::history_path_for(&path, "work");
        std::fs::create_dir_all(db.join("blocker")).unwrap();

        let err = accounts.delete("work").await.unwrap_err();
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(err.code, "history_cleanup_failed");
        assert!(stored_account(&path, "work").is_none());
        assert_eq!(work.settings().unwrap_err().code, "account_not_found");
        assert_eq!(
            accounts.get("work").err().unwrap().code,
            "account_not_found"
        );
    }

    fn side_file(db: &Path, suffix: &str) -> PathBuf {
        let mut name = db.as_os_str().to_os_string();
        name.push(suffix);
        PathBuf::from(name)
    }

    #[tokio::test]
    async fn create_refuses_to_inherit_history_files_left_on_disk() {
        for id in ["work", DEFAULT_ACCOUNT_ID] {
            for suffix in ["", "-journal", "-wal", "-shm"] {
                let (_, accounts, path) = rig();
                let other = account(&accounts, "other");
                other
                    .update_settings(with_cookies("auth-other", ""))
                    .await
                    .unwrap();
                let config_before = std::fs::read(&path).unwrap();
                let leftover = side_file(&notifications::history_path_for(&path, id), suffix);
                std::fs::write(&leftover, b"somebody's history").unwrap();

                let err = accounts.create(id).unwrap_err();
                let what = format!("{id} {suffix:?}");
                assert_eq!(err.status, StatusCode::CONFLICT, "{what}");
                assert_eq!(err.code, "history_cleanup_required", "{what}");
                // The message says what to do, exposes no path of this machine, and is short
                // enough for the WebUI to show as it is (it drops messages over 200 characters).
                assert!(!err.message.contains(path.to_str().unwrap()), "{what}");
                assert!(err.message.chars().count() <= 200, "{what}");

                // Nothing changed: not the config, not the leftover file, not the registry.
                assert_eq!(std::fs::read(&path).unwrap(), config_before, "{what}");
                assert_eq!(
                    std::fs::read(&leftover).unwrap(),
                    b"somebody's history",
                    "{what}"
                );
                assert!(stored_account(&path, id).is_none(), "{what}");
                assert_eq!(accounts.get(id).err().unwrap().code, "account_not_found");
                assert_eq!(ids(&accounts), ["other"], "{what}");
                assert!(accounts.get("other").is_ok());

                // Once the person has removed the files, the id can be created, and starts empty.
                std::fs::remove_file(&leftover).unwrap();
                accounts.create(id).unwrap();
                assert!(stored_account(&path, id).is_some(), "{what}");
                let page = accounts
                    .get(id)
                    .unwrap()
                    .notifications(None, 10)
                    .await
                    .unwrap();
                assert!(page.items.is_empty(), "{what}");
            }
        }
    }

    #[tokio::test]
    async fn create_without_a_config_file_leaves_none_behind_when_it_refuses() {
        let (_, accounts, path) = rig();
        let db = notifications::history_path_for(&path, DEFAULT_ACCOUNT_ID);
        std::fs::write(&db, b"history of an older single-account setup").unwrap();
        assert_eq!(
            accounts.create(DEFAULT_ACCOUNT_ID).unwrap_err().code,
            "history_cleanup_required"
        );
        assert!(!path.exists(), "a refused create must not write the config");
        assert_eq!(
            std::fs::read(&db).unwrap(),
            b"history of an older single-account setup"
        );
        // Another id is unaffected by the default account's file.
        accounts.create("work").unwrap();
        assert!(stored_account(&path, "work").is_some());
    }

    #[tokio::test]
    async fn an_existing_account_stays_a_duplicate_even_when_history_files_exist() {
        let (_, accounts, path) = rig();
        account(&accounts, "work");
        record(&path, "work", "n-1");
        assert!(notifications::history_path_for(&path, "work").is_file());
        let config_before = std::fs::read(&path).unwrap();

        let err = accounts.create("work").unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);
        assert_eq!(err.code, "account_exists");
        assert_eq!(std::fs::read(&path).unwrap(), config_before);
        assert!(notifications::history_path_for(&path, "work").is_file());
    }

    #[tokio::test]
    async fn a_deletion_whose_cleanup_failed_blocks_the_id_until_the_files_are_removed() {
        let (_, accounts, path) = rig();
        account(&accounts, "work");
        // A directory where the database should be makes the cleanup fail; it also stays behind.
        let db = notifications::history_path_for(&path, "work");
        std::fs::create_dir_all(db.join("blocker")).unwrap();
        let wal = side_file(&db, "-wal");
        std::fs::write(&wal, b"wal").unwrap();

        let err = accounts.delete("work").await.unwrap_err();
        assert_eq!(err.code, "history_cleanup_failed");
        assert!(stored_account(&path, "work").is_none());
        // The removal of the sibling files was still attempted.
        assert!(!wal.exists());
        assert!(
            db.join("blocker").is_dir(),
            "unknown content is never removed"
        );

        let config_before = std::fs::read(&path).unwrap();
        assert_eq!(
            accounts.create("work").unwrap_err().code,
            "history_cleanup_required"
        );
        assert_eq!(std::fs::read(&path).unwrap(), config_before);
        assert!(db.join("blocker").is_dir());
        assert_eq!(
            accounts.get("work").err().unwrap().code,
            "account_not_found"
        );

        std::fs::remove_dir_all(&db).unwrap();
        accounts.create("work").unwrap();
        assert!(stored_account(&path, "work").is_some());
    }

    #[tokio::test]
    async fn startup_starts_every_registered_account_and_shutdown_stops_them_all() {
        let (fake, accounts, path) = rig();
        for (id, registered) in [("one", true), ("two", true), ("three", false)] {
            let service = account(&accounts, id);
            service
                .update_settings(with_cookies(&format!("auth-{id}"), ""))
                .await
                .unwrap();
            if registered {
                service.register().await.unwrap();
            }
        }
        accounts.shutdown().await;
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 0);

        // A fresh process: nothing cached, the config is the only source.
        let again = Accounts::new(Arc::clone(&fake), path.clone(), Environment::default())
            .with_timeout(Duration::from_millis(200));
        again.startup().await;
        wait_for("the registered receivers", || {
            fake.active_for("one") == 1 && fake.active_for("two") == 1
        })
        .await;
        assert_eq!(fake.active_for("three"), 0);
        assert_eq!(
            again
                .list()
                .unwrap()
                .accounts
                .iter()
                .map(|a| a.receiver_state)
                .collect::<Vec<_>>(),
            [
                StateName::Connected,
                StateName::Connected,
                StateName::Unregistered
            ]
        );

        again.shutdown().await;
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 0);
        assert_eq!(
            again.get("one").unwrap().status().unwrap().state,
            StateName::Stopped
        );
    }

    #[tokio::test]
    async fn the_account_list_reports_registration_and_receiver_state() {
        let (_, accounts, _) = rig();
        assert!(accounts.list().unwrap().accounts.is_empty());
        let one = account(&accounts, "one");
        account(&accounts, "two");
        one.update_settings(with_cookies("auth-one", ""))
            .await
            .unwrap();
        one.register().await.unwrap();
        wait_for("the receiver", || {
            one.status().is_ok_and(|s| s.state == StateName::Connected)
        })
        .await;
        assert_eq!(
            accounts.list().unwrap(),
            AccountListDto {
                accounts: vec![
                    AccountDto {
                        id: "one".into(),
                        registered: true,
                        receiver_state: StateName::Connected
                    },
                    AccountDto {
                        id: "two".into(),
                        registered: false,
                        receiver_state: StateName::Unconfigured
                    },
                ]
            }
        );
    }

    #[tokio::test]
    async fn a_malformed_config_fails_the_list_without_details() {
        let (_, accounts, path) = rig();
        std::fs::write(&path, "this is [not valid toml").unwrap();
        let err = accounts.list().unwrap_err();
        assert_eq!(err.code, "config_invalid");
        assert!(!err.message.contains("toml"));
    }

    // ---- environment variables (the `default` account only) ----

    fn env_with_cookies() -> Environment {
        Environment::new(Some("env-auth"), Some("env-ct0"), None, None).unwrap()
    }

    #[tokio::test]
    async fn environment_cookies_define_default_without_a_config_entry() {
        let (fake, accounts, path) = rig_with(env_with_cookies(), fast_following());
        assert_eq!(ids(&accounts), ["default"]);
        let default = accounts.get("default").unwrap();
        let settings = default.settings().unwrap();
        assert!(settings.configured && settings.auth_token_set && settings.ct0_set);
        assert_eq!(default.status().unwrap().state, StateName::Unregistered);
        assert!(!path.exists());

        // The environment's cookies are what reaches X, and they are never written to the file.
        default.register().await.unwrap();
        assert_eq!(*fake.registered_with.lock().unwrap(), ["env-auth"]);
        let stored = config::test_default_account(&path);
        assert!(stored.twitter.is_empty());
        assert!(stored.registration.is_some());
        assert!(!std::fs::read_to_string(&path).unwrap().contains("env-"));
        assert!(default.validate_credentials().await.unwrap().valid);
        assert_eq!(*fake.screen_name_creds.lock().unwrap(), ["env-auth"]);
    }

    #[tokio::test]
    async fn environment_cookies_cannot_be_overwritten_or_removed_from_the_webui() {
        let (_, accounts, path) = rig_with(env_with_cookies(), fast_following());
        let default = accounts.get("default").unwrap();

        let err = default
            .update_settings(with_cookies("typed", ""))
            .await
            .unwrap_err();
        assert_eq!(err.code, "credentials_from_environment");
        assert!(!path.exists());
        // The webhook and the filter remain editable, and creating the config entry that way
        // does not copy the environment's cookies into it.
        let mut update = keep_everything();
        update.webhook_secret = Some("typed-secret".into());
        update.notification_filter = filter_input(false, true, true);
        let dto = default.update_settings(update).await.unwrap();
        assert!(dto.configured && dto.webhook_secret_set && !dto.notification_filter.tweet);
        let stored = config::test_default_account(&path);
        assert!(stored.twitter.is_empty());
        assert_eq!(stored.webhook_secret.as_deref(), Some("typed-secret"));

        let err = accounts.delete("default").await.unwrap_err();
        assert_eq!(err.status, StatusCode::CONFLICT);
        assert_eq!(err.code, "account_from_environment");
        assert!(stored_account(&path, "default").is_some());
        assert_eq!(
            accounts.create("default").unwrap_err().code,
            "account_exists"
        );
        assert_eq!(ids(&accounts), ["default"]);
    }

    #[tokio::test]
    async fn environment_values_apply_to_default_and_never_to_other_accounts() {
        let env = Environment::new(
            Some("env-auth"),
            Some("env-ct0"),
            Some("https://env.example/hook"),
            Some("env-secret"),
        )
        .unwrap();
        let (fake, accounts, path) = rig_with(env, fast_following());
        let default = accounts.get("default").unwrap();
        let work = account(&accounts, "work");

        let d = default.settings().unwrap();
        assert_eq!(d.webhook_source, WebhookSource::Environment);
        assert_eq!(
            d.webhook_endpoint.as_deref(),
            Some("https://env.example/hook")
        );
        assert!(d.webhook_secret_set && d.configured);

        let w = work.settings().unwrap();
        assert_eq!(w.webhook_source, WebhookSource::None);
        assert_eq!(w.webhook_endpoint, None);
        assert!(!w.webhook_secret_set && !w.configured && !w.auth_token_set);
        assert_eq!(work.status().unwrap().state, StateName::Unconfigured);
        // Without cookies of its own it cannot register, even though the environment has some.
        assert_eq!(work.register().await.unwrap_err().code, "not_configured");
        assert!(fake.registered_with.lock().unwrap().is_empty());
        assert_eq!(
            work.validate_credentials().await.unwrap_err().code,
            "not_configured"
        );

        // The environment's endpoint wins for `default`, and the environment's secret cannot be replaced.
        let mut edit = keep_everything();
        edit.webhook_endpoint = "https://typed.example/hook".into();
        default.update_settings(edit).await.unwrap();
        assert_eq!(
            config::test_default_account(&path).webhook_endpoint,
            None,
            "the environment endpoint is not overwritten or copied"
        );
        let mut secret = keep_everything();
        secret.webhook_secret = Some("typed".into());
        assert_eq!(
            default.update_settings(secret).await.unwrap_err().code,
            "webhook_secret_from_environment"
        );
        // Another account edits its own webhook freely.
        let mut own = with_cookies("auth-work", "https://work.example/hook");
        own.webhook_secret = Some("work-secret".into());
        let w = work.update_settings(own).await.unwrap();
        assert_eq!(w.webhook_source, WebhookSource::Config);
        assert!(w.webhook_secret_set && w.configured);
        work.register().await.unwrap();
        assert_eq!(*fake.registered_with.lock().unwrap(), ["auth-work"]);
    }

    #[test]
    fn half_set_environment_cookies_are_rejected() {
        assert!(Environment::new(Some("a"), None, None, None).is_err());
        assert!(Environment::new(None, Some("c"), None, None).is_err());
        assert!(Environment::new(Some(" "), Some(" "), None, None).is_ok());
        assert!(Environment::new(None, None, Some("https://x"), None).is_ok());
    }

    #[tokio::test]
    async fn without_environment_default_is_an_ordinary_account() {
        let (_, accounts, _) = rig();
        assert!(ids(&accounts).is_empty());
        assert_eq!(
            accounts.get("default").err().unwrap().code,
            "account_not_found"
        );
        accounts.create("default").unwrap();
        accounts.delete("default").await.unwrap();
        assert!(ids(&accounts).is_empty());
    }

    #[test]
    fn dtos_never_contain_secrets() {
        let account = Account {
            twitter: TwitterConfig {
                auth_token: "AUTH-SECRET".into(),
                ct0: "CT0-SECRET".into(),
            },
            registration: Some(Registration {
                endpoint: "https://push.example/PRIVATE-ENDPOINT".into(),
                autopush: AutoPushSession {
                    uaid: "uaid".into(),
                    channel_id: "chan".into(),
                },
                keys: config::WebPushKeys {
                    public_key: vec![1, 2, 3],
                    private_key: vec![9, 8, 7],
                    auth_secret: vec![5, 5, 5],
                },
            }),
            webhook_endpoint: None,
            webhook_secret: Some("HOOK-SECRET".into()),
            notification_filter: NotificationFilter::default(),
            ..Account::new(DEFAULT_ACCOUNT_ID)
        };
        let json = serde_json::to_string(&settings_dto(&Environment::default(), &account)).unwrap()
            + &serde_json::to_string(&registration_dto(&account)).unwrap();
        for secret in [
            "AUTH-SECRET",
            "CT0-SECRET",
            "HOOK-SECRET",
            "PRIVATE-ENDPOINT",
            "private_key",
            "[9,8,7]",
        ] {
            assert!(!json.contains(secret), "{secret} leaked");
        }
        assert!(json.contains("\"auth_token_set\":true"));
        assert!(json.contains("\"webhook_secret_set\":true"));
    }
}
