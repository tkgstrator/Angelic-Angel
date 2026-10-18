//! Separate, authenticated machine-only router. Never merge this router with the management UI.
use crate::config::{TwitterConfig, validate_account_id};
use crate::error::{AngelicAngelError, Result};
use crate::machine_ledger::{self, Ledger, Operation};
use crate::machine_types::{FollowResult, LikeResult, PostInfo, UserProfile};
use crate::twitter::{Identity, XApi, XError, is_valid_user_id};
use crate::web::service::{Accounts, Backend, Service};
use crate::x_metadata::{ActionMetadata, CreateTweet};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::BTreeSet;
use std::future::Future;
use std::path::Path as FilePath;
use std::sync::Arc;
use std::time::Duration;

#[cfg(not(test))]
const TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const TIMEOUT: Duration = Duration::from_secs(2);

#[path = "machine_accounts.rs"]
mod accounts;
#[cfg(test)]
#[path = "machine_tests.rs"]
mod tests;
const PREFIX: &str = "/api/machine/v1";
const OPERATIONS: [&str; 7] = [
    "read",
    "identity",
    "post",
    "operations",
    "like",
    "follow",
    "accounts",
];

type Failure = (StatusCode, Json<Value>);
type Api<T> = std::result::Result<Json<T>, Failure>;
type XResult<T> = std::result::Result<T, XError>;

fn failure(status: StatusCode, code: &'static str) -> Failure {
    (status, Json(json!({"error": {"code": code}})))
}

fn storage(error: machine_ledger::Error) -> Failure {
    match error {
        machine_ledger::Error::Conflict => failure(StatusCode::CONFLICT, "idempotency_conflict"),
        machine_ledger::Error::Storage => {
            failure(StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable")
        },
    }
}

fn remote(_: XError) -> Failure {
    failure(StatusCode::BAD_GATEWAY, "x_unavailable")
}

fn unavailable(code: &'static str) -> Failure {
    failure(StatusCode::SERVICE_UNAVAILABLE, code)
}

/// A single service principal with explicit account and operation allowlists. No wildcard scopes.
/// Deliberately has no Debug/Serialize implementation.
pub struct Settings {
    pub bind: std::net::SocketAddr,
    token: String,
    client: String,
    accounts: BTreeSet<String>,
    operations: BTreeSet<String>,
}

impl Settings {
    pub fn from_process() -> Result<Option<Self>> {
        Self::from_values(
            std::env::var("MACHINE_BIND").ok(),
            std::env::var("MACHINE_SERVICE_TOKEN").ok(),
            std::env::var("MACHINE_CLIENT_ID").ok(),
            std::env::var("MACHINE_ACCOUNTS").ok(),
            std::env::var("MACHINE_OPERATIONS").ok(),
        )
    }

    fn from_values(
        bind: Option<String>,
        token: Option<String>,
        client: Option<String>,
        accounts: Option<String>,
        operations: Option<String>,
    ) -> Result<Option<Self>> {
        if [&bind, &token, &client, &accounts, &operations]
            .iter()
            .all(|v| v.is_none())
        {
            return Ok(None);
        }
        let invalid =
            || AngelicAngelError::Config("invalid or incomplete machine API configuration".into());
        // Token absent means disabled, even if a previous deployment left non-secret scope settings.
        let Some(token) = token else {
            return Ok(None);
        };
        if token.len() < 32 || token.len() > 1024 || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(invalid());
        }
        let bind = bind
            .unwrap_or_else(|| "127.0.0.1:8081".into())
            .parse()
            .map_err(|_| invalid())?;
        let client = client.ok_or_else(invalid)?;
        if client.is_empty()
            || client.len() > 64
            || !client
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            return Err(invalid());
        }
        let operations: BTreeSet<_> = operations
            .ok_or_else(invalid)?
            .split(',')
            .map(str::to_owned)
            .collect();
        // Managing accounts through the API makes the static allowlist optional.
        let manages = operations.contains("accounts");
        let accounts: BTreeSet<_> = match accounts {
            Some(list) if !list.is_empty() => list.split(',').map(str::to_owned).collect(),
            _ if manages => BTreeSet::new(),
            _ => return Err(invalid()),
        };
        if accounts.iter().any(|id| validate_account_id(id).is_err())
            || operations.is_empty()
            || operations
                .iter()
                .any(|op| !OPERATIONS.contains(&op.as_str()))
        {
            return Err(invalid());
        }
        Ok(Some(Self {
            bind,
            token,
            client,
            accounts,
            operations,
        }))
    }

    fn authenticate(&self, headers: &HeaderMap) -> bool {
        let mut values = headers.get_all("authorization").iter();
        let supplied = values
            .next()
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if values.next().is_some() {
            return false;
        }
        let Some(supplied) = supplied else {
            return false;
        };
        if supplied.len() > 1024 {
            return false;
        }
        // HMAC verification performs a constant-time tag comparison, including unequal token lengths.
        let mac = |value: &str| {
            let mut mac =
                Hmac::<Sha256>::new_from_slice(b"angelic-angel/machine-auth/v1").expect("HMAC key");
            mac.update(value.as_bytes());
            mac
        };
        mac(supplied)
            .verify_slice(&mac(&self.token).finalize().into_bytes())
            .is_ok()
    }

    fn allow(&self, account: Option<&str>, operation: &str) -> std::result::Result<(), Failure> {
        if !self.operations.contains(operation)
            || account.is_some_and(|id| !self.accounts.contains(id))
        {
            return Err(failure(StatusCode::FORBIDDEN, "scope_denied"));
        }
        Ok(())
    }
}

