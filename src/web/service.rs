//! Shared registration/settings service owning the managed receiver.

use super::error::ApiError;
use super::following::{FollowingJobs, FollowingNotificationsDto};
use crate::config::{
    self, AutoPushSession, Config, NotificationFilter, Registration, TwitterConfig, WebhookSource,
};
use crate::error::{AngelicAngelError, Result};
use crate::notifications::{self, NotificationPage};
use crate::push::PushSubscription;
use crate::status::{Counters, Operation, ReceiverState, Telemetry};
use crate::twitter::{FollowingPage, XApi, XError};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

const REMOTE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CREDENTIAL_LEN: usize = 1024;
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

/// Production backend talking to AutoPush, X and the real listener.
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
        registration: Registration,
        config_path: PathBuf,
        telemetry: Arc<Telemetry>,
    ) -> Result<()> {
        crate::listener::listen_managed(registration, &config_path, &telemetry).await
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

// ---- DTOs -----------------------------------------------------------------

#[derive(Debug, Serialize, PartialEq)]
pub struct SettingsDto {
    pub configured: bool,
    pub auth_token_set: bool,
    pub ct0_set: bool,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsUpdate {
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default)]
    pub ct0: Option<String>,
    pub webhook_endpoint: String,
    /// 省略時は保存済みの設定を保持する。
    #[serde(default)]
    pub notification_filter: Option<NotificationFilterInput>,
}

/// 更新では 3 区分すべての指定を必須にし、部分指定で他の区分が黙って有効に戻るのを防ぐ。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationFilterInput {
    pub tweet: bool,
    pub xchat_message_received: bool,
    pub other: bool,
}

impl From<NotificationFilterInput> for NotificationFilter {
    fn from(input: NotificationFilterInput) -> Self {
        Self {
            tweet: input.tweet,
            xchat_message_received: input.xchat_message_received,
            other: input.other,
        }
    }
}

fn settings_dto(config: Option<&Config>) -> SettingsDto {
    let stored = config.and_then(|c| c.webhook_endpoint.as_deref());
    let (webhook_endpoint, webhook_source) = config::resolve_webhook(stored);
    SettingsDto {
        configured: config.is_some(),
        auth_token_set: config.is_some_and(|c| !c.twitter.auth_token.is_empty()),
        ct0_set: config.is_some_and(|c| !c.twitter.ct0.is_empty()),
        webhook_endpoint,
        webhook_source,
        credentials_locked: config.is_some_and(|c| c.registration.is_some()),
        notification_filter: config.map(|c| c.notification_filter).unwrap_or_default(),
    }
}

