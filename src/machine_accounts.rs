//! Account management for machine clients: verify cookies, create an account, replace its cookies
//! or webhook, set its permissions, delete it. Cookies are write-only: nothing here returns, logs
//! or formats one. Only accounts this principal created (`machine_owner`) can be managed, and any
//! other account answers with the same 404 as one that does not exist.
use super::{Api, Failure, Machine, MachineBackend, PREFIX, Shared, TIMEOUT, failure, require_json};
use crate::config::{Account, TwitterConfig};
use crate::listener::checked_webhook_url;
use crate::twitter::{Identity, XError};
use crate::web::error::ApiError;
use crate::web::service::{Backend, MachineProfile, Service};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, post, put};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

#[path = "machine_accounts_flow.rs"]
mod flow;
#[cfg(test)]
#[path = "machine_accounts_tests.rs"]
mod tests;

const MAX_COOKIE_LEN: usize = 1024;
const MAX_SECRET_LEN: usize = 4096;
const MAX_URL_LEN: usize = 2048;
const AVATAR_SIZES: [&str; 3] = ["_normal", "_bigger", "_mini"];

/// Which permissions to change. A missing key keeps the saved value; a new account starts off.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Switches {
    post: Option<bool>,
    like: Option<bool>,
    follow: Option<bool>,
}

impl Switches {
    fn apply(&self, account: &mut Account) {
        account.machine_post_enabled = self.post.unwrap_or(account.machine_post_enabled);
        account.machine_like_enabled = self.like.unwrap_or(account.machine_like_enabled);
        account.machine_follow_enabled = self.follow.unwrap_or(account.machine_follow_enabled);
    }
}

#[derive(Serialize)]
struct Granted {
    post: bool,
    like: bool,
    follow: bool,
}

impl From<&MachineProfile> for Granted {
    fn from(profile: &MachineProfile) -> Self {
        Self {
            post: profile.post,
            like: profile.like,
            follow: profile.follow,
        }
    }
}

/// The identity as the Bot reads it: camelCase, with the avatar in its original size.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicIdentity {
    user_id: String,
    screen_name: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    avatar_url: Option<String>,
}

impl From<Identity> for PublicIdentity {
    fn from(identity: Identity) -> Self {
        Self {
            user_id: identity.user_id,
            screen_name: identity.screen_name,
            name: identity.name,
            avatar_url: identity.avatar_url.as_deref().map(original_avatar),
        }
    }
}

/// The profile picture without the `_normal`, `_bigger` or `_mini` size suffix.
fn original_avatar(url: &str) -> String {
    let Some((directory, file)) = url.rsplit_once('/') else {
        return url.to_owned();
    };
    let (name, extension) = match file.rsplit_once('.') {
        Some((name, extension)) => (name, format!(".{extension}")),
        None => (file, String::new()),
    };
    for size in AVATAR_SIZES {
        if let Some(original) = name.strip_suffix(size) {
            return format!("{directory}/{original}{extension}");
        }
    }
    url.to_owned()
}

fn invalid() -> Failure {
    failure(StatusCode::BAD_REQUEST, "invalid_request")
}

fn missing() -> Failure {
    failure(StatusCode::NOT_FOUND, "account_not_found")
}

fn mismatch() -> Failure {
    failure(StatusCode::CONFLICT, "identity_mismatch")
}

/// The fixed answer for a failed account operation. The service's own wording is never passed on.
fn service_failure(error: ApiError) -> Failure {
    match error.code {
        "account_not_found" => missing(),
        "account_exists" => failure(StatusCode::CONFLICT, "account_exists"),
        "busy" | "following_job_active" | "credentials_locked" => {
            failure(StatusCode::CONFLICT, "busy")
        },
        "history_cleanup_required" => failure(StatusCode::CONFLICT, "history_cleanup_required"),
        "history_cleanup_failed" => {
            failure(StatusCode::INTERNAL_SERVER_ERROR, "history_cleanup_failed")
        },
        "provider_failure" => failure(StatusCode::BAD_GATEWAY, "registration_failed"),
        _ => failure(StatusCode::SERVICE_UNAVAILABLE, "accounts_unavailable"),
    }
}

/// A failed deletion: the push registration could not be removed, so the account stays.
fn unregistration_failure(error: ApiError) -> Failure {
    match error.code {
        "provider_failure" => failure(StatusCode::BAD_GATEWAY, "unregistration_failed"),
        _ => service_failure(error),
    }
}

fn api<T>(result: std::result::Result<T, ApiError>) -> std::result::Result<T, Failure> {
    result.map_err(service_failure)
}

/// X refusing the cookies is the caller's to fix; any other trouble is X being unavailable.
fn x_failure(error: XError) -> Failure {
    match error {
        XError::Unauthorized | XError::Forbidden => {
            failure(StatusCode::UNPROCESSABLE_ENTITY, "credentials_rejected")
        },
        _ => failure(StatusCode::BAD_GATEWAY, "x_unavailable"),
    }
}

/// Visible ASCII only, so the value can go into a request header as it is. No `;`.
fn cookie_ok(value: &str) -> bool {
    let clean = |b: u8| b.is_ascii_graphic() && b != b';';
    !value.is_empty() && value.len() <= MAX_COOKIE_LEN && value.bytes().all(clean)
}

fn secret_ok(value: &str) -> bool {
    let printable = value.chars().all(|c| !c.is_control());
    !value.trim().is_empty() && value.len() <= MAX_SECRET_LEN && printable
}

/// The same rule as the webhook sender: HTTPS, or HTTP only to loopback or the Docker host.
fn endpoint_ok(value: &str) -> bool {
    let plain = value.len() <= MAX_URL_LEN && value.trim() == value;
    plain && !value.chars().any(char::is_control) && checked_webhook_url(value).is_ok()
}