pub trait MachineBackend: Send + Sync + 'static {
    fn ready(&self) -> impl Future<Output = bool> + Send;
    fn prepare(&self) -> impl Future<Output = std::result::Result<CreateTweet, XError>> + Send;
    fn identity(
        &self,
        twitter: &TwitterConfig,
    ) -> impl Future<Output = std::result::Result<Identity, XError>> + Send;
    fn post(
        &self,
        twitter: &TwitterConfig,
        metadata: &CreateTweet,
        request: &Post,
    ) -> impl Future<Output = std::result::Result<String, XError>> + Send;
    /// Cache-only readiness of the lookup/like metadata; never contacts X.
    fn has_lookup_post(&self) -> impl Future<Output = bool> + Send;
    fn has_lookup_user(&self) -> impl Future<Output = bool> + Send;
    fn has_like(&self) -> impl Future<Output = bool> + Send;
    /// May contact X or GitHub to (re)load the lookup/like operation metadata.
    fn prepare_actions(&self) -> impl Future<Output = XResult<ActionMetadata>> + Send;
    fn lookup_post(
        &self,
        twitter: &TwitterConfig,
        metadata: &ActionMetadata,
        post_id: &str,
    ) -> impl Future<Output = XResult<Option<PostInfo>>> + Send;
    fn lookup_user(
        &self,
        twitter: &TwitterConfig,
        metadata: &ActionMetadata,
        user_id: &str,
    ) -> impl Future<Output = XResult<Option<UserProfile>>> + Send;
    fn like_post(
        &self,
        twitter: &TwitterConfig,
        metadata: &ActionMetadata,
        post_id: &str,
    ) -> impl Future<Output = XResult<LikeResult>> + Send;
    fn follow_user(
        &self,
        twitter: &TwitterConfig,
        user_id: &str,
    ) -> impl Future<Output = XResult<FollowResult>> + Send;
}

impl MachineBackend for XApi {
    async fn ready(&self) -> bool {
        self.machine_ready().await
    }
    async fn prepare(&self) -> std::result::Result<CreateTweet, XError> {
        self.prepare_post().await
    }
    async fn identity(&self, twitter: &TwitterConfig) -> std::result::Result<Identity, XError> {
        XApi::identity(self, twitter).await
    }
    async fn post(
        &self,
        twitter: &TwitterConfig,
        metadata: &CreateTweet,
        request: &Post,
    ) -> std::result::Result<String, XError> {
        self.create_post(
            twitter,
            metadata,
            &request.text,
            request.reply_to_post_id.as_deref(),
            &request.expected_x_user_id,
        )
        .await
    }
    async fn has_lookup_post(&self) -> bool {
        XApi::has_lookup_post(self).await
    }
    async fn has_lookup_user(&self) -> bool {
        XApi::has_lookup_user(self).await
    }
    async fn has_like(&self) -> bool {
        XApi::has_like(self).await
    }
    async fn prepare_actions(&self) -> XResult<ActionMetadata> {
        XApi::prepare_actions(self).await
    }
    async fn lookup_post(
        &self,
        twitter: &TwitterConfig,
        metadata: &ActionMetadata,
        post_id: &str,
    ) -> XResult<Option<PostInfo>> {
        XApi::lookup_post(self, twitter, metadata, post_id).await
    }
    async fn lookup_user(
        &self,
        twitter: &TwitterConfig,
        metadata: &ActionMetadata,
        user_id: &str,
    ) -> XResult<Option<UserProfile>> {
        XApi::lookup_user(self, twitter, metadata, user_id).await
    }
    async fn like_post(
        &self,
        twitter: &TwitterConfig,
        metadata: &ActionMetadata,
        post_id: &str,
    ) -> XResult<LikeResult> {
        XApi::like_post(self, twitter, metadata, post_id).await
    }
    async fn follow_user(&self, twitter: &TwitterConfig, user_id: &str) -> XResult<FollowResult> {
        XApi::follow_user(self, twitter, user_id).await
    }
}