fn registration_dto(config: Option<&Config>) -> RegistrationDto {
    let registration = config.and_then(|c| c.registration.as_ref());
    RegistrationDto {
        registered: registration.is_some(),
        uaid: registration.map(|r| r.autopush.uaid.clone()),
        channel_id: registration.map(|r| r.autopush.channel_id.clone()),
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

pub struct Service<B: Backend> {
    backend: Arc<B>,
    config_path: PathBuf,
    telemetry: Arc<Telemetry>,
    remote_timeout: Duration,
    /// Serializes mutations and owns the receiver task handle.
    receiver: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    /// Bulk notification job; independent of `receiver`, so it never stops the receiver.
    following: FollowingJobs,
}

impl<B: Backend> Service<B> {
    pub fn new(backend: B, config_path: PathBuf) -> Self {
        Self {
            backend: Arc::new(backend),
            config_path,
            telemetry: Arc::new(Telemetry::default()),
            remote_timeout: REMOTE_TIMEOUT,
            receiver: tokio::sync::Mutex::new(None),
            following: FollowingJobs::default(),
        }
    }

    #[cfg(test)]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.remote_timeout = timeout;
        self
    }

    #[cfg(test)]
    pub fn with_following_timings(mut self, timings: super::following::Timings) -> Self {
        self.following = FollowingJobs::new(timings);
        self
    }

    fn load(&self) -> std::result::Result<Option<Config>, ApiError> {
        Ok(Config::load_optional(&self.config_path)?)
    }

    /// Starts the receiver when a registration is already present.
    pub async fn startup(&self) {
        let mut slot = self.receiver.lock().await;
        match Config::load_optional(&self.config_path) {
            Ok(Some(Config {
                registration: Some(registration),
                ..
            })) => self.start_receiver(&mut slot, registration).await,
            Ok(_) => {},
            Err(_) => tracing::warn!("configuration could not be read at startup"),
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
        *slot = Some(tokio::spawn(async move {
            let result = backend
                .run_receiver(registration, config_path, Arc::clone(&telemetry))
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
                tracing::warn!(error = %error, "{what} failed");
                Err(ApiError::provider(format!("{what} failed.")))
            },
            Err(_) => {
                tracing::warn!("{what} timed out");
                Err(ApiError::provider(format!("{what} timed out.")))
            },
        }
    }

    // ---- reads ----

    pub fn settings(&self) -> std::result::Result<SettingsDto, ApiError> {
        Ok(settings_dto(self.load()?.as_ref()))
    }

    pub fn registration(&self) -> std::result::Result<RegistrationDto, ApiError> {
        Ok(registration_dto(self.load()?.as_ref()))
    }

    pub fn status(&self) -> StatusDto {
        let (receiver, operation, mut counters) = self.telemetry.snapshot();
        let (state, registered) = match (operation, Config::load_optional(&self.config_path)) {
            (_, Err(_)) => {
                counters.last_error = Some("configuration is invalid".to_string());
                (StateName::Error, false)
            },
            (Some(Operation::Registering), Ok(c)) => (
                StateName::Registering,
                c.is_some_and(|c| c.registration.is_some()),
            ),
            (Some(Operation::Unregistering), Ok(c)) => (
                StateName::Unregistering,
                c.is_some_and(|c| c.registration.is_some()),
            ),
            (None, Ok(None)) => (StateName::Unconfigured, false),
            (None, Ok(Some(c))) if c.registration.is_none() => (StateName::Unregistered, false),
            (None, Ok(Some(_))) => (
                match receiver {
                    ReceiverState::Stopped => StateName::Stopped,
                    ReceiverState::Connecting => StateName::Connecting,
                    ReceiverState::Connected => StateName::Connected,
                    ReceiverState::Retrying => StateName::Retrying,
                    ReceiverState::Error => StateName::Error,
                },
                true,
            ),
        };
        StatusDto {
            state,
            registered,
            counters,
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
        let path = self.config_path.clone();
        tokio::task::spawn_blocking(move || {
            let filter = Config::load_optional(&path)?
                .map(|c| c.notification_filter)
                .unwrap_or_default();
            notifications::list_filtered(&path, before.as_deref(), limit, &filter)
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

    /// Saved X cookies, or a 400 when either one is missing.
    fn saved_credentials(&self) -> std::result::Result<TwitterConfig, ApiError> {
        self.load()?
            .map(|c| c.twitter)
            .filter(|t| !t.auth_token.is_empty() && !t.ct0.is_empty())
            .ok_or_else(|| {
                ApiError::invalid("not_configured", "Save the credentials in settings first.")
            })
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
                tracing::warn!(error = %error, "cookie check failed");
                Err(ApiError::provider(format!(
                    "The cookies could not be checked. {error}"
                )))
            },
            Err(_) => {
                tracing::warn!("cookie check timed out");
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
        let overridden = config::resolve_webhook(None).1 == WebhookSource::Environment;
        let auth_token = validate_credential("auth_token", update.auth_token)?;
        let ct0 = validate_credential("ct0", update.ct0)?;
        let webhook = if overridden {
            None
        } else {
            validate_webhook(&update.webhook_endpoint)?
        };

        let _slot = self.receiver.try_lock().map_err(|_| ApiError::busy())?;

        let saved = Config::modify(&self.config_path, |current| {
            let config = match current {
                None => {
                    let (Some(auth_token), Some(ct0)) = (auth_token, ct0) else {
                        return Err(ApiError::invalid(
                            "credentials_required",
                            "Both auth_token and ct0 are required for the first setup.",
                        ));
                    };
                    Config {
                        twitter: TwitterConfig { auth_token, ct0 },
                        registration: None,
                        webhook_endpoint: webhook,
                        notification_filter: update
                            .notification_filter
                            .map(NotificationFilter::from)
                            .unwrap_or_default(),
                    }
                },
                Some(mut config) => {
                    let changed = auth_token
                        .as_ref()
                        .is_some_and(|v| *v != config.twitter.auth_token)
                        || ct0.as_ref().is_some_and(|v| *v != config.twitter.ct0);
                    if changed && config.registration.is_some() {
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
                    if let Some(v) = auth_token {
                        config.twitter.auth_token = v;
                    }
                    if let Some(v) = ct0 {
                        config.twitter.ct0 = v;
                    }
                    if !overridden {
                        config.webhook_endpoint = webhook;
                    }
                    if let Some(filter) = update.notification_filter {
                        config.notification_filter = filter.into();
                    }
                    config
                },
            };
            Ok((Some(config.clone()), config))
        })?;
        Ok(settings_dto(Some(&saved)))
    }

    pub async fn register(&self) -> std::result::Result<RegistrationDto, ApiError> {
        let mut slot = self.receiver.try_lock().map_err(|_| ApiError::busy())?;
        self.ensure_no_following_job()?;
        let config = self.load()?.ok_or_else(|| {
            ApiError::conflict("not_configured", "Save the credentials in settings first.")
        })?;

        if let Some(registration) = config.registration.clone() {
            let running = slot.as_ref().is_some_and(|h| !h.is_finished());
            if !running {
                self.start_receiver(&mut slot, registration).await;
            }
            return Ok(registration_dto(Some(&config)));
        }

        let _operation = OperationGuard::new(&self.telemetry, Operation::Registering);
        self.stop_receiver(&mut slot).await;

        let subscription = self
            .remote("AutoPush registration", self.backend.subscribe())
            .await?;
        if let Err(error) = self
            .remote(
                "X push registration",
                self.backend
                    .twitter_register(&config.twitter, &subscription),
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
        let saved = Config::modify(&self.config_path, |current| {
            let mut config = current.ok_or_else(|| {
                ApiError::conflict("not_configured", "Save the credentials in settings first.")
            })?;
            config.registration = Some(registration.clone());
            Ok::<_, ApiError>((Some(config.clone()), config))
        });
        let saved = match saved {
            Ok(saved) => saved,
            Err(error) => {
                self.cleanup_subscription(&subscription.autopush).await;
                return Err(error);
            },
        };

        self.start_receiver(&mut slot, registration).await;
        Ok(registration_dto(Some(&saved)))
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
        let config = self.load()?;
        let Some(registration) = config.as_ref().and_then(|c| c.registration.clone()) else {
            self.stop_receiver(&mut slot).await;
            return Ok(registration_dto(config.as_ref()));
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

        let saved = Config::modify(&self.config_path, |current| {
            Ok::<_, ApiError>(match current {
                Some(mut config) => {
                    config.registration = None;
                    (Some(config.clone()), Some(config))
                },
                None => (None, None),
            })
        });
        match saved {
            Ok(saved) => Ok(registration_dto(saved.as_ref())),
            Err(error) => {
                self.start_receiver(&mut slot, registration).await;
                Err(error)
            },
        }
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
        pub hang_twitter: AtomicBool,
        pub subscribes: AtomicUsize,
        pub unregisters: AtomicUsize,
        /// Receivers currently alive (must never exceed one).
        pub active_receivers: Arc<AtomicUsize>,
        pub max_active: Arc<AtomicUsize>,
        pub started: AtomicUsize,
        pub events: Mutex<Vec<String>>,
    }

    struct ActiveGuard(Arc<AtomicUsize>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
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

        async fn twitter_register(&self, _: &TwitterConfig, _: &PushSubscription) -> Result<()> {
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
            if self.fail_unregister.load(Ordering::SeqCst) {
                return Err(fail("unregister"));
            }
            Ok(())
        }

        async fn run_receiver(
            &self,
            _: Registration,
            _: PathBuf,
            telemetry: Arc<Telemetry>,
        ) -> Result<()> {
            self.started.fetch_add(1, Ordering::SeqCst);
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
    use super::fake::FakeBackend;
    use super::*;
    use std::sync::atomic::Ordering;

    fn setup() -> (Arc<FakeBackend>, Service<Arc<FakeBackend>>, PathBuf) {
        let dir = config::test_temp_dir();
        let path = dir.join("c.toml");
        let fake = Arc::new(FakeBackend::default());
        let service =
            Service::new(Arc::clone(&fake), path.clone()).with_timeout(Duration::from_millis(200));
        (fake, service, path)
    }

    fn first_setup() -> SettingsUpdate {
        SettingsUpdate {
            auth_token: Some("auth-secret".into()),
            ct0: Some("ct0-secret".into()),
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
        assert_eq!(service.status().state, StateName::Unconfigured);

        let dto = service.update_settings(first_setup()).await.unwrap();
        assert!(dto.configured && dto.auth_token_set && dto.ct0_set);
        assert_eq!(service.status().state, StateName::Unregistered);
    }

    #[tokio::test]
    async fn blank_credentials_retain_and_url_is_validated() {
        let (_, service, path) = setup();
        service.update_settings(first_setup()).await.unwrap();
        service
            .update_settings(SettingsUpdate {
                auth_token: None,
                ct0: Some(String::new()),
                webhook_endpoint: String::new(),
                notification_filter: None,
            })
            .await
            .unwrap();
        let stored = Config::load(&path).unwrap();
        assert_eq!(stored.twitter.auth_token, "auth-secret");
        assert_eq!(stored.twitter.ct0, "ct0-secret");
        assert!(stored.webhook_endpoint.is_none());

        for bad in ["ftp://x/y", "not a url", "javascript:alert(1)"] {
            let err = service
                .update_settings(SettingsUpdate {
                    auth_token: None,
                    ct0: None,
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
                webhook_endpoint: "https://example.com/new".into(),
                notification_filter: None,
            })
            .await
            .unwrap();
        assert_eq!(
            Config::load(&path).unwrap().webhook_endpoint.as_deref(),
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
        assert_eq!(service.status().state, StateName::Unregistered);
    }

    #[tokio::test]
    async fn remote_failures_are_redacted_and_do_not_leave_registration() {
        let (fake, service, path) = setup();
        service.update_settings(first_setup()).await.unwrap();

        fake.fail_twitter.store(true, Ordering::SeqCst);
        let err = service.register().await.unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_GATEWAY);
        assert!(!err.message.contains("SECRET-DETAIL"));
        assert!(Config::load(&path).unwrap().registration.is_none());
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
        assert!(Config::load(&path).unwrap().registration.is_some());
        tokio::task::yield_now().await;
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 1);
        assert_eq!(fake.max_active.load(Ordering::SeqCst), 1);
        assert!(service.status().registered);
    }

    #[tokio::test]
    async fn startup_starts_receiver_and_shutdown_joins_it() {
        let (fake, service, _) = setup();
        service.update_settings(first_setup()).await.unwrap();
        service.register().await.unwrap();
        service.shutdown().await;
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 0);
        assert_eq!(service.status().state, StateName::Stopped);

        service.startup().await;
        tokio::task::yield_now().await;
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 1);
        service.shutdown().await;
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn concurrent_mutation_is_rejected_as_busy() {
        let (fake, service, _) = setup();
        let service = Arc::new(service);
        service.update_settings(first_setup()).await.unwrap();
        fake.hang_twitter.store(true, Ordering::SeqCst);
        let slow = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.register().await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(service.status().state, StateName::Registering);
        let err = service.unregister().await.unwrap_err();
        assert_eq!(err.code, "busy");
        let _ = slow.await;
    }

    fn fast_following() -> super::super::following::Timings {
        super::super::following::Timings {
            request_timeout: Duration::from_millis(150),
            interval: Duration::from_millis(1),
            max_duration: Duration::from_secs(30),
        }
    }

    fn setup_following() -> (Arc<FakeBackend>, Arc<Service<Arc<FakeBackend>>>, PathBuf) {
        let (fake, service, path) = setup();
        let service = Arc::new(service.with_following_timings(fast_following()));
        fake.pages.lock().unwrap().insert(
            -1,
            Ok(FollowingPage {
                ids: vec!["1".into(), "2".into(), "3".into()],
                next_cursor: 0,
            }),
        );
        (fake, service, path)
    }

    /// Job requests never time out on their own, so a hung update stays in flight until cancelled.
    fn setup_slow_following() -> (Arc<FakeBackend>, Service<Arc<FakeBackend>>, PathBuf) {
        let dir = config::test_temp_dir();
        let path = dir.join("c.toml");
        let fake = Arc::new(FakeBackend::default());
        fake.pages.lock().unwrap().insert(
            -1,
            Ok(FollowingPage {
                ids: vec!["1".into(), "2".into()],
                next_cursor: 0,
            }),
        );
        fake.hang_updates.lock().unwrap().push("1".to_string());
        let slow = super::super::following::Timings {
            request_timeout: Duration::from_secs(30),
            ..fast_following()
        };
        let service = Service::new(Arc::clone(&fake), path.clone())
            .with_timeout(Duration::from_millis(200))
            .with_following_timings(slow);
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
                super::super::following::JobState::Loading
                    | super::super::following::JobState::Running
                    | super::super::following::JobState::Cancelling
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
            super::super::following::JobState::Idle
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
        assert!(service.status().registered);
        assert_eq!(service.status().state, StateName::Connected);
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
        assert_eq!(done.state, super::super::following::JobState::Failed);
        assert_eq!(done.remaining_count, Some(3));
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 1);
        assert_eq!(service.status().state, StateName::Connected);
    }

    #[tokio::test]
    async fn credentials_cannot_change_while_the_bulk_job_runs() {
        let (fake, service, path) = setup_following();
        service.update_settings(first_setup()).await.unwrap();
        fake.hang_updates.lock().unwrap().push("1".to_string());
        let slow = super::super::following::Timings {
            request_timeout: Duration::from_secs(30),
            ..fast_following()
        };
        let service = Arc::new(
            Service::new(Arc::clone(&fake), path.clone())
                .with_timeout(Duration::from_millis(200))
                .with_following_timings(slow),
        );
        service.start_following_notifications().unwrap();
        let err = service.start_following_notifications().unwrap_err();
        assert_eq!(err.code, "following_job_active");

        let err = service
            .update_settings(SettingsUpdate {
                auth_token: Some("other".into()),
                ct0: None,
                webhook_endpoint: String::new(),
                notification_filter: None,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code, "following_job_active");
        assert_eq!(
            Config::load(&path).unwrap().twitter.auth_token,
            "auth-secret"
        );
        service
            .update_settings(SettingsUpdate {
                auth_token: None,
                ct0: None,
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
            service.status().state == StateName::Registering
        })
        .await;
        let err = service.start_following_notifications().unwrap_err();
        assert_eq!(err.code, "busy");
        let _ = slow.await;
        assert_eq!(
            service.following_notifications().state,
            super::super::following::JobState::Idle
        );
    }

    #[tokio::test]
    async fn shutdown_cancels_the_bulk_job_and_stops_the_receiver() {
        let dir = config::test_temp_dir();
        let fake = Arc::new(FakeBackend::default());
        fake.pages.lock().unwrap().insert(
            -1,
            Ok(FollowingPage {
                ids: vec!["1".into(), "2".into()],
                next_cursor: 0,
            }),
        );
        fake.hang_updates.lock().unwrap().push("1".to_string());
        let slow = super::super::following::Timings {
            request_timeout: Duration::from_secs(30),
            ..fast_following()
        };
        let service =
            Service::new(Arc::clone(&fake), dir.join("c.toml")).with_following_timings(slow);
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
            super::super::following::JobState::Cancelled
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

    fn filter_input(tweet: bool, chat: bool, other: bool) -> Option<NotificationFilterInput> {
        Some(NotificationFilterInput {
            tweet,
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
        assert!(Config::load(&path).unwrap().webhook_endpoint.is_some());
        assert!(!Config::load(&path).unwrap().notification_filter.tweet);

        // 省略した更新は保存済みのフィルターを変えない。
        let dto = service
            .update_settings(SettingsUpdate {
                auth_token: None,
                ct0: None,
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
        assert!(Config::load(&path).unwrap().webhook_endpoint.is_none());

        let dto = service
            .update_settings(SettingsUpdate {
                auth_token: None,
                ct0: None,
                webhook_endpoint: String::new(),
                notification_filter: filter_input(true, false, false),
            })
            .await
            .unwrap();
        assert_eq!(
            Config::load(&path).unwrap().notification_filter,
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
        assert!(Config::load(&path).unwrap().registration.is_some());
        assert_eq!(fake.unregisters.load(Ordering::SeqCst), 0);
        assert_eq!(fake.active_receivers.load(Ordering::SeqCst), 1);
        service
            .update_settings(SettingsUpdate {
                auth_token: None,
                ct0: None,
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

    #[test]
    fn dtos_never_contain_secrets() {
        let config = Config {
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
            notification_filter: NotificationFilter::default(),
        };
        let json = serde_json::to_string(&settings_dto(Some(&config))).unwrap()
            + &serde_json::to_string(&registration_dto(Some(&config))).unwrap();
        for secret in [
            "AUTH-SECRET",
            "CT0-SECRET",
            "PRIVATE-ENDPOINT",
            "private_key",
            "[9,8,7]",
        ] {
            assert!(!json.contains(secret), "{secret} leaked");
        }
        assert!(json.contains("\"auth_token_set\":true"));
    }
}