fn cookies(auth_token: String, ct0: String) -> std::result::Result<TwitterConfig, Failure> {
    if cookie_ok(&auth_token) && cookie_ok(&ct0) {
        Ok(TwitterConfig { auth_token, ct0 })
    } else {
        Err(invalid())
    }
}

/// A strictly typed JSON object. Any problem, including an unknown field, is the same fixed 400,
/// and the parser's own message (which can quote a value) is dropped. A bare array is refused
/// too: serde would otherwise read it as the fields of a struct in order.
fn parse<T: DeserializeOwned>(headers: &HeaderMap, body: &[u8]) -> std::result::Result<T, Failure> {
    require_json(headers)?;
    let value: Value = serde_json::from_slice(body).map_err(|_| invalid())?;
    if !value.is_object() {
        return Err(invalid());
    }
    serde_json::from_value(value).map_err(|_| invalid())
}

/// The service of an account this principal owns, after the `accounts` scope check. Another
/// client's account, an unmanaged one and a missing one all give the same 404.
fn owned_service<B: Backend, X: MachineBackend>(
    state: &Machine<B, X>,
    id: &str,
) -> std::result::Result<Arc<Service<B>>, Failure> {
    state.settings.allow(None, "accounts")?;
    if !state.owns(id) {
        return Err(missing());
    }
    api(state.accounts.get(id))
}

/// The saved profile, read inside the account lock and only while this principal owns the account.
fn confirm<B: Backend, X: MachineBackend>(
    state: &Machine<B, X>,
    service: &Service<B>,
) -> std::result::Result<MachineProfile, Failure> {
    let profile = api(service.machine_profile())?;
    if profile.owner.as_deref() != Some(state.settings.client.as_str()) {
        return Err(missing());
    }
    Ok(profile)
}

/// Asks X who the cookies belong to, within the machine API's usual deadline. Stores nothing.
async fn lookup_identity<B: Backend, X: MachineBackend>(
    state: &Machine<B, X>,
    twitter: &TwitterConfig,
) -> std::result::Result<Identity, Failure> {
    tokio::time::timeout(TIMEOUT, state.x.identity(twitter))
        .await
        .map_err(|_| failure(StatusCode::GATEWAY_TIMEOUT, "x_timeout"))?
        .map_err(x_failure)
}

pub(super) fn routes<B: Backend, X: MachineBackend>() -> Router<Shared<B, X>> {
    Router::new()
        .route(
            &format!("{PREFIX}/credentials/verify"),
            post(verify_cookies::<B, X>),
        )
        .route(&format!("{PREFIX}/accounts"), post(flow::create::<B, X>))
        .route(
            &format!("{PREFIX}/accounts/{{id}}"),
            delete(delete_account::<B, X>),
        )
        .route(
            &format!("{PREFIX}/accounts/{{id}}/credentials"),
            put(flow::credentials::<B, X>),
        )
        .route(
            &format!("{PREFIX}/accounts/{{id}}/webhook"),
            put(webhook::<B, X>),
        )
        .route(
            &format!("{PREFIX}/accounts/{{id}}/permissions"),
            put(permissions::<B, X>),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Verify {
    auth_token: String,
    ct0: String,
}

#[derive(Serialize)]
struct Verified {
    identity: PublicIdentity,
}

/// Confirms who a pair of cookies belongs to without storing anything.
async fn verify_cookies<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    headers: HeaderMap,
    body: Bytes,
) -> Api<Verified> {
    state.settings.allow(None, "accounts")?;
    let request: Verify = parse(&headers, &body)?;
    let twitter = cookies(request.auth_token, request.ct0)?;
    let identity = PublicIdentity::from(lookup_identity(&state, &twitter).await?);
    Ok(Json(Verified { identity }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Webhook {
    endpoint: String,
    secret: String,
}

#[derive(Serialize)]
struct Updated {
    updated: bool,
}

/// Replaces the forwarding target and its signing secret. Cookies and revision stay as they are.
async fn webhook<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Api<Updated> {
    let service = owned_service(&state, &id)?;
    let request: Webhook = parse(&headers, &body)?;
    if !endpoint_ok(&request.endpoint) || !secret_ok(&request.secret) {
        return Err(invalid());
    }
    let _slot = api(service.machine_lock().await)?;
    confirm(&state, &service)?;
    let saved = service.machine_edit(|account| {
        account.webhook_endpoint = Some(request.endpoint);
        account.webhook_secret = Some(request.secret);
        Ok(())
    });
    api(saved)?;
    Ok(Json(Updated { updated: true }))
}

#[derive(Serialize)]
struct Permissions {
    permissions: Granted,
}

/// Sets the post, like and follow permissions. A key left out keeps its saved value.
async fn permissions<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Api<Permissions> {
    let service = owned_service(&state, &id)?;
    let request: Switches = parse(&headers, &body)?;
    let _slot = api(service.machine_lock().await)?;
    confirm(&state, &service)?;
    let saved = service.machine_edit(|account| {
        request.apply(account);
        Ok(())
    });
    api(saved)?;
    let profile = api(service.machine_profile())?;
    let permissions = Granted::from(&profile);
    Ok(Json(Permissions { permissions }))
}

/// Unregisters and deletes the account. The operation ledger keeps its records.
async fn delete_account<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    Path(id): Path<String>,
) -> std::result::Result<StatusCode, Failure> {
    let service = owned_service(&state, &id)?;
    let mut slot = api(service.machine_lock().await)?;
    confirm(&state, &service)?;
    let deleted = state.accounts.delete_locked(&service, &mut slot).await;
    deleted.map_err(unregistration_failure)?;
    Ok(StatusCode::NO_CONTENT)
}
