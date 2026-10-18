use super::*;
use crate::config::Config;
use crate::machine::{Post, Settings, XResult, router};
use crate::machine_types::{FollowResult, LikeResult, PostInfo, UserProfile};
use crate::web::service::{Accounts, Environment, fake::FakeBackend};
use crate::x_metadata::{ActionMetadata, CreateTweet};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

#[path = "machine_accounts_tests_create.rs"]
mod create;
#[path = "machine_accounts_tests_update.rs"]
mod update;

/// Every secret in these tests starts with `SECRET-`, so one search finds any leak.
const TOKEN: &str = "SECRET-machine-token-at-least-32-characters";
const AUTH: &str = "SECRET-AUTH";
const CT0: &str = "SECRET-CT0";
const NEXT_AUTH: &str = "SECRET-NEXT-AUTH";
const NEXT_CT0: &str = "SECRET-NEXT-CT0";
const HOOK: &str = "https://bot.example/webhooks/angelic/abc123";
const HOOK_SECRET: &str = "SECRET-WEBHOOK";
const CLIENT: &str = "fixture-client";
const NEW: &str = "/accounts";
const VERIFY: &str = "/credentials/verify";
const ACCOUNT: &str = "/accounts/abc123";
const CREDS: &str = "/accounts/abc123/credentials";
const HOOKS: &str = "/accounts/abc123/webhook";
const PERMS: &str = "/accounts/abc123/permissions";
const READY: &str = "/accounts/abc123/readiness";

/// X as these tests script it: who the cookies belong to, and how the lookup fails.
#[derive(Default)]
struct FakeX {
    user: Mutex<Option<String>>,
    avatar: Mutex<Option<String>>,
    reject: AtomicBool,
    down: AtomicBool,
    hang: AtomicBool,
    lookups: AtomicUsize,
}

impl MachineBackend for Arc<FakeX> {
    async fn ready(&self) -> bool {
        false
    }
    async fn prepare(&self) -> std::result::Result<CreateTweet, XError> {
        Err(XError::Malformed)
    }
    async fn identity(&self, _: &TwitterConfig) -> std::result::Result<Identity, XError> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        if self.hang.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        if self.reject.load(Ordering::SeqCst) {
            return Err(XError::Unauthorized);
        }
        if self.down.load(Ordering::SeqCst) {
            return Err(XError::Transport);
        }
        let user = self.user.lock().unwrap().clone();
        let avatar_url = self.avatar.lock().unwrap().clone();
        Ok(Identity {
            user_id: user.unwrap_or_else(|| "123".into()),
            screen_name: "fixture".into(),
            name: "Fixture".into(),
            avatar_url,
        })
    }
    async fn post(
        &self,
        _: &TwitterConfig,
        _: &CreateTweet,
        _: &Post,
    ) -> std::result::Result<String, XError> {
        Err(XError::Malformed)
    }
    async fn has_lookup_post(&self) -> bool {
        false
    }
    async fn has_lookup_user(&self) -> bool {
        false
    }
    async fn has_like(&self) -> bool {
        false
    }
    async fn prepare_actions(&self) -> XResult<ActionMetadata> {
        Err(XError::Malformed)
    }
    async fn lookup_post(
        &self,
        _: &TwitterConfig,
        _: &ActionMetadata,
        _: &str,
    ) -> XResult<Option<PostInfo>> {
        Ok(None)
    }
    async fn lookup_user(
        &self,
        _: &TwitterConfig,
        _: &ActionMetadata,
        _: &str,
    ) -> XResult<Option<UserProfile>> {
        Ok(None)
    }
    async fn like_post(
        &self,
        _: &TwitterConfig,
        _: &ActionMetadata,
        _: &str,
    ) -> XResult<LikeResult> {
        Err(XError::Malformed)
    }
    async fn follow_user(&self, _: &TwitterConfig, _: &str) -> XResult<FollowResult> {
        Err(XError::Malformed)
    }
}

type TestMachine = Machine<Arc<FakeBackend>, Arc<FakeX>>;

