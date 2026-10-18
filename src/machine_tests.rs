use super::*;
use crate::config::{Account, Config};
use crate::machine_types::UserRelation;
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
    action_preparations: AtomicUsize,
    post_lookups: AtomicUsize,
    user_lookups: AtomicUsize,
    likes: AtomicUsize,
    follows: AtomicUsize,
    /// Every post/user ID that reached the fake, in order.
    targets: std::sync::Mutex<Vec<String>>,
    missing: AtomicBool,
    already_liked: AtomicBool,
    approval_required: AtomicBool,
    lookup_post_ready: AtomicBool,
    lookup_user_ready: AtomicBool,
    like_ready: AtomicBool,
    identity_hang: AtomicBool,
    identity_entered: tokio::sync::Notify,
    identity_release: tokio::sync::Notify,
}

/// The one place that builds the lookup/like operation metadata for the fake.
fn action_metadata() -> ActionMetadata {
    ActionMetadata::default()
}

fn post_info(id: &str) -> PostInfo {
    PostInfo {
        id: id.into(),
        text: "synthetic post".into(),
        author_id: "555".into(),
        author_screen_name: "author".into(),
        conversation_id: id.into(),
        created_at: Some("2026-01-02T03:04:05Z".into()),
        reply_to_post_id: None,
        reply_to_user_id: None,
        quoted_post_id: None,
        quoted_author_id: None,
    }
}