pub struct Machine<B: Backend, X: MachineBackend> {
    settings: Settings,
    accounts: Arc<Accounts<B>>,
    ledger: Ledger,
    x: X,
}

impl<B: Backend, X: MachineBackend> Machine<B, X> {
    pub fn new(
        settings: Settings,
        accounts: Arc<Accounts<B>>,
        config: &FilePath,
        x: X,
    ) -> Result<Arc<Self>> {
        let mut path = config.as_os_str().to_os_string();
        path.push(".operations.sqlite3");
        let ledger = Ledger::open(FilePath::new(&path)).map_err(|_| {
            AngelicAngelError::Config("machine operation storage unavailable".into())
        })?;
        Ok(Arc::new(Self {
            settings,
            accounts,
            ledger,
            x,
        }))
    }

    pub fn bind(&self) -> std::net::SocketAddr {
        self.settings.bind
    }

    /// Whether this principal created the account through the machine API. A configuration that
    /// cannot be read counts as "not owned".
    fn owns(&self, id: &str) -> bool {
        self.accounts
            .machine_owners()
            .is_ok_and(|owners| owners.get(id) == Some(&self.settings.client))
    }

    /// The allowlist and scope check, extended to the accounts this principal owns. A principal
    /// that manages accounts cannot tell a missing account from another client's: both are 404.
    fn allow(&self, id: &str, operation: &str) -> std::result::Result<(), Failure> {
        if self.settings.accounts.contains(id) || self.owns(id) {
            return self.settings.allow(None, operation);
        }
        self.settings.allow(None, operation)?;
        if self.settings.operations.contains("accounts") {
            return Err(failure(StatusCode::NOT_FOUND, "account_not_found"));
        }
        Err(failure(StatusCode::FORBIDDEN, "scope_denied"))
    }

    fn revision(
        &self,
        id: &str,
        twitter: &TwitterConfig,
        incarnation: &str,
    ) -> std::result::Result<String, Failure> {
        let bytes = serde_json::to_vec(&(incarnation, &twitter.auth_token, &twitter.ct0))
            .map_err(|_| failure(StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable"))?;
        self.ledger
            .revision(id, &machine_ledger::hash(&bytes))
            .map_err(storage)
    }

    /// The saved operation for this key, or a conflict when the key was used for other content.
    fn saved(
        &self,
        id: &str,
        key: &str,
        hash: &str,
    ) -> std::result::Result<Option<Operation>, Failure> {
        self.ledger
            .existing(id, &self.settings.client, key, hash)
            .map_err(storage)
    }
}

type Shared<B, X> = Arc<Machine<B, X>>;

async fn auth<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = if state.settings.authenticate(request.headers()) {
        next.run(request).await
    } else {
        failure(StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    };
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert("x-content-type-options", "nosniff".parse().unwrap());
    response
}

pub fn router<B: Backend, X: MachineBackend>(state: Shared<B, X>) -> Router {
    Router::new()
        .route(&format!("{PREFIX}/accounts"), get(list::<B, X>))
        .merge(accounts::routes::<B, X>())
        .route(
            &format!("{PREFIX}/accounts/{{id}}/readiness"),
            get(readiness::<B, X>),
        )
        .route(
            &format!("{PREFIX}/accounts/{{id}}/identity"),
            get(identity::<B, X>),
        )
        .route(
            &format!("{PREFIX}/accounts/{{id}}/posts"),
            post(create::<B, X>),
        )
        .route(
            &format!("{PREFIX}/accounts/{{id}}/posts/{{post_id}}"),
            get(lookup_post::<B, X>),
        )
        .route(
            &format!("{PREFIX}/accounts/{{id}}/users/{{user_id}}"),
            get(lookup_user::<B, X>),
        )
        .route(
            &format!("{PREFIX}/accounts/{{id}}/likes"),
            post(like::<B, X>),
        )
        .route(
            &format!("{PREFIX}/accounts/{{id}}/follows"),
            post(follow::<B, X>),
        )
        .route(
            &format!("{PREFIX}/accounts/{{id}}/operations/{{operation}}"),
            get(result::<B, X>),
        )
        .fallback(|| async { failure(StatusCode::NOT_FOUND, "not_found") })
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth::<B, X>,
        ))
        .with_state(state)
}