struct Rig {
    state: Arc<TestMachine>,
    backend: Arc<FakeBackend>,
    x: Arc<FakeX>,
    path: PathBuf,
    base: String,
    client: reqwest::Client,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn build(allowlist: Option<&str>, scopes: &str) -> crate::error::Result<Option<Settings>> {
    Settings::from_values(
        None,
        Some(TOKEN.into()),
        Some(CLIENT.into()),
        allowlist.map(str::to_owned),
        Some(scopes.into()),
    )
}

/// A machine listener over a config holding two accounts that this client did not create: `shared`
/// (made in the WebUI) and `theirs` (made through the API by another client).
async fn rig(scopes: &str, allowlist: Option<&str>) -> Rig {
    let path = crate::config::test_temp_dir().join("accounts.toml");
    let mut shared = Account::new("shared");
    shared.twitter = TwitterConfig {
        auth_token: "SECRET-STATIC-AUTH".into(),
        ct0: "SECRET-STATIC-CT0".into(),
    };
    let mut theirs = Account::new("theirs");
    theirs.machine_owner = Some("other-client".into());
    Config {
        accounts: vec![shared, theirs],
    }
    .save(&path)
    .unwrap();
    let backend = Arc::new(FakeBackend::default());
    let accounts = Accounts::new(Arc::clone(&backend), path.clone(), Environment::default());
    let x = Arc::new(FakeX::default());
    let settings = build(allowlist, scopes).unwrap().unwrap();
    let state = Machine::new(settings, Arc::new(accounts), &path, Arc::clone(&x)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = router(Arc::clone(&state));
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Rig {
        state,
        backend,
        x,
        path,
        base,
        client: reqwest::Client::new(),
        task,
    }
}

impl Rig {
    /// The status and JSON body (null when empty or not JSON). A response that contains any
    /// secret fails the test right here, so every test below also checks for leaks.
    async fn call(&self, method: &str, path: &str, body: &Value) -> (u16, Value) {
        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let url = format!("{}{PREFIX}{path}", self.base);
        let mut request = self.client.request(method, url).bearer_auth(TOKEN);
        if !body.is_null() {
            request = request.json(body);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        assert!(!text.contains("SECRET-"), "leaked a secret: {text}");
        (status, serde_json::from_str(&text).unwrap_or_default())
    }
    async fn get(&self, path: &str) -> (u16, Value) {
        self.call("GET", path, &Value::Null).await
    }
    async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.call("POST", path, body).await
    }
    async fn put(&self, path: &str, body: &Value) -> (u16, Value) {
        self.call("PUT", path, body).await
    }
    async fn delete(&self, path: &str) -> (u16, Value) {
        self.call("DELETE", path, &Value::Null).await
    }
    fn account(&self, id: &str) -> Option<Account> {
        Config::load(&self.path).unwrap().account(id).cloned()
    }
    fn file(&self) -> Vec<u8> {
        std::fs::read(&self.path).unwrap()
    }
    async fn create(&self, id: &str) -> Value {
        ok(self.post(NEW, &create_body(id)).await, 201)
    }
}

/// Waits, for at most two seconds, until `count` receivers of `abc123` are running.
async fn receivers(h: &Rig, count: usize) {
    for _ in 0..200 {
        if h.backend.active_for("abc123") == count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {count} receivers");
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

/// Asserts the fixed error a reply carries.
fn is(reply: (u16, Value), status: u16, expected: &str) {
    assert_eq!(reply.0, status, "{}", reply.1);
    assert_eq!(code(&reply.1), expected);
}

/// Asserts the status and returns the body.
fn ok(reply: (u16, Value), status: u16) -> Value {
    assert_eq!(reply.0, status, "{}", reply.1);
    reply.1
}

fn create_body(id: &str) -> Value {
    json!({"id": id, "authToken": AUTH, "ct0": CT0, "expectedXUserId": "123",
        "webhookEndpoint": HOOK, "webhookSecret": HOOK_SECRET})
}

/// Every account route as (method, path, valid body) for the account `id`.
fn all_routes(id: &str) -> Vec<(&'static str, String, Value)> {
    let verify = json!({"authToken": AUTH, "ct0": CT0});
    let fresh = json!({"authToken": AUTH, "ct0": CT0, "expectedXUserId": "123"});
    let hook = json!({"endpoint": HOOK, "secret": HOOK_SECRET});
    vec![
        ("POST", VERIFY.into(), verify),
        ("POST", NEW.into(), create_body(id)),
        ("PUT", format!("/accounts/{id}/credentials"), fresh),
        ("PUT", format!("/accounts/{id}/webhook"), hook),
        ("PUT", format!("/accounts/{id}/permissions"), json!({})),
        ("DELETE", format!("/accounts/{id}"), Value::Null),
    ]
}

#[test]
fn the_allowlist_is_optional_only_with_the_accounts_scope() {
    for list in [None, Some("")] {
        assert!(build(list, "accounts").unwrap().is_some());
        assert!(build(list, "read,accounts").unwrap().is_some());
        assert!(build(list, "read").is_err());
    }
    assert!(build(Some("default"), "read").unwrap().is_some());
    assert!(build(Some("default"), "read,accounts").unwrap().is_some());
    assert!(build(Some("default,"), "accounts").is_err());
    assert!(build(Some("Bad Id"), "accounts").is_err());
    assert!(build(Some("default"), "account").is_err());
}

#[test]
fn avatars_lose_their_size_suffix() {
    let base = "https://pbs.twimg.com/profile_images/1/abc";
    for size in ["_normal", "_bigger", "_mini"] {
        let sized = format!("{base}{size}.jpg");
        assert_eq!(original_avatar(&sized), format!("{base}.jpg"));
    }
    let plain = format!("{base}.png");
    assert_eq!(original_avatar(&plain), plain);
    let other = format!("{base}_400x400.png");
    assert_eq!(original_avatar(&other), other);
    assert_eq!(original_avatar(&format!("{base}_normal")), base);
}

#[test]
fn old_files_have_no_owner_and_unset_fields_are_not_written() {
    let old = "[[accounts]]\nid = 'default'\n[accounts.twitter]\nauth_token = 'a'\nct0 = 'b'\n";
    let config: Config = toml::from_str(old).unwrap();
    let account = config.account("default").unwrap();
    assert_eq!(account.machine_owner, None);
    assert_eq!(account.x_user_id, None);
    let written = toml::to_string_pretty(&config).unwrap();
    assert!(!written.contains("machine_owner") && !written.contains("x_user_id"));
    let legacy: Config = toml::from_str("[twitter]\nauth_token = 'a'\nct0 = 'b'\n").unwrap();
    assert_eq!(legacy.account("default").unwrap().machine_owner, None);

    let mut owned = Account::new("abc123");
    owned.machine_owner = Some(CLIENT.into());
    owned.x_user_id = Some("123".into());
    let config = Config {
        accounts: vec![owned],
    };
    let text = toml::to_string_pretty(&config).unwrap();
    let back: Config = toml::from_str(&text).unwrap();
    let account = back.account("abc123").unwrap();
    assert_eq!(account.machine_owner.as_deref(), Some(CLIENT));
    assert_eq!(account.x_user_id.as_deref(), Some("123"));
}

#[tokio::test]
async fn without_the_accounts_scope_every_route_is_denied() {
    let h = rig("read", Some("shared")).await;
    let before = h.file();
    for (method, path, body) in all_routes("shared") {
        let reply = h.call(method, &path, &body).await;
        assert_eq!(reply.0, 403, "{method} {path}");
        assert_eq!(code(&reply.1), "scope_denied", "{method} {path}");
    }
    assert_eq!(h.file(), before);
    assert_eq!(h.x.lookups.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn only_accounts_this_client_created_can_be_managed() {
    let h = rig("accounts,read", Some("shared")).await;
    let before = h.file();
    for id in ["shared", "theirs", "missing"] {
        for (method, path, body) in all_routes(id).into_iter().skip(2) {
            let reply = h.call(method, &path, &body).await;
            assert_eq!(reply.0, 404, "{id} {method} {path}");
            assert_eq!(code(&reply.1), "account_not_found", "{id} {method}");
        }
    }
    // An id that exists is never taken over, whoever made it.
    for id in ["shared", "theirs"] {
        is(h.post(NEW, &create_body(id)).await, 409, "account_exists");
    }
    assert_eq!(h.file(), before);
    assert_eq!(h.x.lookups.load(Ordering::SeqCst), 0);
    assert_eq!(h.backend.subscribes.load(Ordering::SeqCst), 0);

    // The client sees its allowlist and its own accounts, never another client's. With the
    // `accounts` scope another client's account looks exactly like a missing one.
    for id in ["theirs", "missing"] {
        let reply = h.get(&format!("/accounts/{id}/readiness")).await;
        is(reply, 404, "account_not_found");
        // A scope the client lacks is still refused first.
        let reply = h.get(&format!("/accounts/{id}/identity")).await;
        is(reply, 403, "scope_denied");
    }
    let listed = ok(h.get(NEW).await, 200);
    assert_eq!(listed["accounts"].as_array().unwrap().len(), 1);
    let shared = ok(h.get("/accounts/shared/readiness").await, 200);
    assert!(shared["x_user_id"].is_null());
    let off = json!({"post": false, "like": false, "follow": false});
    assert_eq!(shared["permissions"], off);
}
