use crate::config::TwitterConfig;
use crate::error::{AngelicAngelError, Result};
use crate::push::PushSubscription;
use crate::transaction::{Transaction, ondemand_file_hash};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::header::{CONTENT_TYPE, HeaderValue, LOCATION};
use reqwest::{Client, RequestBuilder, Response, StatusCode, Url};
use serde::Deserialize;
use serde_json::json;
use std::fmt;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const TWITTER_API_BASE: &str = "https://x.com/i/api/1.1";
const TWITTER_SETTINGS_BASE: &str = "https://api.x.com/1.1";
const TWITTER_HOME_URL: &str = "https://x.com/home";
const ONDEMAND_URL_PREFIX: &str = "https://abs.twimg.com/responsive-web/client-web/ondemand.s.";
const ONDEMAND_URL_SUFFIX: &str = "a.js";
const TRANSACTION_HEADER: &str = "x-client-transaction-id";
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36";
/// 呼び出し側の外側タイムアウト (30 秒) より十分短くし、初期化が毎回途中で切られないようにする。
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BOOTSTRAP_BYTES: usize = 4 * 1024 * 1024;
/// 生成器の有効期間。X 側の鍵が入れ替わっても再起動なしで追従するための上限で、期限後の最初の呼び出しが取り直す。
const TRANSACTION_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_ONDEMAND_HASH_LEN: usize = 64;
/// 匿名 bootstrap だけが辿る同一オリジン内の redirect 回数。未ログインの /home は 1 回転送される。
const MAX_BOOTSTRAP_REDIRECTS: usize = 3;
/// 上流の `Transaction.create()` が x.com/home に付ける匿名ヘッダ (User-Agent は client 既定)。
const HOME_HEADERS: [(&str, &str); 6] = [
    ("authority", "x.com"),
    ("accept-language", "en-US,en;q=0.9"),
    ("cache-control", "no-cache"),
    ("referer", "https://x.com"),
    ("x-twitter-active-user", "yes"),
    ("x-twitter-client-language", "en"),
];
const AUTHORIZATION_BEARER: &str = "Bearer AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA";
const DEVICE_ID: &str = "Mac/Firefox";
const FRIENDS_PAGE_SIZE: &str = "5000";
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_USER_ID_LEN: usize = 20;
const MAX_SCREEN_NAME_LEN: usize = 15;

/// Adds a credential header; an unrepresentable value is passed through so reqwest reports a builder error.
fn secret_header(request: RequestBuilder, name: &'static str, value: String) -> RequestBuilder {
    match HeaderValue::try_from(value.as_str()) {
        Ok(mut header) => {
            header.set_sensitive(true);
            request.header(name, header)
        },
        Err(_) => request.header(name, value),
    }
}

/// Attaches the web-session headers shared by every X request.
fn with_session(request: RequestBuilder, twitter: &TwitterConfig) -> RequestBuilder {
    let request = request
        .header("Authorization", AUTHORIZATION_BEARER)
        .header("x-twitter-auth-type", "OAuth2Session")
        .header("x-twitter-active-user", "yes")
        .header("x-twitter-client-language", "en");
    let request = secret_header(request, "x-csrf-token", twitter.ct0.clone());
    secret_header(
        request,
        "Cookie",
        format!("auth_token={}; ct0={}", twitter.auth_token, twitter.ct0),
    )
}

pub async fn register(
    twitter_config: &TwitterConfig,
    push_subscription: &PushSubscription,
) -> Result<()> {
    let client = Client::new();
    register_push_subscription(&client, twitter_config, push_subscription).await?;
    Ok(())
}

async fn register_push_subscription(
    client: &Client,
    twitter_config: &TwitterConfig,
    push_subscription: &PushSubscription,
) -> Result<()> {
    let url = format!("{}/notifications/settings/login.json", TWITTER_API_BASE);

    let token = &push_subscription.endpoint;
    let encryption_key1 = URL_SAFE_NO_PAD.encode(&push_subscription.keys.public_key);
    let encryption_key2 = URL_SAFE_NO_PAD.encode(&push_subscription.keys.auth_secret);

    let body = json!({
        "push_device_info": {
            "os_version": DEVICE_ID,
            "udid": DEVICE_ID,
            "env": 3,
            "locale": "en",
            "protocol_version": 1,
            "token": token,
            "encryption_key1": encryption_key1,
            "encryption_key2": encryption_key2
        }
    });

    tracing::debug!(url = %url, "sending push subscription request");
    tracing::debug!(body = %serde_json::to_string_pretty(&body).unwrap(), "request body");

    let response = with_session(client.post(&url), twitter_config)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    tracing::debug!(status = %status, "response status");

    if !response.status().is_success() {
        let error_text = response.text().await?;
        tracing::debug!(body = %error_text, "error response body");

        return Err(AngelicAngelError::TwitterApi(format!(
            "push subscription registration failed ({}): {}",
            status, error_text
        )));
    }

    let response_json: serde_json::Value = response.json().await?;
    tracing::debug!(body = %serde_json::to_string_pretty(&response_json).unwrap(), "success response body");

    Ok(())
}

// ---- following list / per-account notifications ---------------------------

/// Failure of an unofficial X call, reduced to a category that is always safe to log and show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XError {
    Unauthorized,
    Forbidden,
    RateLimited,
    Status(u16),
    Transport,
    Malformed,
}

impl fmt::Display for XError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized => f.write_str("X rejected the saved credentials (401)."),
            Self::Forbidden => f.write_str("X denied the request (403)."),
            Self::RateLimited => f.write_str("X rate limit reached (429). Try again later."),
            Self::Status(code) => write!(f, "X returned an error (HTTP {code})."),
            Self::Transport => f.write_str("Could not reach X."),
            Self::Malformed => f.write_str("X returned an unexpected response."),
        }
    }
}

