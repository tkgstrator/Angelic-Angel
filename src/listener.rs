use crate::autopush::{self, ConnectResult};
use crate::config::{self, Config, Registration};
use crate::error::{AngelicAngelError, Result};
use crate::notifications;
use crate::status::Telemetry;
use crate::tweet_lookup::TweetLookup;
use crate::twitter;
use hmac::{Hmac, Mac};
use reqwest::header::{CONTENT_TYPE, HeaderName, HeaderValue};
use reqwest::{Client, Url, redirect};
use sha2::Sha256;
use std::net::IpAddr;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::Instrument;

const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(30);
const SIGNATURE_HEADER: HeaderName = HeaderName::from_static("webhook-signature");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForwardOutcome {
    Forwarded,
    Failed,
    Skipped,
    /// 設定ファイルにこのアカウントが無い。履歴にも転送にも回さない。
    AccountRemoved,
}

/// Outcome of a single listen session.
///
/// Follows the design of Firefox's PushServiceWebSocket.sys.mjs, explicitly
/// distinguishing whether the connection was ever established. Firefox resets
/// _retryFailCount to 0 on any message receipt (including the hello response),
/// so we need to know if hello succeeded to mirror that behavior.
enum SessionOutcome {
    /// WebSocket closed normally (connection was established).
    NormalClose,
    /// Disconnected after a successful handshake (hello succeeded, then a WebSocket error).
    DisconnectedAfterConnect(AngelicAngelError),
    /// Failed before establishing a connection (hello never completed).
    ConnectionFailed(AngelicAngelError),
    /// Unrecoverable error (e.g. UAID invalidated and re-registration also failed).
    Fatal(AngelicAngelError),
    /// The account is gone from the config file; the receiver ends without reconnecting.
    AccountRemoved,
}

/// Why the notification loop of a connected session returned.
enum LoopEnd {
    Closed,
    AccountRemoved,
}

/// Result of [`try_connect`].
enum Connection {
    Ready(Box<autopush::AutoPushClient>),
    AccountRemoved,
}

/// What `listen_managed` does once a session has ended.
enum Next {
    Wait(Duration),
    Finish(Result<()>),
}

/// Whether the config file exists and no longer has the account. A missing or unreadable file is
/// not a removal, so a transient read error never stops a receiver.
fn account_removed(config_path: &Path, account_id: &str) -> bool {
    matches!(
        Config::load_optional(config_path),
        Ok(Some(config)) if config.account(account_id).is_none()
    )
}

/// Computes an exponential backoff delay compatible with Firefox's implementation.
///
/// Firefox (PushServiceWebSocket.sys.mjs L408-431):
///   retryTimeout = retryBaseInterval * 2^retryFailCount
///   retryTimeout = min(retryTimeout, pingInterval)
///
/// - retryBaseInterval = 5s  (dom.push.retryBaseInterval = 5000)
/// - pingInterval      = 5m  (capped; Firefox uses 30m but we use a shorter ping interval)
fn calc_backoff(retry_count: u32) -> u64 {
    const RETRY_BASE_INTERVAL_SECS: u64 = 5;
    const PING_INTERVAL_SECS: u64 = 5 * 60;
    std::cmp::min(
        RETRY_BASE_INTERVAL_SECS.saturating_mul(2u64.saturating_pow(retry_count.saturating_sub(1))),
        PING_INTERVAL_SECS,
    )
}

/// Main listen loop with automatic reconnection.
///
/// Reconnection strategy mirrors Firefox (PushServiceWebSocket.sys.mjs):
/// - Reset retry counter on any successful message receipt (connection established).
/// - No upper limit on retry attempts (infinite retries).
/// - Exponential backoff: 5s * 2^n, capped at 5 minutes.
/// - Server-initiated backoff via close code 4774 delays reconnection for 30 minutes.
///
/// Every log line of the receiver carries the account id through the `account` span. The loop ends
/// with `Ok` when the account disappears from the config file.
pub async fn listen(
    account_id: &str,
    registration: Registration,
    config_path: &Path,
) -> Result<()> {
    listen_managed(account_id, registration, config_path, &Telemetry::default()).await
}

/// Same as [`listen`], additionally reporting state and counters to `telemetry`.
pub async fn listen_managed(
    account_id: &str,
    mut registration: Registration,
    config_path: &Path,
    telemetry: &Telemetry,
) -> Result<()> {
    let mut retry_count: u32 = 0;
    telemetry.set_connecting();

    async {
        loop {
            let outcome = listen_once(account_id, &mut registration, config_path, telemetry).await;
            match next_step(account_id, outcome, &mut retry_count, telemetry) {
                Next::Wait(delay) => tokio::time::sleep(delay).await,
                Next::Finish(result) => return result,
            }
        }
    }
    .instrument(tracing::info_span!("account", id = account_id))
    .await
}

/// Records the session outcome in `telemetry` and decides whether to reconnect after a delay or to
/// stop.
fn next_step(
    account_id: &str,
    outcome: SessionOutcome,
    retry_count: &mut u32,
    telemetry: &Telemetry,
) -> Next {
    match outcome {
        SessionOutcome::NormalClose => {
            *retry_count = 0;
            telemetry.set_retrying("connection closed");
            tracing::info!("WebSocket connection closed, reconnecting");
            Next::Wait(Duration::from_secs(1))
        },
        SessionOutcome::DisconnectedAfterConnect(e) => {
            if e.to_string().contains("BACKOFF:") {
                telemetry.set_retrying("server requested backoff");
                tracing::warn!("server requested backoff, delaying reconnect for 30 minutes");
                Next::Wait(Duration::from_secs(30 * 60))
            } else {
                *retry_count = 0;
                telemetry.set_retrying("connection lost");
                tracing::info!(error = %e, "disconnected after connect, reconnecting");
                Next::Wait(Duration::from_secs(1))
            }
        },
        SessionOutcome::ConnectionFailed(e) => {
            *retry_count += 1;
            telemetry.set_retrying("connection failed");
            let delay = calc_backoff(*retry_count);
            tracing::warn!(
                retry_count = *retry_count,
                delay_secs = delay,
                error = %e,
                "WebSocket connection failed, retrying"
            );
            Next::Wait(Duration::from_secs(delay))
        },
        SessionOutcome::Fatal(e) => {
            telemetry.set_error("re-registration required");
            tracing::error!(error = %e, "fatal error, re-registration required");
            Next::Finish(Err(e))
        },
        SessionOutcome::AccountRemoved => {
            telemetry.set_stopped();
            tracing::info!(
                account = account_id,
                "account was removed from the config, stopping its receiver"
            );
            Next::Finish(Ok(()))
        },
    }
}

/// Runs a single listen session: connect, receive notifications, return outcome.
async fn listen_once(
    account_id: &str,
    registration: &mut Registration,
    config_path: &Path,
    telemetry: &Telemetry,
) -> SessionOutcome {
    if account_removed(config_path, account_id) {
        return SessionOutcome::AccountRemoved;
    }

    let mut client = match try_connect(account_id, registration, config_path).await {
        Ok(Connection::Ready(client)) => client,
        Ok(Connection::AccountRemoved) => return SessionOutcome::AccountRemoved,
        Err(e) => {
            if e.to_string().contains("UAID invalid")
                || e.to_string().contains("still failed after re-registration")
            {
                return SessionOutcome::Fatal(e);
            }
            return SessionOutcome::ConnectionFailed(e);
        },
    };

    tracing::info!("WebSocket connection established, listening for notifications");
    telemetry.set_connected();

    match run_notification_loop(
        account_id,
        &mut client,
        registration,
        config_path,
        telemetry,
    )
    .await
    {
        Ok(LoopEnd::Closed) => SessionOutcome::NormalClose,
        Ok(LoopEnd::AccountRemoved) => SessionOutcome::AccountRemoved,
        Err(e) => SessionOutcome::DisconnectedAfterConnect(e),
    }
}

/// Establishes a connection to AutoPush, handling UAID invalidation transparently.
async fn try_connect(
    account_id: &str,
    registration: &mut Registration,
    config_path: &Path,
) -> Result<Connection> {
    let connect_result =
        autopush::connect_and_listen(&registration.autopush, &registration.keys).await?;

    match connect_result {
        ConnectResult::Connected(client) => {
            tracing::info!("connected with existing session");
            Ok(Connection::Ready(client))
        },
        ConnectResult::NeedsReregistration(reregistration_info) => {
            let new_reg = &reregistration_info.registration;
            let new_keys = reregistration_info.keys;

            tracing::warn!(
                new_uaid = %new_reg.uaid,
                new_channel_id = %new_reg.channel_id,
                "UAID invalidated (pushsubscriptionchange), re-registering"
            );

            // 削除済みのアカウントの認証情報で X に再登録しない。
            if account_removed(config_path, account_id) {
                return Ok(Connection::AccountRemoved);
            }

            tracing::info!("re-registering with Twitter API");
            // 環境変数で自動登録した設定は [twitter] が空なので、ファイルだけでなく環境変数も見る。
            let (env_auth, env_csrf) = config::twitter_env_pair();
            let twitter_credentials = reregistration_twitter(
                account_id,
                config_path,
                env_auth.as_deref(),
                env_csrf.as_deref(),
            )?;

            let subscription = crate::push::PushSubscription {
                endpoint: new_reg.endpoint.clone(),
                autopush: config::AutoPushSession {
                    uaid: new_reg.uaid.clone(),
                    channel_id: new_reg.channel_id.clone(),
                },
                keys: new_keys.clone(),
            };

            twitter::register(&twitter_credentials, &subscription).await?;
            tracing::info!("Twitter API re-registration complete");

            registration.endpoint = new_reg.endpoint.clone();
            registration.autopush.uaid = new_reg.uaid.clone();
            registration.autopush.channel_id = new_reg.channel_id.clone();
            registration.keys = new_keys;
            match save_reregistration(config_path, account_id, registration.clone())? {
                ReregistrationSaved::Saved => tracing::info!("saved updated registration"),
                ReregistrationSaved::Unregistered => {
                    tracing::warn!("registration was removed concurrently, not saving the new one");
                },
                ReregistrationSaved::AccountRemoved => return Ok(Connection::AccountRemoved),
            }

            match autopush::connect_and_listen(&registration.autopush, &registration.keys).await? {
                ConnectResult::Connected(client) => Ok(Connection::Ready(client)),
                ConnectResult::NeedsReregistration(_) => Err(AngelicAngelError::AutoPush(
                    "still failed after re-registration".to_string(),
                )),
            }
        },
    }
}

