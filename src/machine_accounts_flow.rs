//! The two account flows that change stored cookies and the push registration together: creating
//! an account and replacing its cookies. Each runs inside the account's mutation lock, so a
//! concurrent request for the same account waits instead of seeing a half-finished state.
use super::{
    Failure, Granted, Machine, MachineBackend, PublicIdentity, Shared, Switches, api, confirm,
    cookies, endpoint_ok, failure, invalid, lookup_identity, mismatch, owned_service, parse,
    secret_ok, service_failure,
};
use crate::config::{Account, TwitterConfig, validate_account_id};
use crate::machine_ledger;
use crate::twitter::is_valid_user_id;
use crate::web::service::{Backend, Service, StateName};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct NewAccount {
    id: String,
    auth_token: String,
    ct0: String,
    expected_x_user_id: String,
    webhook_endpoint: String,
    webhook_secret: String,
    #[serde(default)]
    permissions: Switches,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Created {
    id: String,
    credential_revision: String,
    identity: PublicIdentity,
    registered: bool,
    receiver_state: StateName,
    permissions: Granted,
}

/// The saved state of one account, read after a flow finished.
struct Snapshot {
    revision: String,
    registered: bool,
    receiver: StateName,
    permissions: Granted,
}

fn snapshot<B: Backend, X: MachineBackend>(
    state: &Machine<B, X>,
    service: &Service<B>,
    id: &str,
) -> std::result::Result<Snapshot, Failure> {
    let (twitter, incarnation) = service
        .machine_credentials()
        .map_err(|_| failure(StatusCode::SERVICE_UNAVAILABLE, "credentials_unavailable"))?;
    let revision = state.revision(id, &twitter, &incarnation)?;
    let status = api(service.status())?;
    let profile = api(service.machine_profile())?;
    Ok(Snapshot {
        revision,
        registered: status.registered,
        receiver: status.state,
        permissions: Granted::from(&profile),
    })
}

/// Registers the new account and builds the 201 body. Any failure here makes the caller undo the
/// creation, so nothing is returned half-done.
async fn register_new<B: Backend, X: MachineBackend>(
    state: &Machine<B, X>,
    service: &Service<B>,
    slot: &mut Option<JoinHandle<()>>,
    id: &str,
    identity: PublicIdentity,
) -> std::result::Result<Created, Failure> {
    api(service.register_locked(slot).await)?;
    let saved = snapshot(state, service, id)?;
    if !saved.registered {
        return Err(failure(StatusCode::BAD_GATEWAY, "registration_failed"));
    }
    Ok(Created {
        id: id.to_owned(),
        credential_revision: saved.revision,
        identity,
        registered: saved.registered,
        receiver_state: saved.receiver,
        permissions: saved.permissions,
    })
}

/// Creates an account, stores its cookies and registers for notifications, or leaves nothing.
pub(super) async fn create<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    headers: HeaderMap,
    body: Bytes,
) -> std::result::Result<(StatusCode, Json<Created>), Failure> {
    state.settings.allow(None, "accounts")?;
    let request: NewAccount = parse(&headers, &body)?;
    let valid = validate_account_id(&request.id).is_ok()
        && is_valid_user_id(&request.expected_x_user_id)
        && endpoint_ok(&request.webhook_endpoint)
        && secret_ok(&request.webhook_secret);
    if !valid {
        return Err(invalid());
    }
    let twitter = cookies(request.auth_token, request.ct0)?;
    // An existing id is never taken over, whoever asks.
    match state.accounts.get(&request.id) {
        Ok(_) => return Err(exists()),
        Err(error) if error.is_account_not_found() => {},
        Err(error) => return Err(service_failure(error)),
    }
    let identity = lookup_identity(&state, &twitter).await?;
    if identity.user_id != request.expected_x_user_id {
        return Err(mismatch());
    }
    let mut account = Account::new(request.id.as_str());
    account.machine_owner = Some(state.settings.client.clone());
    account.x_user_id = Some(identity.user_id.clone());
    account.twitter = twitter;
    account.webhook_endpoint = Some(request.webhook_endpoint);
    account.webhook_secret = Some(request.webhook_secret);
    request.permissions.apply(&mut account);
    // A second create of the same id that arrives while the first is still registering finds the
    // account busy. It is a duplicate all the same.
    match state.accounts.create_account(account) {
        Ok(_) => {},
        Err(error) if error.code == "busy" => return Err(exists()),
        Err(error) => return Err(service_failure(error)),
    }

    let service = match state.accounts.get(&request.id) {
        Ok(service) => service,
        Err(error) if error.is_account_not_found() => return Err(vanished()),
        Err(_) => return Err(failure(StatusCode::INTERNAL_SERVER_ERROR, "rollback_failed")),
    };
    let Ok(mut slot) = service.machine_lock().await else {
        return Err(vanished());
    };
    let identity = PublicIdentity::from(identity);
    let created = register_new(&state, &service, &mut slot, &request.id, identity).await;
    match created {
        Ok(created) => Ok((StatusCode::CREATED, Json(created))),
        Err(error) => {
            let undone = state.accounts.delete_locked(&service, &mut slot).await;
            Err(match undone {
                Ok(()) => error,
                Err(gone) if gone.is_account_not_found() => error,
                Err(_) => failure(StatusCode::INTERNAL_SERVER_ERROR, "rollback_failed"),
            })
        },
    }
}