impl XError {
    /// Errors that mean every further call would fail the same way.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            Self::Unauthorized | Self::Forbidden | Self::RateLimited
        )
    }
}

/// One page of `friends/ids`; ids are validated digit strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowingPage {
    pub ids: Vec<String>,
    pub next_cursor: i64,
}

#[derive(Deserialize)]
struct SettingsResponse {
    screen_name: String,
}

#[derive(Deserialize)]
struct FriendsIdsResponse {
    ids: Vec<String>,
    #[serde(default)]
    next_cursor_str: Option<String>,
    #[serde(default)]
    next_cursor: Option<i64>,
}

pub fn is_valid_user_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_USER_ID_LEN && id.bytes().all(|b| b.is_ascii_digit())
}

pub fn is_valid_screen_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SCREEN_NAME_LEN
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn classify(status: StatusCode) -> std::result::Result<(), XError> {
    match status.as_u16() {
        200..=299 => Ok(()),
        401 => Err(XError::Unauthorized),
        403 => Err(XError::Forbidden),
        429 => Err(XError::RateLimited),
        code => Err(XError::Status(code)),
    }
}

fn transport(error: reqwest::Error) -> XError {
    tracing::debug!(
        timeout = error.is_timeout(),
        connect = error.is_connect(),
        "X request did not complete"
    );
    XError::Transport
}

/// Reads at most `MAX_RESPONSE_BYTES`; bodies are never logged.
async fn read_body(response: Response) -> std::result::Result<Vec<u8>, XError> {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
    {
        return Err(XError::Malformed);
    }
    let bytes = response.bytes().await.map_err(transport)?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(XError::Malformed);
    }
    Ok(bytes.to_vec())
}

/// 接続先。本番は固定の X のホストで、テストだけが差し替える。
struct Endpoints {
    settings_base: String,
    api_base: String,
    home_url: String,
    ondemand_prefix: String,
}

impl Endpoints {
    fn production() -> Self {
        Self {
            settings_base: TWITTER_SETTINGS_BASE.to_string(),
            api_base: TWITTER_API_BASE.to_string(),
            home_url: TWITTER_HOME_URL.to_string(),
            ondemand_prefix: ONDEMAND_URL_PREFIX.to_string(),
        }
    }
}

/// Client for the unofficial endpoints used by the bulk-notification job.
pub struct XApi {
    client: Client,
    endpoints: Endpoints,
    /// 成功した生成器と作成時刻。失敗は保持せず、期限切れは次の呼び出しで取り直す。
    transaction: StdMutex<Option<(Arc<Transaction>, Instant)>>,
    /// bootstrap を同時に 1 本へ絞る。待った側は先行の結果をそのまま使う。
    bootstrap_gate: Mutex<()>,
    bootstrap_timeout: Duration,
    transaction_ttl: Duration,
}

impl XApi {
    pub fn new() -> Self {
        Self::build(
            Endpoints::production(),
            None,
            BOOTSTRAP_TIMEOUT,
            TRANSACTION_TTL,
        )
    }

    /// Redirects are disabled so session headers can never be forwarded elsewhere.
    fn build(
        endpoints: Endpoints,
        transaction: Option<Transaction>,
        bootstrap_timeout: Duration,
        transaction_ttl: Duration,
    ) -> Self {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(BROWSER_USER_AGENT)
            .build()
            .expect("failed to initialize the HTTP client");
        Self {
            client,
            endpoints,
            transaction: StdMutex::new(transaction.map(|t| (Arc::new(t), Instant::now()))),
            bootstrap_gate: Mutex::new(()),
            bootstrap_timeout,
            transaction_ttl,
        }
    }

    fn settings_url(&self) -> String {
        format!("{}/account/settings.json", self.endpoints.settings_base)
    }

    fn friends_ids_url(&self) -> String {
        format!("{}/friends/ids.json", self.endpoints.api_base)
    }

    fn friendships_update_url(&self) -> String {
        format!("{}/friendships/update.json", self.endpoints.api_base)
    }

    /// 認証情報なしで HTML とスクリプトを取り、生成器を作る。取得先は固定ホストとハッシュ検証済みの URL だけ。
    async fn bootstrap(&self) -> std::result::Result<Transaction, XError> {
        let work = async {
            let home = self
                .fetch_text(&self.endpoints.home_url, &HOME_HEADERS)
                .await?;
            let hash = ondemand_file_hash(&home)
                .filter(|hash| hash.len() <= MAX_ONDEMAND_HASH_LEN)
                .ok_or(XError::Malformed)?;
            let url = format!(
                "{}{hash}{ONDEMAND_URL_SUFFIX}",
                self.endpoints.ondemand_prefix
            );
            let script = self.fetch_text(&url, &[]).await?;
            Transaction::new(&home, &script).map_err(|error| {
                tracing::debug!(%error, "x-client-transaction-id generator rejected the page");
                XError::Malformed
            })
        };
        match tokio::time::timeout(self.bootstrap_timeout, work).await {
            Ok(result) => result,
            Err(_) => {
                tracing::debug!("x-client-transaction-id bootstrap timed out");
                Err(XError::Transport)
            },
        }
    }

