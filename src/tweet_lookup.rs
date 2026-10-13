//! X の公開埋め込み用 API で、通知のツイートがリプライか引用RT かを調べる。
//!
//! push の payload だけでは通常の投稿と見分けられないので、ツイート ID で照会する。
//! 照会は付加情報にすぎない。失敗の理由を問わず `None` を返し、呼び出し側は通常の投稿として扱う。
//! 応答の本文と URL は機密になりうるため、ログにはステータス程度しか出さない。

use reqwest::header::CONTENT_TYPE;
use reqwest::{Client, StatusCode, Url, redirect};
use serde_json::Value;
use std::time::Duration;

/// 2026-10 時点で `token` は検証されておらず、どんな値でも通る。
/// 検証されるようになれば照会は失敗し、通知は通常の投稿として扱われる。
const TOKEN: &str = "0";
/// 接続から本文の読み込み完了までの上限。
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BODY_BYTES: usize = 256 * 1024;
const MAX_ID_DIGITS: usize = 20;

/// 照会で分かったツイートの形。リプライを優先し、引用RT は引用元の投稿者が同じかで分ける。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TweetShape {
    Reply,
    /// 他のアカウントの投稿の引用RT。投稿者を比べられなかったときもこちらにする。
    Quote,
    /// 自分自身の投稿の引用RT。
    SelfQuote,
    Plain,
}

#[derive(Debug, Clone)]
pub struct TweetLookup {
    client: Client,
    endpoint: Url,
    timeout: Duration,
}

impl TweetLookup {
    /// X の公開埋め込み用 API に照会するクライアント。
    pub fn production() -> crate::error::Result<Self> {
        Self::with_endpoint("https://cdn.syndication.twimg.com/tweet-result")
    }

    /// `endpoint` に照会するクライアント。テストではモックサーバーの URL を渡す。
    pub fn with_endpoint(endpoint: &str) -> crate::error::Result<Self> {
        let endpoint = Url::parse(endpoint).map_err(|_| {
            crate::error::AngelicAngelError::Config("invalid tweet lookup endpoint".into())
        })?;
        // User-Agent が無いリクエストを、この API は 400 で拒否する。
        let mut builder = Client::builder()
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .redirect(redirect::Policy::none());
        // ローカル宛ての平文 HTTP は、環境の proxy を経由させず直接接続する。
        if endpoint.scheme() == "http" {
            builder = builder.no_proxy();
        }
        Ok(Self {
            client: builder.build()?,
            endpoint,
            timeout: LOOKUP_TIMEOUT,
        })
    }

    /// ツイート `id` の形を返す。ID が不正、通信や応答が期待どおりでない、削除・非公開などは `None`。
    pub async fn shape(&self, id: &str) -> Option<TweetShape> {
        let url = self.request_url(id)?;
        match tokio::time::timeout(self.timeout, self.fetch(url)).await {
            Ok(shape) => shape,
            Err(_) => {
                tracing::debug!("tweet lookup timed out");
                None
            },
        }
    }

    fn request_url(&self, id: &str) -> Option<Url> {
        let valid =
            (1..=MAX_ID_DIGITS).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit());
        if !valid {
            return None;
        }
        let mut url = self.endpoint.clone();
        url.query_pairs_mut()
            .clear()
            .append_pair("id", id)
            .append_pair("token", TOKEN);
        Some(url)
    }

    async fn fetch(&self, url: Url) -> Option<TweetShape> {
        let mut response = match self.client.get(url).send().await {
            Ok(response) => response,
            Err(_) => {
                tracing::debug!("tweet lookup request failed");
                return None;
            },
        };
        let status = response.status();
        if status != StatusCode::OK {
            tracing::debug!(status = %status, "tweet lookup returned an unexpected status");
            return None;
        }
        if !is_json(&response) {
            tracing::debug!("tweet lookup returned a non-JSON content type");
            return None;
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BODY_BYTES as u64)
        {
            tracing::debug!("tweet lookup response is too large");
            return None;
        }
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > MAX_BODY_BYTES {
                        tracing::debug!("tweet lookup response is too large");
                        return None;
                    }
                    body.extend_from_slice(&chunk);
                },
                Ok(None) => break,
                Err(_) => {
                    tracing::debug!("tweet lookup response could not be read");
                    return None;
                },
            }
        }
        shape_of(&serde_json::from_slice(&body).ok()?)
    }
}

fn is_json(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
}

