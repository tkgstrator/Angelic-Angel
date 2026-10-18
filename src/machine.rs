//! Separate, authenticated machine-only router. Never merge this router with the management UI.
use crate::config::{TwitterConfig, validate_account_id};
use crate::error::{AngelicAngelError, Result};
use crate::machine_ledger::{self, Ledger, Operation};
use crate::twitter::{Identity, XApi, XError, is_valid_user_id};
use crate::web::service::{Accounts, Backend};
use crate::x_metadata::CreateTweet;
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

#[cfg(test)]
#[path = "machine_tests.rs"]
mod tests;
const PREFIX: &str = "/api/machine/v1";
const OPERATIONS: [&str; 4] = ["read", "identity", "post", "operations"];

type Failure = (StatusCode, Json<Value>);
type Api<T> = std::result::Result<Json<T>, Failure>;

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
        let accounts: BTreeSet<_> = accounts
            .ok_or_else(invalid)?
            .split(',')
            .map(str::to_owned)
            .collect();
        let operations: BTreeSet<_> = operations
            .ok_or_else(invalid)?
            .split(',')
            .map(str::to_owned)
            .collect();
        if accounts.is_empty()
            || accounts.iter().any(|id| validate_account_id(id).is_err())
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
    let mut accounts = Vec::new();
    for account in list.accounts {
        if state.settings.accounts.contains(&account.id) {
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
    let configured = !twitter.auth_token.is_empty() && !twitter.ct0.is_empty();
    let metadata_ready = state.x.ready().await;
    Ok(
        json!({"id": id, "registered": status.registered, "receiver_state": status.state,
        "credential_revision": revision, "credentials_configured": configured,
        "metadata_ready": metadata_ready, "post_ready": configured && metadata_ready,
        "capabilities": {"identity": state.settings.operations.contains("identity"),
            "text_post": state.settings.operations.contains("post"),
            "reply": state.settings.operations.contains("post")}}),
    )
}

async fn readiness<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    Path(id): Path<String>,
) -> Api<Value> {
    state.settings.allow(Some(&id), "read")?;
    Ok(Json(account_readiness(&state, &id).await?))
}

async fn identity<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    Path(id): Path<String>,
) -> Api<Value> {
    state.settings.allow(Some(&id), "identity")?;
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

async fn create<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Api<Operation> {
    state.settings.allow(Some(&id), "post")?;
    if headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| v.split(';').next() != Some("application/json"))
    {
        return Err(failure(StatusCode::UNSUPPORTED_MEDIA_TYPE, "json_required"));
    }
    let request: Post = serde_json::from_slice(&body)
        .map_err(|_| failure(StatusCode::BAD_REQUEST, "invalid_request"))?;
    if !request.valid() {
        return Err(failure(StatusCode::BAD_REQUEST, "invalid_request"));
    }
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|key| {
            !key.is_empty() && key.len() <= 128 && key.bytes().all(|b| b.is_ascii_graphic())
        })
        .ok_or_else(|| failure(StatusCode::BAD_REQUEST, "idempotency_key_required"))?;
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

async fn result<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    Path((id, operation)): Path<(String, String)>,
) -> Api<Operation> {
    state.settings.allow(Some(&id), "operations")?;
    let operation = state
        .ledger
        .get(&id, &state.settings.client, &operation)
        .map_err(storage)?
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "operation_not_found"))?;
    Ok(Json(operation))
}