async fn list<B: Backend, X: MachineBackend>(State(state): State<Shared<B, X>>) -> Api<Value> {
    state.settings.allow(None, "read")?;
    let list = state
        .accounts
        .list()
        .map_err(|_| failure(StatusCode::SERVICE_UNAVAILABLE, "accounts_unavailable"))?;
    let owners = state
        .accounts
        .machine_owners()
        .map_err(|_| failure(StatusCode::SERVICE_UNAVAILABLE, "accounts_unavailable"))?;
    let mut accounts = Vec::new();
    for account in list.accounts {
        let owned = owners.get(&account.id) == Some(&state.settings.client);
        if owned || state.settings.accounts.contains(&account.id) {
            accounts.push(account_readiness(&state, &account.id).await?);
        }
    }
    Ok(Json(json!({"accounts": accounts})))
}

async fn account_readiness<B: Backend, X: MachineBackend>(
    state: &Machine<B, X>,
    id: &str,
) -> std::result::Result<Value, Failure> {
    let service = state
        .accounts
        .get(id)
        .map_err(|_| failure(StatusCode::NOT_FOUND, "account_not_found"))?;
    let _guard = service
        .machine_lock()
        .await
        .map_err(|_| failure(StatusCode::NOT_FOUND, "account_not_found"))?;
    let (twitter, incarnation) = service
        .machine_credentials()
        .map_err(|_| failure(StatusCode::SERVICE_UNAVAILABLE, "credentials_unavailable"))?;
    let revision = state.revision(id, &twitter, &incarnation)?;
    let status = service
        .status()
        .map_err(|_| failure(StatusCode::SERVICE_UNAVAILABLE, "accounts_unavailable"))?;
    let profile = service
        .machine_profile()
        .map_err(|_| failure(StatusCode::SERVICE_UNAVAILABLE, "accounts_unavailable"))?;
    let configured = !twitter.auth_token.is_empty() && !twitter.ct0.is_empty();
    let metadata_ready = state.x.ready().await;
    let post_allowed = state.settings.operations.contains("post")
        && service
            .machine_post_enabled()
            .map_err(|_| failure(StatusCode::SERVICE_UNAVAILABLE, "accounts_unavailable"))?;
    let like_allowed = state.settings.operations.contains("like")
        && service
            .machine_like_enabled()
            .map_err(|_| failure(StatusCode::SERVICE_UNAVAILABLE, "accounts_unavailable"))?;
    let follow_allowed = state.settings.operations.contains("follow")
        && service
            .machine_follow_enabled()
            .map_err(|_| failure(StatusCode::SERVICE_UNAVAILABLE, "accounts_unavailable"))?;
    // Cache-only: none of these readiness checks contacts X.
    let read_allowed = state.settings.operations.contains("read");
    let lookup_post = read_allowed && configured && state.x.has_lookup_post().await;
    let lookup_user = read_allowed && configured && state.x.has_lookup_user().await;
    let like_ready = like_allowed && configured && state.x.has_like().await;
    let follow_ready = follow_allowed && configured;
    Ok(
        json!({"id": id, "registered": status.registered, "receiver_state": status.state,
        "credential_revision": revision, "credentials_configured": configured,
        "x_user_id": profile.x_user_id,
        "permissions": {"post": profile.post, "like": profile.like, "follow": profile.follow},
        "metadata_ready": metadata_ready, "post_ready": configured && metadata_ready && post_allowed,
        "capabilities": {"identity": state.settings.operations.contains("identity"),
            "text_post": configured && metadata_ready && post_allowed,
            "reply": configured && metadata_ready && post_allowed,
            "lookup_post": lookup_post, "lookup_user": lookup_user,
            "like": like_ready, "follow": follow_ready}}),
    )
}

async fn readiness<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    Path(id): Path<String>,
) -> Api<Value> {
    state.allow(&id, "read")?;
    Ok(Json(account_readiness(&state, &id).await?))
}