/// What became of the registration obtained by a re-registration.
#[derive(Debug, PartialEq, Eq)]
enum ReregistrationSaved {
    Saved,
    /// The account has no registration any more (it was unregistered concurrently).
    Unregistered,
    AccountRemoved,
}

/// Writes the new registration into the account `account_id` only. The latest config is re-read so
/// concurrent settings edits are never overwritten, and a registration that was removed
/// concurrently is not resurrected.
fn save_reregistration(
    config_path: &Path,
    account_id: &str,
    updated: Registration,
) -> Result<ReregistrationSaved> {
    Config::modify_account(config_path, account_id, |current| {
        Ok::<_, AngelicAngelError>(match current {
            Some(mut latest) if latest.registration.is_some() => {
                latest.registration = Some(updated);
                (Some(latest), ReregistrationSaved::Saved)
            },
            Some(_) => (None, ReregistrationSaved::Unregistered),
            None => (None, ReregistrationSaved::AccountRemoved),
        })
    })
}

/// UAID 無効化後の再登録で X に渡す認証情報（`default` は環境変数が優先、無ければ設定ファイル）。
fn reregistration_twitter(
    account_id: &str,
    config_path: &Path,
    env_auth: Option<&str>,
    env_csrf: Option<&str>,
) -> Result<config::TwitterConfig> {
    let stored = config::Config::load(config_path)?;
    let stored = stored
        .account(account_id)
        .map(|a| a.twitter.clone())
        .unwrap_or_default();
    config::require_twitter_with(account_id, env_auth, env_csrf, &stored)
}

/// Receives and processes notifications in a loop until the connection drops or the account is
/// removed.
async fn run_notification_loop(
    account_id: &str,
    client: &mut autopush::AutoPushClient,
    registration: &Registration,
    config_path: &Path,
    telemetry: &Telemetry,
) -> Result<LoopEnd> {
    // 非 Unicode の値は未設定として扱う。
    let webhook_secret = std::env::var(config::WEBHOOK_SECRET_ENV).ok();
    let lookup = TweetLookup::production()?;
    while let Some(notification) = client.next_notification().await? {
        tracing::info!(
            channel_id = %notification.channel_id,
            version = %notification.version,
            "notification received"
        );

        telemetry.record_received();
        let mut account_gone = false;
        let ack_code = if let Some(ref data) = notification.data {
            let history_id = format!("{}:{}", notification.channel_id, notification.version);
            match handle_notification_data(
                account_id,
                data,
                &notification.headers,
                &registration.keys,
                config_path,
                &history_id,
                webhook_secret.as_deref(),
                &lookup,
            )
            .await
            {
                Ok(ForwardOutcome::Forwarded) => {
                    telemetry.record_forwarded();
                    autopush::AckCode::Delivered
                },
                Ok(ForwardOutcome::Failed) => {
                    telemetry.record_failed("webhook returned an error status");
                    autopush::AckCode::Delivered
                },
                Ok(ForwardOutcome::Skipped) => autopush::AckCode::Delivered,
                Ok(ForwardOutcome::AccountRemoved) => {
                    account_gone = true;
                    autopush::AckCode::Delivered
                },
                Err(ref e) if is_decryption_error(e) => {
                    telemetry.record_failed("notification decryption failed");
                    tracing::warn!(error = %e, "decryption error, sending ACK with decryption_error");
                    autopush::AckCode::DecryptionError
                },
                Err(ref e) => {
                    telemetry.record_failed("notification forwarding failed");
                    tracing::warn!(error = %e, "notification processing error, sending ACK with not_delivered");
                    autopush::AckCode::NotDelivered
                },
            }
        } else {
            tracing::info!("empty notification (no data)");
            autopush::AckCode::Delivered
        };

        client
            .ack_notification(
                notification.channel_id.clone(),
                notification.version.clone(),
                ack_code,
            )
            .await?;
        tracing::debug!(
            channel_id = %notification.channel_id,
            ack_code = ?ack_code,
            "ACK sent"
        );
        if account_gone {
            return Ok(LoopEnd::AccountRemoved);
        }
    }

    Ok(LoopEnd::Closed)
}

fn is_decryption_error(e: &AngelicAngelError) -> bool {
    matches!(e, AngelicAngelError::Decryption(_))
}

#[allow(clippy::too_many_arguments)]
async fn handle_notification_data(
    account_id: &str,
    data: &str,
    headers: &Option<std::collections::HashMap<String, String>>,
    keys: &crate::config::WebPushKeys,
    config_path: &Path,
    history_id: &str,
    env_webhook_secret: Option<&str>,
    lookup: &TweetLookup,
) -> Result<ForwardOutcome> {
    let encrypted = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, data)
        .map_err(|e| AngelicAngelError::Decryption(format!("base64 decode failed: {}", e)))?;

    tracing::debug!(
        encrypted_size = encrypted.len(),
        "decoding encrypted message"
    );

    if let Some(hdrs) = headers {
        tracing::debug!(?hdrs, "notification headers");
    }

    let decrypted = match decrypt_ece(&encrypted, headers, keys) {
        Ok(decrypted) => {
            tracing::debug!(decrypted_size = decrypted.len(), "ECE decryption succeeded");
            decrypted
        },
        Err(e) => {
            tracing::error!(
                error = %e,
                encrypted_head = ?&encrypted[..encrypted.len().min(16)],
                "ECE decryption failed"
            );
            return Err(e);
        },
    };

    let text = String::from_utf8(decrypted)
        .map_err(|e| AngelicAngelError::Decryption(format!("UTF-8 conversion failed: {}", e)))?;

    let payload_bytes = text.len();
    let payload: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({ "raw": text }));

    tracing::debug!(payload_bytes, "notification decrypted");

    let kind = notifications::resolve_kind(&payload, lookup).await;

    // 設定を一度だけ読み、削除の判定・フィルター・Webhook の解決に同じ内容を使う。履歴は設定が
    // 読めなくても残すので、読み込みの失敗は履歴を保存した後に返す。
    let loaded = Config::load_optional(config_path);
    // 削除済みのアカウントの履歴ファイルを作り直さない。
    if matches!(&loaded, Ok(Some(config)) if config.account(account_id).is_none()) {
        return Ok(ForwardOutcome::AccountRemoved);
    }
    record_history(config_path, account_id, history_id, &payload, kind).await;

    let config = loaded?;
    let account = config.as_ref().and_then(|c| c.account(account_id));
    let filter = account.map(|a| a.notification_filter).unwrap_or_default();
    if !filter.allows(kind) {
        tracing::info!("notification kind is disabled by the filter, skipping forward");
        return Ok(ForwardOutcome::Skipped);
    }
    let stored = account.and_then(|a| a.webhook_endpoint.as_deref());
    let Some(webhook_url) = config::resolve_webhook(account_id, stored).0 else {
        tracing::info!("webhook endpoint is not set, skipping forward");
        return Ok(ForwardOutcome::Skipped);
    };
    let secret = config::resolve_webhook_secret_with(
        account_id,
        env_webhook_secret,
        account.and_then(|a| a.webhook_secret.as_deref()),
    )
    .0;

    send_to_webhook(account_id, &payload, &webhook_url, secret.as_deref()).await
}

/// Stores the notification in the account's local history. A failure never blocks forwarding or
/// the ACK.
async fn record_history(
    config_path: &Path,
    account_id: &str,
    history_id: &str,
    payload: &serde_json::Value,
    kind: notifications::NotificationKind,
) {
    let config_path = config_path.to_path_buf();
    let account_id = account_id.to_owned();
    let history_id = history_id.to_owned();
    let payload = payload.clone();
    let saved = tokio::task::spawn_blocking(move || {
        notifications::append_as_for(&config_path, &account_id, &history_id, &payload, kind)
    })
    .await;
    if !matches!(saved, Ok(Ok(()))) {
        tracing::warn!("failed to record notification in local history");
    }
}

/// 署名鍵が無いときのエラー。アカウントを特定できるよう ID を含めるが、鍵の値は含めない。
fn missing_secret_error(account_id: &str) -> AngelicAngelError {
    let source = if account_id == config::DEFAULT_ACCOUNT_ID {
        format!(
            "{} or the account's webhook secret",
            config::WEBHOOK_SECRET_ENV
        )
    } else {
        "the account's webhook secret".to_string()
    };
    AngelicAngelError::Config(format!(
        "{source} is required to send to the webhook of account {account_id:?}"
    ))
}