fn user_profile(id: &str) -> UserProfile {
    UserProfile {
        user_id: id.into(),
        screen_name: "target".into(),
        name: Some("Target".into()),
        description: None,
        created_at: None,
        automated_label: Some(true),
        default_profile_image: None,
        is_protected: Some(false),
        followers_count: Some(10),
        following_count: Some(20),
        posts_count: Some(30),
        relation: UserRelation {
            following: Some(false),
            ..UserRelation::default()
        },
    }
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
        if self.identity_hang.load(Ordering::SeqCst) {
            self.identity_entered.notify_one();
            self.identity_release.notified().await;
        }
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
    async fn has_lookup_post(&self) -> bool {
        self.lookup_post_ready.load(Ordering::SeqCst)
    }
    async fn has_lookup_user(&self) -> bool {
        self.lookup_user_ready.load(Ordering::SeqCst)
    }
    async fn has_like(&self) -> bool {
        self.like_ready.load(Ordering::SeqCst)
    }
    async fn prepare_actions(&self) -> XResult<ActionMetadata> {
        self.action_preparations.fetch_add(1, Ordering::SeqCst);
        if self.prepare_hang.load(Ordering::SeqCst) {
            self.prepare_entered.notify_one();
            self.prepare_release.notified().await;
        }
        if self.fail_metadata.load(Ordering::SeqCst) {
            return Err(XError::Malformed);
        }
        Ok(action_metadata())
    }
    async fn lookup_post(
        &self,
        _: &TwitterConfig,
        _: &ActionMetadata,
        post_id: &str,
    ) -> XResult<Option<PostInfo>> {
        self.post_lookups.fetch_add(1, Ordering::SeqCst);
        self.targets.lock().unwrap().push(post_id.into());
        if self.missing.load(Ordering::SeqCst) {
            return Ok(None);
        }
        Ok(Some(post_info(post_id)))
    }
    async fn lookup_user(
        &self,
        _: &TwitterConfig,
        _: &ActionMetadata,
        user_id: &str,
    ) -> XResult<Option<UserProfile>> {
        self.user_lookups.fetch_add(1, Ordering::SeqCst);
        self.targets.lock().unwrap().push(user_id.into());
        if self.missing.load(Ordering::SeqCst) {
            return Ok(None);
        }
        Ok(Some(user_profile(user_id)))
    }
    async fn like_post(
        &self,
        _: &TwitterConfig,
        _: &ActionMetadata,
        post_id: &str,
    ) -> XResult<LikeResult> {
        self.likes.fetch_add(1, Ordering::SeqCst);
        self.targets.lock().unwrap().push(post_id.into());
        self.entered.notify_one();
        if self.hang.load(Ordering::SeqCst) {
            self.release.notified().await;
        }
        if self.uncertain.load(Ordering::SeqCst) {
            return Err(XError::Transport);
        }
        Ok(LikeResult {
            post_id: post_id.into(),
            already_liked: self.already_liked.load(Ordering::SeqCst),
        })
    }
    async fn follow_user(&self, _: &TwitterConfig, user_id: &str) -> XResult<FollowResult> {
        self.follows.fetch_add(1, Ordering::SeqCst);
        self.targets.lock().unwrap().push(user_id.into());
        self.entered.notify_one();
        if self.hang.load(Ordering::SeqCst) {
            self.release.notified().await;
        }
        if self.uncertain.load(Ordering::SeqCst) {
            return Err(XError::Transport);
        }
        let approval = self.approval_required.load(Ordering::SeqCst);
        Ok(FollowResult {
            user_id: user_id.into(),
            following: Some(!approval),
            follow_request_sent: Some(approval),
        })
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
    harness_with_toggles(scopes, enabled, false, false).await
}

/// Like and follow are OFF in every pre-existing harness; action tests switch them on explicitly.
async fn actions(scopes: &str) -> Harness {
    harness_with_toggles(scopes, true, true, true).await
}

async fn harness_with_toggles(scopes: &str, post: bool, like: bool, follow: bool) -> Harness {
    let directory = crate::config::test_temp_dir();
    let path = directory.join("fixture.toml");
    let mut account = Account::new("default");
    account.machine_post_enabled = post;
    account.machine_like_enabled = like;
    account.machine_follow_enabled = follow;
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
    /// A like (`likes`) or follow (`follows`) request for the default account.
    fn action(&self, kind: &str, key: &str, body: &Value) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}{PREFIX}/accounts/default/{kind}", self.base))
            .bearer_auth(TOKEN)
            .header("idempotency-key", key)
            .json(body)
    }
    /// The status and JSON body of a like/follow request.
    async fn call(&self, kind: &str, key: &str, body: &Value) -> (u16, Value) {
        let response = self.action(kind, key, body).send().await.unwrap();
        (response.status().as_u16(), response.json().await.unwrap())
    }
    async fn run(&self, kind: &str, key: &str, body: &Value) -> Value {
        self.call(kind, key, body).await.1
    }
    async fn code(&self, kind: &str, key: &str, body: &Value) -> u16 {
        self.call(kind, key, body).await.0
    }
    async fn revision(&self) -> Value {
        let ready = account_readiness(&self.state, "default").await.unwrap();
        ready["credential_revision"].clone()
    }
    /// A body for `likes` (`postId`) or `follows` (`userId`) bound to the current credentials.
    async fn action_body(&self, field: &str, target: &str) -> Value {
        let mut body = json!({"expectedXUserId": "123"});
        body[field] = json!(target);
        body["expectedCredentialRevision"] = self.revision().await;
        body
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
    for scopes in ["*", "likes", "read,lookup", "read,", "follow ", ""] {
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
    for scopes in ["like", "follow", "read,like,follow"] {
        assert!(
            Settings::from_values(
                None,
                Some(TOKEN.into()),
                Some("b".into()),
                Some("default".into()),
                Some(scopes.into())
            )
            .unwrap()
            .is_some()
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
    ] {
        assert!(!body.contains(secret), "leaked {secret}");
    }
    // Operations outside the `read` scope are reported as unavailable, never as available.
    let listed: Value = serde_json::from_str(&body).unwrap();
    let capabilities = &listed["accounts"][0]["capabilities"];
    for name in ["lookup_post", "lookup_user", "like", "follow"] {
        assert_eq!(capabilities[name], false, "{name}");
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

fn count(counter: &AtomicUsize) -> usize {
    counter.load(Ordering::SeqCst)
}

/// Whether the key left any row in the ledger, whatever its content.
fn reserved(h: &Harness, key: &str) -> bool {
    let found = h
        .state
        .ledger
        .existing("default", "fixture-client", key, "any");
    !matches!(found, Ok(None))
}

/// Nothing reached X: no identity check, no metadata, no lookup, no like, no follow, no post.
fn assert_no_x_traffic(h: &Harness) {
    let x = &h.state.x;
    for counter in [
        &x.identities,
        &x.preparations,
        &x.action_preparations,
        &x.likes,
        &x.follows,
        &x.posts,
        &x.post_lookups,
        &x.user_lookups,
    ] {
        assert_eq!(count(counter), 0);
    }
}

async fn set_actions(h: &Harness, like: Option<bool>, follow: Option<bool>) {
    let mut update = json!({"webhook_endpoint": ""});
    if let Some(enabled) = like {
        update["machine_like_enabled"] = json!(enabled);
    }
    if let Some(enabled) = follow {
        update["machine_follow_enabled"] = json!(enabled);
    }
    let update = serde_json::from_value(update).unwrap();
    h.state
        .accounts
        .get("default")
        .unwrap()
        .update_settings(update)
        .await
        .unwrap();
}

/// Synthetic fixture-only mutation that deliberately bypasses the management lock.
fn write_actions(h: &Harness, like: bool, follow: bool) {
    Config::modify_account::<_, AngelicAngelError>(&h.path, "default", |current| {
        let mut account = current.unwrap();
        account.machine_like_enabled = like;
        account.machine_follow_enabled = follow;
        Ok((Some(account), ()))
    })
    .unwrap();
}

fn write_credentials(h: &Harness, configured: bool) {
    Config::modify_account::<_, AngelicAngelError>(&h.path, "default", |current| {
        let mut account = current.unwrap();
        account.twitter = if configured {
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
}

fn like_off() -> SettingsUpdate {
    let mut body = json!({"webhook_endpoint": ""});
    body["machine_like_enabled"] = json!(false);
    serde_json::from_value(body).unwrap()
}

#[tokio::test]
async fn lookups_return_facts_and_fixed_404s_without_identity_or_ledger() {
    let h = harness("read").await;
    let post_path = "/accounts/default/posts/4242";
    let user_path = "/accounts/default/users/9001";
    let response = h.get(post_path).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let text = response.text().await.unwrap();
    for secret in [TOKEN, "SYNTHETIC-COOKIE", "SYNTHETIC-CSRF"] {
        assert!(!text.contains(secret), "leaked {secret}");
    }
    let post: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(post["id"], "4242");
    assert_eq!(post["authorId"], "555");
    assert_eq!(post["authorScreenName"], "author");
    assert_eq!(post["conversationId"], "4242");
    assert_eq!(post["createdAt"], "2026-01-02T03:04:05Z");
    assert!(post["replyToPostId"].is_null());
    let response = h.get(user_path).send().await.unwrap();
    let user: Value = response.json().await.unwrap();
    assert_eq!(user["userId"], "9001");
    assert_eq!(user["screenName"], "target");
    assert_eq!(user["automatedLabel"], true);
    assert!(user["defaultProfileImage"].is_null());
    assert_eq!(user["relation"]["following"], false);
    assert!(user["relation"]["blockedBy"].is_null());
    let x = &h.state.x;
    assert_eq!(count(&x.post_lookups), 1);
    assert_eq!(count(&x.user_lookups), 1);
    assert_eq!(count(&x.action_preparations), 2);
    assert_eq!(*x.targets.lock().unwrap(), ["4242", "9001"]);
    // Lookups are read-only: no identity check and no ledger row.
    assert_eq!(count(&x.identities), 0);
    assert_eq!(count(&x.likes) + count(&x.follows) + count(&x.posts), 0);
    x.missing.store(true, Ordering::SeqCst);
    let response = h.get(post_path).send().await.unwrap();
    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "post_not_found");
    let response = h.get(user_path).send().await.unwrap();
    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "user_not_found");
}

#[tokio::test]
async fn lookups_reject_bad_ids_scopes_accounts_and_missing_auth_before_x() {
    let h = harness("read").await;
    for id in ["abc", "12a", "-1", "1.5", "000000000000000000000"] {
        for kind in ["posts", "users"] {
            let path = format!("/accounts/default/{kind}/{id}");
            let response = h.get(&path).send().await.unwrap();
            assert_eq!(response.status(), 400, "{path}");
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["error"]["code"], "invalid_request");
        }
    }
    let status = h
        .get("/accounts/private/posts/4242")
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 403);
    let url = format!("{}{PREFIX}/accounts/default/users/9001", h.base);
    assert_eq!(h.client.get(url).send().await.unwrap().status(), 401);
    assert_no_x_traffic(&h);
    // `read` is the scope for lookups: other scopes alone do not grant them.
    let denied = harness("identity,post,like,follow,operations").await;
    for kind in ["posts", "users"] {
        let path = format!("/accounts/default/{kind}/4242");
        let response = denied.get(&path).send().await.unwrap();
        assert_eq!(response.status(), 403);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], "scope_denied");
    }
    assert_no_x_traffic(&denied);
}

#[tokio::test]
async fn lookup_failures_use_fixed_codes_and_never_hold_the_account_lock() {
    let h = harness("read").await;
    let post = "/accounts/default/posts/4242";
    write_credentials(&h, false);
    let response = h.get(post).send().await.unwrap();
    assert_eq!(response.status(), 503);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "credentials_unavailable");
    assert_no_x_traffic(&h);
    write_credentials(&h, true);
    let x = &h.state.x;
    x.fail_metadata.store(true, Ordering::SeqCst);
    let response = h.get(post).send().await.unwrap();
    assert_eq!(response.status(), 502);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "x_unavailable");
    assert_eq!(count(&x.post_lookups), 0);
    x.fail_metadata.store(false, Ordering::SeqCst);
    // A lookup does not wait for, or block, the per-account lock used by writes.
    let service = h.state.accounts.get("default").unwrap();
    let guard = service.machine_lock().await.unwrap();
    let response = h.get(post).send().await.unwrap();
    assert_eq!(response.status(), 200);
    drop(guard);
    x.prepare_hang.store(true, Ordering::SeqCst);
    let response = h.get(post).send().await.unwrap();
    assert_eq!(response.status(), 504);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "metadata_timeout");
    assert_eq!(count(&x.post_lookups), 1);
}