async fn identity<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    Path(id): Path<String>,
) -> Api<Value> {
    state.allow(&id, "identity")?;
    let service = state
        .accounts
        .get(&id)
        .map_err(|_| failure(StatusCode::NOT_FOUND, "account_not_found"))?;
    let _guard = service
        .machine_lock()
        .await
        .map_err(|_| failure(StatusCode::NOT_FOUND, "account_not_found"))?;
    let (twitter, incarnation) = service
        .machine_credentials()
        .map_err(|_| failure(StatusCode::SERVICE_UNAVAILABLE, "credentials_unavailable"))?;
    let revision = state.revision(&id, &twitter, &incarnation)?;
    let me = tokio::time::timeout(TIMEOUT, state.x.identity(&twitter))
        .await
        .map_err(|_| failure(StatusCode::GATEWAY_TIMEOUT, "x_timeout"))?
        .map_err(remote)?;
    // Explicit identity requests may warm metadata; failure is visible rather than false readiness.
    tokio::time::timeout(TIMEOUT, state.x.prepare())
        .await
        .map_err(|_| failure(StatusCode::GATEWAY_TIMEOUT, "metadata_timeout"))?
        .map_err(remote)?;
    Ok(Json(
        json!({"account_id": id, "credential_revision": revision, "identity": me}),
    ))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Post {
    text: String,
    #[serde(default)]
    reply_to_post_id: Option<String>,
    expected_credential_revision: String,
    expected_x_user_id: String,
}

impl Post {
    fn valid(&self) -> bool {
        !self.text.trim().is_empty()
            && self.text.chars().count() <= 280
            && self.text.len() <= 4096
            && !self.text.contains('\0')
            && self
                .reply_to_post_id
                .as_deref()
                .is_none_or(is_valid_user_id)
            && is_valid_user_id(&self.expected_x_user_id)
            && UuidCheck::valid(&self.expected_credential_revision)
    }
}

struct UuidCheck;
impl UuidCheck {
    fn valid(value: &str) -> bool {
        uuid::Uuid::parse_str(value).is_ok()
    }
}

fn require_json(headers: &HeaderMap) -> std::result::Result<(), Failure> {
    if headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| v.split(';').next() != Some("application/json"))
    {
        return Err(failure(StatusCode::UNSUPPORTED_MEDIA_TYPE, "json_required"));
    }
    Ok(())
}

fn idempotency_key(headers: &HeaderMap) -> std::result::Result<&str, Failure> {
    headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|key| {
            !key.is_empty() && key.len() <= 128 && key.bytes().all(|b| b.is_ascii_graphic())
        })
        .ok_or_else(|| failure(StatusCode::BAD_REQUEST, "idempotency_key_required"))
}