/// 通常のツイートの応答から形を読む。削除・非公開などの `TweetTombstone` は `None`。
fn shape_of(value: &Value) -> Option<TweetShape> {
    if value.get("__typename")?.as_str()? != "Tweet" {
        return None;
    }
    let is_reply = value
        .get("in_reply_to_status_id_str")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty());
    let quoted = value
        .get("quoted_tweet")
        .filter(|quoted| quoted.is_object());
    Some(match (is_reply, quoted) {
        (true, _) => TweetShape::Reply,
        (false, Some(quoted)) if same_author(value, quoted) => TweetShape::SelfQuote,
        (false, Some(_)) => TweetShape::Quote,
        (false, None) => TweetShape::Plain,
    })
}

/// 二つのツイートの投稿者が同じか。`user.id_str` を優先し、どちらかに無いときだけ
/// `user.screen_name` を大文字小文字を無視して比べる。判別できなければ別人として扱う。
fn same_author(a: &Value, b: &Value) -> bool {
    fn user_field<'a>(tweet: &'a Value, name: &str) -> Option<&'a str> {
        tweet
            .get("user")?
            .get(name)?
            .as_str()
            .filter(|value| !value.is_empty())
    }
    match (user_field(a, "id_str"), user_field(b, "id_str")) {
        (Some(a), Some(b)) => a == b,
        _ => match (user_field(a, "screen_name"), user_field(b, "screen_name")) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        },
    }
}

#[cfg(test)]
pub mod mock {
    //! 本物の API に通信しないための、ローカルのモック HTTP サーバー。

    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    /// 接続のたびに同じ応答を返し、受け取ったリクエストの先頭部（リクエスト行とヘッダー）を記録する。
    /// 破棄すると停止する。
    pub struct MockApi {
        pub url: String,
        heads: Arc<Mutex<Vec<String>>>,
        task: JoinHandle<()>,
    }

    impl MockApi {
        pub async fn start(response: Vec<u8>) -> Self {
            Self::spawn(response, false).await
        }

        /// 応答を送ったあとも接続を閉じない。
        pub async fn start_stalled(response: Vec<u8>) -> Self {
            Self::spawn(response, true).await
        }