    /// 匿名 GET。redirect は同一オリジンへの最大 `MAX_BOOTSTRAP_REDIRECTS` 回だけ手動で辿り、
    /// 別オリジン・ループ・回数超過は転送先へ送らず `Status` で失敗させる。
    /// 失敗は常に `Status` / `Transport` / `Malformed` にし、401・403・429 を認証失敗として扱わない。
    async fn fetch_text(
        &self,
        start: &str,
        headers: &[(&str, &str)],
    ) -> std::result::Result<String, XError> {
        let mut url = Url::parse(start).map_err(|_| XError::Malformed)?;
        let origin = url.origin();
        let mut visited = vec![url.clone()];
        let mut response = loop {
            let mut request = self.client.get(url.clone());
            for (name, value) in headers {
                request = request.header(*name, *value);
            }
            let response = request.send().await.map_err(transport)?;
            let status = response.status();
            tracing::debug!(
                status = status.as_u16(),
                "x-client-transaction-id bootstrap response"
            );
            if !status.is_redirection() {
                break response;
            }
            let next = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|location| url.join(location).ok());
            match next {
                Some(next)
                    if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
                        && next.origin() == origin
                        && visited.len() <= MAX_BOOTSTRAP_REDIRECTS
                        && !visited.contains(&next) =>
                {
                    visited.push(next.clone());
                    url = next;
                },
                _ => return Err(XError::Status(status.as_u16())),
            }
        };
        let status = response.status();
        if !status.is_success() {
            return Err(XError::Status(status.as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_BOOTSTRAP_BYTES as u64)
        {
            return Err(XError::Malformed);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport)? {
            if body.len() + chunk.len() > MAX_BOOTSTRAP_BYTES {
                return Err(XError::Malformed);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    fn cached_transaction(&self) -> Option<Arc<Transaction>> {
        let cached = self.transaction.lock().unwrap_or_else(|e| e.into_inner());
        cached
            .as_ref()
            .filter(|(_, created)| created.elapsed() < self.transaction_ttl)
            .map(|(transaction, _)| Arc::clone(transaction))
    }

    async fn transaction(&self) -> std::result::Result<Arc<Transaction>, XError> {
        if let Some(transaction) = self.cached_transaction() {
            return Ok(transaction);
        }
        let _gate = self.bootstrap_gate.lock().await;
        if let Some(transaction) = self.cached_transaction() {
            return Ok(transaction);
        }
        let transaction = Arc::new(self.bootstrap().await?);
        *self.transaction.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((Arc::clone(&transaction), Instant::now()));
        Ok(transaction)
    }

    async fn send(
        &self,
        request: RequestBuilder,
        twitter: &TwitterConfig,
    ) -> std::result::Result<Vec<u8>, XError> {
        let mut request = with_session(request, twitter).build().map_err(transport)?;
        // 生成器を用意できないときは資格情報を載せたリクエストを送らない
        let transaction = self.transaction().await?;
        let id = transaction.generate(request.method().as_str(), request.url().path());
        let id = HeaderValue::try_from(id).map_err(|_| XError::Malformed)?;
        request.headers_mut().insert(TRANSACTION_HEADER, id);

        let response = self.client.execute(request).await.map_err(transport)?;
        let status = response.status();
        let body = read_body(response).await;
        tracing::debug!(status = status.as_u16(), "X response");
        classify(status)?;
        body
    }

    /// Screen name of the logged-in account (`account/settings`).
    pub async fn screen_name(
        &self,
        twitter: &TwitterConfig,
    ) -> std::result::Result<String, XError> {
        let body = self
            .send(self.client.get(self.settings_url()), twitter)
            .await?;
        let parsed: SettingsResponse =
            serde_json::from_slice(&body).map_err(|_| XError::Malformed)?;
        if !is_valid_screen_name(&parsed.screen_name) {
            return Err(XError::Malformed);
        }
        Ok(parsed.screen_name)
    }

    /// One page of the accounts followed by `screen_name`.
    pub async fn following_page(
        &self,
        twitter: &TwitterConfig,
        screen_name: &str,
        cursor: i64,
    ) -> std::result::Result<FollowingPage, XError> {
        if !is_valid_screen_name(screen_name) {
            return Err(XError::Malformed);
        }
        let mut url = Url::parse(&self.friends_ids_url()).map_err(|_| XError::Malformed)?;
        url.query_pairs_mut()
            .append_pair("screen_name", screen_name)
            .append_pair("count", FRIENDS_PAGE_SIZE)
            .append_pair("stringify_ids", "true")
            .append_pair("cursor", &cursor.to_string());
        let body = self.send(self.client.get(url), twitter).await?;
        let parsed: FriendsIdsResponse =
            serde_json::from_slice(&body).map_err(|_| XError::Malformed)?;
        let next_cursor = parsed
            .next_cursor_str
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok())
            .or(parsed.next_cursor)
            .ok_or(XError::Malformed)?;
        if !parsed.ids.iter().all(|id| is_valid_user_id(id)) {
            return Err(XError::Malformed);
        }
        Ok(FollowingPage {
            ids: parsed.ids,
            next_cursor,
        })
    }

    /// Turns on device notifications for one followed account (`friendships/update`).
    pub async fn enable_device_notifications(
        &self,
        twitter: &TwitterConfig,
        user_id: &str,
    ) -> std::result::Result<(), XError> {
        if !is_valid_user_id(user_id) {
            return Err(XError::Malformed);
        }
        let request = self
            .client
            .post(self.friendships_update_url())
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(format!("user_id={user_id}&device=true"));
        self.send(request, twitter).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transaction::fixture;
    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::http::{HeaderMap, Method, Uri};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    const SCRIPT_PATH: &str = "/ondemand.s.c0ffee1a.js";
    const CREDENTIAL_HEADERS: [&str; 5] = [
        "authorization",
        "cookie",
        "x-csrf-token",
        "x-twitter-auth-type",
        "x-client-transaction-id",
    ];

    #[derive(Clone)]
    struct Seen {
        method: String,
        path: String,
        query: String,
        headers: HeaderMap,
        body: String,
    }

    type Log = Arc<Mutex<Vec<Seen>>>;

    /// 終了時に必ず listener を止めるテスト用サーバー。
    struct TestServer {
        base: String,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn twitter() -> TwitterConfig {
        TwitterConfig {
            auth_token: "AUTH-SECRET".into(),
            ct0: "CT0-SECRET".into(),
        }
    }

    async fn serve(app: Router) -> TestServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        TestServer { base, task }
    }

    fn endpoints(base: &str) -> Endpoints {
        Endpoints {
            settings_base: format!("{base}/1.1"),
            api_base: format!("{base}/i/api/1.1"),
            home_url: format!("{base}/home"),
            ondemand_prefix: format!("{base}/ondemand.s."),
        }
    }

    /// 生成器を注入した XApi。接続先はすべてローカルで、実際の X には届かない。
    fn injected(base: &str) -> XApi {
        XApi::build(
            endpoints(base),
            Some(fixture::transaction()),
            Duration::from_secs(5),
            TRANSACTION_TTL,
        )
    }

    /// 合成した bootstrap 用ページをローカルサーバーから取って初期化する XApi。
    fn bootstrapping(base: &str, timeout: Duration) -> XApi {
        XApi::build(endpoints(base), None, timeout, TRANSACTION_TTL)
    }

    fn reply(status: u16, body: impl Into<String>) -> Response {
        (StatusCode::from_u16(status).unwrap(), body.into()).into_response()
    }

    /// 受けたリクエストをすべて記録し、`respond` の返事を返す。
    fn recording<F>(log: Log, respond: F) -> Router
    where
        F: Fn(&Seen) -> Response + Clone + Send + Sync + 'static,
    {
        Router::new().fallback(
            move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                let log = Arc::clone(&log);
                let respond = respond.clone();
                async move {
                    let seen = Seen {
                        method: method.to_string(),
                        path: uri.path().to_string(),
                        query: uri.query().unwrap_or_default().to_string(),
                        headers,
                        body: String::from_utf8_lossy(&body).into_owned(),
                    };
                    let response = respond(&seen);
                    log.lock().unwrap().push(seen);
                    response
                }
            },
        )
    }

    #[derive(Clone)]
    struct Site {
        home: Arc<Mutex<(u16, String)>>,
        script: Arc<Mutex<(u16, String)>>,
        settings: Arc<Mutex<(u16, String)>>,
    }

    impl Site {
        fn new() -> Self {
            Self {
                home: Arc::new(Mutex::new((200, fixture::home_html()))),
                script: Arc::new(Mutex::new((200, fixture::ondemand_js()))),
                settings: Arc::new(Mutex::new((200, r#"{"screen_name":"me_1"}"#.into()))),
            }
        }

        fn respond(&self, seen: &Seen) -> Response {
            let scripted = |slot: &Arc<Mutex<(u16, String)>>| {
                let (status, body) = slot.lock().unwrap().clone();
                reply(status, body)
            };
            match (seen.method.as_str(), seen.path.as_str()) {
                ("GET", "/home") => scripted(&self.home),
                ("GET", SCRIPT_PATH) => scripted(&self.script),
                ("GET", "/1.1/account/settings.json") => scripted(&self.settings),
                ("GET", "/i/api/1.1/friends/ids.json") => reply(
                    200,
                    r#"{"ids":["1","2"],"next_cursor":55,"next_cursor_str":"55"}"#,
                ),
                ("POST", "/i/api/1.1/friendships/update.json") => reply(200, "{}"),
                _ => reply(404, "not found"),
            }
        }

        fn router(&self, log: &Log) -> Router {
            let site = self.clone();
            recording(Arc::clone(log), move |seen| site.respond(seen))
        }
    }

    fn count(log: &Log, path: &str) -> usize {
        log.lock()
            .unwrap()
            .iter()
            .filter(|s| s.path == path)
            .count()
    }

    fn assert_valid_id(seen: &Seen) {
        let transaction = fixture::transaction();
        let id = seen.headers["x-client-transaction-id"].to_str().unwrap();
        assert_eq!(
            id,
            fixture::expected_id(&transaction, &seen.method, &seen.path, id),
            "{} {}",
            seen.method,
            seen.path
        );
        let (_, time) = fixture::decode_id(&transaction, id);
        assert!(
            (fixture::relative_now() - time).abs() < 60,
            "時刻が現在に近い"
        );
    }

    #[tokio::test]
    async fn requests_use_web_session_headers_and_exact_parameters() {
        let log = Log::default();
        let server = serve(Site::new().router(&log)).await;
        let api = injected(&server.base);
        let twitter = twitter();

        assert_eq!(api.screen_name(&twitter).await.unwrap(), "me_1");
        let page = api.following_page(&twitter, "me_1", -1).await.unwrap();
        assert_eq!(page.ids, ["1", "2"]);
        assert_eq!(page.next_cursor, 55);
        api.enable_device_notifications(&twitter, "12345")
            .await
            .unwrap();

        let seen = log.lock().unwrap();
        let routes: Vec<String> = seen
            .iter()
            .map(|s| format!("{} {}", s.method, s.path))
            .collect();
        assert_eq!(
            routes,
            [
                "GET /1.1/account/settings.json",
                "GET /i/api/1.1/friends/ids.json",
                "POST /i/api/1.1/friendships/update.json",
            ]
        );
        assert_eq!(
            seen[1].query,
            "screen_name=me_1&count=5000&stringify_ids=true&cursor=-1"
        );
        assert_eq!(seen[2].body, "user_id=12345&device=true");
        assert_eq!(
            seen[2].headers["content-type"],
            "application/x-www-form-urlencoded"
        );
        for request in seen.iter() {
            let headers = &request.headers;
            assert_eq!(headers["x-csrf-token"], "CT0-SECRET");
            assert_eq!(headers["cookie"], "auth_token=AUTH-SECRET; ct0=CT0-SECRET");
            assert_eq!(headers["x-twitter-auth-type"], "OAuth2Session");
            assert_eq!(headers["user-agent"], BROWSER_USER_AGENT);
            assert!(
                headers["authorization"]
                    .to_str()
                    .unwrap()
                    .starts_with("Bearer ")
            );
            assert_valid_id(request);
        }
    }

    #[tokio::test]
    async fn production_endpoints_pin_the_path_forms() {
        let api = XApi::new();
        let pinned = |url: String| {
            let url = Url::parse(&url).unwrap();
            (
                url.scheme().to_string(),
                url.host_str().unwrap().to_string(),
                url.path().to_string(),
            )
        };
        let expect =
            |host: &str, path: &str| ("https".to_string(), host.to_string(), path.to_string());
        assert_eq!(
            pinned(api.settings_url()),
            expect("api.x.com", "/1.1/account/settings.json")
        );
        assert_eq!(
            pinned(api.friends_ids_url()),
            expect("x.com", "/i/api/1.1/friends/ids.json")
        );
        assert_eq!(
            pinned(api.friendships_update_url()),
            expect("x.com", "/i/api/1.1/friendships/update.json")
        );
        assert_eq!(
            pinned(api.endpoints.home_url.clone()),
            expect("x.com", "/home")
        );
        assert_eq!(
            pinned(format!(
                "{}{}{}",
                api.endpoints.ondemand_prefix, "abc123", ONDEMAND_URL_SUFFIX
            )),
            expect(
                "abs.twimg.com",
                "/responsive-web/client-web/ondemand.s.abc123a.js"
            )
        );
        assert_eq!(
            TWITTER_API_BASE.to_string() + "/notifications/settings/login.json",
            "https://x.com/i/api/1.1/notifications/settings/login.json"
        );
    }

    #[tokio::test]
    async fn bootstrap_is_credential_free_cached_and_shared() {
        let log = Log::default();
        let server = serve(Site::new().router(&log)).await;
        let api = bootstrapping(&server.base, Duration::from_secs(5));
        let twitter = twitter();

        let (first, second) = tokio::join!(api.screen_name(&twitter), api.screen_name(&twitter));
        assert_eq!(first.unwrap(), "me_1");
        assert_eq!(second.unwrap(), "me_1");
        api.following_page(&twitter, "me_1", -1).await.unwrap();
        api.enable_device_notifications(&twitter, "1")
            .await
            .unwrap();

        assert_eq!(count(&log, "/home"), 1);
        assert_eq!(count(&log, SCRIPT_PATH), 1);
        let seen = log.lock().unwrap();
        for request in seen.iter() {
            if request.path == "/home" || request.path == SCRIPT_PATH {
                for name in CREDENTIAL_HEADERS {
                    assert!(
                        !request.headers.contains_key(name),
                        "{} に {name} が付いている",
                        request.path
                    );
                }
                assert_eq!(request.headers["user-agent"], BROWSER_USER_AGENT);
            } else {
                assert_valid_id(request);
            }
        }
        let home = seen.iter().find(|s| s.path == "/home").unwrap();
        assert_eq!(home.headers["referer"], "https://x.com");
        assert_eq!(home.headers["x-twitter-active-user"], "yes");
        assert_eq!(seen.iter().filter(|s| s.path.contains("/1.1/")).count(), 4);
    }

    #[tokio::test]
    async fn bootstrap_failures_are_not_cached_and_never_send_credentials() {
        let log = Log::default();
        let site = Site::new();
        let server = serve(site.router(&log)).await;
        let api = bootstrapping(&server.base, Duration::from_secs(5));
        let twitter = twitter();
        let good_home = fixture::home_html();
        let long_hash_home = good_home.replace("c0ffee1", &"a".repeat(65));

        let home_cases = [
            (503, "unavailable".to_string(), XError::Status(503)),
            (403, "forbidden".into(), XError::Status(403)),
            (401, "unauthorized".into(), XError::Status(401)),
            (429, "slow down".into(), XError::Status(429)),
            (404, "gone".into(), XError::Status(404)),
            (302, String::new(), XError::Status(302)),
            (200, "<html>no hash</html>".into(), XError::Malformed),
            (200, long_hash_home, XError::Malformed),
            (
                200,
                good_home.replace("content=\"", "content=\"!"),
                XError::Malformed,
            ),
        ];
        for (status, body, expected) in home_cases {
            *site.home.lock().unwrap() = (status, body);
            let error = api.screen_name(&twitter).await.unwrap_err();
            assert_eq!(error, expected, "home {status}");
            assert!(!error.is_fatal(), "bootstrap の失敗で一括処理を止めない");
        }
        *site.home.lock().unwrap() = (200, good_home);
        assert_eq!(
            count(&log, SCRIPT_PATH),
            1,
            "壊れた鍵のケースだけが script の取得まで進む"
        );
        assert!(
            log.lock().unwrap().iter().all(|s| !s.path.contains("aaaa")),
            "検証に通らない hash は取りに行かない"
        );

        for (status, body, expected) in [
            (500, "oops", XError::Status(500)),
            (404, "gone", XError::Status(404)),
            (403, "forbidden", XError::Status(403)),
            (200, "no key indices here", XError::Malformed),
        ] {
            *site.script.lock().unwrap() = (status, body.to_string());
            let error = api.screen_name(&twitter).await.unwrap_err();
            assert_eq!(error, expected, "script {status}");
            assert!(!error.is_fatal());
        }

        assert!(
            log.lock()
                .unwrap()
                .iter()
                .all(|s| s.path == "/home" || s.path == SCRIPT_PATH),
            "bootstrap が失敗している間は API にリクエストを送らない"
        );
        let attempts = count(&log, "/home");

        *site.script.lock().unwrap() = (200, fixture::ondemand_js());
        assert_eq!(api.screen_name(&twitter).await.unwrap(), "me_1");
        assert_eq!(api.screen_name(&twitter).await.unwrap(), "me_1");
        assert_eq!(count(&log, "/home"), attempts + 1, "成功後は再取得しない");
    }

    #[tokio::test]
    async fn bootstrap_timeout_does_not_poison_the_cache() {
        let slow = Arc::new(AtomicBool::new(true));
        let app = Router::new()
            .route(
                "/home",
                get({
                    let slow = Arc::clone(&slow);
                    move || {
                        let slow = Arc::clone(&slow);
                        async move {
                            if slow.load(Ordering::SeqCst) {
                                tokio::time::sleep(Duration::from_secs(60)).await;
                            }
                            fixture::home_html()
                        }
                    }
                }),
            )
            .route(SCRIPT_PATH, get(|| async { fixture::ondemand_js() }))
            .route(
                "/1.1/account/settings.json",
                get(|| async { r#"{"screen_name":"me_1"}"# }),
            );
        let server = serve(app).await;
        let twitter = twitter();

        let api = bootstrapping(&server.base, Duration::from_millis(200));
        let started = Instant::now();
        assert_eq!(
            api.screen_name(&twitter).await.unwrap_err(),
            XError::Transport
        );
        assert!(started.elapsed() < Duration::from_secs(5));

        // 呼び出し側のタイムアウトで future が捨てられても次の呼び出しがやり直す
        let api = bootstrapping(&server.base, Duration::from_secs(30));
        assert!(
            tokio::time::timeout(Duration::from_millis(200), api.screen_name(&twitter))
                .await
                .is_err()
        );
        slow.store(false, Ordering::SeqCst);
        assert_eq!(api.screen_name(&twitter).await.unwrap(), "me_1");
    }

    #[tokio::test]
    async fn oversized_bootstrap_bodies_are_rejected() {
        let size = MAX_BOOTSTRAP_BYTES + 1;
        let app = Router::new()
            .route("/home", get(move || async move { "a".repeat(size) }))
            .route(
                "/streamed/home",
                get(|| async {
                    let chunk = Bytes::from(vec![b'a'; 1024 * 1024]);
                    let chunks = vec![Ok::<_, std::convert::Infallible>(chunk); 5];
                    Body::from_stream(futures_util::stream::iter(chunks))
                }),
            );
        let server = serve(app).await;
        let twitter = twitter();

        let api = bootstrapping(&server.base, Duration::from_secs(10));
        assert_eq!(
            api.screen_name(&twitter).await.unwrap_err(),
            XError::Malformed
        );
        let mut streamed = endpoints(&server.base);
        streamed.home_url = format!("{}/streamed/home", server.base);
        let api = XApi::build(streamed, None, Duration::from_secs(10), TRANSACTION_TTL);
        assert_eq!(
            api.screen_name(&twitter).await.unwrap_err(),
            XError::Malformed
        );
    }

    #[tokio::test]
    async fn redirects_are_never_followed_and_nothing_reaches_the_target() {
        let sink_log = Log::default();
        let sink = serve(recording(Arc::clone(&sink_log), |_| reply(200, "{}"))).await;
        let location = format!("{}/elsewhere", sink.base);

        let redirect = {
            let location = location.clone();
            move |_: &Seen| (StatusCode::FOUND, [("location", location.clone())]).into_response()
        };
        let log = Log::default();
        let server = serve(recording(Arc::clone(&log), redirect)).await;
        let twitter = twitter();

        let api = bootstrapping(&server.base, Duration::from_secs(5));
        assert_eq!(
            api.screen_name(&twitter).await.unwrap_err(),
            XError::Status(302)
        );
        let api = injected(&server.base);
        assert_eq!(
            api.screen_name(&twitter).await.unwrap_err(),
            XError::Status(302)
        );
        assert_eq!(
            api.enable_device_notifications(&twitter, "1")
                .await
                .unwrap_err(),
            XError::Status(302)
        );
        assert!(sink_log.lock().unwrap().is_empty());
    }

    /// (path, status, Location, body)
    type Route = (&'static str, u16, Option<String>, String);

    /// 台本どおりに返す。台本にない path は 404。
    fn scripted(log: &Log, routes: Vec<Route>) -> Router {
        recording(Arc::clone(log), move |seen| {
            let Some((_, status, location, body)) =
                routes.iter().find(|(path, ..)| *path == seen.path)
            else {
                return reply(404, "not found");
            };
            let mut builder = Response::builder().status(*status);
            if let Some(location) = location {
                builder = builder.header("location", location);
            }
            builder.body(Body::from(body.clone())).unwrap()
        })
    }

    #[tokio::test]
    async fn bootstrap_follows_same_origin_redirects_anonymously() {
        let log = Log::default();
        let site = vec![
            (
                "/home",
                307,
                Some("/a?redirect_after_login=%2Fhome&mode=login".to_string()),
                String::new(),
            ),
            ("/a", 302, Some("b".to_string()), String::new()),
            ("/b", 200, None, fixture::home_html()),
            (SCRIPT_PATH, 200, None, fixture::ondemand_js()),
            (
                "/1.1/account/settings.json",
                200,
                None,
                r#"{"screen_name":"me_1"}"#.to_string(),
            ),
        ];
        let server = serve(scripted(&log, site)).await;
        let api = bootstrapping(&server.base, Duration::from_secs(5));
        assert_eq!(api.screen_name(&twitter()).await.unwrap(), "me_1");
        assert_eq!(api.screen_name(&twitter()).await.unwrap(), "me_1");

        let seen = log.lock().unwrap();
        let routes: Vec<&str> = seen.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(
            routes,
            [
                "/home",
                "/a",
                "/b",
                SCRIPT_PATH,
                "/1.1/account/settings.json",
                "/1.1/account/settings.json"
            ]
        );
        assert_eq!(seen[1].query, "redirect_after_login=%2Fhome&mode=login");
        for hop in &seen[..3] {
            for name in CREDENTIAL_HEADERS {
                assert!(!hop.headers.contains_key(name), "{} に {name}", hop.path);
            }
            for (name, value) in HOME_HEADERS {
                assert_eq!(hop.headers[name], value, "{} の {name}", hop.path);
            }
            assert_eq!(hop.headers["user-agent"], BROWSER_USER_AGENT);
        }
        assert_valid_id(&seen[4]);
    }

    #[tokio::test]
    async fn bootstrap_refuses_foreign_looping_and_long_redirects() {
        let sink_log = Log::default();
        let sink = serve(recording(Arc::clone(&sink_log), |_| reply(200, "{}"))).await;
        let hijack = |path: &'static str, location: String| {
            vec![
                ("/home", 307, Some(location), fixture::home_html()),
                (path, 200, None, String::new()),
            ]
        };
        let twitter = twitter();

        // 別オリジン (別ポート・別ホスト名・別スキーム・scheme-relative) への redirect は 1 件も届かない
        let sink_port = sink.base.rsplit(':').next().unwrap().to_string();
        let foreign = [
            format!("{}/x", sink.base),
            format!("http://localhost:{sink_port}/x"),
            format!("https://127.0.0.1:{sink_port}/x"),
            "//evil.invalid/x".to_string(),
        ];
        for location in foreign {
            let log = Log::default();
            let server = serve(scripted(&log, hijack("/x", location.clone()))).await;
            let api = bootstrapping(&server.base, Duration::from_secs(5));
            assert_eq!(
                api.screen_name(&twitter).await.unwrap_err(),
                XError::Status(307),
                "{location}"
            );
            let paths: Vec<String> = log.lock().unwrap().iter().map(|s| s.path.clone()).collect();
            assert_eq!(paths, ["/home"], "{location}: 転送先に届いてはいけない");
        }
        assert!(sink_log.lock().unwrap().is_empty());

        // ループ・自己参照・回数超過
        let cases: Vec<(&str, Vec<Route>, usize)> = vec![
            (
                "loop",
                vec![
                    ("/home", 307, Some("/a".into()), String::new()),
                    ("/a", 307, Some("/home".into()), String::new()),
                ],
                2,
            ),
            (
                "self",
                vec![("/home", 308, Some("/home".into()), String::new())],
                1,
            ),
            (
                "too many hops",
                vec![
                    ("/home", 307, Some("/h1".into()), String::new()),
                    ("/h1", 307, Some("/h2".into()), String::new()),
                    ("/h2", 307, Some("/h3".into()), String::new()),
                    ("/h3", 307, Some("/h4".into()), String::new()),
                    ("/h4", 200, None, fixture::home_html()),
                ],
                4,
            ),
            ("no location", vec![("/home", 307, None, String::new())], 1),
            (
                "not a redirect we follow",
                vec![("/home", 300, Some("/a".into()), String::new())],
                1,
            ),
        ];
        for (name, routes, requests) in cases {
            let log = Log::default();
            let server = serve(scripted(&log, routes.clone())).await;
            let api = bootstrapping(&server.base, Duration::from_secs(5));
            let error = api.screen_name(&twitter).await.unwrap_err();
            assert!(
                matches!(error, XError::Status(300 | 307 | 308)),
                "{name}: {error:?}"
            );
            assert_eq!(log.lock().unwrap().len(), requests, "{name}");
        }
    }

    #[tokio::test]
    async fn script_redirect_to_another_origin_is_refused() {
        let sink_log = Log::default();
        let sink = serve(recording(Arc::clone(&sink_log), |_| reply(200, "{}"))).await;
        let log = Log::default();
        let site = vec![
            ("/home", 200, None, fixture::home_html()),
            (
                SCRIPT_PATH,
                302,
                Some(format!("{}/x", sink.base)),
                String::new(),
            ),
        ];
        let server = serve(scripted(&log, site)).await;
        let api = bootstrapping(&server.base, Duration::from_secs(5));
        assert_eq!(
            api.screen_name(&twitter()).await.unwrap_err(),
            XError::Status(302)
        );
        assert!(sink_log.lock().unwrap().is_empty());
        assert!(
            log.lock()
                .unwrap()
                .iter()
                .all(|s| s.path != "/1.1/account/settings.json")
        );
    }

    #[tokio::test]
    async fn expired_generator_is_refreshed_once_and_failures_are_not_reused() {
        let log = Log::default();
        let site = Site::new();
        let server = serve(site.router(&log)).await;
        let api = XApi::build(
            endpoints(&server.base),
            None,
            Duration::from_secs(5),
            Duration::from_millis(300),
        );
        let twitter = twitter();

        api.screen_name(&twitter).await.unwrap();
        api.screen_name(&twitter).await.unwrap();
        assert_eq!(count(&log, "/home"), 1, "期限内は再取得しない");

        tokio::time::sleep(Duration::from_millis(400)).await;
        let (a, b) = tokio::join!(api.screen_name(&twitter), api.screen_name(&twitter));
        a.unwrap();
        b.unwrap();
        assert_eq!(
            count(&log, "/home"),
            2,
            "期限切れの同時呼び出しは 1 回に集約する"
        );

        // 取り直しに失敗したら、期限切れの生成器で資格情報つきリクエストを送らない
        tokio::time::sleep(Duration::from_millis(400)).await;
        *site.home.lock().unwrap() = (503, "down".into());
        let before = count(&log, "/1.1/account/settings.json");
        assert_eq!(
            api.screen_name(&twitter).await.unwrap_err(),
            XError::Status(503)
        );
        assert_eq!(count(&log, "/1.1/account/settings.json"), before);

        *site.home.lock().unwrap() = (200, fixture::home_html());
        api.screen_name(&twitter).await.unwrap();
        assert_eq!(count(&log, "/home"), 4);
    }

    #[tokio::test]
    async fn a_rejected_request_is_not_retried_automatically() {
        let log = Log::default();
        let site = Site::new();
        *site.settings.lock().unwrap() = (404, "x".into());
        let server = serve(site.router(&log)).await;
        let api = bootstrapping(&server.base, Duration::from_secs(5));
        let twitter = twitter();

        assert_eq!(
            api.screen_name(&twitter).await.unwrap_err(),
            XError::Status(404)
        );
        assert_eq!(count(&log, "/1.1/account/settings.json"), 1);
        assert_eq!(count(&log, "/home"), 1);

        let before = count(&log, "/i/api/1.1/friendships/update.json");
        let post = Site::new();
        let post_log = Log::default();
        let router = recording(Arc::clone(&post_log), move |seen| {
            if seen.method == "POST" {
                reply(404, "x")
            } else {
                post.respond(seen)
            }
        });
        let server = serve(router).await;
        let api = bootstrapping(&server.base, Duration::from_secs(5));
        assert_eq!(
            api.enable_device_notifications(&twitter, "1")
                .await
                .unwrap_err(),
            XError::Status(404)
        );
        assert_eq!(count(&post_log, "/i/api/1.1/friendships/update.json"), 1);
        assert_eq!(before, 0);
    }

    #[tokio::test]
    async fn settings_404_is_a_plain_status_not_a_credential_verdict() {
        let site = Site::new();
        *site.settings.lock().unwrap() = (404, "SECRET-BODY".into());
        let log = Log::default();
        let server = serve(site.router(&log)).await;
        let api = injected(&server.base);

        let error = api.screen_name(&twitter()).await.unwrap_err();
        assert_eq!(error, XError::Status(404));
        assert!(!error.is_fatal());
        assert!(!error.to_string().contains("SECRET"));
    }

    #[tokio::test]
    async fn statuses_map_to_safe_categories_without_bodies() {
        let app = Router::new()
            .route(
                "/1.1/account/settings.json",
                get(|| async { (StatusCode::UNAUTHORIZED, "SECRET-BODY") }),
            )
            .route(
                "/i/api/1.1/friends/ids.json",
                get(|| async { (StatusCode::TOO_MANY_REQUESTS, "SECRET-BODY") }),
            )
            .route(
                "/i/api/1.1/friendships/update.json",
                axum::routing::post(|| async { (StatusCode::FORBIDDEN, "SECRET-BODY") }),
            );
        let server = serve(app).await;
        let api = injected(&server.base);
        let twitter = twitter();
        assert_eq!(
            api.screen_name(&twitter).await.unwrap_err(),
            XError::Unauthorized
        );
        assert_eq!(
            api.following_page(&twitter, "me", -1).await.unwrap_err(),
            XError::RateLimited
        );
        let err = api
            .enable_device_notifications(&twitter, "1")
            .await
            .unwrap_err();
        assert_eq!(err, XError::Forbidden);
        assert!(!err.to_string().contains("SECRET"));
        assert!(err.is_fatal());
        assert!(!XError::Status(500).is_fatal());
    }

    #[tokio::test]
    async fn malformed_payloads_and_ids_are_rejected_not_defaulted() {
        let app = Router::new()
            .route(
                "/1.1/account/settings.json",
                get(|| async { r#"{"screen_name":"bad name!"}"# }),
            )
            .route(
                "/i/api/1.1/friends/ids.json",
                get(|| async { r#"{"ids":["1","2x"],"next_cursor_str":"0"}"# }),
            )
            .route(
                "/i/api/1.1/friendships/update.json",
                axum::routing::post(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
            );
        let server = serve(app).await;
        let api = injected(&server.base);
        let twitter = twitter();
        assert_eq!(
            api.screen_name(&twitter).await.unwrap_err(),
            XError::Malformed
        );
        assert_eq!(
            api.following_page(&twitter, "me", -1).await.unwrap_err(),
            XError::Malformed
        );
        assert_eq!(
            api.following_page(&twitter, "me&x=1", -1)
                .await
                .unwrap_err(),
            XError::Malformed
        );
        assert_eq!(
            api.enable_device_notifications(&twitter, "1&device=false")
                .await
                .unwrap_err(),
            XError::Malformed
        );
        assert_eq!(
            api.enable_device_notifications(&twitter, "1")
                .await
                .unwrap_err(),
            XError::Status(500)
        );
    }

    #[tokio::test]
    async fn missing_ids_or_cursor_is_malformed() {
        let app = Router::new()
            .route(
                "/i/api/1.1/friends/ids.json",
                get(|| async { r#"{"next_cursor_str":"0"}"# }),
            )
            .route(
                "/1.1/account/settings.json",
                get(|| async { r#"{"ids":[],"next_cursor":0}"# }),
            );
        let server = serve(app).await;
        let api = injected(&server.base);
        let twitter = twitter();
        assert_eq!(
            api.following_page(&twitter, "me", -1).await.unwrap_err(),
            XError::Malformed
        );
        assert_eq!(
            api.screen_name(&twitter).await.unwrap_err(),
            XError::Malformed
        );
    }

    #[test]
    fn id_and_screen_name_validation() {
        assert!(is_valid_user_id("1234567890123456789"));
        assert!(!is_valid_user_id(""));
        assert!(!is_valid_user_id("12 3"));
        assert!(!is_valid_user_id(&"1".repeat(21)));
        assert!(is_valid_screen_name("Abc_123"));
        assert!(!is_valid_screen_name("a-b"));
        assert!(!is_valid_screen_name(&"a".repeat(16)));
    }
}