async fn create<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Api<Operation> {
    state.allow(&id, "post")?;
    require_json(&headers)?;
    let request: Post = serde_json::from_slice(&body)
        .map_err(|_| failure(StatusCode::BAD_REQUEST, "invalid_request"))?;
    if !request.valid() {
        return Err(failure(StatusCode::BAD_REQUEST, "invalid_request"));
    }
    let key = idempotency_key(&headers)?;
    let bytes = serde_json::to_vec(&request)
        .map_err(|_| failure(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let hash = machine_ledger::hash(&bytes);
    if let Some(operation) = state
        .ledger
        .existing(&id, &state.settings.client, key, &hash)
        .map_err(storage)?
    {
        return Ok(Json(operation));
    }
    let service = state
        .accounts
        .get(&id)
        .map_err(|_| failure(StatusCode::NOT_FOUND, "account_not_found"))?;
    let _guard = service
        .machine_lock()
        .await
        .map_err(|_| failure(StatusCode::NOT_FOUND, "account_not_found"))?;
    // A concurrent winner may have reserved this key while this request waited for the lock.
    // Existing results remain readable after the account permission is disabled.
    if let Some(operation) = state
        .ledger
        .existing(&id, &state.settings.client, key, &hash)
        .map_err(storage)?
    {
        return Ok(Json(operation));
    }
    if !service
        .machine_post_enabled()
        .map_err(|_| failure(StatusCode::SERVICE_UNAVAILABLE, "accounts_unavailable"))?
    {
        return Err(failure(StatusCode::FORBIDDEN, "account_post_disabled"));
    }
    let (twitter, incarnation) = service
        .machine_credentials()
        .map_err(|_| failure(StatusCode::SERVICE_UNAVAILABLE, "credentials_unavailable"))?;
    let revision = state.revision(&id, &twitter, &incarnation)?;
    if revision != request.expected_credential_revision {
        return Err(failure(StatusCode::CONFLICT, "credential_revision_changed"));
    }
    let (mut operation, fresh) = state
        .ledger
        .reserve(&id, &state.settings.client, key, &hash, &revision)
        .map_err(storage)?;
    if !fresh {
        return Ok(Json(operation));
    }
    let prepared = tokio::time::timeout(TIMEOUT, async {
        let identity = state.x.identity(&twitter).await?;
        if identity.user_id != request.expected_x_user_id {
            return Err(XError::Forbidden);
        }
        state.x.prepare().await
    })
    .await;
    let metadata = match prepared {
        Ok(Ok(metadata)) => metadata,
        _ => {
            state
                .ledger
                .transition(
                    &operation.operation_id,
                    "accepted",
                    "failed",
                    None,
                    Some("preflight_failed"),
                )
                .map_err(storage)?;
            operation.state = "failed".into();
            operation.error = Some("preflight_failed".into());
            return Ok(Json(operation));
        },
    };
    // Re-read permission immediately before the durable dispatch marker, under the same lock.
    // Read failures also fail closed; a reserved key must never become eligible for replay.
    if !service.machine_post_enabled().unwrap_or(false) {
        state
            .ledger
            .transition(
                &operation.operation_id,
                "accepted",
                "failed",
                None,
                Some("account_post_disabled"),
            )
            .map_err(storage)?;
        operation.state = "failed".into();
        operation.error = Some("account_post_disabled".into());
        return Ok(Json(operation));
    }
    // The account lock is still held: update_settings/delete cannot change this snapshot.
    state
        .ledger
        .transition(
            &operation.operation_id,
            "accepted",
            "dispatching",
            None,
            None,
        )
        .map_err(storage)?;
    let sent = tokio::time::timeout(TIMEOUT, state.x.post(&twitter, &metadata, &request)).await;
    let (status, post_id, error) = match sent {
        Ok(Ok(post)) => ("succeeded", Some(post), None),
        _ => ("unknown", None, Some("delivery_uncertain")),
    };
    if state
        .ledger
        .transition(
            &operation.operation_id,
            "dispatching",
            status,
            post_id.as_deref(),
            error,
        )
        .is_err()
    {
        operation.state = "unknown".into();
        operation.error = Some("result_storage_failed".into());
        // Persistent dispatching is a tombstone, never an instruction to resend.
        return Ok(Json(operation));
    }
    operation.state = status.into();
    operation.post_id = post_id;
    operation.error = error.map(str::to_owned);
    Ok(Json(operation))
}

/// Looks up one post as the account sees it. Read-only: no ledger and no account lock, so a
/// slow lookup never makes a settings save report `busy`.
async fn lookup_post<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    Path((id, post_id)): Path<(String, String)>,
) -> Api<PostInfo> {
    state.allow(&id, "read")?;
    if !is_valid_user_id(&post_id) {
        return Err(failure(StatusCode::BAD_REQUEST, "invalid_request"));
    }
    let (twitter, metadata) = lookup_context(&state, &id).await?;
    let lookup = state.x.lookup_post(&twitter, &metadata, &post_id);
    let found = tokio::time::timeout(TIMEOUT, lookup)
        .await
        .map_err(|_| failure(StatusCode::GATEWAY_TIMEOUT, "x_timeout"))?
        .map_err(remote)?;
    found
        .map(Json)
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "post_not_found"))
}

/// Looks up one user profile and the account's relation to that user. Read-only like posts.
async fn lookup_user<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    Path((id, user_id)): Path<(String, String)>,
) -> Api<UserProfile> {
    state.allow(&id, "read")?;
    if !is_valid_user_id(&user_id) {
        return Err(failure(StatusCode::BAD_REQUEST, "invalid_request"));
    }
    let (twitter, metadata) = lookup_context(&state, &id).await?;
    let lookup = state.x.lookup_user(&twitter, &metadata, &user_id);
    let found = tokio::time::timeout(TIMEOUT, lookup)
        .await
        .map_err(|_| failure(StatusCode::GATEWAY_TIMEOUT, "x_timeout"))?
        .map_err(remote)?;
    found
        .map(Json)
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "user_not_found"))
}