        async fn spawn(response: Vec<u8>, stall: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/tweet-result", listener.local_addr().unwrap());
            let heads = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::clone(&heads);
            let task = tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let response = response.clone();
                    let seen = Arc::clone(&seen);
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        let mut chunk = [0u8; 4096];
                        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            match stream.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        }
                        let end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(0);
                        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                        seen.lock().unwrap().push(head);
                        let _ = stream.write_all(&response).await;
                        if stall {
                            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                        }
                        let _ = stream.shutdown().await;
                    });
                }
            });
            Self { url, heads, task }
        }

        /// 受け取ったリクエスト行（例: `GET /tweet-result?id=1&token=0 HTTP/1.1`）。
        pub fn requests(&self) -> Vec<String> {
            let heads = self.heads.lock().unwrap();
            heads
                .iter()
                .map(|head| head.lines().next().unwrap_or_default().to_string())
                .collect()
        }

        /// 受け取ったリクエストごとの、名前を小文字にしたヘッダー（名前と値の組）。
        pub fn headers(&self) -> Vec<Vec<(String, String)>> {
            let heads = self.heads.lock().unwrap();
            heads
                .iter()
                .map(|head| {
                    head.lines()
                        .skip(1)
                        .filter_map(|line| line.split_once(':'))
                        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
                        .collect()
                })
                .collect()
        }
    }

    impl Drop for MockApi {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// `Content-Length` 付きの応答。
    pub fn http_response(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    pub fn json_response(body: &str) -> Vec<u8> {
        http_response("200 OK", "application/json; charset=utf-8", body.as_bytes())
    }

    /// 接続を拒否されるローカルの URL。ポート 1 には何も待ち受けていない。
    pub const REFUSED_ENDPOINT: &str = "http://127.0.0.1:1/tweet-result";
}

#[cfg(test)]
mod tests {
    use super::mock::{MockApi, REFUSED_ENDPOINT, http_response, json_response};
    use super::*;
    use serde_json::json;

    const ID: &str = "2000000000000000001";

    fn lookup(endpoint: &str) -> TweetLookup {
        TweetLookup::with_endpoint(endpoint).unwrap()
    }

    async fn shape_from(body: &Value) -> Option<TweetShape> {
        let api = MockApi::start(json_response(&body.to_string())).await;
        lookup(&api.url).shape(ID).await
    }

    #[tokio::test]
    async fn reply_quote_and_plain_are_told_apart() {
        let plain = json!({ "__typename": "Tweet", "id_str": ID, "text": "hi" });
        assert_eq!(shape_from(&plain).await, Some(TweetShape::Plain));

        let quote = json!({ "__typename": "Tweet", "quoted_tweet": { "id_str": "1" } });
        assert_eq!(shape_from(&quote).await, Some(TweetShape::Quote));

        let reply = json!({
            "__typename": "Tweet",
            "in_reply_to_status_id_str": "1",
            "in_reply_to_screen_name": "someone",
        });
        assert_eq!(shape_from(&reply).await, Some(TweetShape::Reply));
    }

    #[tokio::test]
    async fn reply_wins_over_quote() {
        let both = json!({
            "__typename": "Tweet",
            "in_reply_to_status_id_str": "1",
            "quoted_tweet": { "id_str": "2" },
        });
        assert_eq!(shape_from(&both).await, Some(TweetShape::Reply));
    }

    fn quote_by(quoting: Value, quoted: Value) -> Value {
        json!({
            "__typename": "Tweet",
            "user": quoting,
            "quoted_tweet": { "id_str": "1", "user": quoted },
        })
    }

    #[tokio::test]
    async fn a_quote_of_the_same_author_is_a_self_quote() {
        // id_str が一致すれば、screen_name が違っていても（改名など）自己引用。
        let same_id = quote_by(
            json!({ "id_str": "42", "screen_name": "new_name" }),
            json!({ "id_str": "42", "screen_name": "old_name" }),
        );
        assert_eq!(shape_from(&same_id).await, Some(TweetShape::SelfQuote));

        // id_str が両方にあって食い違えば、screen_name が同じでも他アカウント。
        let other_id = quote_by(
            json!({ "id_str": "42", "screen_name": "same" }),
            json!({ "id_str": "43", "screen_name": "same" }),
        );
        assert_eq!(shape_from(&other_id).await, Some(TweetShape::Quote));

        let other = quote_by(
            json!({ "id_str": "42", "screen_name": "alice" }),
            json!({ "id_str": "43", "screen_name": "bob" }),
        );
        assert_eq!(shape_from(&other).await, Some(TweetShape::Quote));
    }

    #[tokio::test]
    async fn the_screen_name_decides_only_when_an_id_is_missing() {
        let cases = [
            (
                "id missing on the quoting side",
                json!({ "screen_name": "Alice_JP" }),
                json!({ "id_str": "42", "screen_name": "alice_jp" }),
                TweetShape::SelfQuote,
            ),
            (
                "id missing on the quoted side",
                json!({ "id_str": "42", "screen_name": "alice_jp" }),
                json!({ "screen_name": "ALICE_JP" }),
                TweetShape::SelfQuote,
            ),
            (
                "ids missing on both sides",
                json!({ "screen_name": "alice_jp" }),
                json!({ "screen_name": "Alice_Jp" }),
                TweetShape::SelfQuote,
            ),
            (
                "empty or non-string ids count as missing",
                json!({ "id_str": "", "screen_name": "alice" }),
                json!({ "id_str": 42, "screen_name": "alice" }),
                TweetShape::SelfQuote,
            ),
            (
                "different screen names",
                json!({ "screen_name": "alice" }),
                json!({ "id_str": "42", "screen_name": "bob" }),
                TweetShape::Quote,
            ),
            (
                "an id on one side and a name on the other cannot be compared",
                json!({ "id_str": "42" }),
                json!({ "screen_name": "alice" }),
                TweetShape::Quote,
            ),
            (
                "nothing to compare",
                json!({}),
                json!({}),
                TweetShape::Quote,
            ),
            (
                "empty names count as missing",
                json!({ "screen_name": "" }),
                json!({ "screen_name": "" }),
                TweetShape::Quote,
            ),
            (
                "non-string names count as missing",
                json!({ "screen_name": 1 }),
                json!({ "screen_name": 1 }),
                TweetShape::Quote,
            ),
        ];
        for (name, quoting, quoted, expected) in cases {
            let body = quote_by(quoting, quoted);
            assert_eq!(shape_from(&body).await, Some(expected), "{name}");
        }
    }

    #[tokio::test]
    async fn a_quote_without_user_data_is_another_accounts_quote() {
        let no_user_anywhere = json!({ "__typename": "Tweet", "quoted_tweet": {} });
        assert_eq!(shape_from(&no_user_anywhere).await, Some(TweetShape::Quote));

        let only_quoting = json!({
            "__typename": "Tweet",
            "user": { "id_str": "42", "screen_name": "alice" },
            "quoted_tweet": { "id_str": "1" },
        });
        assert_eq!(shape_from(&only_quoting).await, Some(TweetShape::Quote));

        let user_not_an_object = json!({
            "__typename": "Tweet",
            "user": "alice",
            "quoted_tweet": { "user": "alice" },
        });
        assert_eq!(
            shape_from(&user_not_an_object).await,
            Some(TweetShape::Quote)
        );
    }

    #[tokio::test]
    async fn a_reply_wins_over_a_self_quote_and_a_plain_tweet_stays_plain() {
        let same = json!({ "id_str": "42", "screen_name": "alice" });
        let reply = json!({
            "__typename": "Tweet",
            "in_reply_to_status_id_str": "1",
            "user": same,
            "quoted_tweet": { "user": same },
        });
        assert_eq!(shape_from(&reply).await, Some(TweetShape::Reply));

        let plain = json!({ "__typename": "Tweet", "user": same });
        assert_eq!(shape_from(&plain).await, Some(TweetShape::Plain));
    }

    #[tokio::test]
    async fn empty_or_malformed_markers_do_not_count() {
        for body in [
            json!({ "__typename": "Tweet", "in_reply_to_status_id_str": "" }),
            json!({ "__typename": "Tweet", "in_reply_to_status_id_str": null }),
            json!({ "__typename": "Tweet", "in_reply_to_status_id_str": 1 }),
            json!({ "__typename": "Tweet", "quoted_tweet": null }),
            json!({ "__typename": "Tweet", "quoted_tweet": "1" }),
            json!({ "__typename": "Tweet", "quoted_tweet": [] }),
        ] {
            assert_eq!(shape_from(&body).await, Some(TweetShape::Plain), "{body}");
        }
    }

    #[tokio::test]
    async fn anything_but_a_tweet_is_unknown() {
        for body in [
            json!({ "__typename": "TweetTombstone", "quoted_tweet": {} }),
            json!({ "__typename": 1 }),
            json!({ "quoted_tweet": {} }),
            json!([]),
            json!(null),
        ] {
            assert_eq!(shape_from(&body).await, None, "{body}");
        }
    }

    #[tokio::test]
    async fn unusable_responses_are_unknown() {
        let tweet = json!({ "__typename": "Tweet", "quoted_tweet": {} }).to_string();
        let cases = [
            (
                "html with 200",
                http_response("200 OK", "text/html", b"<html>not found</html>"),
            ),
            (
                "json body under a html type",
                http_response("200 OK", "text/html", tweet.as_bytes()),
            ),
            (
                "no content type",
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{tweet}",
                    tweet.len()
                )
                .into_bytes(),
            ),
            (
                "404",
                http_response("404 Not Found", "application/json", tweet.as_bytes()),
            ),
            (
                "500",
                http_response(
                    "500 Internal Server Error",
                    "application/json",
                    tweet.as_bytes(),
                ),
            ),
            (
                "202",
                http_response("202 Accepted", "application/json", tweet.as_bytes()),
            ),
            (
                "invalid json",
                http_response("200 OK", "application/json", b"{\"__typename\":"),
            ),
            (
                "empty body",
                http_response("200 OK", "application/json", b""),
            ),
        ];
        for (name, response) in cases {
            let api = MockApi::start(response).await;
            assert_eq!(lookup(&api.url).shape(ID).await, None, "{name}");
            assert_eq!(api.requests().len(), 1, "{name}");
        }
    }

    fn padded_tweet(total_bytes: usize) -> Vec<u8> {
        let prefix = br#"{"__typename":"Tweet","quoted_tweet":{},"pad":""#;
        let suffix = br#""}"#;
        let mut body = prefix.to_vec();
        body.resize(total_bytes - suffix.len(), b'x');
        body.extend_from_slice(suffix);
        assert_eq!(body.len(), total_bytes);
        body
    }

    #[tokio::test]
    async fn oversized_bodies_are_unknown_with_or_without_a_length() {
        let at_limit = padded_tweet(MAX_BODY_BYTES);
        let over = padded_tweet(MAX_BODY_BYTES + 1);

        let api = MockApi::start(http_response("200 OK", "application/json", &at_limit)).await;
        assert_eq!(lookup(&api.url).shape(ID).await, Some(TweetShape::Quote));

        let api = MockApi::start(http_response("200 OK", "application/json", &over)).await;
        assert_eq!(lookup(&api.url).shape(ID).await, None, "declared length");

        // Content-Length が無い応答は、読み込み中に上限を超えた時点で打ち切る。
        let mut streamed =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n"
                .to_vec();
        streamed.extend_from_slice(&over);
        let api = MockApi::start(streamed.clone()).await;
        assert_eq!(lookup(&api.url).shape(ID).await, None, "streamed");

        let mut streamed_at_limit = streamed[..streamed.len() - over.len()].to_vec();
        streamed_at_limit.extend_from_slice(&at_limit);
        let api = MockApi::start(streamed_at_limit).await;
        assert_eq!(lookup(&api.url).shape(ID).await, Some(TweetShape::Quote));
    }

    #[tokio::test]
    async fn the_request_carries_the_id_and_a_fixed_token() {
        let api = MockApi::start(json_response(r#"{"__typename":"Tweet"}"#)).await;
        assert_eq!(lookup(&api.url).shape(ID).await, Some(TweetShape::Plain));
        assert_eq!(
            api.requests(),
            [format!("GET /tweet-result?id={ID}&token={TOKEN} HTTP/1.1")]
        );

        // 送信先にもともと付いていた query は引き継がない。
        let api = MockApi::start(json_response(r#"{"__typename":"Tweet"}"#)).await;
        let url = format!("{}?id=1&lang=ja", api.url);
        assert_eq!(lookup(&url).shape("42").await, Some(TweetShape::Plain));
        assert_eq!(api.requests(), ["GET /tweet-result?id=42&token=0 HTTP/1.1"]);
    }

    #[tokio::test]
    async fn the_request_names_this_client_in_user_agent() {
        // この API は User-Agent が無いリクエストを 400 で拒否する。
        let api = MockApi::start(json_response(r#"{"__typename":"Tweet"}"#)).await;
        assert_eq!(lookup(&api.url).shape(ID).await, Some(TweetShape::Plain));

        let headers = api.headers();
        assert_eq!(headers.len(), 1);
        let agents: Vec<&str> = headers[0]
            .iter()
            .filter(|(name, _)| name == "user-agent")
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(agents.len(), 1, "exactly one User-Agent header");
        assert!(
            agents[0].starts_with("angelic-angel/") && agents[0].len() > "angelic-angel/".len(),
            "{}",
            agents[0]
        );
    }

    #[tokio::test]
    async fn invalid_ids_are_rejected_before_any_request() {
        let api = MockApi::start(json_response(r#"{"__typename":"Tweet"}"#)).await;
        let client = lookup(&api.url);
        let too_long = "1".repeat(MAX_ID_DIGITS + 1);
        for id in [
            "",
            "abc",
            "12a",
            "-1",
            "1 2",
            "1&token=x",
            "1/../2",
            "１２３",
            too_long.as_str(),
        ] {
            assert_eq!(client.shape(id).await, None, "{id:?}");
        }
        assert!(api.requests().is_empty());

        let longest = "9".repeat(MAX_ID_DIGITS);
        assert_eq!(client.shape(&longest).await, Some(TweetShape::Plain));
        assert_eq!(api.requests().len(), 1);
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        let target = MockApi::start(json_response(r#"{"__typename":"Tweet"}"#)).await;
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            target.url
        );
        let api = MockApi::start(redirect.into_bytes()).await;

        assert_eq!(lookup(&api.url).shape(ID).await, None);
        assert_eq!(api.requests().len(), 1);
        assert!(target.requests().is_empty());
    }

    #[tokio::test]
    async fn connection_failure_is_unknown() {
        assert_eq!(lookup(REFUSED_ENDPOINT).shape(ID).await, None);
    }

    #[tokio::test]
    async fn a_stalled_body_times_out() {
        // 本文を 1000 バイトと宣言しながら、途中で止める。
        let stalled = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{\"__typename\"".to_vec();
        let api = MockApi::start_stalled(stalled).await;
        let mut client = lookup(&api.url);
        client.timeout = Duration::from_millis(300);

        let started = std::time::Instant::now();
        assert_eq!(client.shape(ID).await, None);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn a_malformed_endpoint_is_rejected_at_construction() {
        assert!(TweetLookup::with_endpoint("not a url").is_err());
        assert!(TweetLookup::production().is_ok());
    }
}