fn exists() -> Failure {
    failure(StatusCode::CONFLICT, "account_exists")
}

/// The account was deleted between its creation and the lock: nothing is left to undo.
fn vanished() -> Failure {
    failure(StatusCode::BAD_GATEWAY, "registration_failed")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct NewCookies {
    auth_token: String,
    ct0: String,
    expected_x_user_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Renewed {
    credential_revision: String,
    identity: PublicIdentity,
    registered: bool,
}

/// A digest of the pair, so two pairs are compared without comparing the secrets themselves.
fn fingerprint(twitter: &TwitterConfig) -> String {
    let joined = format!("{}\0{}", twitter.auth_token, twitter.ct0);
    machine_ledger::hash(joined.as_bytes())
}

/// Replaces the cookies of an owned account: check them with X, unregister, save, register again.
///
/// If the new registration fails the new cookies stay saved and the account stays unregistered;
/// the same request, sent again, finishes the job.
pub(super) async fn credentials<B: Backend, X: MachineBackend>(
    State(state): State<Shared<B, X>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> std::result::Result<Json<Renewed>, Failure> {
    let service = owned_service(&state, &id)?;
    let request: NewCookies = parse(&headers, &body)?;
    if !is_valid_user_id(&request.expected_x_user_id) {
        return Err(invalid());
    }
    let twitter = cookies(request.auth_token, request.ct0)?;
    let mut slot = api(service.machine_lock().await)?;
    let profile = confirm(&state, &service)?;
    let identity = lookup_identity(&state, &twitter).await?;
    let same_person = profile.x_user_id.as_deref() == Some(identity.user_id.as_str());
    if identity.user_id != request.expected_x_user_id || !same_person {
        return Err(mismatch());
    }
    let (saved, _) = service
        .machine_credentials()
        .map_err(|_| failure(StatusCode::SERVICE_UNAVAILABLE, "credentials_unavailable"))?;
    if fingerprint(&saved) != fingerprint(&twitter) {
        api(service.unregister_locked(&mut slot).await)?;
        api(service.machine_set_credentials(twitter))?;
    }
    api(service.register_locked(&mut slot).await)?;
    let saved = snapshot(&state, &service, &id)?;
    if !saved.registered {
        return Err(failure(StatusCode::BAD_GATEWAY, "registration_failed"));
    }
    Ok(Json(Renewed {
        credential_revision: saved.revision,
        identity: identity.into(),
        registered: saved.registered,
    }))
}