/// The saved credentials and the lookup metadata for one read-only request.
async fn lookup_context<B: Backend, X: MachineBackend>(
    state: &Machine<B, X>,
    id: &str,
) -> std::result::Result<(TwitterConfig, ActionMetadata), Failure> {
    let service = state
        .accounts
        .get(id)
        .map_err(|_| failure(StatusCode::NOT_FOUND, "account_not_found"))?;
    let (twitter, _) = service
        .machine_credentials()
        .map_err(|_| unavailable("credentials_unavailable"))?;
    if twitter.auth_token.is_empty() || twitter.ct0.is_empty() {
        return Err(unavailable("credentials_unavailable"));
    }
    let metadata = tokio::time::timeout(TIMEOUT, state.x.prepare_actions())
        .await
        .map_err(|_| failure(StatusCode::GATEWAY_TIMEOUT, "metadata_timeout"))?
        .map_err(remote)?;
    Ok((twitter, metadata))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Like {
    post_id: String,
    expected_credential_revision: String,
    expected_x_user_id: String,
}

impl Like {
    fn valid(&self) -> bool {
        is_valid_user_id(&self.post_id)
            && is_valid_user_id(&self.expected_x_user_id)
            && UuidCheck::valid(&self.expected_credential_revision)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Follow {
    user_id: String,
    expected_credential_revision: String,
    expected_x_user_id: String,
}

impl Follow {
    fn valid(&self) -> bool {
        is_valid_user_id(&self.user_id)
            && is_valid_user_id(&self.expected_x_user_id)
            && UuidCheck::valid(&self.expected_credential_revision)
    }
}

/// A validated like or follow. Both share one admission, ledger and dispatch path.
enum Act {
    Like(Like),
    Follow(Follow),
}

impl Act {
    /// The ledger kind, which is also the operation scope.
    fn kind(&self) -> &'static str {
        match self {
            Self::Like(_) => "like",
            Self::Follow(_) => "follow",
        }
    }

    /// The fixed error code returned while the account has this operation switched off.
    fn disabled(&self) -> &'static str {
        match self {
            Self::Like(_) => "account_like_disabled",
            Self::Follow(_) => "account_follow_disabled",
        }
    }

    fn expected_revision(&self) -> &str {
        match self {
            Self::Like(request) => &request.expected_credential_revision,
            Self::Follow(request) => &request.expected_credential_revision,
        }
    }

    fn expected_user(&self) -> &str {
        match self {
            Self::Like(request) => &request.expected_x_user_id,
            Self::Follow(request) => &request.expected_x_user_id,
        }
    }

    /// The bytes hashed for idempotency. The kind is part of them, so one key can never be
    /// reused for another kind of operation or another target.
    fn canonical(&self) -> serde_json::Result<Vec<u8>> {
        match self {
            Self::Like(request) => serde_json::to_vec(&("like", request)),
            Self::Follow(request) => serde_json::to_vec(&("follow", request)),
        }
    }

    /// The saved switch for this operation; `None` when it cannot be read, so callers fail closed.
    fn enabled<B: Backend>(&self, service: &Service<B>) -> Option<bool> {
        match self {
            Self::Like(_) => service.machine_like_enabled().ok(),
            Self::Follow(_) => service.machine_follow_enabled().ok(),
        }
    }
}

async fn like<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Api<Operation> {
    state.allow(&id, "like")?;
    require_json(&headers)?;
    let request: Like = serde_json::from_slice(&body)
        .map_err(|_| failure(StatusCode::BAD_REQUEST, "invalid_request"))?;
    if !request.valid() {
        return Err(failure(StatusCode::BAD_REQUEST, "invalid_request"));
    }
    let key = idempotency_key(&headers)?;
    act(&state, &id, key, Act::Like(request)).await
}

async fn follow<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Api<Operation> {
    state.allow(&id, "follow")?;
    require_json(&headers)?;
    let request: Follow = serde_json::from_slice(&body)
        .map_err(|_| failure(StatusCode::BAD_REQUEST, "invalid_request"))?;
    if !request.valid() {
        return Err(failure(StatusCode::BAD_REQUEST, "invalid_request"));
    }
    let key = idempotency_key(&headers)?;
    act(&state, &id, key, Act::Follow(request)).await
}

/// Sends the already-preflighted like or follow. The target is not looked up again: the caller
/// resolved it before asking.
async fn dispatch<X: MachineBackend>(
    x: &X,
    twitter: &TwitterConfig,
    metadata: Option<&ActionMetadata>,
    action: &Act,
) -> XResult<Value> {
    let result = match (action, metadata) {
        (Act::Like(request), Some(metadata)) => {
            let liked = x.like_post(twitter, metadata, &request.post_id).await?;
            serde_json::to_value(liked)
        },
        (Act::Follow(request), _) => {
            let followed = x.follow_user(twitter, &request.user_id).await?;
            serde_json::to_value(followed)
        },
        (Act::Like(_), None) => return Err(XError::Malformed),
    };
    result.map_err(|_| XError::Malformed)
}

/// The like/follow counterpart of [`create`]: same admission order, same at-most-once ledger.
async fn act<B: Backend, X: MachineBackend>(
    state: &Machine<B, X>,
    id: &str,
    key: &str,
    action: Act,
) -> Api<Operation> {
    let bytes = action
        .canonical()
        .map_err(|_| failure(StatusCode::BAD_REQUEST, "invalid_request"))?;
    let hash = machine_ledger::hash(&bytes);
    if let Some(operation) = state.saved(id, key, &hash)? {
        return Ok(Json(operation));
    }
    let service = state
        .accounts
        .get(id)
        .map_err(|_| failure(StatusCode::NOT_FOUND, "account_not_found"))?;
    let _guard = service
        .machine_lock()
        .await
        .map_err(|_| failure(StatusCode::NOT_FOUND, "account_not_found"))?;
    // A concurrent winner may have reserved this key while this request waited for the lock.
    // Existing results remain readable after the account switch is turned off.
    if let Some(operation) = state.saved(id, key, &hash)? {
        return Ok(Json(operation));
    }
    let enabled = action
        .enabled(&service)
        .ok_or_else(|| unavailable("accounts_unavailable"))?;
    if !enabled {
        return Err(failure(StatusCode::FORBIDDEN, action.disabled()));
    }
    let (twitter, incarnation) = service
        .machine_credentials()
        .map_err(|_| unavailable("credentials_unavailable"))?;
    let revision = state.revision(id, &twitter, &incarnation)?;
    if revision != action.expected_revision() {
        return Err(failure(StatusCode::CONFLICT, "credential_revision_changed"));
    }
    let client = &state.settings.client;
    let (mut operation, fresh) = state
        .ledger
        .reserve_as(action.kind(), id, client, key, &hash, &revision)
        .map_err(storage)?;
    if !fresh {
        return Ok(Json(operation));
    }
    let prepared = tokio::time::timeout(TIMEOUT, async {
        let identity = state.x.identity(&twitter).await?;
        if identity.user_id != action.expected_user() {
            return Err(XError::Forbidden);
        }
        match &action {
            Act::Like(_) => state.x.prepare_actions().await.map(Some),
            Act::Follow(_) => Ok(None),
        }
    })
    .await;
    let metadata = match prepared {
        Ok(Ok(metadata)) => metadata,
        _ => {
            state
                .ledger
                .transition(
                    &operation.operation_id,
                    "accepted",
                    "failed",
                    None,
                    Some("preflight_failed"),
                )
                .map_err(storage)?;
            operation.state = "failed".into();
            operation.error = Some("preflight_failed".into());
            return Ok(Json(operation));
        },
    };
    // Re-read the switch immediately before the durable dispatch marker, under the same lock.
    // Read failures also fail closed; a reserved key must never become eligible for replay.
    if !action.enabled(&service).unwrap_or(false) {
        let code = action.disabled();
        state
            .ledger
            .transition(
                &operation.operation_id,
                "accepted",
                "failed",
                None,
                Some(code),
            )
            .map_err(storage)?;
        operation.state = "failed".into();
        operation.error = Some(code.into());
        return Ok(Json(operation));
    }
    // The account lock is still held: update_settings/delete cannot change this snapshot.
    state
        .ledger
        .transition(
            &operation.operation_id,
            "accepted",
            "dispatching",
            None,
            None,
        )
        .map_err(storage)?;
    let send = dispatch(&state.x, &twitter, metadata.as_ref(), &action);
    let sent = tokio::time::timeout(TIMEOUT, send).await;
    let (status, result, error) = match sent {
        Ok(Ok(value)) => ("succeeded", Some(value), None),
        _ => ("unknown", None, Some("delivery_uncertain")),
    };
    if state
        .ledger
        .transition_result(
            &operation.operation_id,
            "dispatching",
            status,
            result.as_ref(),
            error,
        )
        .is_err()
    {
        operation.state = "unknown".into();
        operation.error = Some("result_storage_failed".into());
        // Persistent dispatching is a tombstone, never an instruction to resend.
        return Ok(Json(operation));
    }
    operation.state = status.into();
    operation.result = result;
    operation.error = error.map(str::to_owned);
    Ok(Json(operation))
}

async fn result<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    Path((id, operation)): Path<(String, String)>,
) -> Api<Operation> {
    state.allow(&id, "operations")?;
    let operation = state
        .ledger
        .get(&id, &state.settings.client, &operation)
        .map_err(storage)?
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "operation_not_found"))?;
    Ok(Json(operation))
}
