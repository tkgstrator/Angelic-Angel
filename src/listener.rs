use crate::autopush::{self, ConnectResult};
use crate::config::{self, Config, Registration};
use crate::error::{AngelicAngelError, Result};
use crate::notifications;
use crate::status::Telemetry;
use crate::twitter;
use reqwest::Client;
use std::path::Path;
use std::time::Duration;

const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForwardOutcome {
    Forwarded,
    Failed,
    Skipped,
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
pub async fn listen(registration: Registration, config_path: &Path) -> Result<()> {
    listen_managed(registration, config_path, &Telemetry::default()).await
}

/// Same as [`listen`], additionally reporting state and counters to `telemetry`.
pub async fn listen_managed(
    mut registration: Registration,
    config_path: &Path,
    telemetry: &Telemetry,
) -> Result<()> {
    let mut retry_count: u32 = 0;
    telemetry.set_connecting();

    loop {
        match listen_once(&mut registration, config_path, telemetry).await {
            SessionOutcome::NormalClose => {
                retry_count = 0;
                telemetry.set_retrying("connection closed");
                tracing::info!("WebSocket connection closed, reconnecting");
                tokio::time::sleep(Duration::from_secs(1)).await;
            },
            SessionOutcome::DisconnectedAfterConnect(e) => {
                if e.to_string().contains("BACKOFF:") {
                    telemetry.set_retrying("server requested backoff");
                    tracing::warn!("server requested backoff, delaying reconnect for 30 minutes");
                    tokio::time::sleep(Duration::from_secs(30 * 60)).await;
                } else {
                    retry_count = 0;
                    telemetry.set_retrying("connection lost");
                    tracing::info!(error = %e, "disconnected after connect, reconnecting");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            },
            SessionOutcome::ConnectionFailed(e) => {
                retry_count += 1;
                telemetry.set_retrying("connection failed");
                let delay = calc_backoff(retry_count);
                tracing::warn!(
                    retry_count,
                    delay_secs = delay,
                    error = %e,
                    "WebSocket connection failed, retrying"
                );
                tokio::time::sleep(Duration::from_secs(delay)).await;
            },
            SessionOutcome::Fatal(e) => {
                telemetry.set_error("re-registration required");
                tracing::error!(error = %e, "fatal error, re-registration required");
                return Err(e);
            },
        }
    }
}

/// Runs a single listen session: connect, receive notifications, return outcome.
async fn listen_once(
    registration: &mut Registration,
    config_path: &Path,
    telemetry: &Telemetry,
) -> SessionOutcome {
    let mut client = match try_connect(registration, config_path).await {
        Ok(client) => client,
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

    match run_notification_loop(&mut client, registration, config_path, telemetry).await {
        Ok(()) => SessionOutcome::NormalClose,
        Err(e) => SessionOutcome::DisconnectedAfterConnect(e),
    }
}

/// Establishes a connection to AutoPush, handling UAID invalidation transparently.
async fn try_connect(
    registration: &mut Registration,
    config_path: &Path,
) -> Result<autopush::AutoPushClient> {
    let connect_result =
        autopush::connect_and_listen(&registration.autopush, &registration.keys).await?;

    match connect_result {
        ConnectResult::Connected(client) => {
            tracing::info!("connected with existing session");
            Ok(*client)
        },
        ConnectResult::NeedsReregistration(reregistration_info) => {
            let new_reg = &reregistration_info.registration;
            let new_keys = reregistration_info.keys;

            tracing::warn!(
                new_uaid = %new_reg.uaid,
                new_channel_id = %new_reg.channel_id,
                "UAID invalidated (pushsubscriptionchange), re-registering"
            );

            tracing::info!("re-registering with Twitter API");
            let full_config = config::Config::load(config_path)?;

            let subscription = crate::push::PushSubscription {
                endpoint: new_reg.endpoint.clone(),
                autopush: config::AutoPushSession {
                    uaid: new_reg.uaid.clone(),
                    channel_id: new_reg.channel_id.clone(),
                },
                keys: new_keys.clone(),
            };

            twitter::register(&full_config.twitter, &subscription).await?;
            tracing::info!("Twitter API re-registration complete");

            registration.endpoint = new_reg.endpoint.clone();
            registration.autopush.uaid = new_reg.uaid.clone();
            registration.autopush.channel_id = new_reg.channel_id.clone();
            registration.keys = new_keys;
            // Re-read the latest config so concurrent settings edits are never overwritten.
            let updated = registration.clone();
            config::Config::modify(config_path, |current| {
                Ok::<_, AngelicAngelError>(match current {
                    Some(mut latest) if latest.registration.is_some() => {
                        latest.registration = Some(updated);
                        (Some(latest), ())
                    },
                    _ => (None, ()),
                })
            })?;
            tracing::info!("saved updated registration");

            match autopush::connect_and_listen(&registration.autopush, &registration.keys).await? {
                ConnectResult::Connected(client) => Ok(*client),
                ConnectResult::NeedsReregistration(_) => Err(AngelicAngelError::AutoPush(
                    "still failed after re-registration".to_string(),
                )),
            }
        },
    }
}

/// Receives and processes notifications in a loop until the connection drops.
async fn run_notification_loop(
    client: &mut autopush::AutoPushClient,
    registration: &Registration,
    config_path: &Path,
    telemetry: &Telemetry,
) -> Result<()> {
    while let Some(notification) = client.next_notification().await? {
        tracing::info!(
            channel_id = %notification.channel_id,
            version = %notification.version,
            "notification received"
        );

        telemetry.record_received();
        let ack_code = if let Some(ref data) = notification.data {
            let history_id = format!("{}:{}", notification.channel_id, notification.version);
            match handle_notification_data(
                data,
                &notification.headers,
                &registration.keys,
                config_path,
                &history_id,
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
    }

    Ok(())
}

fn is_decryption_error(e: &AngelicAngelError) -> bool {
    matches!(e, AngelicAngelError::Decryption(_))
}

async fn handle_notification_data(
    data: &str,
    headers: &Option<std::collections::HashMap<String, String>>,
    keys: &crate::config::WebPushKeys,
    config_path: &Path,
    history_id: &str,
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

    record_history(config_path, history_id, &payload).await;

    // 履歴保存後に一度だけ設定を読み、フィルターと Webhook の解決に同じ内容を使う。
    let config = Config::load_optional(config_path)?;
    let filter = config
        .as_ref()
        .map(|c| c.notification_filter)
        .unwrap_or_default();
    if !filter.allows(notifications::classify(&payload)) {
        tracing::info!("notification kind is disabled by the filter, skipping forward");
        return Ok(ForwardOutcome::Skipped);
    }
    let stored = config.as_ref().and_then(|c| c.webhook_endpoint.as_deref());
    let Some(webhook_url) = config::resolve_webhook(stored).0 else {
        tracing::info!("webhook endpoint is not set, skipping forward");
        return Ok(ForwardOutcome::Skipped);
    };

    send_to_webhook(&payload, &webhook_url).await
}

/// Stores the notification in the local history. A failure never blocks forwarding or the ACK.
async fn record_history(config_path: &Path, history_id: &str, payload: &serde_json::Value) {
    let config_path = config_path.to_path_buf();
    let history_id = history_id.to_owned();
    let payload = payload.clone();
    let saved = tokio::task::spawn_blocking(move || {
        notifications::append(&config_path, &history_id, &payload)
    })
    .await;
    if !matches!(saved, Ok(Ok(()))) {
        tracing::warn!("failed to record notification in local history");
    }
}

/// Sends the decrypted notification payload to the configured webhook endpoint via HTTP POST.
///
/// Returns whether the endpoint answered with a 2xx status.
async fn send_to_webhook(payload: &serde_json::Value, webhook_url: &str) -> Result<ForwardOutcome> {
    let client = Client::builder().timeout(WEBHOOK_TIMEOUT).build()?;

    tracing::info!(url = %webhook_url, "sending to webhook");

    let response = client.post(webhook_url).json(payload).send().await?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| String::from("failed to read response body"));
        tracing::warn!(status = %status, body = %error_text, "webhook request failed");
        Ok(ForwardOutcome::Failed)
    } else {
        tracing::info!(status = %response.status(), "webhook request succeeded");
        Ok(ForwardOutcome::Forwarded)
    }
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
    use crate::config::{Config, NotificationFilter, TwitterConfig, WebPushKeys, test_temp_dir};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Accepts one HTTP request and answers 200, handing the request body to the caller.
    async fn mock_webhook() -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = stream.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).into_owned();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(str::to_owned)
                        })
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if body.len() >= length || n == 0 {
                        stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await
                            .unwrap();
                        return body.to_string();
                    }
                }
                if n == 0 {
                    return String::new();
                }
            }
        });
        (url, task)
    }

    fn write_config(path: &Path, webhook: &str) {
        write_config_with(path, Some(webhook));
    }

    fn write_config_with(path: &Path, webhook: Option<&str>) {
        write_config_filtered(path, webhook, NotificationFilter::default());
    }

    fn write_config_filtered(path: &Path, webhook: Option<&str>, filter: NotificationFilter) {
        Config {
            twitter: TwitterConfig {
                auth_token: "synthetic".into(),
                ct0: "synthetic".into(),
            },
            registration: None,
            webhook_endpoint: webhook.map(str::to_string),
            notification_filter: filter,
        }
        .save(path)
        .unwrap();
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

        let outcome = handle_notification_data(&data, &None, &keys, &path, "chan:7")
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

        let outcome = handle_notification_data(&data, &None, &keys, &path, "chan:8")
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

        handle_notification_data(&data, &None, &keys, &path, "chan:9")
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

        let outcome = handle_notification_data(&data, &None, &keys, &path, id)
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

        let outcome = handle_notification_data(&data, &None, &keys, &path, "chan:12")
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

            let outcome =
                handle_notification_data(&data, &None, &keys, &path, &format!("chan:{n}"))
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
    async fn only_enabled_kinds_are_forwarded() {
        let path = test_temp_dir().join("c.toml");
        let (url, server) = mock_webhook().await;
        write_config_filtered(&path, Some(&url), only(false, true, false));
        let (data, keys) =
            encrypted_message(r#"{"title":"Chat","data":{"type":"xchat_message_received"}}"#);

        let outcome = handle_notification_data(&data, &None, &keys, &path, "chan:20")
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

        let outcome = handle_notification_data(&data, &None, &keys, &path, "chan:21")
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

        let outcome = handle_notification_data(&data, &None, &keys, &path, "chan:22")
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

        let outcome = handle_notification_data(&data, &None, &keys, &path, "chan:23")
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

        let result = handle_notification_data(&data, &None, &keys, &path, "chan:24").await;

        assert!(result.is_err());
        assert!(!is_decryption_error(&result.unwrap_err()));
        let page = notifications::list(&path, None, 10).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].title, "Kept");
    }
}
