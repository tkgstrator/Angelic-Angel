use super::*;
use crate::config::{Account, Config};
use crate::web::service::{Environment, SettingsUpdate, fake::FakeBackend};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const TOKEN: &str = "synthetic-machine-token-at-least-32-characters";

#[derive(Default)]
struct FakeX {
    posts: AtomicUsize,
    identities: AtomicUsize,
    preparations: AtomicUsize,
    fail_metadata: AtomicBool,
    uncertain: AtomicBool,
    hang: AtomicBool,
    metadata_ready: AtomicBool,
    prepare_hang: AtomicBool,
    prepare_entered: tokio::sync::Notify,
    prepare_release: tokio::sync::Notify,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl MachineBackend for Arc<FakeX> {
    async fn ready(&self) -> bool {
        self.metadata_ready.load(Ordering::SeqCst)
    }
    async fn prepare(&self) -> std::result::Result<CreateTweet, XError> {
        self.preparations.fetch_add(1, Ordering::SeqCst);
        if self.prepare_hang.load(Ordering::SeqCst) {
            self.prepare_entered.notify_one();
            self.prepare_release.notified().await;
        }
        if self.fail_metadata.load(Ordering::SeqCst) {
            return Err(XError::Malformed);
        }
        crate::x_metadata::parse(include_str!("../tests/fixtures/machine-metadata.json"))
    }
    async fn identity(&self, _: &TwitterConfig) -> std::result::Result<Identity, XError> {
        self.identities.fetch_add(1, Ordering::SeqCst);
        Ok(Identity {
            user_id: "123".into(),
            screen_name: "fixture".into(),
            name: "Fixture".into(),
            avatar_url: None,
        })
    }
    async fn post(
        &self,
        _: &TwitterConfig,
        _: &CreateTweet,
        _: &Post,
    ) -> std::result::Result<String, XError> {
        self.posts.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        if self.hang.load(Ordering::SeqCst) {
            self.release.notified().await;
        }
        if self.uncertain.load(Ordering::SeqCst) {
            Err(XError::Transport)
        } else {
            Ok("789".into())
        }
    }
}

type TestMachine = Machine<Arc<FakeBackend>, Arc<FakeX>>;

struct Harness {
    state: Arc<TestMachine>,
    path: std::path::PathBuf,
    base: String,
    client: reqwest::Client,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn settings(scopes: &str) -> Settings {
    Settings::from_values(
        None,
        Some(TOKEN.into()),
        Some("fixture-client".into()),
        Some("default".into()),
        Some(scopes.into()),
    )
    .unwrap()
    .unwrap()
}

async fn harness(scopes: &str) -> Harness {
    harness_with_permission(scopes, true).await
}

async fn harness_with_permission(scopes: &str, enabled: bool) -> Harness {
    let directory = crate::config::test_temp_dir();
    let path = directory.join("fixture.toml");
    let mut account = Account::new("default");
    account.machine_post_enabled = enabled;
    account.twitter = TwitterConfig {
        auth_token: "SYNTHETIC-COOKIE".into(),
        ct0: "SYNTHETIC-CSRF".into(),
    };
    Config {
        accounts: vec![account, Account::new("private")],
    }
    .save(&path)
    .unwrap();
    let accounts = Arc::new(Accounts::new(
        Arc::new(FakeBackend::default()),
        path.clone(),
        Environment::default(),
    ));
    let state = Machine::new(
        settings(scopes),
        accounts,
        &path,
        Arc::new(FakeX::default()),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = router(Arc::clone(&state));
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Harness {
        state,
        path,
        base,
        client: reqwest::Client::new(),
        task,
    }
}

impl Harness {
    fn get(&self, suffix: &str) -> reqwest::RequestBuilder {
        self.client
            .get(format!("{}{PREFIX}{suffix}", self.base))
            .bearer_auth(TOKEN)
    }
    fn send(&self, key: &str, body: &Value) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{PREFIX}/accounts/default/posts", self.base))
            .bearer_auth(TOKEN)
            .header("idempotency-key", key)
            .json(body)
    }
    async fn body(&self) -> Value {
        let response: Value = self
            .get("/accounts/default/readiness")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        json!({"text": "synthetic fixture", "expectedCredentialRevision": response["credential_revision"],
            "expectedXUserId": "123"})
    }
}

#[test]
fn absent_token_disables_and_invalid_tokens_or_scopes_fail_closed() {
    assert!(
        Settings::from_values(None, None, None, None, None)
            .unwrap()
            .is_none()
    );
    assert!(
        Settings::from_values(Some("127.0.0.1:8081".into()), None, None, None, None)
            .unwrap()
            .is_none()
    );
    for token in [
        "",
        "short",
        "a token with whitespace that is still long enough",
    ] {
        assert!(
            Settings::from_values(
                None,
                Some(token.into()),
                Some("b".into()),
                Some("default".into()),
                Some("read".into())
            )
            .is_err()
        );
    }
    for scopes in ["*", "like", "read,lookup", ""] {
        assert!(
            Settings::from_values(
                None,
                Some(TOKEN.into()),
                Some("b".into()),
                Some("default".into()),
                Some(scopes.into())
            )
            .is_err()
        );
    }
    let settings = settings("read");
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    assert!(settings.authenticate(&headers));
    headers.append("authorization", "Bearer second".parse().unwrap());
    assert!(!settings.authenticate(&headers));
}

#[tokio::test]
async fn auth_scope_route_isolation_and_secret_free_cache_only_readiness() {
    let h = harness("read").await;
    for token in [None, Some("wrong"), Some("SYNTHETIC-COOKIE")] {
        let req = h.client.get(format!("{}{PREFIX}/accounts", h.base));
        let req = if let Some(token) = token {
            req.bearer_auth(token)
        } else {
            req
        };
        assert_eq!(req.send().await.unwrap().status(), 401);
    }
    let response = h.get("/accounts").send().await.unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body = response.text().await.unwrap();
    for secret in [
        TOKEN,
        "SYNTHETIC-COOKIE",
        "SYNTHETIC-CSRF",
        "private",
        "auth_token",
        "ct0",
        "lookup",
        "like",
        "follow",
    ] {
        assert!(!body.contains(secret), "leaked {secret}");
    }
    assert_eq!(h.state.x.identities.load(Ordering::SeqCst), 0);
    assert_eq!(h.state.x.preparations.load(Ordering::SeqCst), 0);
    assert_eq!(
        h.get("/accounts/private/readiness")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        h.get("/accounts/default/identity")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    for path in [
        "/api/accounts",
        "/api/accounts/default/settings",
        "/",
        "/health",
    ] {
        assert_eq!(
            h.client
                .get(format!("{}{path}", h.base))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }
    let management = crate::web::router(
        Arc::clone(&h.state.accounts),
        FilePath::new("/nonexistent-fixture-assets"),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, management).await.unwrap();
    });
    struct Stop(tokio::task::JoinHandle<()>);
    impl Drop for Stop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _stop = Stop(task);
    assert_eq!(
        h.client
            .get(format!("http://{address}{PREFIX}/accounts"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        h.client
            .get(format!("http://{address}/api/accounts"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn identity_and_posts_bind_to_revision_and_expected_user() {
    let h = harness("read,identity,post,operations").await;
    let identity: Value = h
        .get("/accounts/default/identity")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(identity["identity"]["user_id"], "123");
    let body = h.body().await;
    let operation: Value = h
        .send("fixture-key", &body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(operation["state"], "succeeded");
    assert_eq!(operation["post_id"], "789");
    let again: Value = h
        .send("fixture-key", &body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(operation, again);
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 1);
    let id = operation["operation_id"].as_str().unwrap();
    let result: Value = h
        .get(&format!("/accounts/default/operations/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result, operation);
    let mut changed = body.clone();
    changed["text"] = json!("different");
    assert_eq!(
        h.send("fixture-key", &changed)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let mut wrong_user = body;
    wrong_user["expectedXUserId"] = json!("456");
    let result: Value = h
        .send("wrong-user", &wrong_user)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["state"], "failed");
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn simultaneous_same_key_dispatches_once() {
    let h = harness("read,post").await;
    let body = h.body().await;
    let first = h.send("parallel", &body);
    let second = h.send("parallel", &body);
    let (first, second) = tokio::join!(first.send(), second.send());
    assert_eq!(first.unwrap().status(), 200);
    assert_eq!(second.unwrap().status(), 200);
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unsupported_fields_metadata_failure_and_storage_failure_never_send() {
    let h = harness("read,post").await;
    let mut body = h.body().await;
    body["media"] = json!([]);
    assert_eq!(h.send("media", &body).send().await.unwrap().status(), 400);
    body.as_object_mut().unwrap().remove("media");
    h.state.x.fail_metadata.store(true, Ordering::SeqCst);
    let result: Value = h
        .send("metadata", &body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["state"], "failed");
    h.state.x.fail_metadata.store(false, Ordering::SeqCst);
    h.state.ledger.read_only();
    assert_eq!(h.send("storage", &body).send().await.unwrap().status(), 503);
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn timeout_and_lost_response_are_unknown_and_never_replayed() {
    let h = harness("read,post").await;
    let body = h.body().await;
    h.state.x.hang.store(true, Ordering::SeqCst);
    let result: Value = h
        .send("timeout", &body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["state"], "unknown");
    h.state.x.hang.store(false, Ordering::SeqCst);
    h.state.x.uncertain.store(true, Ordering::SeqCst);
    let result: Value = h
        .send("lost-response", &body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["state"], "unknown");
    for key in ["timeout", "lost-response"] {
        let result: Value = h
            .send(key, &body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(result["state"], "unknown");
    }
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 2);
}

fn update() -> SettingsUpdate {
    serde_json::from_value(
        json!({"auth_token":"NEW-SYNTHETIC-COOKIE", "ct0":"NEW-SYNTHETIC-CSRF",
        "webhook_endpoint":""}),
    )
    .unwrap()
}

#[tokio::test]
async fn credential_update_and_delete_cannot_overtake_dispatch() {
    let h = harness("read,post").await;
    let body = h.body().await;
    h.state.x.hang.store(true, Ordering::SeqCst);
    let request = h.send("race", &body);
    let task = tokio::spawn(async move { request.send().await.unwrap() });
    h.state.x.entered.notified().await;
    let service = h.state.accounts.get("default").unwrap();
    assert!(service.update_settings(update()).await.is_err());
    assert!(h.state.accounts.delete("default").await.is_err());
    h.state.x.release.notify_one();
    assert_eq!(task.await.unwrap().status(), 200);
    service.update_settings(update()).await.unwrap();
    assert_eq!(h.send("stale", &body).send().await.unwrap().status(), 409);
    h.state.accounts.delete("default").await.unwrap();
    // Old resolved handles cannot restore a deleted account or dispatch with its snapshot.
    assert!(service.machine_lock().await.is_err());
    h.state.accounts.create("default").unwrap();
    h.state
        .accounts
        .get("default")
        .unwrap()
        .update_settings(update())
        .await
        .unwrap();
    assert!(
        !h.state
            .accounts
            .get("default")
            .unwrap()
            .settings()
            .unwrap()
            .machine_post_enabled
    );
    set_permission(&h, true).await;
    let new_body = h.body().await;
    assert_ne!(
        body["expectedCredentialRevision"],
        new_body["expectedCredentialRevision"]
    );
    assert_eq!(
        h.send("stale-after-recreate", &body)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
}

#[tokio::test]
async fn success_storage_failure_is_a_non_replayable_dispatching_tombstone() {
    let h = harness("read,post").await;
    let body = h.body().await;
    h.state.x.hang.store(true, Ordering::SeqCst);
    let request = h.send("save-failure", &body);
    let task = tokio::spawn(async move { request.send().await.unwrap() });
    h.state.x.entered.notified().await;
    h.state.ledger.read_only();
    h.state.x.release.notify_one();
    let result: Value = task.await.unwrap().json().await.unwrap();
    assert_eq!(result["state"], "unknown");
    assert_eq!(result["error"], "result_storage_failed");
    let result: Value = h
        .send("save-failure", &body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["state"], "dispatching");
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn legacy_and_environment_accounts_keep_stable_opaque_revisions() {
    let h = harness("read").await;
    // Synthetic old layout, not a real configuration or cookie.
    std::fs::write(
        &h.path,
        "[twitter]\nauth_token='fixture-auth'\nct0='fixture-csrf'\n",
    )
    .unwrap();
    let first = h.body().await;
    let second = h.body().await;
    assert_eq!(
        first["expectedCredentialRevision"],
        second["expectedCredentialRevision"]
    );
    let accounts = Arc::new(Accounts::new(
        Arc::new(FakeBackend::default()),
        h.path.with_file_name("absent.toml"),
        Environment::new(
            Some("fixture-env-auth"),
            Some("fixture-env-csrf"),
            None,
            None,
        )
        .unwrap(),
    ));
    let state = Machine::new(
        settings("read"),
        accounts,
        &h.path.with_file_name("absent.toml"),
        Arc::new(FakeX::default()),
    )
    .unwrap();
    let first = account_readiness(&state, "default").await.unwrap();
    let second = account_readiness(&state, "default").await.unwrap();
    assert_eq!(first["credential_revision"], second["credential_revision"]);
    assert_eq!(first["credentials_configured"], true);
    assert_eq!(first["post_ready"], false);
    assert_eq!(first["capabilities"]["text_post"], false);
}

async fn set_permission(h: &Harness, enabled: bool) {
    let update = serde_json::from_value(json!({
        "webhook_endpoint": "", "machine_post_enabled": enabled
    }))
    .unwrap();
    h.state
        .accounts
        .get("default")
        .unwrap()
        .update_settings(update)
        .await
        .unwrap();
}

#[tokio::test]
async fn disabled_posts_and_replies_never_reserve_or_contact_x() {
    let h = harness_with_permission("read,identity,post,operations", false).await;
    let body = h.body().await;
    for reply in [None, Some("456")] {
        let mut request = body.clone();
        if let Some(reply) = reply {
            request["replyToPostId"] = json!(reply);
        }
        let response = h.send("disabled", &request).send().await.unwrap();
        assert_eq!(response.status(), 403);
        let response: Value = response.json().await.unwrap();
        assert_eq!(response["error"]["code"], "account_post_disabled");
        assert!(
            h.state
                .ledger
                .existing("default", "fixture-client", "disabled", "any-hash")
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 0);
    assert_eq!(h.state.x.identities.load(Ordering::SeqCst), 0);
    assert_eq!(h.state.x.preparations.load(Ordering::SeqCst), 0);
    // Refused-before-reservation keys may be used after an explicit grant.
    set_permission(&h, true).await;
    let result: Value = h
        .send("disabled", &body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["state"], "succeeded");
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn posting_readiness_requires_scope_permission_credentials_and_metadata() {
    for scopes in ["read,identity", "read,identity,post"] {
        for enabled in [false, true] {
            let h = harness_with_permission(scopes, enabled).await;
            for credentials in [false, true] {
                Config::modify_account::<_, AngelicAngelError>(&h.path, "default", |current| {
                    let mut account = current.unwrap();
                    account.twitter = if credentials {
                        TwitterConfig {
                            auth_token: "fixture-auth".into(),
                            ct0: "fixture-csrf".into(),
                        }
                    } else {
                        TwitterConfig::default()
                    };
                    Ok((Some(account), ()))
                })
                .unwrap();
                for metadata in [false, true] {
                    h.state.x.metadata_ready.store(metadata, Ordering::SeqCst);
                    let ready = account_readiness(&h.state, "default").await.unwrap();
                    let expected = scopes.contains("post") && enabled && credentials && metadata;
                    assert_eq!(ready["capabilities"]["identity"], true);
                    assert_eq!(ready["post_ready"], expected);
                    assert_eq!(ready["capabilities"]["text_post"], expected);
                    assert_eq!(ready["capabilities"]["reply"], expected);
                }
            }
            assert_eq!(h.state.x.identities.load(Ordering::SeqCst), 0);
            assert_eq!(h.state.x.preparations.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn toggles_do_not_grant_scopes_accounts_or_machine_settings_access() {
    let h = harness("read").await;
    let body = h.body().await;
    assert_eq!(
        h.send("scope-denied", &body).send().await.unwrap().status(),
        403
    );
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 0);
    let h = harness("read,post").await;
    Config::modify_account::<_, AngelicAngelError>(&h.path, "private", |current| {
        let mut account = current.unwrap();
        account.machine_post_enabled = true;
        Ok((Some(account), ()))
    })
    .unwrap();
    assert_eq!(
        h.client
            .post(format!("{}{PREFIX}/accounts/private/posts", h.base))
            .bearer_auth(TOKEN)
            .header("idempotency-key", "private")
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let mut injected = h.body().await;
    injected["machine_post_enabled"] = json!(true);
    assert_eq!(
        h.send("inject", &injected).send().await.unwrap().status(),
        400
    );
    for path in [
        "/api/accounts/default/settings",
        "/api/machine/v1/accounts/default/settings",
    ] {
        assert_eq!(
            h.client
                .put(format!("{}{path}", h.base))
                .bearer_auth(TOKEN)
                .json(&json!({"machine_post_enabled":true,"webhook_endpoint":""}))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }
    assert_eq!(h.state.x.identities.load(Ordering::SeqCst), 0);
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn disabling_preserves_key_results_and_never_replays_after_reenable() {
    let h = harness("read,post,operations").await;
    let body = h.body().await;
    let first: Value = h
        .send("saved", &body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    set_permission(&h, false).await;
    let replay: Value = h
        .send("saved", &body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first, replay);
    let result: Value = h
        .get(&format!(
            "/accounts/default/operations/{}",
            first["operation_id"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result, first);
    let mut changed = body.clone();
    changed["text"] = json!("different synthetic text");
    assert_eq!(
        h.send("saved", &changed).send().await.unwrap().status(),
        409
    );
    assert_eq!(
        h.send("new-disabled", &body).send().await.unwrap().status(),
        403
    );
    set_permission(&h, true).await;
    let replay: Value = h
        .send("saved", &body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first, replay);
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn disabling_cannot_overtake_preflight_or_dispatch_and_success_blocks_new_sends() {
    let h = harness("read,post").await;
    let body = h.body().await;
    h.state.x.prepare_hang.store(true, Ordering::SeqCst);
    h.state.x.hang.store(true, Ordering::SeqCst);
    let request = h.send("in-flight", &body);
    let task = tokio::spawn(async move { request.send().await.unwrap() });
    h.state.x.prepare_entered.notified().await;
    let service = h.state.accounts.get("default").unwrap();
    let disable = || {
        serde_json::from_value(json!({"machine_post_enabled":false,"webhook_endpoint":""})).unwrap()
    };
    assert_eq!(
        service.update_settings(disable()).await.unwrap_err().code,
        "busy"
    );
    assert!(service.settings().unwrap().machine_post_enabled);
    h.state.x.prepare_release.notify_one();
    h.state.x.entered.notified().await;
    assert_eq!(
        service.update_settings(disable()).await.unwrap_err().code,
        "busy"
    );
    h.state.x.release.notify_one();
    assert_eq!(task.await.unwrap().status(), 200);
    service.update_settings(disable()).await.unwrap();
    assert_eq!(
        h.send("after-disable", &body)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dispatch_rechecks_permission_and_persists_a_nonreplayable_failure() {
    let h = harness("read,post,operations").await;
    let body = h.body().await;
    h.state.x.prepare_hang.store(true, Ordering::SeqCst);
    let request = h.send("recheck", &body);
    let task = tokio::spawn(async move { request.send().await.unwrap() });
    h.state.x.prepare_entered.notified().await;
    // Synthetic fixture-only mutation deliberately bypasses the management lock to exercise
    // the final defensive check. Editing a live configuration externally is unsupported.
    Config::modify_account::<_, AngelicAngelError>(&h.path, "default", |current| {
        let mut account = current.unwrap();
        account.machine_post_enabled = false;
        Ok((Some(account), ()))
    })
    .unwrap();
    h.state.x.prepare_release.notify_one();
    let result: Value = task.await.unwrap().json().await.unwrap();
    assert_eq!(result["state"], "failed");
    assert_eq!(result["error"], "account_post_disabled");
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 0);
    set_permission(&h, true).await;
    let replay: Value = h
        .send("recheck", &body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result, replay);
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn waiting_same_key_rechecks_existing_before_disabled_permission() {
    let h = harness("read,post").await;
    let body = h.body().await;
    let service = h.state.accounts.get("default").unwrap();
    let guard = service.machine_lock().await.unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    headers.insert("idempotency-key", "waited".parse().unwrap());
    let request = create(
        State(Arc::clone(&h.state)),
        Path("default".into()),
        headers,
        Bytes::from(serde_json::to_vec(&body).unwrap()),
    );
    tokio::pin!(request);
    // Poll until the already-resolved request is waiting at the account lock, without sleeps.
    tokio::select! {
        biased;
        _ = &mut request => panic!("request bypassed the held account lock"),
        _ = std::future::ready(()) => {},
    }
    let parsed: Post = serde_json::from_value(body.clone()).unwrap();
    let hash = machine_ledger::hash(&serde_json::to_vec(&parsed).unwrap());
    let (operation, _) = h
        .state
        .ledger
        .reserve(
            "default",
            "fixture-client",
            "waited",
            &hash,
            body["expectedCredentialRevision"].as_str().unwrap(),
        )
        .unwrap();
    h.state
        .ledger
        .transition(
            &operation.operation_id,
            "accepted",
            "failed",
            None,
            Some("preflight_failed"),
        )
        .unwrap();
    // Model the winning management mutation while owning precisely its account lock.
    Config::modify_account::<_, AngelicAngelError>(&h.path, "default", |current| {
        let mut account = current.unwrap();
        account.machine_post_enabled = false;
        Ok((Some(account), ()))
    })
    .unwrap();
    drop(guard);
    let result = request.await.unwrap().0;
    assert_eq!(result.operation_id, operation.operation_id);
    assert_eq!(result.state, "failed");
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn off_save_wins_over_waiting_new_request_without_reservation() {
    let h = harness("read,post").await;
    let body = h.body().await;
    let service = h.state.accounts.get("default").unwrap();
    let guard = service.machine_lock().await.unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    headers.insert("idempotency-key", "waited-new".parse().unwrap());
    let request = create(
        State(Arc::clone(&h.state)),
        Path("default".into()),
        headers,
        Bytes::from(serde_json::to_vec(&body).unwrap()),
    );
    tokio::pin!(request);
    tokio::select! {
        biased;
        _ = &mut request => panic!("request bypassed the held account lock"),
        _ = std::future::ready(()) => {},
    }
    // Model a successful OFF save holding the same per-account mutation lock.
    Config::modify_account::<_, AngelicAngelError>(&h.path, "default", |current| {
        let mut account = current.unwrap();
        account.machine_post_enabled = false;
        Ok((Some(account), ()))
    })
    .unwrap();
    // Other accounts remain independently editable even while this account is locked.
    let private = h.state.accounts.get("private").unwrap();
    let update = serde_json::from_value(json!({
        "auth_token":"fixture-private", "ct0":"fixture-private-csrf",
        "webhook_endpoint":"", "machine_post_enabled":true
    }))
    .unwrap();
    assert!(
        private
            .update_settings(update)
            .await
            .unwrap()
            .machine_post_enabled
    );
    drop(guard);
    assert_eq!(request.await.unwrap_err().0, StatusCode::FORBIDDEN);
    assert!(
        h.state
            .ledger
            .existing("default", "fixture-client", "waited-new", "any")
            .unwrap()
            .is_none()
    );
    assert_eq!(h.state.x.identities.load(Ordering::SeqCst), 0);
    assert_eq!(h.state.x.preparations.load(Ordering::SeqCst), 0);
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_permission_result_storage_still_prevents_dispatch() {
    let h = harness("read,post").await;
    let body = h.body().await;
    h.state.x.prepare_hang.store(true, Ordering::SeqCst);
    let request = h.send("failed-permission-save", &body);
    let task = tokio::spawn(async move { request.send().await.unwrap() });
    h.state.x.prepare_entered.notified().await;
    Config::modify_account::<_, AngelicAngelError>(&h.path, "default", |current| {
        let mut account = current.unwrap();
        account.machine_post_enabled = false;
        Ok((Some(account), ()))
    })
    .unwrap();
    h.state.ledger.read_only();
    h.state.x.prepare_release.notify_one();
    assert_eq!(task.await.unwrap().status(), 503);
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 0);
    let replay: Value = h
        .send("failed-permission-save", &body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay["state"], "accepted");
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn legacy_and_environment_only_accounts_cannot_post_without_explicit_permission() {
    let h = harness("read,post").await;
    std::fs::write(
        &h.path,
        "[twitter]\nauth_token='fixture-auth'\nct0='fixture-csrf'\n",
    )
    .unwrap();
    let body = h.body().await;
    assert_eq!(
        h.send("legacy-off", &body).send().await.unwrap().status(),
        403
    );
    assert_eq!(h.state.x.identities.load(Ordering::SeqCst), 0);
    assert_eq!(h.state.x.posts.load(Ordering::SeqCst), 0);
    let path = h.path.with_file_name("env-only.toml");
    let accounts = Arc::new(Accounts::new(
        Arc::new(FakeBackend::default()),
        path.clone(),
        Environment::new(
            Some("fixture-env-auth"),
            Some("fixture-env-csrf"),
            None,
            None,
        )
        .unwrap(),
    ));
    let state = Machine::new(
        settings("read,post"),
        accounts,
        &path,
        Arc::new(FakeX::default()),
    )
    .unwrap();
    let readiness = account_readiness(&state, "default").await.unwrap();
    let body = json!({"text":"synthetic fixture", "expectedXUserId":"123",
        "expectedCredentialRevision":readiness["credential_revision"]});
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    headers.insert("idempotency-key", "env-off".parse().unwrap());
    let result = create(
        State(Arc::clone(&state)),
        Path("default".into()),
        headers,
        Bytes::from(serde_json::to_vec(&body).unwrap()),
    )
    .await;
    assert_eq!(result.unwrap_err().0, StatusCode::FORBIDDEN);
    assert!(!path.exists());
    assert_eq!(state.x.identities.load(Ordering::SeqCst), 0);
    assert_eq!(state.x.preparations.load(Ordering::SeqCst), 0);
    assert_eq!(state.x.posts.load(Ordering::SeqCst), 0);
}