/// 通知を署名付き(`Webhook-Signature`)で Webhook へ POST し、2xx で応答したかを返す。
async fn send_to_webhook(
    account_id: &str,
    payload: &serde_json::Value,
    webhook_url: &str,
    secret: Option<&str>,
) -> Result<ForwardOutcome> {
    let url = checked_webhook_url(webhook_url)?;
    let secret = secret
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| missing_secret_error(account_id))?;

    let body = serde_json::to_vec(payload)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AngelicAngelError::Config("system clock is before the Unix epoch".into()))?
        .as_secs();
    let signature = signature_header(secret, timestamp, &body)?;

    // 別オリジンへ本文と署名が流出しないようリダイレクトは追従しない。
    let mut builder = Client::builder()
        .timeout(WEBHOOK_TIMEOUT)
        .redirect(redirect::Policy::none());
    // ローカル宛ての平文 HTTP は、環境の proxy に本文と署名を渡さないよう直接接続する。
    if url.scheme() == "http" {
        builder = builder.no_proxy();
    }
    let client = builder.build()?;

    tracing::info!("sending to webhook");

    let response = client
        .post(url)
        .header(CONTENT_TYPE, "application/json")
        .header(SIGNATURE_HEADER, signature)
        .body(body)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;

    // 応答本文と URL には機密が含まれうるため、ログにはステータスだけを出す。
    let status = response.status();
    if status.is_success() {
        tracing::info!(status = %status, "webhook request succeeded");
        Ok(ForwardOutcome::Forwarded)
    } else {
        tracing::warn!(status = %status, "webhook request failed");
        Ok(ForwardOutcome::Failed)
    }
}

/// HTTPS のみ許可し、HTTP は loopback または Docker ホスト名宛てに限る。
fn checked_webhook_url(raw: &str) -> Result<Url> {
    let rejected = || {
        AngelicAngelError::Config(
            "webhook URL must be HTTPS (HTTP only for loopback or host.docker.internal)".into(),
        )
    };
    let url = Url::parse(raw).map_err(|_| rejected())?;
    match url.scheme() {
        "https" => Ok(url),
        "http" if is_local_http_host(&url) => Ok(url),
        _ => Err(rejected()),
    }
}

fn is_local_http_host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    host == "host.docker.internal"
        || host == "localhost"
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// `t=<秒>,v1=<hex>` の署名ヘッダー値を作る。MAC の対象は `<秒>.<本文 bytes>`。
fn signature_header(secret: &str, timestamp: u64, body: &[u8]) -> Result<HeaderValue> {
    let failed = || AngelicAngelError::Config("failed to sign the webhook request".into());
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(|_| failed())?;
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let mut value =
        HeaderValue::from_str(&format!("t={timestamp},v1={hex}")).map_err(|_| failed())?;
    value.set_sensitive(true);
    Ok(value)
}

fn decrypt_ece(
    encrypted: &[u8],
    headers: &Option<std::collections::HashMap<String, String>>,
    keys: &crate::config::WebPushKeys,
) -> Result<Vec<u8>> {
    tracing::debug!(
        private_key_len = keys.private_key.len(),
        public_key_len = keys.public_key.len(),
        auth_secret_len = keys.auth_secret.len(),
        encrypted_len = encrypted.len(),
        "starting ECE decryption"
    );

    let key_pair = ece::EcKeyComponents::new(keys.private_key.clone(), keys.public_key.clone());

    let encoding = headers
        .as_ref()
        .and_then(|h| h.get("encoding"))
        .map(|s| s.as_str());

    match encoding {
        Some("aesgcm") => {
            tracing::debug!("decrypting with aesgcm encoding");
            decrypt_aesgcm(encrypted, headers, &key_pair, &keys.auth_secret)
        },
        Some("aes128gcm") | None => {
            tracing::debug!("decrypting with aes128gcm encoding");
            let decrypted = ece::decrypt(&key_pair, &keys.auth_secret, encrypted).map_err(|e| {
                tracing::debug!(error = %e, "aes128gcm decryption error");
                AngelicAngelError::Decryption(format!("aes128gcm decryption failed: {}", e))
            })?;

            tracing::debug!(
                decrypted_len = decrypted.len(),
                "aes128gcm decryption succeeded"
            );
            Ok(decrypted)
        },
        Some(other) => Err(AngelicAngelError::Decryption(format!(
            "unsupported encoding: {}",
            other
        ))),
    }
}

fn decrypt_aesgcm(
    encrypted: &[u8],
    headers: &Option<std::collections::HashMap<String, String>>,
    key_pair: &ece::EcKeyComponents,
    auth_secret: &[u8],
) -> Result<Vec<u8>> {
    let headers = headers.as_ref().ok_or_else(|| {
        AngelicAngelError::Decryption(
            "aesgcm encoding requires headers but none were provided".to_string(),
        )
    })?;

    let dh_b64 = parse_header_param(headers.get("crypto_key"), "dh")?;
    let sender_public_key =
        base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &dh_b64)
            .map_err(|e| {
                AngelicAngelError::Decryption(format!(
                    "failed to base64-decode sender public key: {}",
                    e
                ))
            })?;

    tracing::debug!(
        sender_public_key_len = sender_public_key.len(),
        "parsed sender public key"
    );

    let salt_b64 = parse_header_param(headers.get("encryption"), "salt")?;
    let salt = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &salt_b64)
        .map_err(|e| {
            AngelicAngelError::Decryption(format!("failed to base64-decode salt: {}", e))
        })?;

    tracing::debug!(salt_len = salt.len(), "parsed salt");

    let block =
        ece::legacy::AesGcmEncryptedBlock::new(&sender_public_key, &salt, 4096, encrypted.to_vec())
            .map_err(|e| {
                AngelicAngelError::Decryption(format!(
                    "failed to construct AesGcmEncryptedBlock: {}",
                    e
                ))
            })?;

    let decrypted = ece::legacy::decrypt_aesgcm(key_pair, auth_secret, &block).map_err(|e| {
        tracing::debug!(error = %e, "aesgcm decryption error");
        AngelicAngelError::Decryption(format!("aesgcm decryption failed: {}", e))
    })?;

    tracing::debug!(
        decrypted_len = decrypted.len(),
        "aesgcm decryption succeeded"
    );
    Ok(decrypted)
}