#[tokio::test]
async fn likes_dispatch_once_replay_and_report_already_liked() {
    let h = actions("read,like,operations").await;
    let body = h.action_body("postId", "4242").await;
    let first = h.run("likes", "like-key", &body).await;
    assert_eq!(first["kind"], "like");
    assert_eq!(first["state"], "succeeded");
    assert_eq!(first["result"]["postId"], "4242");
    assert_eq!(first["result"]["alreadyLiked"], false);
    assert!(first["post_id"].is_null());
    assert!(first["error"].is_null());
    let again = h.run("likes", "like-key", &body).await;
    assert_eq!(first, again);
    let x = &h.state.x;
    assert_eq!(count(&x.likes), 1);
    assert_eq!(count(&x.identities), 1);
    assert_eq!(count(&x.action_preparations), 1);
    // The target is acted on as given: Angel does not look it up again.
    assert_eq!(count(&x.post_lookups), 0);
    assert_eq!(*x.targets.lock().unwrap(), ["4242"]);
    let id = first["operation_id"].as_str().unwrap();
    let saved: Value = h
        .get(&format!("/accounts/default/operations/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved, first);
    // "Already liked" is a success that says so.
    x.already_liked.store(true, Ordering::SeqCst);
    let liked = h.run("likes", "already-liked", &body).await;
    assert_eq!(liked["state"], "succeeded");
    assert_eq!(liked["result"]["alreadyLiked"], true);
    assert!(liked["error"].is_null());
    assert_eq!(count(&x.likes), 2);
}

#[tokio::test]
async fn follows_report_the_relation_x_gave_including_approval_requests() {
    let h = actions("read,follow,operations").await;
    let body = h.action_body("userId", "9001").await;
    let first = h.run("follows", "follow-key", &body).await;
    assert_eq!(first["kind"], "follow");
    assert_eq!(first["state"], "succeeded");
    assert_eq!(first["result"]["userId"], "9001");
    assert_eq!(first["result"]["following"], true);
    assert_eq!(first["result"]["followRequestSent"], false);
    assert!(first["post_id"].is_null());
    let again = h.run("follows", "follow-key", &body).await;
    assert_eq!(first, again);
    let x = &h.state.x;
    // A protected account answers with a pending request: a success, not a failure.
    x.approval_required.store(true, Ordering::SeqCst);
    let pending = h.run("follows", "approval", &body).await;
    assert_eq!(pending["state"], "succeeded");
    assert_eq!(pending["result"]["following"], false);
    assert_eq!(pending["result"]["followRequestSent"], true);
    assert_eq!(count(&x.follows), 2);
    assert_eq!(count(&x.identities), 2);
    // Following needs no operation metadata and never looks the user up.
    assert_eq!(count(&x.action_preparations), 0);
    assert_eq!(count(&x.user_lookups), 0);
    assert_eq!(*x.targets.lock().unwrap(), ["9001", "9001"]);
    let id = pending["operation_id"].as_str().unwrap();
    let saved: Value = h
        .get(&format!("/accounts/default/operations/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved, pending);
}

#[tokio::test]
async fn one_key_cannot_be_reused_across_kinds_or_targets() {
    let h = actions("read,post,like,follow,operations").await;
    let post = h.body().await;
    let like = h.action_body("postId", "4242").await;
    let follow = h.action_body("userId", "9001").await;
    let result: Value = h
        .send("shared", &post)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["state"], "succeeded");
    assert_eq!(h.code("likes", "shared", &like).await, 409);
    assert_eq!(h.code("follows", "shared", &follow).await, 409);
    let liked = h.run("likes", "liked", &like).await;
    assert_eq!(liked["state"], "succeeded");
    let status = h.send("liked", &post).send().await.unwrap().status();
    assert_eq!(status, 409);
    assert_eq!(h.code("follows", "liked", &follow).await, 409);
    let mut other = like.clone();
    other["postId"] = json!("4243");
    assert_eq!(h.code("likes", "liked", &other).await, 409);
    let followed = h.run("follows", "followed", &follow).await;
    assert_eq!(followed["state"], "succeeded");
    assert_eq!(h.code("likes", "followed", &like).await, 409);
    let mut other = follow.clone();
    other["userId"] = json!("9002");
    assert_eq!(h.code("follows", "followed", &other).await, 409);
    let x = &h.state.x;
    assert_eq!(count(&x.posts), 1);
    assert_eq!(count(&x.likes), 1);
    assert_eq!(count(&x.follows), 1);
}

#[tokio::test]
async fn simultaneous_same_key_likes_and_follows_dispatch_once() {
    let h = actions("read,like,follow").await;
    let like = h.action_body("postId", "4242").await;
    let follow = h.action_body("userId", "9001").await;
    let first = h.action("likes", "parallel-like", &like);
    let second = h.action("likes", "parallel-like", &like);
    let (first, second) = tokio::join!(first.send(), second.send());
    assert_eq!(first.unwrap().status(), 200);
    assert_eq!(second.unwrap().status(), 200);
    let first = h.action("follows", "parallel-follow", &follow);
    let second = h.action("follows", "parallel-follow", &follow);
    let (first, second) = tokio::join!(first.send(), second.send());
    assert_eq!(first.unwrap().status(), 200);
    assert_eq!(second.unwrap().status(), 200);
    assert_eq!(count(&h.state.x.likes), 1);
    assert_eq!(count(&h.state.x.follows), 1);
}

#[tokio::test]
async fn like_and_follow_need_scope_and_account_switch_and_never_touch_x_otherwise() {
    for scopes in ["read", "read,like", "read,follow", "read,like,follow"] {
        for like_on in [false, true] {
            for follow_on in [false, true] {
                let h = harness_with_toggles(scopes, true, like_on, follow_on).await;
                let like = h.action_body("postId", "4242").await;
                let follow = h.action_body("userId", "9001").await;
                let like_scope = scopes.contains("like");
                let follow_scope = scopes.contains("follow");
                let like_allowed = like_scope && like_on;
                let follow_allowed = follow_scope && follow_on;
                let (status, reply) = h.call("likes", "matrix-like", &like).await;
                if like_allowed {
                    assert_eq!(status, 200);
                    assert_eq!(reply["state"], "succeeded");
                } else {
                    assert_eq!(status, 403);
                    let code = if like_scope {
                        "account_like_disabled"
                    } else {
                        "scope_denied"
                    };
                    assert_eq!(reply["error"]["code"], code);
                    assert!(!reserved(&h, "matrix-like"));
                }
                let (status, reply) = h.call("follows", "matrix-follow", &follow).await;
                if follow_allowed {
                    assert_eq!(status, 200);
                    assert_eq!(reply["state"], "succeeded");
                } else {
                    assert_eq!(status, 403);
                    let code = if follow_scope {
                        "account_follow_disabled"
                    } else {
                        "scope_denied"
                    };
                    assert_eq!(reply["error"]["code"], code);
                    assert!(!reserved(&h, "matrix-follow"));
                }
                let x = &h.state.x;
                assert_eq!(count(&x.likes), usize::from(like_allowed));
                assert_eq!(count(&x.follows), usize::from(follow_allowed));
                assert_eq!(count(&x.action_preparations), usize::from(like_allowed));
                let sent = usize::from(like_allowed) + usize::from(follow_allowed);
                assert_eq!(count(&x.identities), sent);
                assert_eq!(count(&x.posts), 0);
            }
        }
    }
}

#[tokio::test]
async fn disabled_actions_never_reserve_or_contact_x_and_keys_survive_a_grant() {
    let h = harness_with_toggles("read,like,follow,operations", true, false, false).await;
    let like = h.action_body("postId", "4242").await;
    let follow = h.action_body("userId", "9001").await;
    let (status, reply) = h.call("likes", "disabled-like", &like).await;
    assert_eq!(status, 403);
    assert_eq!(reply["error"]["code"], "account_like_disabled");
    let (status, reply) = h.call("follows", "disabled-follow", &follow).await;
    assert_eq!(status, 403);
    assert_eq!(reply["error"]["code"], "account_follow_disabled");
    assert!(!reserved(&h, "disabled-like"));
    assert!(!reserved(&h, "disabled-follow"));
    assert_no_x_traffic(&h);
    // Each switch grants only its own operation; a refused key is usable after the grant.
    set_actions(&h, Some(true), None).await;
    let liked = h.run("likes", "disabled-like", &like).await;
    assert_eq!(liked["state"], "succeeded");
    assert_eq!(h.code("follows", "disabled-follow", &follow).await, 403);
    set_actions(&h, None, Some(true)).await;
    let followed = h.run("follows", "disabled-follow", &follow).await;
    assert_eq!(followed["state"], "succeeded");
    assert_eq!(count(&h.state.x.likes), 1);
    assert_eq!(count(&h.state.x.follows), 1);
}

#[tokio::test]
async fn disabling_actions_preserves_results_and_never_replays_after_reenable() {
    let h = actions("read,like,follow,operations").await;
    let like = h.action_body("postId", "4242").await;
    let follow = h.action_body("userId", "9001").await;
    let first_like = h.run("likes", "saved-like", &like).await;
    let first_follow = h.run("follows", "saved-follow", &follow).await;
    let revision = h.revision().await;
    set_actions(&h, Some(false), Some(false)).await;
    // Switching never rotates the credential revision.
    assert_eq!(h.revision().await, revision);
    let replay = h.run("likes", "saved-like", &like).await;
    assert_eq!(replay, first_like);
    let replay = h.run("follows", "saved-follow", &follow).await;
    assert_eq!(replay, first_follow);
    let id = first_like["operation_id"].as_str().unwrap();
    let saved: Value = h
        .get(&format!("/accounts/default/operations/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved, first_like);
    let mut changed = like.clone();
    changed["postId"] = json!("4243");
    assert_eq!(h.code("likes", "saved-like", &changed).await, 409);
    assert_eq!(h.code("likes", "new-like", &like).await, 403);
    assert_eq!(h.code("follows", "new-follow", &follow).await, 403);
    set_actions(&h, Some(true), Some(true)).await;
    let replay = h.run("likes", "saved-like", &like).await;
    assert_eq!(replay, first_like);
    let replay = h.run("follows", "saved-follow", &follow).await;
    assert_eq!(replay, first_follow);
    assert_eq!(count(&h.state.x.likes), 1);
    assert_eq!(count(&h.state.x.follows), 1);
}

#[tokio::test]
async fn waiting_same_key_rechecks_existing_before_disabled_action() {
    let h = actions("read,like").await;
    let body = h.action_body("postId", "4242").await;
    let service = h.state.accounts.get("default").unwrap();
    let guard = service.machine_lock().await.unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    headers.insert("idempotency-key", "waited-like".parse().unwrap());
    let request = like(
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
    let parsed: Like = serde_json::from_value(body.clone()).unwrap();
    let hash = machine_ledger::hash(&Act::Like(parsed).canonical().unwrap());
    let (operation, _) = h
        .state
        .ledger
        .reserve_as(
            "like",
            "default",
            "fixture-client",
            "waited-like",
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
    write_actions(&h, false, true);
    drop(guard);
    let result = request.await.unwrap().0;
    assert_eq!(result.operation_id, operation.operation_id);
    assert_eq!(result.state, "failed");
    assert_no_x_traffic(&h);
}

#[tokio::test]
async fn off_save_wins_over_waiting_new_follow_without_reservation() {
    let h = actions("read,follow").await;
    let body = h.action_body("userId", "9001").await;
    let service = h.state.accounts.get("default").unwrap();
    let guard = service.machine_lock().await.unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    headers.insert("idempotency-key", "waited-new".parse().unwrap());
    let request = follow(
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
    write_actions(&h, true, false);
    drop(guard);
    assert_eq!(request.await.unwrap_err().0, StatusCode::FORBIDDEN);
    assert!(!reserved(&h, "waited-new"));
    assert_no_x_traffic(&h);
}

#[tokio::test]
async fn dispatch_rechecks_the_switch_and_persists_a_nonreplayable_failure() {
    let h = actions("read,like,follow,operations").await;
    let like = h.action_body("postId", "4242").await;
    let follow = h.action_body("userId", "9001").await;
    let x = &h.state.x;
    x.prepare_hang.store(true, Ordering::SeqCst);
    let request = h.action("likes", "recheck-like", &like);
    let task = tokio::spawn(async move { request.send().await.unwrap() });
    x.prepare_entered.notified().await;
    write_actions(&h, false, true);
    x.prepare_release.notify_one();
    let failed_like: Value = task.await.unwrap().json().await.unwrap();
    assert_eq!(failed_like["kind"], "like");
    assert_eq!(failed_like["state"], "failed");
    assert_eq!(failed_like["error"], "account_like_disabled");
    assert_eq!(count(&x.likes), 0);
    x.identity_hang.store(true, Ordering::SeqCst);
    let request = h.action("follows", "recheck-follow", &follow);
    let task = tokio::spawn(async move { request.send().await.unwrap() });
    x.identity_entered.notified().await;
    write_actions(&h, true, false);
    x.identity_release.notify_one();
    let failed_follow: Value = task.await.unwrap().json().await.unwrap();
    assert_eq!(failed_follow["kind"], "follow");
    assert_eq!(failed_follow["state"], "failed");
    assert_eq!(failed_follow["error"], "account_follow_disabled");
    assert_eq!(count(&x.follows), 0);
    // Granting again never resurrects a key that already failed.
    x.identity_hang.store(false, Ordering::SeqCst);
    x.prepare_hang.store(false, Ordering::SeqCst);
    write_actions(&h, true, true);
    let replay = h.run("likes", "recheck-like", &like).await;
    assert_eq!(replay, failed_like);
    let replay = h.run("follows", "recheck-follow", &follow).await;
    assert_eq!(replay, failed_follow);
    assert_eq!(count(&x.likes), 0);
    assert_eq!(count(&x.follows), 0);
}

#[tokio::test]
async fn disabling_cannot_overtake_a_like_in_flight() {
    let h = actions("read,like").await;
    let body = h.action_body("postId", "4242").await;
    let x = &h.state.x;
    x.prepare_hang.store(true, Ordering::SeqCst);
    x.hang.store(true, Ordering::SeqCst);
    let request = h.action("likes", "in-flight", &body);
    let task = tokio::spawn(async move { request.send().await.unwrap() });
    x.prepare_entered.notified().await;
    let service = h.state.accounts.get("default").unwrap();
    let busy = service.update_settings(like_off()).await.unwrap_err();
    assert_eq!(busy.code, "busy");
    assert!(service.settings().unwrap().machine_like_enabled);
    x.prepare_release.notify_one();
    x.entered.notified().await;
    let busy = service.update_settings(like_off()).await.unwrap_err();
    assert_eq!(busy.code, "busy");
    x.release.notify_one();
    assert_eq!(task.await.unwrap().status(), 200);
    service.update_settings(like_off()).await.unwrap();
    assert_eq!(h.code("likes", "after-disable", &body).await, 403);
    assert_eq!(count(&x.likes), 1);
}

#[tokio::test]
async fn credential_update_and_delete_cannot_overtake_a_follow_in_flight() {
    let h = actions("read,follow").await;
    let body = h.action_body("userId", "9001").await;
    h.state.x.hang.store(true, Ordering::SeqCst);
    let request = h.action("follows", "race", &body);
    let task = tokio::spawn(async move { request.send().await.unwrap() });
    h.state.x.entered.notified().await;
    let service = h.state.accounts.get("default").unwrap();
    assert!(service.update_settings(update()).await.is_err());
    assert!(h.state.accounts.delete("default").await.is_err());
    h.state.x.release.notify_one();
    assert_eq!(task.await.unwrap().status(), 200);
    service.update_settings(update()).await.unwrap();
    assert_eq!(h.code("follows", "stale", &body).await, 409);
}

#[tokio::test]
async fn uncertain_likes_and_follows_are_unknown_and_never_replayed() {
    let h = actions("read,like,follow,operations").await;
    let like = h.action_body("postId", "4242").await;
    let follow = h.action_body("userId", "9001").await;
    let x = &h.state.x;
    x.hang.store(true, Ordering::SeqCst);
    let timed_out = h.run("likes", "timeout-like", &like).await;
    assert_eq!(timed_out["state"], "unknown");
    assert_eq!(timed_out["error"], "delivery_uncertain");
    assert!(timed_out["result"].is_null());
    x.hang.store(false, Ordering::SeqCst);
    x.uncertain.store(true, Ordering::SeqCst);
    let lost = h.run("follows", "lost-follow", &follow).await;
    assert_eq!(lost["state"], "unknown");
    assert_eq!(lost["error"], "delivery_uncertain");
    x.uncertain.store(false, Ordering::SeqCst);
    let replay = h.run("likes", "timeout-like", &like).await;
    assert_eq!(replay, timed_out);
    let replay = h.run("follows", "lost-follow", &follow).await;
    assert_eq!(replay, lost);
    assert_eq!(count(&x.likes), 1);
    assert_eq!(count(&x.follows), 1);
}

#[tokio::test]
async fn action_result_storage_failure_is_a_nonreplayable_dispatching_tombstone() {
    let h = actions("read,like").await;
    let body = h.action_body("postId", "4242").await;
    h.state.x.hang.store(true, Ordering::SeqCst);
    let request = h.action("likes", "save-failure", &body);
    let task = tokio::spawn(async move { request.send().await.unwrap() });
    h.state.x.entered.notified().await;
    h.state.ledger.read_only();
    h.state.x.release.notify_one();
    let result: Value = task.await.unwrap().json().await.unwrap();
    assert_eq!(result["state"], "unknown");
    assert_eq!(result["error"], "result_storage_failed");
    let replay = h.run("likes", "save-failure", &body).await;
    assert_eq!(replay["state"], "dispatching");
    assert_eq!(count(&h.state.x.likes), 1);
}

#[tokio::test]
async fn failed_preflight_and_storage_failures_never_send_likes_or_follows() {
    let h = actions("read,like,follow").await;
    let like = h.action_body("postId", "4242").await;
    let follow = h.action_body("userId", "9001").await;
    let x = &h.state.x;
    let mut wrong = like.clone();
    wrong["expectedXUserId"] = json!("456");
    let result = h.run("likes", "wrong-user-like", &wrong).await;
    assert_eq!(result["state"], "failed");
    assert_eq!(result["error"], "preflight_failed");
    let mut wrong = follow.clone();
    wrong["expectedXUserId"] = json!("456");
    let result = h.run("follows", "wrong-user-follow", &wrong).await;
    assert_eq!(result["state"], "failed");
    assert_eq!(result["error"], "preflight_failed");
    assert_eq!(count(&x.likes) + count(&x.follows), 0);
    // A like needs its operation metadata; a follow does not.
    x.fail_metadata.store(true, Ordering::SeqCst);
    let result = h.run("likes", "metadata-like", &like).await;
    assert_eq!(result["state"], "failed");
    assert_eq!(result["error"], "preflight_failed");
    assert_eq!(count(&x.likes), 0);
    let result = h.run("follows", "metadata-follow", &follow).await;
    assert_eq!(result["state"], "succeeded");
    x.fail_metadata.store(false, Ordering::SeqCst);
    h.state.ledger.read_only();
    assert_eq!(h.code("likes", "storage-like", &like).await, 503);
    assert_eq!(h.code("follows", "storage-follow", &follow).await, 503);
    assert_eq!(count(&x.likes), 0);
    assert_eq!(count(&x.follows), 1);
}

#[tokio::test]
async fn stale_or_unknown_credential_revisions_conflict_before_reservation() {
    let h = actions("read,like,follow").await;
    let like = h.action_body("postId", "4242").await;
    let follow = h.action_body("userId", "9001").await;
    let service = h.state.accounts.get("default").unwrap();
    service.update_settings(update()).await.unwrap();
    for (kind, key, body) in [
        ("likes", "stale-like", &like),
        ("follows", "stale-follow", &follow),
    ] {
        let (status, reply) = h.call(kind, key, body).await;
        assert_eq!(status, 409);
        assert_eq!(reply["error"]["code"], "credential_revision_changed");
        assert!(!reserved(&h, key));
    }
    let mut unknown = h.action_body("postId", "4242").await;
    unknown["expectedCredentialRevision"] = json!(uuid::Uuid::new_v4().to_string());
    assert_eq!(h.code("likes", "unknown-like", &unknown).await, 409);
    let mut unknown = h.action_body("userId", "9001").await;
    unknown["expectedCredentialRevision"] = json!(uuid::Uuid::new_v4().to_string());
    assert_eq!(h.code("follows", "unknown-follow", &unknown).await, 409);
    assert_no_x_traffic(&h);
}

#[tokio::test]
async fn malformed_action_requests_are_rejected_before_any_state_or_x_traffic() {
    let h = actions("read,like,follow").await;
    let like = h.action_body("postId", "4242").await;
    let follow = h.action_body("userId", "9001").await;
    // Permission and field injection through the request body is refused.
    for field in [
        "machine_like_enabled",
        "machine_follow_enabled",
        "machine_post_enabled",
        "media",
        "operation",
    ] {
        let mut injected = like.clone();
        injected[field] = json!(true);
        assert_eq!(h.code("likes", "inject-like", &injected).await, 400);
        let mut injected = follow.clone();
        injected[field] = json!(true);
        assert_eq!(h.code("follows", "inject-follow", &injected).await, 400);
    }
    // The two bodies are not interchangeable.
    assert_eq!(h.code("likes", "swapped-like", &follow).await, 400);
    assert_eq!(h.code("follows", "swapped-follow", &like).await, 400);
    for bad in ["", "abc", "12a", "-1", "000000000000000000000"] {
        let mut body = like.clone();
        body["postId"] = json!(bad);
        assert_eq!(h.code("likes", "bad-post", &body).await, 400);
        let mut body = follow.clone();
        body["userId"] = json!(bad);
        assert_eq!(h.code("follows", "bad-user", &body).await, 400);
        let mut body = like.clone();
        body["expectedXUserId"] = json!(bad);
        assert_eq!(h.code("likes", "bad-expected", &body).await, 400);
    }
    let mut body = like.clone();
    body["postId"] = json!(4242);
    assert_eq!(h.code("likes", "number-id", &body).await, 400);
    let mut body = follow.clone();
    body["expectedCredentialRevision"] = json!("not-a-uuid");
    assert_eq!(h.code("follows", "bad-revision", &body).await, 400);
    let mut body = like.clone();
    body.as_object_mut().unwrap().remove("expectedXUserId");
    assert_eq!(h.code("likes", "missing-field", &body).await, 400);
    let long = "k".repeat(129);
    for key in ["", "a b", long.as_str()] {
        assert_eq!(h.code("likes", key, &like).await, 400, "{key:?}");
    }
    let url = format!("{}{PREFIX}/accounts/default/likes", h.base);
    let response = h
        .client
        .post(&url)
        .bearer_auth(TOKEN)
        .json(&like)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let reply: Value = response.json().await.unwrap();
    assert_eq!(reply["error"]["code"], "idempotency_key_required");
    let response = h
        .client
        .post(&url)
        .bearer_auth(TOKEN)
        .header("idempotency-key", "plain-text")
        .header("content-type", "text/plain")
        .body(serde_json::to_vec(&like).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 415);
    assert_no_x_traffic(&h);
    assert!(!reserved(&h, "plain-text"));
}

#[tokio::test]
async fn readiness_capabilities_combine_scope_switch_credentials_and_operation_metadata() {
    for scopes in [
        "identity",
        "read",
        "read,like",
        "read,follow",
        "read,like,follow",
    ] {
        for like_on in [false, true] {
            for follow_on in [false, true] {
                let h = harness_with_toggles(scopes, true, like_on, follow_on).await;
                let read = scopes.contains("read");
                let like_scope = scopes.contains("like");
                let follow_scope = scopes.contains("follow");
                for credentials in [false, true] {
                    write_credentials(&h, credentials);
                    for flags in 0..8_u8 {
                        let on = |bit: u8| flags & (1 << bit) != 0;
                        let x = &h.state.x;
                        x.lookup_post_ready.store(on(0), Ordering::SeqCst);
                        x.lookup_user_ready.store(on(1), Ordering::SeqCst);
                        x.like_ready.store(on(2), Ordering::SeqCst);
                        let ready = account_readiness(&h.state, "default").await.unwrap();
                        let capabilities = &ready["capabilities"];
                        let lookup_post = read && credentials && on(0);
                        let lookup_user = read && credentials && on(1);
                        let like = like_scope && like_on && credentials && on(2);
                        let follow = follow_scope && follow_on && credentials;
                        assert_eq!(capabilities["lookup_post"], lookup_post);
                        assert_eq!(capabilities["lookup_user"], lookup_user);
                        assert_eq!(capabilities["like"], like);
                        assert_eq!(capabilities["follow"], follow);
                        assert_eq!(capabilities["identity"], scopes.contains("identity"));
                        assert_eq!(ready["post_ready"], false);
                    }
                }
                // Readiness is cache-only: nothing above contacted X.
                assert_no_x_traffic(&h);
            }
        }
    }
}

#[tokio::test]
async fn action_capabilities_do_not_change_post_readiness() {
    let h = harness_with_toggles("read,post,like,follow", true, false, false).await;
    let x = &h.state.x;
    x.metadata_ready.store(true, Ordering::SeqCst);
    let ready = account_readiness(&h.state, "default").await.unwrap();
    assert_eq!(ready["post_ready"], true);
    assert_eq!(ready["capabilities"]["text_post"], true);
    assert_eq!(ready["capabilities"]["reply"], true);
    assert_eq!(ready["capabilities"]["like"], false);
    assert_eq!(ready["capabilities"]["follow"], false);
    // Lookup readiness is tracked on its own, not through the post catalogue.
    x.metadata_ready.store(false, Ordering::SeqCst);
    x.lookup_post_ready.store(true, Ordering::SeqCst);
    let ready = account_readiness(&h.state, "default").await.unwrap();
    assert_eq!(ready["post_ready"], false);
    assert_eq!(ready["capabilities"]["lookup_post"], true);
    assert_eq!(ready["capabilities"]["lookup_user"], false);
}

#[tokio::test]
async fn action_switches_do_not_grant_scopes_accounts_or_machine_settings_access() {
    let h = actions("read").await;
    let like = h.action_body("postId", "4242").await;
    let follow = h.action_body("userId", "9001").await;
    assert_eq!(h.code("likes", "no-scope-like", &like).await, 403);
    assert_eq!(h.code("follows", "no-scope-follow", &follow).await, 403);
    assert_no_x_traffic(&h);
    let h = actions("read,like,follow").await;
    Config::modify_account::<_, AngelicAngelError>(&h.path, "private", |current| {
        let mut account = current.unwrap();
        account.machine_like_enabled = true;
        account.machine_follow_enabled = true;
        Ok((Some(account), ()))
    })
    .unwrap();
    for (kind, body) in [("likes", &like), ("follows", &follow)] {
        let url = format!("{}{PREFIX}/accounts/private/{kind}", h.base);
        let response = h
            .client
            .post(url)
            .bearer_auth(TOKEN)
            .header("idempotency-key", "private")
            .json(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
    }
    for path in [
        "/api/accounts/default/settings",
        "/api/machine/v1/accounts/default/settings",
    ] {
        let body = json!({"machine_like_enabled": false, "webhook_endpoint": ""});
        let response = h
            .client
            .put(format!("{}{path}", h.base))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
    }
    let service = h.state.accounts.get("default").unwrap();
    let saved = service.settings().unwrap();
    assert!(saved.machine_like_enabled);
    assert!(saved.machine_follow_enabled);
    assert_no_x_traffic(&h);
}

#[tokio::test]
async fn legacy_and_environment_accounts_cannot_like_or_follow_by_default() {
    let h = actions("read,like,follow").await;
    std::fs::write(
        &h.path,
        "[twitter]\nauth_token='fixture-auth'\nct0='fixture-csrf'\n",
    )
    .unwrap();
    let like_body = h.action_body("postId", "4242").await;
    let follow_body = h.action_body("userId", "9001").await;
    let (status, reply) = h.call("likes", "legacy-like", &like_body).await;
    assert_eq!(status, 403);
    assert_eq!(reply["error"]["code"], "account_like_disabled");
    let (status, reply) = h.call("follows", "legacy-follow", &follow_body).await;
    assert_eq!(status, 403);
    assert_eq!(reply["error"]["code"], "account_follow_disabled");
    assert_no_x_traffic(&h);
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
        settings("read,like,follow"),
        accounts,
        &path,
        Arc::new(FakeX::default()),
    )
    .unwrap();
    let readiness = account_readiness(&state, "default").await.unwrap();
    assert_eq!(readiness["capabilities"]["like"], false);
    assert_eq!(readiness["capabilities"]["follow"], false);
    let mut body = json!({"expectedXUserId": "123"});
    body["userId"] = json!("9001");
    body["expectedCredentialRevision"] = readiness["credential_revision"].clone();
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    headers.insert("idempotency-key", "env-off".parse().unwrap());
    let result = follow(
        State(Arc::clone(&state)),
        Path("default".into()),
        headers,
        Bytes::from(serde_json::to_vec(&body).unwrap()),
    )
    .await;
    assert_eq!(result.unwrap_err().0, StatusCode::FORBIDDEN);
    assert!(!path.exists());
    assert_eq!(count(&state.x.identities), 0);
    assert_eq!(count(&state.x.follows), 0);
}