/// Extracts a named parameter from a semicolon-delimited header value.
///
/// Example: given `"dh=abc123;p256ecdsa=xyz"` and param `"dh"`, returns `"abc123"`.
fn parse_header_param(header_value: Option<&String>, param_name: &str) -> Result<String> {
    let header = header_value.ok_or_else(|| {
        AngelicAngelError::Decryption(format!(
            "missing required header for param '{}'",
            param_name
        ))
    })?;

    for part in header.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(&format!("{}=", param_name)) {
            return Ok(value.to_string());
        }
    }

    Err(AngelicAngelError::Decryption(format!(
        "param '{}' not found in header: {}",
        param_name, header
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        Account, Config, DEFAULT_ACCOUNT_ID, NotificationFilter, TwitterConfig, WebPushKeys,
        test_temp_dir,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use crate::tweet_lookup::mock::{MockApi, REFUSED_ENDPOINT, http_response, json_response};

    const TEST_SECRET: &str = "whsec_synthetic_test_key";
    const OK_RESPONSE: &str = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

    /// モックが受け取ったリクエストの生 bytes。
    struct Captured {
        request_line: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Captured {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    /// 1 リクエストだけ受け付けて `response` を返し、受け取った内容を渡す。
    async fn serve_once(response: String) -> (String, tokio::task::JoinHandle<Captured>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let (head_len, content_length) = loop {
                let n = stream.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                    let length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    break (pos + 4, length);
                }
                assert!(n > 0, "connection closed before the request head");
            };
            while buf.len() < head_len + content_length {
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0, "connection closed before the request body");
                buf.extend_from_slice(&chunk[..n]);
            }
            stream.write_all(response.as_bytes()).await.unwrap();
            let head = String::from_utf8(buf[..head_len - 4].to_vec()).unwrap();
            let mut lines = head.split("\r\n");
            let request_line = lines.next().unwrap().to_string();
            let headers = lines
                .map(|l| {
                    let (k, v) = l.split_once(':').unwrap();
                    (k.to_string(), v.trim().to_string())
                })
                .collect();
            Captured {
                request_line,
                headers,
                body: buf[head_len..head_len + content_length].to_vec(),
            }
        });
        (url, task)
    }

    /// 1 リクエストだけ受け付けて 200 を返し、本文を文字列で渡す。
    async fn mock_webhook() -> (String, tokio::task::JoinHandle<String>) {
        let (url, task) = serve_once(OK_RESPONSE.to_string()).await;
        let body =
            tokio::spawn(
                async move { String::from_utf8_lossy(&task.await.unwrap().body).into_owned() },
            );
        (url, body)
    }

    /// 接続を拒否される照会先。本物の API には通信せず、照会は常に失敗する。
    fn no_lookup() -> TweetLookup {
        TweetLookup::with_endpoint(REFUSED_ENDPOINT).unwrap()
    }

    fn write_config(path: &Path, webhook: &str) {
        write_config_with(path, Some(webhook));
    }

    fn write_config_with(path: &Path, webhook: Option<&str>) {
        write_config_filtered(path, webhook, NotificationFilter::default());
    }

    fn write_config_filtered(path: &Path, webhook: Option<&str>, filter: NotificationFilter) {
        let mut account = Account::new(DEFAULT_ACCOUNT_ID);
        account.twitter = TwitterConfig {
            auth_token: "synthetic".into(),
            ct0: "synthetic".into(),
        };
        account.webhook_endpoint = webhook.map(str::to_string);
        account.notification_filter = filter;
        Config {
            accounts: vec![account],
        }
        .save(path)
        .unwrap();
    }

    #[test]
    fn reregistration_uses_environment_credentials_when_the_file_has_none() {
        let dir = test_temp_dir();
        let path = dir.join("angelic-angel.toml");
        // 環境変数で自動登録した設定は [twitter] を持たない。
        Config {
            accounts: vec![Account::new(DEFAULT_ACCOUNT_ID)],
        }
        .save(&path)
        .unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("twitter"));

        let t =
            reregistration_twitter(DEFAULT_ACCOUNT_ID, &path, Some("env-auth"), Some("env-ct0"))
                .unwrap();
        assert_eq!(
            (t.auth_token.as_str(), t.ct0.as_str()),
            ("env-auth", "env-ct0")
        );
        // 環境変数が無ければ空の認証情報で X を呼ばず、設定の不足として失敗する。
        let err = reregistration_twitter(DEFAULT_ACCOUNT_ID, &path, None, None).unwrap_err();
        assert!(matches!(err, AngelicAngelError::Config(_)), "{err}");
    }

    #[test]
    fn reregistration_prefers_environment_and_rejects_a_half_set_pair() {
        let dir = test_temp_dir();
        let path = dir.join("angelic-angel.toml");
        write_config(&path, "https://example.com/h");

        let t =
            reregistration_twitter(DEFAULT_ACCOUNT_ID, &path, Some("env-auth"), Some("env-ct0"))
                .unwrap();
        assert_eq!(t.auth_token, "env-auth");
        let t = reregistration_twitter(DEFAULT_ACCOUNT_ID, &path, None, None).unwrap();
        assert_eq!(t.auth_token, "synthetic");

        // 片方だけなら、ファイルの値へ黙って戻さずエラーにする。
        for (auth, csrf) in [(Some("env-auth"), None), (None, Some("env-ct0"))] {
            let err = reregistration_twitter(DEFAULT_ACCOUNT_ID, &path, auth, csrf).unwrap_err();
            assert!(matches!(err, AngelicAngelError::Config(_)), "{err}");
        }
    }

    fn registration_named(uaid: &str) -> Registration {
        Registration {
            endpoint: format!("https://push.example/{uaid}"),
            autopush: config::AutoPushSession {
                uaid: uaid.into(),
                channel_id: format!("chan-{uaid}"),
            },
            keys: WebPushKeys {
                public_key: vec![1],
                private_key: vec![2],
                auth_secret: vec![3],
            },
        }
    }

    fn account_named(id: &str, webhook: Option<&str>, filter: NotificationFilter) -> Account {
        let mut account = Account::new(id);
        account.twitter = TwitterConfig {
            auth_token: format!("{id}-auth"),
            ct0: format!("{id}-ct0"),
        };
        account.registration = Some(registration_named(id));
        account.webhook_endpoint = webhook.map(str::to_string);
        account.webhook_secret = Some(format!("{id}-secret"));
        account.notification_filter = filter;
        account
    }

    fn write_accounts(path: &Path, accounts: Vec<Account>) {
        Config { accounts }.save(path).unwrap();
    }

    #[test]
    fn reregistration_uses_only_the_accounts_own_credentials() {
        let path = test_temp_dir().join("angelic-angel.toml");
        let mut empty = Account::new("empty");
        empty.registration = Some(registration_named("empty"));
        write_accounts(
            &path,
            vec![
                account_named("default", None, NotificationFilter::default()),
                account_named("work", None, NotificationFilter::default()),
                empty,
            ],
        );

        // 環境変数は `default` だけに効き、`work` は自分のファイル上の値を使う。
        let t = reregistration_twitter("work", &path, Some("env-auth"), Some("env-ct0")).unwrap();
        assert_eq!(
            (t.auth_token.as_str(), t.ct0.as_str()),
            ("work-auth", "work-ct0")
        );
        let t =
            reregistration_twitter("default", &path, Some("env-auth"), Some("env-ct0")).unwrap();
        assert_eq!(t.auth_token, "env-auth");
        // 認証情報の無いアカウントは、環境変数があっても X を呼ばず失敗する。
        let err =
            reregistration_twitter("empty", &path, Some("env-auth"), Some("env-ct0")).unwrap_err();
        assert!(matches!(err, AngelicAngelError::Config(_)), "{err}");
        assert!(err.to_string().contains("\"empty\""), "{err}");
    }

    fn uaid_of(config: &Config, id: &str) -> Option<String> {
        config
            .account(id)
            .and_then(|a| a.registration.as_ref())
            .map(|r| r.autopush.uaid.clone())
    }

    #[test]
    fn reregistration_is_written_back_to_its_own_account_only() {
        let path = test_temp_dir().join("angelic-angel.toml");
        let mut work = account_named(
            "work",
            Some("https://work.example/h"),
            NotificationFilter::default(),
        );
        work.notification_filter.retweet = false;
        write_accounts(
            &path,
            vec![
                account_named(
                    "default",
                    Some("https://d.example/h"),
                    only(true, true, true),
                ),
                work,
            ],
        );

        let saved = save_reregistration(&path, "work", registration_named("work-renewed")).unwrap();

        assert_eq!(saved, ReregistrationSaved::Saved);
        let loaded = Config::load(&path).unwrap();
        assert_eq!(uaid_of(&loaded, "work").as_deref(), Some("work-renewed"));
        assert_eq!(uaid_of(&loaded, "default").as_deref(), Some("default"));
        // 設定の編集は登録の更新で上書きされない。
        let work = loaded.account("work").unwrap();
        assert_eq!(
            work.webhook_endpoint.as_deref(),
            Some("https://work.example/h")
        );
        assert_eq!(work.webhook_secret.as_deref(), Some("work-secret"));
        assert!(!work.notification_filter.retweet);
        assert_eq!(work.twitter.auth_token, "work-auth");
        assert_eq!(
            loaded
                .accounts
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>(),
            ["default", "work"]
        );
    }

    #[test]
    fn reregistration_does_not_resurrect_a_registration_removed_concurrently() {
        let path = test_temp_dir().join("angelic-angel.toml");
        let mut unregistered = account_named("work", None, NotificationFilter::default());
        unregistered.registration = None;
        write_accounts(
            &path,
            vec![
                account_named("default", None, NotificationFilter::default()),
                unregistered,
            ],
        );
        let before = std::fs::read_to_string(&path).unwrap();

        let saved = save_reregistration(&path, "work", registration_named("work-renewed")).unwrap();

        assert_eq!(saved, ReregistrationSaved::Unregistered);
        // ファイルには一切手を入れない。
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let loaded = Config::load(&path).unwrap();
        assert_eq!(uaid_of(&loaded, "work"), None);
        assert_eq!(uaid_of(&loaded, "default").as_deref(), Some("default"));
    }

    #[test]
    fn reregistration_of_a_removed_account_writes_nothing() {
        let path = test_temp_dir().join("angelic-angel.toml");
        write_accounts(
            &path,
            vec![account_named(
                "default",
                None,
                NotificationFilter::default(),
            )],
        );
        let before = std::fs::read_to_string(&path).unwrap();

        let saved = save_reregistration(&path, "work", registration_named("work-renewed")).unwrap();

        assert_eq!(saved, ReregistrationSaved::AccountRemoved);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn account_removal_is_detected_only_from_a_readable_config() {
        let path = test_temp_dir().join("angelic-angel.toml");
        // 設定ファイルが無い・読めないときは、一時的な状態かもしれないので止めない。
        assert!(!account_removed(&path, "work"));
        std::fs::write(&path, "[[accounts]\nbroken").unwrap();
        assert!(!account_removed(&path, "work"));

        write_accounts(
            &path,
            vec![account_named(
                "default",
                None,
                NotificationFilter::default(),
            )],
        );
        assert!(!account_removed(&path, "default"));
        assert!(account_removed(&path, "work"));
        // 全アカウントが消えて空になった設定でも、削除として扱う。
        write_accounts(&path, Vec::new());
        assert!(account_removed(&path, "default"));
    }

    #[test]
    fn a_removed_account_ends_its_receiver_without_retrying() {
        let telemetry = Telemetry::default();
        telemetry.set_connected();
        let mut retries = 3;

        let next = next_step(
            "work",
            SessionOutcome::AccountRemoved,
            &mut retries,
            &telemetry,
        );

        assert!(matches!(next, Next::Finish(Ok(()))));
        assert_eq!(
            telemetry.snapshot().0,
            crate::status::ReceiverState::Stopped
        );
        // 再接続の状態は変えない。
        assert_eq!(retries, 3);

        // 通常の切断は待ってから再接続し、致命的な失敗だけがエラーで終わる。
        let next = next_step(
            "work",
            SessionOutcome::NormalClose,
            &mut retries,
            &telemetry,
        );
        assert!(matches!(next, Next::Wait(d) if d == Duration::from_secs(1)));
        assert_eq!(retries, 0);
        let fatal = SessionOutcome::Fatal(AngelicAngelError::AutoPush("UAID invalid".into()));
        assert!(matches!(
            next_step("work", fatal, &mut retries, &telemetry),
            Next::Finish(Err(_))
        ));
        let failed = SessionOutcome::ConnectionFailed(AngelicAngelError::AutoPush("x".into()));
        assert!(matches!(
            next_step("work", failed, &mut retries, &telemetry),
            Next::Wait(d) if d == Duration::from_secs(calc_backoff(1))
        ));
    }

    #[tokio::test]
    async fn the_receiver_of_a_removed_account_stops_before_connecting() {
        let path = test_temp_dir().join("angelic-angel.toml");
        write_accounts(
            &path,
            vec![account_named(
                "default",
                None,
                NotificationFilter::default(),
            )],
        );
        let telemetry = Telemetry::default();

        // 設定に無いアカウントの受信は、接続を試みずに正常終了する（接続すればここで待たされる）。
        let ended = tokio::time::timeout(
            Duration::from_secs(5),
            listen_managed("work", registration_named("work"), &path, &telemetry),
        )
        .await
        .expect("the receiver of a removed account must not keep running");

        assert!(ended.is_ok(), "{ended:?}");
        assert_eq!(
            telemetry.snapshot().0,
            crate::status::ReceiverState::Stopped
        );
    }

    fn encrypted_message(payload: &str) -> (String, WebPushKeys) {
        let (pair, auth) = ece::generate_keypair_and_auth_secret().unwrap();
        let components = pair.raw_components().unwrap();
        let encrypted = ece::encrypt(components.public_key(), &auth, payload.as_bytes()).unwrap();
        let keys = WebPushKeys {
            public_key: components.public_key().to_vec(),
            private_key: components.private_key().to_vec(),
            auth_secret: auth.to_vec(),
        };
        let data =
            base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, encrypted);
        (data, keys)
    }

    #[tokio::test]
    async fn received_notification_is_recorded_then_forwarded() {
        let path = test_temp_dir().join("c.toml");
        let (url, server) = mock_webhook().await;
        write_config(&path, &url);
        let message =
            r#"{"title":"Synthetic","body":"hello","data":{"url":"https://x.com/i/status/1"}}"#;
        let (data, keys) = encrypted_message(message);

        let outcome = handle_notification_data(
            DEFAULT_ACCOUNT_ID,
            &data,
            &None,
            &keys,
            &path,
            "chan:7",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, ForwardOutcome::Forwarded);
        assert!(server.await.unwrap().contains("Synthetic"));
        let page = notifications::list(&path, None, 10).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, "chan:7");
        assert_eq!(page.items[0].title, "Synthetic");
        assert_eq!(
            page.items[0].url.as_deref(),
            Some("https://x.com/i/status/1")
        );
    }

    #[tokio::test]
    async fn history_write_failure_does_not_block_forwarding() {
        let path = test_temp_dir().join("c.toml");
        let (url, server) = mock_webhook().await;
        write_config(&path, &url);
        // A directory in place of the history file makes every read and write of it fail.
        std::fs::create_dir(path.with_file_name("c.toml.notifications.sqlite3")).unwrap();
        let (data, keys) = encrypted_message(r#"{"title":"Still forwarded"}"#);

        let outcome = handle_notification_data(
            DEFAULT_ACCOUNT_ID,
            &data,
            &None,
            &keys,
            &path,
            "chan:8",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, ForwardOutcome::Forwarded);
        assert!(server.await.unwrap().contains("Still forwarded"));
        assert!(notifications::list(&path, None, 10).is_err());
    }

    #[tokio::test]
    async fn non_json_payload_is_recorded_as_raw() {
        let path = test_temp_dir().join("c.toml");
        let (url, server) = mock_webhook().await;
        write_config(&path, &url);
        let (data, keys) = encrypted_message("not json at all");

        handle_notification_data(
            DEFAULT_ACCOUNT_ID,
            &data,
            &None,
            &keys,
            &path,
            "chan:9",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await
        .unwrap();
        server.await.unwrap();

        let item = notifications::list(&path, None, 1).unwrap().items.remove(0);
        assert_eq!(item.body, "not json at all");
        assert_eq!(
            item.payload,
            serde_json::json!({ "raw": "not json at all" })
        );
    }

    async fn assert_skipped_but_recorded(webhook: Option<&str>, id: &str, title: &str) {
        let path = test_temp_dir().join("c.toml");
        write_config_with(&path, webhook);
        let (data, keys) = encrypted_message(&format!(r#"{{"title":"{title}"}}"#));

        let outcome = handle_notification_data(
            DEFAULT_ACCOUNT_ID,
            &data,
            &None,
            &keys,
            &path,
            id,
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, ForwardOutcome::Skipped);
        let page = notifications::list(&path, None, 10).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, id);
        assert_eq!(page.items[0].title, title);
    }

    #[tokio::test]
    async fn unset_webhook_skips_forwarding_but_records_history() {
        assert_skipped_but_recorded(None, "chan:10", "No webhook").await;
    }

    #[tokio::test]
    async fn blank_webhook_skips_forwarding_but_records_history() {
        assert_skipped_but_recorded(Some("   "), "chan:11", "Blank webhook").await;
    }

    #[tokio::test]
    async fn missing_config_file_skips_forwarding_but_records_history() {
        let path = test_temp_dir().join("c.toml");
        let (data, keys) = encrypted_message(r#"{"title":"No config"}"#);

        let outcome = handle_notification_data(
            DEFAULT_ACCOUNT_ID,
            &data,
            &None,
            &keys,
            &path,
            "chan:12",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, ForwardOutcome::Skipped);
        let page = notifications::list(&path, None, 10).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].title, "No config");
    }

    /// 接続を受け付けたかだけを見る Webhook。フィルター除外で一切送信されないことの確認に使う。
    async fn silent_webhook() -> (String, TcpListener) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        (url, listener)
    }

    async fn nothing_was_sent(listener: &TcpListener) -> bool {
        tokio::time::timeout(Duration::from_millis(300), listener.accept())
            .await
            .is_err()
    }

    fn only(tweet: bool, chat: bool, other: bool) -> NotificationFilter {
        NotificationFilter {
            tweet,
            retweet: true,
            quote: true,
            quote_self: true,
            reply: true,
            xchat_message_received: chat,
            other,
        }
    }

    #[tokio::test]
    async fn disabled_kind_is_recorded_but_not_forwarded() {
        let cases = [
            (
                r#"{"title":"A","data":{"type":"tweet"}}"#,
                only(false, true, true),
            ),
            (
                r#"{"title":"B","data":{"type":"xchat_message_received"}}"#,
                only(true, false, true),
            ),
            (
                r#"{"title":"C","data":{"type":"recommendation"}}"#,
                only(true, true, false),
            ),
            (r#"{"title":"D"}"#, only(true, true, false)),
            (
                r#"{"title":"E","notification":{"data":{"type":"novel"}},"data":{"type":"tweet"}}"#,
                only(true, true, false),
            ),
        ];
        for (n, (message, filter)) in cases.into_iter().enumerate() {
            let path = test_temp_dir().join("c.toml");
            let (url, listener) = silent_webhook().await;
            write_config_filtered(&path, Some(&url), filter);
            let (data, keys) = encrypted_message(message);

            let outcome = handle_notification_data(
                DEFAULT_ACCOUNT_ID,
                &data,
                &None,
                &keys,
                &path,
                &format!("chan:{n}"),
                Some(TEST_SECRET),
                &no_lookup(),
            )
            .await
            .unwrap();

            assert_eq!(outcome, ForwardOutcome::Skipped, "{message}");
            assert!(nothing_was_sent(&listener).await, "{message}");
            let page = notifications::list(&path, None, 10).unwrap();
            assert_eq!(page.items.len(), 1, "{message}");
            assert_eq!(page.items[0].id, format!("chan:{n}"));
        }
    }

    #[tokio::test]
    async fn disabled_retweet_is_recorded_but_not_forwarded() {
        let filter = NotificationFilter {
            retweet: false,
            ..NotificationFilter::default()
        };
        let retweet =
            r#"{"title":"RT","data":{"type":"tweet","uri":"/a/status/1","tag":"tweet-2"}}"#;
        let tweet =
            r#"{"title":"Own","data":{"type":"tweet","uri":"/a/status/3","tag":"tweet-3"}}"#;

        let path = test_temp_dir().join("c.toml");
        let (url, listener) = silent_webhook().await;
        write_config_filtered(&path, Some(&url), filter);
        let (data, keys) = encrypted_message(retweet);
        let outcome = handle_notification_data(
            DEFAULT_ACCOUNT_ID,
            &data,
            &None,
            &keys,
            &path,
            "chan:25",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, ForwardOutcome::Skipped);
        assert!(nothing_was_sent(&listener).await);
        let item = notifications::list(&path, None, 1).unwrap().items.remove(0);
        assert_eq!(item.id, "chan:25");
        assert_eq!(item.kind, notifications::NotificationKind::Retweet);

        // 同じ設定でも、ID が一致する通常の投稿は転送される。
        let path = test_temp_dir().join("c.toml");
        let (url, server) = mock_webhook().await;
        write_config_filtered(&path, Some(&url), filter);
        let (data, keys) = encrypted_message(tweet);
        let outcome = handle_notification_data(
            DEFAULT_ACCOUNT_ID,
            &data,
            &None,
            &keys,
            &path,
            "chan:26",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, ForwardOutcome::Forwarded);
        assert!(server.await.unwrap().contains("Own"));
    }

    /// 照会 ID が 5 の通常投稿（`data.uri` と `data.tag` の ID が一致する）。
    const LOOKED_UP_TWEET: &str =
        r#"{"title":"Looked up","data":{"type":"tweet","uri":"/a/status/5","tag":"tweet-5"}}"#;
    const LOOKED_UP_REQUEST: &str = "GET /tweet-result?id=5&token=0 HTTP/1.1";

    #[tokio::test]
    async fn quote_and_reply_are_decided_by_the_lookup_for_history_and_filter() {
        use notifications::NotificationKind::{Quote, Reply};
        let cases = [
            (
                r#"{"__typename":"Tweet","quoted_tweet":{"id_str":"1"}}"#,
                Quote,
                NotificationFilter {
                    quote: false,
                    ..NotificationFilter::default()
                },
                NotificationFilter {
                    reply: false,
                    ..NotificationFilter::default()
                },
            ),
            (
                r#"{"__typename":"Tweet","in_reply_to_status_id_str":"1"}"#,
                Reply,
                NotificationFilter {
                    reply: false,
                    ..NotificationFilter::default()
                },
                NotificationFilter {
                    quote: false,
                    ..NotificationFilter::default()
                },
            ),
        ];
        for (n, (response, kind, disabled, other_disabled)) in cases.into_iter().enumerate() {
            let api = MockApi::start(json_response(response)).await;
            let lookup = TweetLookup::with_endpoint(&api.url).unwrap();

            // 該当する区分を無効にすると、履歴には残るが転送されない。
            let path = test_temp_dir().join("c.toml");
            let (url, listener) = silent_webhook().await;
            write_config_filtered(&path, Some(&url), disabled);
            let (data, keys) = encrypted_message(LOOKED_UP_TWEET);
            let id = format!("chan:8{n}");
            let outcome = handle_notification_data(
                DEFAULT_ACCOUNT_ID,
                &data,
                &None,
                &keys,
                &path,
                &id,
                Some(TEST_SECRET),
                &lookup,
            )
            .await
            .unwrap();
            assert_eq!(outcome, ForwardOutcome::Skipped, "{kind:?}");
            assert!(nothing_was_sent(&listener).await, "{kind:?}");
            let item = notifications::list(&path, None, 1).unwrap().items.remove(0);
            assert_eq!((item.id.as_str(), item.kind), (id.as_str(), kind));
            assert_eq!(api.requests(), [LOOKED_UP_REQUEST]);

            // もう一方の区分だけを無効にしても、この区分は転送され、同じ区分で記録される。
            let path = test_temp_dir().join("c.toml");
            let (url, server) = mock_webhook().await;
            write_config_filtered(&path, Some(&url), other_disabled);
            let id = format!("chan:9{n}");
            let outcome = handle_notification_data(
                DEFAULT_ACCOUNT_ID,
                &data,
                &None,
                &keys,
                &path,
                &id,
                Some(TEST_SECRET),
                &lookup,
            )
            .await
            .unwrap();
            assert_eq!(outcome, ForwardOutcome::Forwarded, "{kind:?}");
            assert!(server.await.unwrap().contains("Looked up"));
            let item = notifications::list(&path, None, 1).unwrap().items.remove(0);
            assert_eq!((item.id.as_str(), item.kind), (id.as_str(), kind));
        }
    }

    /// 引用した側 (`alice`) の引用RT。引用元の投稿者だけを変える。
    fn quote_by_alice_of(quoted_user_id: &str) -> String {
        serde_json::json!({
            "__typename": "Tweet",
            "user": { "id_str": "100", "screen_name": "alice" },
            "quoted_tweet": { "id_str": "1", "user": { "id_str": quoted_user_id } },
        })
        .to_string()
    }

    #[tokio::test]
    async fn self_quotes_and_other_quotes_are_filtered_separately() {
        use notifications::NotificationKind::{Quote, SelfQuote};
        let self_quote = quote_by_alice_of("100");
        let other_quote = quote_by_alice_of("200");
        let only_self = NotificationFilter {
            quote: false,
            quote_self: true,
            ..NotificationFilter::default()
        };
        let only_other = NotificationFilter {
            quote: true,
            quote_self: false,
            ..NotificationFilter::default()
        };
        // (照会の応答, 期待する区分, 転送される設定, 転送されない設定)
        let cases = [
            (&self_quote, SelfQuote, only_self, only_other),
            (&other_quote, Quote, only_other, only_self),
        ];
        for (n, (response, kind, forwarded, skipped)) in cases.into_iter().enumerate() {
            let api = MockApi::start(json_response(response)).await;
            let lookup = TweetLookup::with_endpoint(&api.url).unwrap();
            let (data, keys) = encrypted_message(LOOKED_UP_TWEET);

            // 無効にした側は履歴に残るが転送されない。
            let path = test_temp_dir().join("c.toml");
            let (url, listener) = silent_webhook().await;
            write_config_filtered(&path, Some(&url), skipped);
            let id = format!("chan:12{n}");
            let outcome = handle_notification_data(
                DEFAULT_ACCOUNT_ID,
                &data,
                &None,
                &keys,
                &path,
                &id,
                Some(TEST_SECRET),
                &lookup,
            )
            .await
            .unwrap();
            assert_eq!(outcome, ForwardOutcome::Skipped, "{kind:?}");
            assert!(nothing_was_sent(&listener).await, "{kind:?}");
            let item = notifications::list(&path, None, 1).unwrap().items.remove(0);
            assert_eq!((item.id.as_str(), item.kind), (id.as_str(), kind));

            // 有効にした側は転送され、同じ区分で記録される。
            let path = test_temp_dir().join("c.toml");
            let (url, server) = mock_webhook().await;
            write_config_filtered(&path, Some(&url), forwarded);
            let id = format!("chan:13{n}");
            let outcome = handle_notification_data(
                DEFAULT_ACCOUNT_ID,
                &data,
                &None,
                &keys,
                &path,
                &id,
                Some(TEST_SECRET),
                &lookup,
            )
            .await
            .unwrap();
            assert_eq!(outcome, ForwardOutcome::Forwarded, "{kind:?}");
            // Webhook 本文は受信した payload のままで、区分は載らない。
            let body = server.await.unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&body).unwrap(),
                serde_json::from_str::<serde_json::Value>(LOOKED_UP_TWEET).unwrap()
            );
            let item = notifications::list(&path, None, 1).unwrap().items.remove(0);
            assert_eq!((item.id.as_str(), item.kind), (id.as_str(), kind));
        }
    }

    #[tokio::test]
    async fn failed_lookup_leaves_the_notification_a_tweet() {
        let failing = [
            (
                "500",
                Some(http_response(
                    "500 Internal Server Error",
                    "text/plain",
                    b"x",
                )),
            ),
            ("refused", None),
        ];
        for (name, response) in failing {
            let api = match response {
                Some(response) => Some(MockApi::start(response).await),
                None => None,
            };
            let lookup = match &api {
                Some(api) => TweetLookup::with_endpoint(&api.url).unwrap(),
                None => no_lookup(),
            };
            let (data, keys) = encrypted_message(LOOKED_UP_TWEET);

            // quote と reply を無効にしていても、通常の投稿として転送される。
            let path = test_temp_dir().join("c.toml");
            let (url, server) = mock_webhook().await;
            let filter = NotificationFilter {
                quote: false,
                reply: false,
                ..NotificationFilter::default()
            };
            write_config_filtered(&path, Some(&url), filter);
            let outcome = handle_notification_data(
                DEFAULT_ACCOUNT_ID,
                &data,
                &None,
                &keys,
                &path,
                "chan:100",
                Some(TEST_SECRET),
                &lookup,
            )
            .await
            .unwrap();
            assert_eq!(outcome, ForwardOutcome::Forwarded, "{name}");
            assert!(server.await.unwrap().contains("Looked up"));
            let item = notifications::list(&path, None, 1).unwrap().items.remove(0);
            assert_eq!(item.kind, notifications::NotificationKind::Tweet, "{name}");

            // tweet を無効にすると、その通常の投稿として除外される。
            let path = test_temp_dir().join("c.toml");
            let (url, listener) = silent_webhook().await;
            write_config_filtered(&path, Some(&url), only(false, true, true));
            let outcome = handle_notification_data(
                DEFAULT_ACCOUNT_ID,
                &data,
                &None,
                &keys,
                &path,
                "chan:101",
                Some(TEST_SECRET),
                &lookup,
            )
            .await
            .unwrap();
            assert_eq!(outcome, ForwardOutcome::Skipped, "{name}");
            assert!(nothing_was_sent(&listener).await, "{name}");
            let item = notifications::list(&path, None, 1).unwrap().items.remove(0);
            assert_eq!(item.kind, notifications::NotificationKind::Tweet, "{name}");
        }
    }

    #[tokio::test]
    async fn only_plain_tweets_are_looked_up() {
        use notifications::NotificationKind::{Other, Retweet, XchatMessageReceived};
        let api = MockApi::start(json_response(
            r#"{"__typename":"Tweet","quoted_tweet":{"id_str":"1"}}"#,
        ))
        .await;
        let lookup = TweetLookup::with_endpoint(&api.url).unwrap();
        let cases = [
            (
                r#"{"data":{"type":"tweet","uri":"/a/status/1","tag":"tweet-2"}}"#,
                Retweet,
            ),
            (
                r#"{"data":{"type":"xchat_message_received","tag":"tweet-3"}}"#,
                XchatMessageReceived,
            ),
            (r#"{"data":{"type":"novel","tag":"tweet-4"}}"#, Other),
            (r#"{"data":{"tag":"tweet-5"}}"#, Other),
            (r#"{"title":"no data"}"#, Other),
        ];
        let path = test_temp_dir().join("c.toml");
        write_config_with(&path, None);
        for (n, (message, kind)) in cases.into_iter().enumerate() {
            let (data, keys) = encrypted_message(message);
            let id = format!("chan:11{n}");
            let outcome = handle_notification_data(
                DEFAULT_ACCOUNT_ID,
                &data,
                &None,
                &keys,
                &path,
                &id,
                Some(TEST_SECRET),
                &lookup,
            )
            .await
            .unwrap();
            assert_eq!(outcome, ForwardOutcome::Skipped);
            let item = notifications::list(&path, None, 1).unwrap().items.remove(0);
            assert_eq!(
                (item.id.as_str(), item.kind),
                (id.as_str(), kind),
                "{message}"
            );
        }
        assert!(api.requests().is_empty());
    }

    #[tokio::test]
    async fn only_enabled_kinds_are_forwarded() {
        let path = test_temp_dir().join("c.toml");
        let (url, server) = mock_webhook().await;
        write_config_filtered(&path, Some(&url), only(false, true, false));
        let (data, keys) =
            encrypted_message(r#"{"title":"Chat","data":{"type":"xchat_message_received"}}"#);

        let outcome = handle_notification_data(
            DEFAULT_ACCOUNT_ID,
            &data,
            &None,
            &keys,
            &path,
            "chan:20",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, ForwardOutcome::Forwarded);
        assert!(server.await.unwrap().contains("Chat"));
    }

    #[tokio::test]
    async fn outer_data_type_is_used_when_the_wrapper_has_none() {
        let path = test_temp_dir().join("c.toml");
        let (url, server) = mock_webhook().await;
        write_config_filtered(&path, Some(&url), only(true, false, false));
        let (data, keys) =
            encrypted_message(r#"{"notification":{"title":"Wrapped"},"data":{"type":"tweet"}}"#);

        let outcome = handle_notification_data(
            DEFAULT_ACCOUNT_ID,
            &data,
            &None,
            &keys,
            &path,
            "chan:21",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, ForwardOutcome::Forwarded);
        assert!(server.await.unwrap().contains("Wrapped"));
    }

    #[tokio::test]
    async fn oversized_payload_is_classified_before_truncation_and_filtered() {
        let path = test_temp_dir().join("c.toml");
        let (url, listener) = silent_webhook().await;
        write_config_filtered(&path, Some(&url), only(false, true, true));
        let message = serde_json::json!({
            "title": "Huge",
            "data": { "type": "tweet", "blob": "x".repeat(40_000) },
        })
        .to_string();
        let (data, keys) = encrypted_message(&message);

        let outcome = handle_notification_data(
            DEFAULT_ACCOUNT_ID,
            &data,
            &None,
            &keys,
            &path,
            "chan:22",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, ForwardOutcome::Skipped);
        assert!(nothing_was_sent(&listener).await);
        let item = notifications::list(&path, None, 1).unwrap().items.remove(0);
        assert_eq!(item.payload["truncated"], serde_json::json!(true));
        assert_eq!(item.kind, notifications::NotificationKind::Tweet);
    }

    #[tokio::test]
    async fn filtered_notification_is_skipped_even_without_a_webhook() {
        let path = test_temp_dir().join("c.toml");
        write_config_filtered(&path, None, only(false, true, true));
        let (data, keys) = encrypted_message(r#"{"title":"T","data":{"type":"tweet"}}"#);

        let outcome = handle_notification_data(
            DEFAULT_ACCOUNT_ID,
            &data,
            &None,
            &keys,
            &path,
            "chan:23",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, ForwardOutcome::Skipped);
        assert_eq!(notifications::list(&path, None, 10).unwrap().items.len(), 1);
    }

    #[tokio::test]
    async fn unreadable_config_is_a_forwarding_error_but_history_is_kept() {
        let path = test_temp_dir().join("c.toml");
        std::fs::write(&path, "twitter = [broken").unwrap();
        let (data, keys) = encrypted_message(r#"{"title":"Kept","data":{"type":"tweet"}}"#);

        let result = handle_notification_data(
            DEFAULT_ACCOUNT_ID,
            &data,
            &None,
            &keys,
            &path,
            "chan:24",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await;

        assert!(result.is_err());
        assert!(!is_decryption_error(&result.unwrap_err()));
        let page = notifications::list(&path, None, 10).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].title, "Kept");
    }

    /// アカウントの履歴DBに記録された通知 ID（古い順）。DB が無ければ空。
    fn history_ids(config: &Path, account_id: &str) -> Vec<String> {
        let db = notifications::history_path_for(config, account_id);
        if !db.exists() {
            return Vec::new();
        }
        let conn = rusqlite::Connection::open(db).unwrap();
        let mut statement = conn
            .prepare("SELECT id FROM notifications ORDER BY seq")
            .unwrap();
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap();
        rows.map(|id| id.unwrap()).collect()
    }

    const TWEET: &str = r#"{"title":"Shared","data":{"type":"tweet"}}"#;

    #[tokio::test]
    async fn accounts_do_not_share_filter_endpoint_secret_or_history() {
        let path = test_temp_dir().join("c.toml");
        let (default_url, default_hook) = silent_webhook().await;
        let (work_url, work_server) = serve_once(OK_RESPONSE.to_string()).await;
        write_accounts(
            &path,
            vec![
                // `default` は tweet を転送しない設定。
                account_named("default", Some(&default_url), only(false, true, true)),
                account_named("work", Some(&work_url), NotificationFilter::default()),
            ],
        );
        let (data, keys) = encrypted_message(TWEET);

        // `work` は自分の転送先へ、自分の secret で署名して送る。環境変数の secret は使わない。
        let outcome = handle_notification_data(
            "work",
            &data,
            &None,
            &keys,
            &path,
            "chan:work",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, ForwardOutcome::Forwarded);
        let captured = work_server.await.unwrap();
        let header = captured.header("webhook-signature").unwrap();
        assert!(verifies("work-secret", header, &captured.body));
        assert!(!verifies(TEST_SECRET, header, &captured.body));
        assert!(!verifies("default-secret", header, &captured.body));
        assert!(nothing_was_sent(&default_hook).await);

        // 同じ通知が `default` にも届いても、`default` のフィルターで除外され、どこにも送らない。
        let outcome = handle_notification_data(
            "default",
            &data,
            &None,
            &keys,
            &path,
            "chan:default",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, ForwardOutcome::Skipped);
        assert!(nothing_was_sent(&default_hook).await);

        // 履歴はアカウントごとのファイルに、それぞれの通知だけが残る。
        assert_eq!(history_ids(&path, "work"), ["chan:work"]);
        assert_eq!(history_ids(&path, "default"), ["chan:default"]);
        assert_ne!(
            notifications::history_path_for(&path, "work"),
            notifications::history_path_for(&path, "default")
        );
    }

    #[tokio::test]
    async fn the_secret_environment_variable_signs_default_only() {
        let path = test_temp_dir().join("c.toml");
        let (url, server) = serve_once(OK_RESPONSE.to_string()).await;
        write_accounts(
            &path,
            vec![account_named(
                "default",
                Some(&url),
                NotificationFilter::default(),
            )],
        );
        let (data, keys) = encrypted_message(TWEET);

        let outcome = handle_notification_data(
            "default",
            &data,
            &None,
            &keys,
            &path,
            "chan:1",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, ForwardOutcome::Forwarded);
        let captured = server.await.unwrap();
        let header = captured.header("webhook-signature").unwrap();
        assert!(verifies(TEST_SECRET, header, &captured.body));
        assert!(!verifies("default-secret", header, &captured.body));
    }

    #[tokio::test]
    async fn another_account_never_falls_back_to_the_environment_secret() {
        let path = test_temp_dir().join("c.toml");
        let (url, hook) = silent_webhook().await;
        let mut work = account_named("work", Some(&url), NotificationFilter::default());
        work.webhook_secret = None;
        write_accounts(&path, vec![work]);
        let (data, keys) = encrypted_message(TWEET);

        let error = handle_notification_data(
            "work",
            &data,
            &None,
            &keys,
            &path,
            "chan:2",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, AngelicAngelError::Config(_)), "{error}");
        // どのアカウントの secret が無いかが分かり、値そのものは出ない。
        let message = error.to_string();
        assert!(message.contains("\"work\""), "{message}");
        assert!(!message.contains(TEST_SECRET), "{message}");
        assert!(nothing_was_sent(&hook).await);
        assert_eq!(history_ids(&path, "work"), ["chan:2"]);
    }

    #[test]
    fn missing_secret_error_names_the_account_without_a_secret_value() {
        let default = missing_secret_error(DEFAULT_ACCOUNT_ID).to_string();
        assert!(default.contains(config::WEBHOOK_SECRET_ENV), "{default}");
        assert!(default.contains("\"default\""), "{default}");

        let other = missing_secret_error("work-2").to_string();
        assert!(other.contains("\"work-2\""), "{other}");
        // 環境変数は `default` 専用なので、ほかのアカウントには案内しない。
        assert!(!other.contains(config::WEBHOOK_SECRET_ENV), "{other}");
    }

    #[tokio::test]
    async fn notification_for_a_removed_account_is_neither_recorded_nor_forwarded() {
        let path = test_temp_dir().join("c.toml");
        let (url, hook) = silent_webhook().await;
        write_accounts(
            &path,
            vec![account_named(
                "default",
                Some(&url),
                NotificationFilter::default(),
            )],
        );
        let (data, keys) = encrypted_message(TWEET);

        let outcome = handle_notification_data(
            "ghost",
            &data,
            &None,
            &keys,
            &path,
            "chan:3",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, ForwardOutcome::AccountRemoved);
        assert!(nothing_was_sent(&hook).await);
        // 削除済みのアカウントの履歴ファイルを作り直さない。
        assert!(!notifications::history_path_for(&path, "ghost").exists());
        assert!(history_ids(&path, "default").is_empty());
    }

    #[tokio::test]
    async fn live_config_edits_apply_to_the_next_notification_of_that_account() {
        let path = test_temp_dir().join("c.toml");
        let (url, server) = serve_once(OK_RESPONSE.to_string()).await;
        let (other_url, other_hook) = silent_webhook().await;
        write_accounts(
            &path,
            vec![
                account_named("default", Some(&other_url), NotificationFilter::default()),
                account_named("work", None, NotificationFilter::default()),
            ],
        );
        let (data, keys) = encrypted_message(TWEET);

        let skipped = handle_notification_data(
            "work",
            &data,
            &None,
            &keys,
            &path,
            "chan:4",
            None,
            &no_lookup(),
        )
        .await
        .unwrap();
        assert_eq!(skipped, ForwardOutcome::Skipped);

        // 受信中に `work` の転送先だけを設定しても、次の通知から反映される。
        let mut config = Config::load(&path).unwrap();
        config.account_mut("work").unwrap().webhook_endpoint = Some(url);
        config.save(&path).unwrap();
        let forwarded = handle_notification_data(
            "work",
            &data,
            &None,
            &keys,
            &path,
            "chan:5",
            None,
            &no_lookup(),
        )
        .await
        .unwrap();
        assert_eq!(forwarded, ForwardOutcome::Forwarded);
        assert!(server.await.unwrap().header("webhook-signature").is_some());
        assert!(nothing_was_sent(&other_hook).await);
    }

    fn hex_decode(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    /// `t=<秒>,v1=<hex>` を受信側として検証する。
    fn verifies(secret: &str, header: &str, body: &[u8]) -> bool {
        let Some((t, v1)) = header
            .strip_prefix("t=")
            .and_then(|rest| rest.split_once(",v1="))
        else {
            return false;
        };
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(t.as_bytes());
        mac.update(b".");
        mac.update(body);
        v1.len() == 64
            && v1
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && mac.verify_slice(&hex_decode(v1)).is_ok()
    }

    // 同じシークレット・時刻・本文を Python hmac と openssl dgst でも計算して一致を確認した既知解。
    const VECTOR_BODY: &[u8] = "{\"body\":\"quote \\\" é\",\"title\":\"通知\\n二行目\"}".as_bytes();
    const VECTOR_HEADER: &str =
        "t=1700000000,v1=860c9bda7c63dd307b17465f32b8a9a3f11327f6050ca8b8c7ad70d3c764ce2b";

    #[test]
    fn signature_matches_the_known_answer() {
        let value = signature_header(TEST_SECRET, 1_700_000_000, VECTOR_BODY).unwrap();

        assert_eq!(value.to_str().unwrap(), VECTOR_HEADER);
        assert!(value.is_sensitive());
    }

    #[test]
    fn signature_changes_with_secret_timestamp_or_body() {
        assert!(verifies(TEST_SECRET, VECTOR_HEADER, VECTOR_BODY));
        assert!(!verifies("whsec_other", VECTOR_HEADER, VECTOR_BODY));
        assert!(!verifies(
            TEST_SECRET,
            &VECTOR_HEADER.replace("t=1700000000", "t=1700000001"),
            VECTOR_BODY
        ));
        assert!(!verifies(TEST_SECRET, VECTOR_HEADER, b"{\"body\":\"x\"}"));
        // シークレットは trim も hex デコードもせずそのまま UTF-8 として使う。
        let spaced = signature_header(" whsec ", 1, b"{}").unwrap();
        assert!(verifies(" whsec ", spaced.to_str().unwrap(), b"{}"));
        assert!(!verifies("whsec", spaced.to_str().unwrap(), b"{}"));
    }

    #[test]
    fn webhook_url_allows_https_and_local_http_only() {
        for ok in [
            "https://example.com/hook?token=s",
            "http://localhost:8080/hook",
            "http://host.docker.internal:15175/api/webhook",
            "http://127.0.0.1:9/hook",
            "http://127.0.0.2/hook",
            "http://[::1]:3000/hook",
        ] {
            assert!(checked_webhook_url(ok).is_ok(), "{ok}");
        }
        for ng in [
            "http://example.com/hook",
            "http://192.0.2.1/hook",
            "http://localhost.example.com/hook",
            "http://127.0.0.1.example.com/hook",
            "http://127.0.0.1@example.com/hook",
            "http://host.docker.internal.example.com/hook",
            "http://host.docker.internal@example.com/hook",
            "http://myhost.docker.internal/hook",
            "ftp://host.docker.internal/hook",
            "http://[2001:db8::1]/hook",
            "ftp://127.0.0.1/hook",
            "not a url",
        ] {
            assert!(
                matches!(checked_webhook_url(ng), Err(AngelicAngelError::Config(_))),
                "{ng}"
            );
        }
    }

    #[tokio::test]
    async fn webhook_request_is_signed_over_the_exact_body_bytes() {
        let path = test_temp_dir().join("c.toml");
        let (url, server) = serve_once(OK_RESPONSE.to_string()).await;
        write_config(&path, &url);
        let message =
            r#"{"title":"通知\n二行目","body":"quote \" é \u0000 /","data":{"type":"tweet"}}"#;
        let (data, keys) = encrypted_message(message);
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let outcome = handle_notification_data(
            DEFAULT_ACCOUNT_ID,
            &data,
            &None,
            &keys,
            &path,
            "chan:30",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await
        .unwrap();

        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(outcome, ForwardOutcome::Forwarded);
        let captured = server.await.unwrap();
        assert_eq!(captured.request_line, "POST /hook HTTP/1.1");
        assert_eq!(captured.header("content-type"), Some("application/json"));
        let header = captured.header("webhook-signature").unwrap();
        let t: u64 = header
            .strip_prefix("t=")
            .and_then(|r| r.split_once(','))
            .unwrap()
            .0
            .parse()
            .unwrap();
        assert!(
            (before..=after).contains(&t),
            "{t} not in {before}..={after}"
        );
        assert!(verifies(TEST_SECRET, header, &captured.body));
        assert!(!verifies("whsec_other", header, &captured.body));
        let mut tampered = captured.body.clone();
        tampered.push(b' ');
        assert!(!verifies(TEST_SECRET, header, &tampered));
        let expected =
            serde_json::to_vec(&serde_json::from_str::<serde_json::Value>(message).unwrap())
                .unwrap();
        assert_eq!(captured.body, expected);
        let text = std::str::from_utf8(&captured.body).unwrap();
        assert!(text.contains("通知\\n二行目"));
        assert!(text.contains("\\\" é"));
        assert!(text.contains("\\u0000"));
    }

    #[tokio::test]
    async fn missing_or_blank_secret_blocks_sending_but_keeps_history() {
        for (n, secret) in [None, Some(""), Some("  \t\n")].into_iter().enumerate() {
            let path = test_temp_dir().join("c.toml");
            let (url, listener) = silent_webhook().await;
            write_config(&path, &url);
            let (data, keys) = encrypted_message(r#"{"title":"Unsigned","data":{"type":"tweet"}}"#);

            let result = handle_notification_data(
                DEFAULT_ACCOUNT_ID,
                &data,
                &None,
                &keys,
                &path,
                &format!("chan:4{n}"),
                secret,
                &no_lookup(),
            )
            .await;

            let error = result.unwrap_err();
            assert!(matches!(error, AngelicAngelError::Config(_)), "{secret:?}");
            assert!(!is_decryption_error(&error));
            assert!(nothing_was_sent(&listener).await, "{secret:?}");
            let page = notifications::list(&path, None, 10).unwrap();
            assert_eq!(page.items.len(), 1, "{secret:?}");
            assert_eq!(page.items[0].title, "Unsigned");
        }
    }

    #[tokio::test]
    async fn skipped_outcomes_do_not_need_a_secret() {
        let path = test_temp_dir().join("c.toml");
        let (url, listener) = silent_webhook().await;
        write_config_filtered(&path, Some(&url), only(false, true, true));
        let (data, keys) = encrypted_message(r#"{"title":"F","data":{"type":"tweet"}}"#);
        let outcome = handle_notification_data(
            DEFAULT_ACCOUNT_ID,
            &data,
            &None,
            &keys,
            &path,
            "chan:50",
            None,
            &no_lookup(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, ForwardOutcome::Skipped);
        assert!(nothing_was_sent(&listener).await);
        assert_eq!(notifications::list(&path, None, 10).unwrap().items.len(), 1);

        let path = test_temp_dir().join("c.toml");
        write_config_with(&path, None);
        let (data, keys) = encrypted_message(r#"{"title":"U"}"#);
        let outcome = handle_notification_data(
            DEFAULT_ACCOUNT_ID,
            &data,
            &None,
            &keys,
            &path,
            "chan:51",
            Some(" "),
            &no_lookup(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, ForwardOutcome::Skipped);
        assert_eq!(notifications::list(&path, None, 10).unwrap().items.len(), 1);
    }

    #[tokio::test]
    async fn non_loopback_http_is_rejected_without_touching_the_network() {
        let path = test_temp_dir().join("c.toml");
        write_config(&path, "http://192.0.2.1/hook?token=secret");
        let (data, keys) = encrypted_message(r#"{"title":"Plain","data":{"type":"tweet"}}"#);

        let result = handle_notification_data(
            DEFAULT_ACCOUNT_ID,
            &data,
            &None,
            &keys,
            &path,
            "chan:60",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await;

        let error = result.unwrap_err();
        assert!(matches!(error, AngelicAngelError::Config(_)));
        assert!(!error.to_string().contains("secret"));
        assert_eq!(notifications::list(&path, None, 10).unwrap().items.len(), 1);
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let path = test_temp_dir().join("c.toml");
        let (target_url, target) = silent_webhook().await;
        let redirect = format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: {target_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let (url, origin) = serve_once(redirect).await;
        write_config(&path, &url);
        let (data, keys) = encrypted_message(r#"{"title":"Moved","data":{"type":"tweet"}}"#);

        let outcome = handle_notification_data(
            DEFAULT_ACCOUNT_ID,
            &data,
            &None,
            &keys,
            &path,
            "chan:70",
            Some(TEST_SECRET),
            &no_lookup(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, ForwardOutcome::Failed);
        assert!(origin.await.unwrap().header("webhook-signature").is_some());
        assert!(nothing_was_sent(&target).await);
    }
}
