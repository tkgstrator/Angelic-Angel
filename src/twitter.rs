use crate::config::TwitterConfig;
use crate::error::{AngelicAngelError, Result};
use crate::machine_types::{FollowResult, LikeResult, PostInfo, UserProfile, UserRelation};
use crate::push::PushSubscription;
use crate::transaction::{
    Transaction, entry_script_urls, is_signer_url, module_import_urls, module_priority,
    ondemand_file_hash,
};
use crate::x_metadata::{ActionMetadata, ReadOperation};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::header::{CONTENT_TYPE, HeaderValue, LOCATION};
use reqwest::{Client, Request, RequestBuilder, Response, StatusCode, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fmt;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const TWITTER_API_BASE: &str = "https://x.com/i/api/1.1";
const TWITTER_SETTINGS_BASE: &str = "https://api.x.com/1.1";
const TWITTER_HOME_URL: &str = "https://x.com/";
const TWITTER_HOME_FALLBACK_URL: &str = "https://x.com/home";
const ASSET_BASE: &str = "https://abs.twimg.com/";
const ONDEMAND_URL_PREFIX: &str = "https://abs.twimg.com/responsive-web/client-web/ondemand.s.";
const ONDEMAND_URL_SUFFIX: &str = "a.js";
const TRANSACTION_HEADER: &str = "x-client-transaction-id";
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36";
/// 呼び出し側の外側タイムアウト (30 秒) より十分短くし、初期化が毎回途中で切られないようにする。
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BOOTSTRAP_BYTES: usize = 4 * 1024 * 1024;
/// 生成器の有効期間。X 側の鍵が入れ替わっても再起動なしで追従するための上限で、期限後の最初の呼び出しが取り直す。
const TRANSACTION_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_ONDEMAND_HASH_LEN: usize = 64;
/// 匿名 bootstrap のホーム取得だけが辿る同一オリジン内の redirect 回数。未ログインの /home は 1 回転送される。
const MAX_BOOTSTRAP_REDIRECTS: usize = 3;
/// 署名モジュール探索の上限 (上流 discovery.ts と同じ)。
const MAX_SIGNER_REQUESTS: usize = 12;
const MAX_SIGNER_DEPTH: usize = 3;
/// 上流 discovery.ts が匿名取得に付けるヘッダ (User-Agent は client 既定)。
const BOOTSTRAP_HEADERS: [(&str, &str); 1] = [("accept-language", "en-US,en;q=0.9")];
const AUTHORIZATION_BEARER: &str = "Bearer AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA";
const DEVICE_ID: &str = "Mac/Firefox";
const FRIENDS_PAGE_SIZE: &str = "5000";
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_USER_ID_LEN: usize = 20;
const MAX_SCREEN_NAME_LEN: usize = 15;
/// Upper bound for one lookup or action request, kept below the caller's own 30 second limit.
const ACTION_TIMEOUT: Duration = Duration::from_secs(12);
/// A catalogue that resolved only some operations is read again soon instead of after hours.
const PARTIAL_ACTIONS_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_POST_TEXT_CHARS: usize = 25_000;
const MAX_PROFILE_TEXT_BYTES: usize = 4096;
/// X's error code for a post the account has already favorited.
const ALREADY_FAVORITED_CODE: u64 = 139;
/// Flags the web client sends with `friendships/create`; the target `user_id` is appended.
const FRIENDSHIPS_CREATE_FORM: &str = "include_profile_interstitial_type=1&include_blocking=1&include_blocked_by=1&include_followed_by=1&include_want_retweets=1&include_mute_edge=1&include_can_dm=1&include_can_media_tag=1&include_ext_has_nft_avatar=1&include_ext_is_blue_verified=1&include_ext_verified_type=1&include_ext_profile_image_shape=1&skip_status=1";

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
    /// A write was sent but its outcome could not be confirmed. It may or may not have taken
    /// effect, so it must not be sent again automatically.
    Uncertain,
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
            Self::Uncertain => f.write_str("X may have applied the request. Result unknown."),
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

type XResult<T> = std::result::Result<T, XError>;

#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct Identity {
    pub user_id: String,
    pub screen_name: String,
    pub name: String,
    pub avatar_url: Option<String>,
}

fn parse_identity(body: &[u8]) -> std::result::Result<Identity, XError> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| XError::Malformed)?;
    if value.get("errors").is_some() {
        return Err(XError::Malformed);
    }
    let user_id = value
        .get("id_str")
        .and_then(serde_json::Value::as_str)
        .filter(|v| is_valid_user_id(v))
        .ok_or(XError::Malformed)?;
    let screen_name = value
        .get("screen_name")
        .and_then(serde_json::Value::as_str)
        .filter(|v| is_valid_screen_name(v))
        .ok_or(XError::Malformed)?;
    let name = value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|v| v.len() <= 512)
        .ok_or(XError::Malformed)?;
    let avatar_url = value
        .get("profile_image_url_https")
        .and_then(serde_json::Value::as_str)
        .filter(|raw| raw.len() <= 2048)
        .and_then(|raw| Url::parse(raw).ok())
        .filter(|url| {
            url.scheme() == "https"
                && url.host_str() == Some("pbs.twimg.com")
                && url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none()
                && url.path().starts_with("/profile_images/")
                && url.query().is_none()
                && url.fragment().is_none()
        })
        .map(|url| url.to_string());
    Ok(Identity {
        user_id: user_id.into(),
        screen_name: screen_name.into(),
        name: name.into(),
        avatar_url,
    })
}

fn parse_post(body: &[u8], expected_user: &str) -> std::result::Result<String, XError> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| XError::Malformed)?;
    if value.get("errors").is_some() {
        return Err(XError::Malformed);
    }
    let result = value
        .pointer("/data/create_tweet/tweet_results/result")
        .ok_or(XError::Malformed)?;
    let id = result
        .get("rest_id")
        .and_then(serde_json::Value::as_str)
        .filter(|v| is_valid_user_id(v))
        .ok_or(XError::Malformed)?;
    let author = result
        .pointer("/core/user_results/result/rest_id")
        .and_then(serde_json::Value::as_str)
        .ok_or(XError::Malformed)?;
    if author != expected_user {
        return Err(XError::Malformed);
    }
    Ok(id.to_string())
}

// ---- lookup and action responses ------------------------------------------
//
// Every parser below keeps only what X stated explicitly. A field that is missing, `null`,
// unreadable, or contradicted by a second source is unknown (`None`), never `false`, `0` or a
// guess. Bodies are never logged and never copied into an error.

fn parse_json(body: &[u8]) -> XResult<Value> {
    serde_json::from_slice(body).map_err(|_| XError::Malformed)
}

/// The value at a JSON pointer, unless it is missing or `null`.
fn json_at<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    value.pointer(path).filter(|found| !found.is_null())
}

fn array_at<'a>(value: &'a Value, path: &str) -> Option<&'a Vec<Value>> {
    json_at(value, path).and_then(Value::as_array)
}

fn text_at<'a>(value: &'a Value, path: &str) -> Option<&'a str> {
    json_at(value, path).and_then(Value::as_str)
}

fn flag_at(value: &Value, path: &str) -> Option<bool> {
    json_at(value, path).and_then(Value::as_bool)
}

fn count_at(value: &Value, path: &str) -> Option<u64> {
    json_at(value, path).and_then(Value::as_u64)
}

fn date_at(value: &Value, path: &str) -> Option<String> {
    text_at(value, path).and_then(parse_x_date)
}

/// A well-formed ID at `path`. Anything else, including a non-string, is unknown.
fn valid_id_at<'a>(value: &'a Value, path: &str) -> Option<&'a str> {
    text_at(value, path).filter(|id| is_valid_user_id(id))
}

fn id_at(value: &Value, path: &str) -> Option<String> {
    valid_id_at(value, path).map(str::to_owned)
}

/// An ID that may be absent but, when X states it, must be well formed.
fn optional_id(value: &Value, path: &str) -> XResult<Option<String>> {
    if json_at(value, path).is_none() {
        return Ok(None);
    }
    id_at(value, path).map(Some).ok_or(XError::Malformed)
}

/// Two readings of one fact (for example the current and the legacy field). When both exist and
/// disagree, the fact is unknown.
fn agreed<T: PartialEq>(first: Option<T>, second: Option<T>) -> Option<T> {
    match (first, second) {
        (Some(a), Some(b)) if a != b => None,
        (Some(a), _) => Some(a),
        (None, other) => other,
    }
}

/// Whether the body reports GraphQL errors. An empty list does not count.
fn has_errors(root: &Value) -> bool {
    match json_at(root, "/errors") {
        Some(errors) => errors.as_array().is_none_or(|list| !list.is_empty()),
        None => false,
    }
}

/// "Not there" is believed only when X reported no errors alongside it.
fn absent<T>(root: &Value) -> XResult<Option<T>> {
    if has_errors(root) {
        return Err(XError::Malformed);
    }
    Ok(None)
}

fn month_number(name: &str) -> Option<u32> {
    match name {
        "Jan" => Some(1),
        "Feb" => Some(2),
        "Mar" => Some(3),
        "Apr" => Some(4),
        "May" => Some(5),
        "Jun" => Some(6),
        "Jul" => Some(7),
        "Aug" => Some(8),
        "Sep" => Some(9),
        "Oct" => Some(10),
        "Nov" => Some(11),
        "Dec" => Some(12),
        _ => None,
    }
}

fn days_in_month(year: u32, month: u32) -> u32 {
    let century = year.is_multiple_of(100);
    let leap = year.is_multiple_of(4) && (!century || year.is_multiple_of(400));
    match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn is_weekday(text: &str) -> bool {
    let names = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    names.contains(&text)
}

/// A fixed-width unsigned decimal. Signs, spaces and other characters are not numbers here.
fn digits(text: &str, width: usize) -> Option<u32> {
    if text.len() != width || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// X's `created_at` (`Wed Oct 10 20:19:24 +0000 2018`) as UTC ISO 8601. Anything that is not
/// exactly that shape, or not a real date, is unknown.
fn parse_x_date(text: &str) -> Option<String> {
    let parts: Vec<&str> = text.split(' ').collect();
    let [weekday, month, day, time, zone, year] = parts.as_slice() else {
        return None;
    };
    if *zone != "+0000" || !is_weekday(weekday) {
        return None;
    }
    let clock: Vec<&str> = time.split(':').collect();
    let [hour, minute, second] = clock.as_slice() else {
        return None;
    };
    let hour = digits(hour, 2)?;
    let minute = digits(minute, 2)?;
    let second = digits(second, 2)?;
    let day = digits(day, 2)?;
    let year = digits(year, 4)?;
    let month = month_number(month)?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    if day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let date = format!("{year:04}-{month:02}-{day:02}");
    let clock = format!("{hour:02}:{minute:02}:{second:02}");
    Some(format!("{date}T{clock}Z"))
}

/// X escapes `& < > " '` in post text. `&amp;` is decoded last so `&amp;lt;` stays `&lt;`.
fn decode_entities(text: &str) -> String {
    let text = text.replace("&lt;", "<").replace("&gt;", ">");
    let text = text.replace("&quot;", "\"").replace("&#39;", "'");
    text.replace("&amp;", "&")
}

/// A tweet found in a timeline: its object, its author object, and the IDs both state.
struct Tweet<'a> {
    node: &'a Value,
    user: &'a Value,
    id: &'a str,
    author_id: &'a str,
}

impl Tweet<'_> {
    /// The author's ID, only when the tweet's own `user_id_str` names the same account as its
    /// author object.
    fn confirmed_author(&self) -> Option<&str> {
        let stated = text_at(self.node, "/legacy/user_id_str");
        (stated == Some(self.author_id)).then_some(self.author_id)
    }
}

/// The `Tweet` object of a timeline result. Tombstones and other unavailable results have none.
fn tweet_node(result: &Value) -> Option<&Value> {
    match text_at(result, "/__typename") {
        Some("Tweet") => Some(result),
        Some("TweetWithVisibilityResults") => json_at(result, "/tweet"),
        _ => None,
    }
}

fn is_user_object(user: &Value) -> bool {
    matches!(text_at(user, "/__typename"), None | Some("User"))
}

/// A timeline result as a tweet with a well-formed ID and author object; otherwise `None`.
fn read_tweet(result: &Value) -> Option<Tweet<'_>> {
    let node = tweet_node(result)?;
    let id = valid_id_at(node, "/rest_id")?;
    let user = json_at(node, "/core/user_results/result")?;
    if !is_user_object(user) {
        return None;
    }
    let author_id = valid_id_at(user, "/rest_id")?;
    Some(Tweet {
        node,
        user,
        id,
        author_id,
    })
}

/// The tweet object that states `post_id` as its ID, whether or not the rest of it is readable.
fn find_named<'a>(results: &[&'a Value], post_id: &str) -> Option<&'a Value> {
    let mut nodes = results.iter().copied().filter_map(tweet_node);
    nodes.find(|node| text_at(node, "/rest_id") == Some(post_id))
}

/// Whether the author object is explicitly something other than a user (suspended, removed).
fn has_unavailable_author(node: &Value) -> bool {
    match json_at(node, "/core/user_results/result") {
        Some(user) => !is_user_object(user),
        None => false,
    }
}

/// The quoted post embedded in a tweet, when X included it.
fn embedded_tweet(node: &Value) -> Option<Tweet<'_>> {
    let quoted = json_at(node, "/quoted_status_result/result");
    quoted.and_then(read_tweet)
}

/// The author of `tweet`, only when it is the post `id` and names that author itself.
fn author_of(tweet: Option<&Tweet<'_>>, id: Option<&str>) -> Option<String> {
    let tweet = tweet?;
    if Some(tweet.id) != id {
        return None;
    }
    tweet.confirmed_author().map(str::to_owned)
}

/// The tweet results of one timeline entry: a single item, or every item of a module.
fn push_results<'a>(entry: &'a Value, results: &mut Vec<&'a Value>) {
    results.extend(entry.pointer("/content/itemContent/tweet_results/result"));
    let items = array_at(entry, "/content/items");
    for item in items.into_iter().flatten() {
        results.extend(item.pointer("/item/itemContent/tweet_results/result"));
    }
}

/// The entries one timeline instruction adds (or the single entry it replaces).
fn entries_of(instruction: &Value) -> Vec<&Value> {
    let many = array_at(instruction, "/entries");
    let one = instruction.get("entry");
    many.into_iter().flatten().chain(one).collect()
}

/// Every tweet result in a `TweetDetail` response, in response order.
fn timeline_results(root: &Value) -> XResult<Vec<&Value>> {
    let path = "/data/threaded_conversation_with_injections_v2/instructions";
    let instructions = array_at(root, path).ok_or(XError::Malformed)?;
    let mut results = Vec::new();
    for instruction in instructions {
        for entry in entries_of(instruction) {
            push_results(entry, &mut results);
        }
        let module_items = array_at(instruction, "/moduleItems");
        for item in module_items.into_iter().flatten() {
            results.extend(item.pointer("/item/itemContent/tweet_results/result"));
        }
    }
    Ok(results)
}

/// A screen name from the current or the legacy user field; it must be well formed.
fn screen_name_of(user: &Value) -> Option<String> {
    let core = text_at(user, "/core/screen_name");
    let name = core.or(text_at(user, "/legacy/screen_name"));
    let valid = name.filter(|n| is_valid_screen_name(n));
    valid.map(str::to_owned)
}

/// The text of a post: the long-form text when X sent it, otherwise the legacy text.
fn post_text(node: &Value, legacy: &Value) -> XResult<String> {
    let long = text_at(node, "/note_tweet/note_tweet_results/result/text");
    let full = text_at(legacy, "/full_text");
    let text = long.or(full).ok_or(XError::Malformed)?;
    if text.is_empty() || text.chars().count() > MAX_POST_TEXT_CHARS {
        return Err(XError::Malformed);
    }
    Ok(decode_entities(text))
}

/// The replied-to user. X's `in_reply_to_user_id_str` and the author of the replied-to post (when
/// the response contains it) must agree. A fact that is stated but unreadable or contradicted is
/// not the same as a missing one, so it makes the answer unknown. Without a replied-to post ID
/// there is nothing to be a reply to, so the user is unknown too.
fn reply_user(tweets: &[Tweet<'_>], legacy: &Value, parent: Option<&str>) -> Option<String> {
    let parent_id = parent?;
    let stated = json_at(legacy, "/in_reply_to_user_id_str");
    let explicit = id_at(legacy, "/in_reply_to_user_id_str");
    let found = tweets.iter().find(|tweet| tweet.id == parent_id);
    let parent_author = author_of(found, Some(parent_id));
    if stated.is_some() && explicit.is_none() {
        return None;
    }
    if found.is_some() && parent_author.is_none() {
        return None;
    }
    agreed(explicit, parent_author)
}

/// A post from a `TweetDetail` response. `Ok(None)` means X did not return that post (deleted,
/// hidden, tombstoned, no conversation at all and no error); a post whose own fields contradict
/// each other is an error.
fn parse_tweet_detail(body: &[u8], post_id: &str) -> XResult<Option<PostInfo>> {
    let root = parse_json(body)?;
    if !json_at(&root, "/data").is_some_and(Value::is_object) {
        return Err(XError::Malformed);
    }
    if json_at(&root, "/data/threaded_conversation_with_injections_v2").is_none() {
        return absent(&root);
    }
    let results = timeline_results(&root)?;
    let tweets: Vec<Tweet<'_>> = results.iter().copied().filter_map(read_tweet).collect();
    let Some(focal) = tweets.iter().find(|tweet| tweet.id == post_id) else {
        // A post that is present but unreadable is not a deleted one, unless its author is
        // explicitly unavailable.
        return match find_named(&results, post_id) {
            Some(node) if !has_unavailable_author(node) => Err(XError::Malformed),
            _ => absent(&root),
        };
    };
    let author_id = focal.confirmed_author().ok_or(XError::Malformed)?;
    let legacy = json_at(focal.node, "/legacy").ok_or(XError::Malformed)?;
    let text = post_text(focal.node, legacy)?;
    let author_screen_name = screen_name_of(focal.user).ok_or(XError::Malformed)?;
    let conversation_id = id_at(legacy, "/conversation_id_str");
    let conversation_id = conversation_id.ok_or(XError::Malformed)?;
    let reply_to_post_id = optional_id(legacy, "/in_reply_to_status_id_str")?;
    let quoted_post_id = id_at(legacy, "/quoted_status_id_str");
    let embedded = embedded_tweet(focal.node);
    let quoted_author_id = author_of(embedded.as_ref(), quoted_post_id.as_deref());
    let reply_to_user_id = reply_user(&tweets, legacy, reply_to_post_id.as_deref());
    Ok(Some(PostInfo {
        id: post_id.to_owned(),
        text,
        author_id: author_id.to_owned(),
        author_screen_name,
        conversation_id,
        created_at: date_at(legacy, "/created_at"),
        reply_to_post_id,
        reply_to_user_id,
        quoted_post_id,
        quoted_author_id,
    }))
}

fn profile_text(text: Option<&str>) -> Option<String> {
    let fits = text.filter(|t| t.len() <= MAX_PROFILE_TEXT_BYTES);
    fits.map(str::to_owned)
}

/// A relation flag X may state both under `relationship_perspectives` and under `legacy`.
fn relation_flag(user: &Value, key: &str) -> Option<bool> {
    let current = format!("/relationship_perspectives/{key}");
    let legacy = format!("/legacy/{key}");
    agreed(flag_at(user, &current), flag_at(user, &legacy))
}

fn relation_of(user: &Value) -> UserRelation {
    UserRelation {
        blocked_by: relation_flag(user, "blocked_by"),
        blocking: relation_flag(user, "blocking"),
        followed_by: relation_flag(user, "followed_by"),
        following: relation_flag(user, "following"),
        follow_request_sent: flag_at(user, "/legacy/follow_request_sent"),
    }
}

/// `Some(true)` only when the profile explicitly carries the automated-account label. A missing
/// label says nothing either way, so it is never `Some(false)`.
fn automated_label(user: &Value) -> Option<bool> {
    let label = text_at(user, "/affiliates_highlighted_label/label/userLabelType");
    (label == Some("AutomatedLabel")).then_some(true)
}

fn profile_of(user: &Value, user_id: &str, screen_name: String) -> UserProfile {
    let core_date = date_at(user, "/core/created_at");
    let created_at = core_date.or(date_at(user, "/legacy/created_at"));
    let bio = text_at(user, "/profile_bio/description");
    let description = bio.or(text_at(user, "/legacy/description"));
    let core_name = text_at(user, "/core/name");
    let name = core_name.or(text_at(user, "/legacy/name"));
    let privacy = flag_at(user, "/privacy/protected");
    let is_protected = agreed(privacy, flag_at(user, "/legacy/protected"));
    UserProfile {
        user_id: user_id.to_owned(),
        screen_name,
        name: profile_text(name),
        description: profile_text(description),
        created_at,
        automated_label: automated_label(user),
        default_profile_image: flag_at(user, "/legacy/default_profile_image"),
        is_protected,
        followers_count: count_at(user, "/legacy/followers_count"),
        following_count: count_at(user, "/legacy/friends_count"),
        posts_count: count_at(user, "/legacy/statuses_count"),
        relation: relation_of(user),
    }
}

/// A profile from a `UserByRestId` response. `Ok(None)` means X stated that the account is not
/// available (suspended) or returned no user and no error (removed, never existed); a different
/// account than the one asked for is an error.
fn parse_user_profile(body: &[u8], user_id: &str) -> XResult<Option<UserProfile>> {
    let root = parse_json(body)?;
    if !json_at(&root, "/data").is_some_and(Value::is_object) {
        return Err(XError::Malformed);
    }
    let Some(wrapper) = json_at(&root, "/data/user") else {
        return absent(&root);
    };
    if !wrapper.is_object() {
        return Err(XError::Malformed);
    }
    let Some(user) = json_at(&root, "/data/user/result") else {
        return absent(&root);
    };
    if !is_user_object(user) {
        return Ok(None);
    }
    if text_at(user, "/rest_id") != Some(user_id) {
        return Err(XError::Malformed);
    }
    let screen_name = screen_name_of(user).ok_or(XError::Malformed)?;
    Ok(Some(profile_of(user, user_id, screen_name)))
}

fn is_already_favorited(error: &Value) -> bool {
    let top = count_at(error, "/code");
    let nested = count_at(error, "/extensions/code");
    agreed(top, nested) == Some(ALREADY_FAVORITED_CODE)
}

/// Whether every reported error is X's "already favorited" code, and there is at least one.
fn only_already_favorited(root: &Value) -> bool {
    match array_at(root, "/errors") {
        Some(list) if !list.is_empty() => list.iter().all(is_already_favorited),
        _ => false,
    }
}

/// The result of `FavoriteTweet`. Success needs X's own confirmation (`favorite_tweet`); the one
/// error accepted is "already favorited", which leaves the post liked all the same.
fn parse_favorite(body: &[u8], post_id: &str) -> XResult<LikeResult> {
    let root = parse_json(body)?;
    let already_liked = has_errors(&root);
    if already_liked && !only_already_favorited(&root) {
        return Err(XError::Malformed);
    }
    let word = text_at(&root, "/data/favorite_tweet");
    let done = word.is_some_and(|w| !w.is_empty());
    if !already_liked && !done {
        return Err(XError::Malformed);
    }
    Ok(LikeResult {
        post_id: post_id.to_owned(),
        already_liked,
    })
}

/// The result of `friendships/create`. Only X returning the requested user proves the follow; its
/// `following` and `follow_request_sent` are reported as stated and never decide success.
fn parse_friendship(body: &[u8], user_id: &str) -> XResult<FollowResult> {
    let root = parse_json(body)?;
    if has_errors(&root) || text_at(&root, "/id_str") != Some(user_id) {
        return Err(XError::Malformed);
    }
    Ok(FollowResult {
        user_id: user_id.to_owned(),
        following: flag_at(&root, "/following"),
        follow_request_sent: flag_at(&root, "/follow_request_sent"),
    })
}

/// A response that arrived but could not be confirmed after a write was sent.
fn unconfirmed<T>(result: XResult<T>) -> XResult<T> {
    result.map_err(|_| XError::Uncertain)
}

/// How long resolved operations are reused: `full` (the generator's lifetime) when every
/// operation resolved, and no longer than `PARTIAL_ACTIONS_TTL` otherwise so a gap is retried soon.
fn actions_ttl(actions: &ActionMetadata, full: Duration) -> Duration {
    if actions.is_complete() {
        full
    } else {
        full.min(PARTIAL_ACTIONS_TTL)
    }
}

fn is_valid_post_id(id: &str) -> bool {
    is_valid_user_id(id)
}

fn has_toggles(toggles: &Value) -> bool {
    toggles.as_object().is_some_and(|map| !map.is_empty())
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

/// Status handling for a write that was sent. A 2xx and the 4xx answers that X gives when it
/// refuses a request are definite; 3xx, 5xx and anything else leave the outcome unknown.
fn classify_write(status: StatusCode) -> std::result::Result<(), XError> {
    match status.as_u16() {
        200..=299 => Ok(()),
        401 => Err(XError::Unauthorized),
        403 => Err(XError::Forbidden),
        429 => Err(XError::RateLimited),
        code @ 400..=499 => Err(XError::Status(code)),
        _ => Err(XError::Uncertain),
    }
}

/// A write that failed after it was handed to the client may have been processed by X, so it is
/// not reported as a plain transport failure. Only a connection that was never established is
/// known not to have carried the request.
fn write_transport(error: reqwest::Error) -> XError {
    let never_sent = error.is_connect();
    let failure = transport(error);
    if never_sent {
        failure
    } else {
        XError::Uncertain
    }
}

/// Reads at most `MAX_RESPONSE_BYTES`; bodies are never logged.
async fn read_body(mut response: Response) -> std::result::Result<Vec<u8>, XError> {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
    {
        return Err(XError::Malformed);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport)? {
        if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(XError::Malformed);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// 接続先。本番は固定の X のホストで、テストだけが差し替える。
struct Endpoints {
    settings_base: String,
    api_base: String,
    graphql_base: String,
    catalogue_url: String,
    /// 最初に取るページ。bootstrap の手掛かりが無いときだけ `fallback_url` を取る。
    home_url: String,
    fallback_url: String,
    /// 署名モジュールを取ってよいオリジン。相対パスは `/x-web/` 配下の `.js` に限る。
    asset_base: Url,
    ondemand_prefix: String,
}

impl Endpoints {
    fn production() -> Self {
        Self {
            settings_base: TWITTER_SETTINGS_BASE.to_string(),
            api_base: TWITTER_API_BASE.to_string(),
            graphql_base: "https://x.com/i/api/graphql".to_string(),
            catalogue_url: crate::x_metadata::CATALOGUE_URL.to_string(),
            home_url: TWITTER_HOME_URL.to_string(),
            fallback_url: TWITTER_HOME_FALLBACK_URL.to_string(),
            asset_base: Url::parse(ASSET_BASE).expect("静的な URL"),
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
    metadata: Mutex<Option<(crate::x_metadata::CreateTweet, Instant)>>,
    /// カタログから解決した参照・いいね系の操作。全部そろったときだけ長く保持する。
    /// 取得中も読めるよう、取得の直列化 (`actions_gate`) とは別に持つ。await をまたいで保持しない。
    actions: StdMutex<Option<(ActionMetadata, Instant)>>,
    actions_gate: Mutex<()>,
    /// 参照・書込み 1 リクエストの上限。テストだけが短くする。
    action_timeout: Duration,
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
            metadata: Mutex::new(None),
            actions: StdMutex::new(None),
            actions_gate: Mutex::new(()),
            action_timeout: ACTION_TIMEOUT,
        }
    }

    /// Metadata readiness is cache-only: listing accounts never contacts X or GitHub.
    pub async fn machine_ready(&self) -> bool {
        self.metadata
            .lock()
            .await
            .as_ref()
            .is_some_and(|(_, fetched)| fetched.elapsed() < TRANSACTION_TTL)
            && self.cached_transaction().is_some()
    }

    pub async fn prepare_post(
        &self,
    ) -> std::result::Result<crate::x_metadata::CreateTweet, XError> {
        let mut slot = self.metadata.lock().await;
        if let Some((operation, fetched)) = slot.as_ref()
            && fetched.elapsed() < TRANSACTION_TTL
        {
            self.transaction().await?;
            return Ok(operation.clone());
        }
        let source = self.fetch_text(&self.endpoints.catalogue_url, 0).await?;
        let operation = crate::x_metadata::parse(&source)?;
        self.transaction().await?;
        *slot = Some((operation.clone(), Instant::now()));
        Ok(operation)
    }

    pub async fn identity(&self, twitter: &TwitterConfig) -> std::result::Result<Identity, XError> {
        let url = format!(
            "{}/account/verify_credentials.json?skip_status=true&include_entities=false",
            self.endpoints.api_base
        );
        let body = self.send(self.client.get(url), twitter).await?;
        parse_identity(&body)
    }

    /// Only locally constructed text/reply variables are sent; catalogue variables are ignored.
    pub async fn create_post(
        &self,
        twitter: &TwitterConfig,
        operation: &crate::x_metadata::CreateTweet,
        text: &str,
        reply: Option<&str>,
        expected_user: &str,
    ) -> std::result::Result<String, XError> {
        let mut variables = json!({
            "tweet_text": text, "dark_request": false,
            "media": {"media_entities": [], "possibly_sensitive": false},
            "semantic_annotation_ids": []
        });
        if let Some(id) = reply {
            variables["reply"] = json!({"in_reply_to_tweet_id": id, "exclude_reply_user_ids": []});
        }
        let url = format!(
            "{}/{}/CreateTweet",
            self.endpoints.graphql_base, operation.query_id
        );
        let body = self
            .send(
                self.client.post(url).json(&json!({
                    "queryId": operation.query_id, "variables": variables,
                    "features": operation.features, "fieldToggles": operation.field_toggles
                })),
                twitter,
            )
            .await?;
        parse_post(&body, expected_user)
    }

    /// Resolves the lookup and like operations from the public catalogue. Each operation is
    /// judged on its own, so a catalogue that lacks one still serves the others; see
    /// `has_lookup_post` and its siblings. Following needs no catalogue entry, only the
    /// transaction signer.
    pub async fn prepare_actions(&self) -> XResult<ActionMetadata> {
        if let Some(actions) = self.cached_actions() {
            self.transaction().await?;
            return Ok(actions);
        }
        let _gate = self.actions_gate.lock().await;
        if let Some(actions) = self.cached_actions() {
            self.transaction().await?;
            return Ok(actions);
        }
        let fetch = self.fetch_text(&self.endpoints.catalogue_url, 0);
        let source = match tokio::time::timeout(self.action_timeout, fetch).await {
            Ok(fetched) => fetched?,
            Err(_) => return Err(XError::Transport),
        };
        let actions = crate::x_metadata::parse_actions(&source)?;
        self.transaction().await?;
        tracing::debug!(
            lookup_post = actions.has_lookup_post(),
            lookup_user = actions.has_lookup_user(),
            like = actions.has_like(),
            "X action operations resolved"
        );
        let mut slot = self.actions.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some((actions.clone(), Instant::now()));
        Ok(actions)
    }

    /// Whether post lookup is ready: the operations and the signer are both cached and fresh.
    /// Cache-only: it never contacts X, and an expired or missing cache is `false`.
    pub async fn has_lookup_post(&self) -> bool {
        self.action_ready(ActionMetadata::has_lookup_post)
    }

    /// Whether user lookup is ready. Cache-only, like `has_lookup_post`.
    pub async fn has_lookup_user(&self) -> bool {
        self.action_ready(ActionMetadata::has_lookup_user)
    }

    /// Whether like is ready. Cache-only, like `has_lookup_post`.
    pub async fn has_like(&self) -> bool {
        self.action_ready(ActionMetadata::has_like)
    }

    /// The resolved operations, unless they were never fetched or have expired.
    fn cached_actions(&self) -> Option<ActionMetadata> {
        let cached = self.actions.lock().unwrap_or_else(|e| e.into_inner());
        let (actions, fetched) = cached.as_ref()?;
        let ttl = actions_ttl(actions, self.transaction_ttl);
        (fetched.elapsed() < ttl).then(|| actions.clone())
    }

    fn action_ready(&self, ready: fn(&ActionMetadata) -> bool) -> bool {
        let cached = self.actions.lock().unwrap_or_else(|e| e.into_inner());
        let Some((actions, fetched)) = cached.as_ref() else {
            return false;
        };
        let ttl = actions_ttl(actions, self.transaction_ttl);
        fetched.elapsed() < ttl && ready(actions) && self.cached_transaction().is_some()
    }

    /// Reads one post. `Ok(None)` when X does not return it (deleted, hidden, unavailable).
    pub async fn lookup_post(
        &self,
        twitter: &TwitterConfig,
        meta: &ActionMetadata,
        post_id: &str,
    ) -> XResult<Option<PostInfo>> {
        if !is_valid_post_id(post_id) {
            return Err(XError::Malformed);
        }
        let operation = meta.tweet_detail.as_ref().ok_or(XError::Malformed)?;
        let variables = json!({
            "focalTweetId": post_id,
            "referrer": "home",
            "with_rux_injections": false,
            "rankingMode": "Relevance",
            "includePromotedContent": true,
            "withCommunity": true,
            "withQuickPromoteEligibilityTweetFields": true,
            "withBirdwatchNotes": true,
            "withVoice": true
        });
        let url = self.read_url("TweetDetail", operation, &variables)?;
        let body = self.send_read(self.client.get(url), twitter).await?;
        parse_tweet_detail(&body, post_id)
    }

    /// Reads one profile. `Ok(None)` when X does not return the account (suspended, removed).
    pub async fn lookup_user(
        &self,
        twitter: &TwitterConfig,
        meta: &ActionMetadata,
        user_id: &str,
    ) -> XResult<Option<UserProfile>> {
        if !is_valid_user_id(user_id) {
            return Err(XError::Malformed);
        }
        let operation = meta.user_by_rest_id.as_ref().ok_or(XError::Malformed)?;
        let variables = json!({"userId": user_id, "withSafetyModeUserFields": true});
        let url = self.read_url("UserByRestId", operation, &variables)?;
        let body = self.send_read(self.client.get(url), twitter).await?;
        parse_user_profile(&body, user_id)
    }

    /// Likes one post. Already liked is a success. A failure after the request was sent that
    /// leaves the outcome unclear is `XError::Uncertain` and must not be retried automatically.
    pub async fn like_post(
        &self,
        twitter: &TwitterConfig,
        meta: &ActionMetadata,
        post_id: &str,
    ) -> XResult<LikeResult> {
        if !is_valid_post_id(post_id) {
            return Err(XError::Malformed);
        }
        let operation = meta.favorite_tweet.as_ref().ok_or(XError::Malformed)?;
        let base = &self.endpoints.graphql_base;
        let query_id = &operation.query_id;
        let url = format!("{base}/{query_id}/FavoriteTweet");
        let variables = json!({"tweet_id": post_id});
        let payload = json!({"queryId": query_id, "variables": variables});
        let request = self.client.post(url).json(&payload);
        let response = self.dispatch(request, twitter).await?;
        let status = response.status();
        if status == StatusCode::FORBIDDEN {
            // X has also answered an already favorited post with 403 and the same error code.
            let body = read_body(response).await.unwrap_or_default();
            return match parse_favorite(&body, post_id) {
                Ok(result) if result.already_liked => Ok(result),
                _ => Err(XError::Forbidden),
            };
        }
        classify_write(status)?;
        let body = unconfirmed(read_body(response).await)?;
        unconfirmed(parse_favorite(&body, post_id))
    }

    /// Follows one account. Only X returning that account proves the follow. A protected account
    /// leaves `following` false with `follow_request_sent` true. A failure after the request was
    /// sent that leaves the outcome unclear is `XError::Uncertain` and must not be retried
    /// automatically.
    pub async fn follow_user(
        &self,
        twitter: &TwitterConfig,
        user_id: &str,
    ) -> XResult<FollowResult> {
        if !is_valid_user_id(user_id) {
            return Err(XError::Malformed);
        }
        let form = format!("user_id={user_id}&{FRIENDSHIPS_CREATE_FORM}");
        let request = self
            .client
            .post(self.friendships_create_url())
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form);
        let body = self.send_write(request, twitter).await?;
        unconfirmed(parse_friendship(&body, user_id))
    }

    /// The GraphQL GET URL of a read. The variables are built locally; only the query ID, the
    /// feature flags and the field toggles come from the catalogue.
    fn read_url(&self, name: &str, op: &ReadOperation, variables: &Value) -> XResult<Url> {
        let base = &self.endpoints.graphql_base;
        let raw = format!("{base}/{}/{name}", op.query_id);
        let mut url = Url::parse(&raw).map_err(|_| XError::Malformed)?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("variables", &variables.to_string());
            query.append_pair("features", &op.features.to_string());
            if has_toggles(&op.field_toggles) {
                query.append_pair("fieldToggles", &op.field_toggles.to_string());
            }
        }
        Ok(url)
    }

    fn friendships_create_url(&self) -> String {
        format!("{}/friendships/create.json", self.endpoints.api_base)
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

    /// 認証情報なしでホーム HTML と署名モジュールを取り、生成器を作る。取得先は X の固定ホストと、
    /// そこから検証を通ったモジュール URL だけで、取得した JavaScript は実行しない。
    async fn bootstrap(&self) -> std::result::Result<Transaction, XError> {
        let work = async {
            let home = self.fetch_home().await?;
            let signer = self.fetch_signer(&home).await?;
            Transaction::new(&home, &signer).map_err(|error| {
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

    /// 旧形式の ondemand.s の URL。hash は 16 進の短いものだけを通す。
    fn legacy_signer_url(&self, html: &str) -> Option<String> {
        let hash = ondemand_file_hash(html).filter(|hash| hash.len() <= MAX_ONDEMAND_HASH_LEN)?;
        Some(format!(
            "{}{hash}{ONDEMAND_URL_SUFFIX}",
            self.endpoints.ondemand_prefix
        ))
    }

    fn entry_urls(&self, html: &str) -> Vec<Url> {
        match Url::parse(&self.endpoints.home_url) {
            Ok(base) => entry_script_urls(html, &base, &self.endpoints.asset_base),
            Err(_) => Vec::new(),
        }
    }

    fn has_bootstrap(&self, html: &str) -> bool {
        self.legacy_signer_url(html).is_some() || !self.entry_urls(html).is_empty()
    }

    /// ルートを取り、手掛かりが無いときだけ /home を取る。取得そのものの失敗では取り直さない。
    async fn fetch_home(&self) -> std::result::Result<String, XError> {
        let html = self
            .fetch_text(&self.endpoints.home_url, MAX_BOOTSTRAP_REDIRECTS)
            .await?;
        if self.has_bootstrap(&html) {
            return Ok(html);
        }
        let fallback = self
            .fetch_text(&self.endpoints.fallback_url, MAX_BOOTSTRAP_REDIRECTS)
            .await?;
        if self.has_bootstrap(&fallback) {
            Ok(fallback)
        } else {
            Err(XError::Malformed)
        }
    }

    /// 署名モジュールを探す。`entry-client` から import を幅優先で辿り、`sign.*.js` に当たったら返す。
    /// 探索は深さ・リクエスト数に上限があり、取得先は `asset_base` 配下の検証済み URL だけ。
    async fn fetch_signer(&self, html: &str) -> std::result::Result<String, XError> {
        if let Some(url) = self.legacy_signer_url(html) {
            return self.fetch_text(&url, 0).await;
        }
        let mut queue: Vec<(Url, usize)> = self
            .entry_urls(html)
            .into_iter()
            .map(|url| (url, 0))
            .collect();
        let mut visited: Vec<Url> = Vec::new();
        while !queue.is_empty() && visited.len() < MAX_SIGNER_REQUESTS {
            let (url, depth) = queue.remove(0);
            if visited.contains(&url) {
                continue;
            }
            visited.push(url.clone());
            let source = self.fetch_text(url.as_str(), 0).await?;
            if is_signer_url(&url) {
                return Ok(source);
            }
            if depth < MAX_SIGNER_DEPTH {
                let imports = module_import_urls(&source, &url, &self.endpoints.asset_base);
                queue.extend(imports.into_iter().map(|next| (next, depth + 1)));
                queue.sort_by_key(|(url, depth)| (module_priority(url), *depth));
            }
        }
        tracing::debug!(
            requests = visited.len(),
            "x-client-transaction-id signer module was not found"
        );
        Err(XError::Malformed)
    }

    /// 匿名 GET。redirect は同一オリジンへの最大 `max_redirects` 回だけ手動で辿り、
    /// 別オリジン・ループ・回数超過は転送先へ送らず `Status` で失敗させる (0 なら一切辿らない)。
    /// 失敗は常に `Status` / `Transport` / `Malformed` にし、401・403・429 を認証失敗として扱わない。
    async fn fetch_text(
        &self,
        start: &str,
        max_redirects: usize,
    ) -> std::result::Result<String, XError> {
        let mut url = Url::parse(start).map_err(|_| XError::Malformed)?;
        let origin = url.origin();
        let mut visited = vec![url.clone()];
        let mut response = loop {
            let mut request = self.client.get(url.clone());
            for (name, value) in BOOTSTRAP_HEADERS {
                request = request.header(name, value);
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
                        && visited.len() <= max_redirects
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

    /// Adds the session headers, the timeout and a fresh transaction ID. Nothing is built for
    /// sending unless the generator is available.
    async fn signed(&self, request: RequestBuilder, twitter: &TwitterConfig) -> XResult<Request> {
        let request = request.timeout(self.action_timeout);
        let mut request = with_session(request, twitter).build().map_err(transport)?;
        let transaction = self.transaction().await?;
        let id = transaction.generate(request.method().as_str(), request.url().path());
        let id = HeaderValue::try_from(id).map_err(|_| XError::Malformed)?;
        request.headers_mut().insert(TRANSACTION_HEADER, id);
        Ok(request)
    }

    /// A request that changes nothing on X.
    async fn send_read(
        &self,
        request: RequestBuilder,
        twitter: &TwitterConfig,
    ) -> XResult<Vec<u8>> {
        let request = self.signed(request, twitter).await?;
        let response = self.client.execute(request).await.map_err(transport)?;
        let status = response.status();
        tracing::debug!(status = status.as_u16(), "X response");
        classify(status)?;
        read_body(response).await
    }

    /// Sends a request that changes something on X and returns the response unjudged. Once it
    /// is sent, a failure that does not say that X refused it is `XError::Uncertain`.
    async fn dispatch(
        &self,
        request: RequestBuilder,
        twitter: &TwitterConfig,
    ) -> XResult<Response> {
        let request = self.signed(request, twitter).await?;
        let sent = self.client.execute(request).await;
        let response = sent.map_err(write_transport)?;
        tracing::debug!(status = response.status().as_u16(), "X response");
        Ok(response)
    }

    /// A write whose answer must be a 2xx with a readable body.
    async fn send_write(
        &self,
        request: RequestBuilder,
        twitter: &TwitterConfig,
    ) -> XResult<Vec<u8>> {
        let response = self.dispatch(request, twitter).await?;
        classify_write(response.status())?;
        unconfirmed(read_body(response).await)
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
    use crate::x_metadata::parse_actions;
    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::http::{HeaderMap, Method, Uri};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    #[test]
    fn machine_identity_is_minimal_and_avatar_is_allowlisted() {
        let body = br#"{"id_str":"123","screen_name":"fixture","name":"Fixture","profile_image_url_https":"https://pbs.twimg.com/profile_images/1/a.jpg","auth_token":"SECRET"}"#;
        let identity = parse_identity(body).unwrap();
        let public = serde_json::to_string(&identity).unwrap();
        assert!(!public.contains("SECRET"));
        assert_eq!(identity.user_id, "123");
        assert!(identity.avatar_url.is_some());
        let bad = String::from_utf8(body.to_vec())
            .unwrap()
            .replace("pbs.twimg.com", "evil.invalid");
        assert!(parse_identity(bad.as_bytes()).unwrap().avatar_url.is_none());
        assert!(parse_identity(br#"{"errors":[{"message":"PRIVATE"}]}"#).is_err());
    }

    #[test]
    fn machine_graphql_requires_result_author_and_no_errors() {
        let body = br#"{"data":{"create_tweet":{"tweet_results":{"result":{"rest_id":"789","core":{"user_results":{"result":{"rest_id":"123"}}}}}}}}"#;
        assert_eq!(parse_post(body, "123").unwrap(), "789");
        assert!(parse_post(body, "456").is_err());
        assert!(parse_post(br#"{"errors":[{"message":"secret"}],"data":{}}"#, "123").is_err());
        assert!(parse_post(br#"{"data":{}}"#, "123").is_err());
        assert!(parse_post(b"not json", "123").is_err());
    }

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
            graphql_base: format!("{base}/i/api/graphql"),
            catalogue_url: format!("{base}/catalogue.json"),
            home_url: format!("{base}/home"),
            fallback_url: format!("{base}/home-fallback"),
            asset_base: Url::parse(base).unwrap(),
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
        fallback: Arc<Mutex<(u16, String)>>,
        script: Arc<Mutex<(u16, String)>>,
        settings: Arc<Mutex<(u16, String)>>,
    }

    impl Site {
        fn new() -> Self {
            Self {
                home: Arc::new(Mutex::new((200, fixture::home_html()))),
                fallback: Arc::new(Mutex::new((200, "<html>no bootstrap</html>".into()))),
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
                ("GET", "/home-fallback") => scripted(&self.fallback),
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
    async fn machine_catalogue_is_anonymous_and_post_uses_the_existing_signer() {
        let log = Log::default();
        let server = serve(recording(Arc::clone(&log), |seen| match seen.path.as_str() {
            "/catalogue.json" => reply(200, include_str!("../tests/fixtures/machine-metadata.json")),
            "/i/api/1.1/account/verify_credentials.json" => reply(200,
                r#"{"id_str":"123","screen_name":"fixture","name":"Fixture"}"#),
            "/i/api/graphql/synthetic-query-1/CreateTweet" => reply(200,
                r#"{"data":{"create_tweet":{"tweet_results":{"result":{"rest_id":"789","core":{"user_results":{"result":{"rest_id":"123"}}}}}}}}"#),
            _ => reply(404, "not found"),
        })).await;
        let api = injected(&server.base);
        let metadata = api.prepare_post().await.unwrap();
        assert!(api.machine_ready().await);
        assert_eq!(api.identity(&twitter()).await.unwrap().user_id, "123");
        assert_eq!(
            api.create_post(&twitter(), &metadata, "fixture", Some("456"), "123")
                .await
                .unwrap(),
            "789"
        );
        let requests = log.lock().unwrap();
        let catalogue = requests
            .iter()
            .find(|r| r.path == "/catalogue.json")
            .unwrap();
        for header in CREDENTIAL_HEADERS {
            assert!(!catalogue.headers.contains_key(header));
        }
        let post = requests.iter().find(|r| r.method == "POST").unwrap();
        assert_valid_id(post);
        let body: serde_json::Value = serde_json::from_str(&post.body).unwrap();
        assert_eq!(body["queryId"], "synthetic-query-1");
        assert_eq!(body["variables"]["tweet_text"], "fixture");
        assert_eq!(body["variables"]["reply"]["in_reply_to_tweet_id"], "456");
        assert_eq!(body["features"]["synthetic_feature_enabled"], true);
    }

    #[tokio::test]
    async fn machine_catalogue_redirects_and_missing_features_fail_closed() {
        let sink_log = Log::default();
        let sink = serve(recording(Arc::clone(&sink_log), |_| reply(200, "{}"))).await;
        let target = sink.base.clone();
        let server = serve(recording(Log::default(), move |_| {
            let mut response = reply(302, "");
            response
                .headers_mut()
                .insert("location", target.parse().unwrap());
            response
        }))
        .await;
        let api = injected(&server.base);
        assert!(api.prepare_post().await.is_err());
        assert!(!api.machine_ready().await);
        assert!(sink_log.lock().unwrap().is_empty());
        let server = serve(recording(Log::default(), |_| reply(200, "{}"))).await;
        assert!(injected(&server.base).prepare_post().await.is_err());
    }

    #[tokio::test]
    async fn machine_http_and_graphql_errors_are_safe() {
        for status in [200, 401, 403, 429, 500] {
            let server = serve(recording(Log::default(), move |_| {
                reply(
                    status,
                    r#"{"errors":[{"message":"SYNTHETIC-PRIVATE-UPSTREAM"}]}"#,
                )
            }))
            .await;
            let api = injected(&server.base);
            let metadata =
                crate::x_metadata::parse(include_str!("../tests/fixtures/machine-metadata.json"))
                    .unwrap();
            let error = api
                .create_post(&twitter(), &metadata, "fixture", None, "123")
                .await
                .unwrap_err();
            assert!(!error.to_string().contains("SYNTHETIC-PRIVATE-UPSTREAM"));
            if status == 401 {
                assert_eq!(error, XError::Unauthorized);
            }
            if status == 403 {
                assert_eq!(error, XError::Forbidden);
            }
            if status == 429 {
                assert_eq!(error, XError::RateLimited);
            }
            if status == 200 {
                assert_eq!(error, XError::Malformed);
            }
        }
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
        assert_eq!(pinned(api.endpoints.home_url.clone()), expect("x.com", "/"));
        assert_eq!(
            pinned(api.endpoints.fallback_url.clone()),
            expect("x.com", "/home")
        );
        assert_eq!(api.endpoints.asset_base.as_str(), "https://abs.twimg.com/");
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
        assert_eq!(home.headers["accept-language"], "en-US,en;q=0.9");
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
                .all(|s| ["/home", "/home-fallback", SCRIPT_PATH].contains(&s.path.as_str())),
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
            for (name, value) in BOOTSTRAP_HEADERS {
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

    const ENTRY_PATH: &str = "/x-web/x-web/entry-client-logged-out-abc.js";
    const ASSETS: &str = "/x-web/x-web/assets";

    fn modern_home(entry_src: &str) -> String {
        fixture::home_html_with(
            false,
            &format!(r#"<script type="module" src="{entry_src}"></script>"#),
        )
    }

    fn module(path: &str, status: u16, body: &str) -> Route {
        // Route の path は 'static なので、テスト内で使う固定文字列だけを渡す
        (
            Box::leak(path.to_string().into_boxed_str()),
            status,
            None,
            body.to_string(),
        )
    }

    fn paths(log: &Log) -> Vec<String> {
        log.lock().unwrap().iter().map(|s| s.path.clone()).collect()
    }

    #[tokio::test]
    async fn bootstrap_discovers_the_signer_through_imports_anonymously() {
        let log = Log::default();
        let site = vec![
            ("/home", 200, None, modern_home(ENTRY_PATH)),
            (
                ENTRY_PATH,
                200,
                None,
                r#"import{x}from"./assets/sentry-filter-def.js";"#.to_string(),
            ),
            module(
                &format!("{ASSETS}/sentry-filter-def.js"),
                200,
                "const s=()=>import(`./sign.o-ghi.js`)",
            ),
            module(
                &format!("{ASSETS}/sign.o-ghi.js"),
                200,
                &fixture::ondemand_js(),
            ),
            module(
                "/1.1/account/settings.json",
                200,
                r#"{"screen_name":"me_1"}"#,
            ),
        ];
        let server = serve(scripted(&log, site)).await;
        let api = bootstrapping(&server.base, Duration::from_secs(5));
        assert_eq!(api.screen_name(&twitter()).await.unwrap(), "me_1");

        assert_eq!(
            paths(&log),
            [
                "/home".to_string(),
                ENTRY_PATH.to_string(),
                format!("{ASSETS}/sentry-filter-def.js"),
                format!("{ASSETS}/sign.o-ghi.js"),
                "/1.1/account/settings.json".to_string(),
            ]
        );
        let seen = log.lock().unwrap();
        for hop in &seen[..4] {
            for name in CREDENTIAL_HEADERS {
                assert!(!hop.headers.contains_key(name), "{} に {name}", hop.path);
            }
            assert_eq!(hop.headers["accept-language"], "en-US,en;q=0.9");
            assert_eq!(hop.headers["user-agent"], BROWSER_USER_AGENT);
        }
        assert_valid_id(&seen[4]);
    }

    #[tokio::test]
    async fn signer_discovery_prefers_relevant_modules_and_stays_bounded() {
        let twitter = twitter();
        // 関連しそうなモジュールを先に取り、深さ 3 までで見つける
        let log = Log::default();
        let site = vec![
            ("/home", 200, None, modern_home(ENTRY_PATH)),
            module(
                ENTRY_PATH,
                200,
                r#"import "./sentry-unrelated.js"; import "./transaction-client.js";"#,
            ),
            module(
                "/x-web/x-web/sentry-unrelated.js",
                200,
                r#"import "./irrelevant.js";"#,
            ),
            module(
                "/x-web/x-web/transaction-client.js",
                200,
                r#"import("./sign.o-live.js")"#,
            ),
            module("/x-web/x-web/sign.o-live.js", 200, &fixture::ondemand_js()),
            module(
                "/1.1/account/settings.json",
                200,
                r#"{"screen_name":"me_1"}"#,
            ),
        ];
        let server = serve(scripted(&log, site)).await;
        let api = bootstrapping(&server.base, Duration::from_secs(5));
        api.screen_name(&twitter).await.unwrap();
        assert_eq!(
            &paths(&log)[1..5],
            [
                ENTRY_PATH,
                "/x-web/x-web/sentry-unrelated.js",
                "/x-web/x-web/transaction-client.js",
                "/x-web/x-web/sign.o-live.js"
            ]
        );
        drop(server);

        // sign が深さ 4 にあれば見つからない
        let log = Log::default();
        let site = vec![
            ("/home", 200, None, modern_home(ENTRY_PATH)),
            module(ENTRY_PATH, 200, r#"import "./a.js""#),
            module("/x-web/x-web/a.js", 200, r#"import "./b.js""#),
            module("/x-web/x-web/b.js", 200, r#"import "./c.js""#),
            module("/x-web/x-web/c.js", 200, r#"import "./sign.o-x.js""#),
            module("/x-web/x-web/sign.o-x.js", 200, &fixture::ondemand_js()),
        ];
        let server = serve(scripted(&log, site)).await;
        let api = bootstrapping(&server.base, Duration::from_secs(5));
        assert_eq!(
            api.screen_name(&twitter).await.unwrap_err(),
            XError::Malformed
        );
        assert!(!paths(&log).iter().any(|p| p.contains("sign.o-x")));
        drop(server);

        // 循環でも広がっても、モジュールの取得は 12 件まで
        let log = Log::default();
        let mut site = vec![
            ("/home", 200, None, modern_home(ENTRY_PATH)),
            module(
                ENTRY_PATH,
                200,
                &(0..20)
                    .map(|i| format!(r#"import "./leaf{i}.js";"#))
                    .collect::<String>(),
            ),
        ];
        for i in 0..20 {
            site.push(module(
                &format!("/x-web/x-web/leaf{i}.js"),
                200,
                "export const x=1",
            ));
        }
        let server = serve(scripted(&log, site)).await;
        let api = bootstrapping(&server.base, Duration::from_secs(5));
        assert_eq!(
            api.screen_name(&twitter).await.unwrap_err(),
            XError::Malformed
        );
        assert_eq!(paths(&log).len(), 1 + MAX_SIGNER_REQUESTS);
        drop(server);

        let log = Log::default();
        let mut site = vec![
            ("/home", 200, None, modern_home(ENTRY_PATH)),
            module(ENTRY_PATH, 200, r#"import "./module0.js""#),
        ];
        for i in 0..20 {
            site.push(module(
                &format!("/x-web/x-web/module{i}.js"),
                200,
                &format!(
                    r#"import "./module{}.js"; import "./entry-client-logged-out-abc.js""#,
                    i + 1
                ),
            ));
        }
        let server = serve(scripted(&log, site)).await;
        let api = bootstrapping(&server.base, Duration::from_secs(5));
        assert_eq!(
            api.screen_name(&twitter).await.unwrap_err(),
            XError::Malformed
        );
        let fetched = paths(&log);
        assert!(fetched.len() <= 1 + MAX_SIGNER_REQUESTS);
        assert_eq!(
            fetched.iter().filter(|p| p.as_str() == ENTRY_PATH).count(),
            1,
            "同じモジュールは 1 回だけ取る"
        );
    }

    #[tokio::test]
    async fn signer_discovery_never_fetches_unsafe_modules_or_follows_redirects() {
        let sink_log = Log::default();
        let sink = serve(recording(Arc::clone(&sink_log), |_| reply(200, "{}"))).await;
        let sink_port = sink.base.rsplit(':').next().unwrap().to_string();
        let hostile = [
            format!(r#"import "{}/x-web/sign.o-a.js";"#, sink.base),
            format!(r#"import "http://localhost:{sink_port}/x-web/sign.o-b.js";"#),
            r#"import "https://evil.invalid/x-web/sign.o-c.js";"#.to_string(),
            r#"import "//evil.invalid/x-web/sign.o-d.js";"#.to_string(),
            r#"import "/other/sign.o-e.js";"#.to_string(),
            r#"import "./sign.o-f.json";"#.to_string(),
            r#"import "../../other/sign.o-g.js";"#.to_string(),
            r#"import(`./${name}.js`);"#.to_string(),
        ]
        .concat();
        let log = Log::default();
        let site = vec![
            ("/home", 200, None, modern_home(ENTRY_PATH)),
            module(ENTRY_PATH, 200, &hostile),
        ];
        let server = serve(scripted(&log, site)).await;
        let api = bootstrapping(&server.base, Duration::from_secs(5));
        assert_eq!(
            api.screen_name(&twitter()).await.unwrap_err(),
            XError::Malformed
        );
        assert_eq!(paths(&log), ["/home", ENTRY_PATH]);
        assert!(sink_log.lock().unwrap().is_empty());
        drop(server);

        // entry が別オリジン・範囲外のときは取りに行かない
        let foreign_entries = [
            format!("{}/x-web/x-web/entry-client-x.js", sink.base),
            "https://evil.invalid/x-web/x-web/entry-client-x.js".to_string(),
            "/other/entry-client-x.js".to_string(),
        ];
        for entry in foreign_entries {
            let log = Log::default();
            let site = vec![
                ("/home", 200, None, modern_home(&entry)),
                ("/home-fallback", 200, None, "<html></html>".to_string()),
            ];
            let server = serve(scripted(&log, site)).await;
            let api = bootstrapping(&server.base, Duration::from_secs(5));
            assert_eq!(
                api.screen_name(&twitter()).await.unwrap_err(),
                XError::Malformed,
                "{entry}"
            );
            assert_eq!(paths(&log), ["/home", "/home-fallback"], "{entry}");
        }
        assert!(sink_log.lock().unwrap().is_empty());

        // モジュールの redirect は辿らない
        let log = Log::default();
        let site = vec![
            ("/home", 200, None, modern_home(ENTRY_PATH)),
            (
                ENTRY_PATH,
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
        assert_eq!(paths(&log), ["/home", ENTRY_PATH]);
        assert!(sink_log.lock().unwrap().is_empty());

        // 同一オリジン内でもモジュールの redirect は辿らない (署名モジュールは redirect しない)
        let log = Log::default();
        let site = vec![
            ("/home", 200, None, modern_home(ENTRY_PATH)),
            (
                ENTRY_PATH,
                302,
                Some("/x-web/x-web/moved.js".to_string()),
                String::new(),
            ),
            module("/x-web/x-web/moved.js", 200, "import './sign.o-a.js'"),
        ];
        let server = serve(scripted(&log, site)).await;
        let api = bootstrapping(&server.base, Duration::from_secs(5));
        assert_eq!(
            api.screen_name(&twitter()).await.unwrap_err(),
            XError::Status(302)
        );
        assert_eq!(paths(&log), ["/home", ENTRY_PATH]);
    }

    #[tokio::test]
    async fn module_failures_are_provider_errors_and_never_reach_the_api() {
        let twitter = twitter();
        for (status, expected) in [
            (503, XError::Status(503)),
            (403, XError::Status(403)),
            (401, XError::Status(401)),
            (429, XError::Status(429)),
        ] {
            let log = Log::default();
            let site = vec![
                ("/home", 200, None, modern_home(ENTRY_PATH)),
                module(ENTRY_PATH, status, "blocked"),
            ];
            let server = serve(scripted(&log, site)).await;
            let api = bootstrapping(&server.base, Duration::from_secs(5));
            let error = api.screen_name(&twitter).await.unwrap_err();
            assert_eq!(error, expected);
            assert!(!error.is_fatal());
            assert!(!paths(&log).iter().any(|p| p.contains("/1.1/")));
        }

        // 署名らしきモジュールが無い
        let log = Log::default();
        let site = vec![
            ("/home", 200, None, modern_home(ENTRY_PATH)),
            module(ENTRY_PATH, 200, "globalThis.remoteExecuted=true;"),
        ];
        let server = serve(scripted(&log, site)).await;
        let api = bootstrapping(&server.base, Duration::from_secs(5));
        assert_eq!(
            api.screen_name(&twitter).await.unwrap_err(),
            XError::Malformed
        );
        assert!(!paths(&log).iter().any(|p| p.contains("/1.1/")));
    }

    #[tokio::test]
    async fn the_fallback_page_is_fetched_only_when_the_first_has_no_bootstrap() {
        let twitter = twitter();
        // 手掛かりが無い → fallback を取る
        let log = Log::default();
        let site = Site::new();
        *site.home.lock().unwrap() = (200, "<html></html>".into());
        *site.fallback.lock().unwrap() = (200, fixture::home_html());
        let server = serve(site.router(&log)).await;
        let api = bootstrapping(&server.base, Duration::from_secs(5));
        assert_eq!(api.screen_name(&twitter).await.unwrap(), "me_1");
        assert_eq!(count(&log, "/home"), 1);
        assert_eq!(count(&log, "/home-fallback"), 1);
        drop(server);

        // 手掛かりがある・取得に失敗した → fallback は取らない
        for (status, body) in [(200, fixture::home_html()), (503, "down".into())] {
            let log = Log::default();
            let site = Site::new();
            *site.home.lock().unwrap() = (status, body);
            let server = serve(site.router(&log)).await;
            let api = bootstrapping(&server.base, Duration::from_secs(5));
            let _ = api.screen_name(&twitter).await;
            assert_eq!(count(&log, "/home-fallback"), 0, "{status}");
        }

        // どちらにも無い
        let log = Log::default();
        let site = Site::new();
        *site.home.lock().unwrap() = (200, "<html></html>".into());
        let server = serve(site.router(&log)).await;
        let api = bootstrapping(&server.base, Duration::from_secs(5));
        assert_eq!(
            api.screen_name(&twitter).await.unwrap_err(),
            XError::Malformed
        );
    }

    #[tokio::test]
    async fn the_legacy_manifest_takes_priority_over_entry_scripts() {
        let log = Log::default();
        let home =
            fixture::home_html_with(true, &format!(r#"<script src="{ENTRY_PATH}"></script>"#));
        let site = vec![
            ("/home", 200, None, home),
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
        assert!(!paths(&log).iter().any(|p| p == ENTRY_PATH));
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

    // ---- lookup and action operations (synthetic fixtures only) ----------------

    const ACTIONS: &str = include_str!("../tests/fixtures/machine-actions-metadata.json");
    const DETAIL: &str = include_str!("../tests/fixtures/machine-tweet-detail.json");
    const USER: &str = include_str!("../tests/fixtures/machine-user.json");
    const ENTRIES: &str = "/data/threaded_conversation_with_injections_v2/instructions/0/entries";
    const PARENT: &str = "1900000000000000001";
    const FOCAL: &str = "1900000000000000002";
    const QUOTED: &str = "1900000000000000003";
    const REPLY: &str = "1900000000000000004";
    const DETAIL_PATH: &str = "/i/api/graphql/synthetic-detail-1/TweetDetail";
    const USER_PATH: &str = "/i/api/graphql/synthetic-user-1/UserByRestId";
    const LIKE_PATH: &str = "/i/api/graphql/synthetic-like-1/FavoriteTweet";
    const FOLLOW_PATH: &str = "/i/api/1.1/friendships/create.json";
    const LIKED: &str = r#"{"data":{"favorite_tweet":"Done"}}"#;
    const FOLLOWED: &str = r#"{"id_str":"2002","following":false,"follow_request_sent":true}"#;
    const LEAK: &str = "SYNTHETIC-PRIVATE-UPSTREAM auth_token=AUTH-SECRET";

    /// The tweet result of timeline entry `index` in the detail fixture: 0 is the parent post,
    /// 1 the focal post (a reply that quotes another post).
    fn result_at(index: usize) -> String {
        format!("{ENTRIES}/{index}/content/itemContent/tweet_results/result")
    }

    fn load(source: &str) -> Value {
        serde_json::from_str(source).unwrap()
    }

    fn dump(root: &Value) -> Vec<u8> {
        root.to_string().into_bytes()
    }

    /// Sets the value at `path`; the parent object must exist.
    fn put(root: &mut Value, path: &str, value: Value) {
        let (parent, key) = path.rsplit_once('/').unwrap();
        root.pointer_mut(parent).unwrap()[key] = value;
    }

    /// The fixture with one value replaced, as a response body.
    fn edited(source: &str, path: &str, value: Value) -> Vec<u8> {
        let mut root = load(source);
        put(&mut root, path, value);
        dump(&root)
    }

    fn read_post(body: &[u8], id: &str) -> PostInfo {
        parse_tweet_detail(body, id).unwrap().unwrap()
    }

    fn read_profile(body: &[u8], id: &str) -> UserProfile {
        parse_user_profile(body, id).unwrap().unwrap()
    }

    /// The path of a field of the user in the user fixture.
    fn at(tail: &str) -> String {
        format!("/data/user/result{tail}")
    }

    fn liked(already_liked: bool) -> LikeResult {
        LikeResult {
            post_id: "42".into(),
            already_liked,
        }
    }

    fn followed(following: Option<bool>, follow_request_sent: Option<bool>) -> FollowResult {
        FollowResult {
            user_id: "42".into(),
            following,
            follow_request_sent,
        }
    }

    #[test]
    fn tweet_detail_reports_only_what_x_stated() {
        let post = read_post(DETAIL.as_bytes(), FOCAL);
        let expected = PostInfo {
            id: FOCAL.into(),
            text: "hello <world> &amp; fixture".into(),
            author_id: "2002".into(),
            author_screen_name: "fixture_focal".into(),
            conversation_id: PARENT.into(),
            created_at: Some("2018-10-10T20:19:24Z".into()),
            reply_to_post_id: Some(PARENT.into()),
            reply_to_user_id: Some("2001".into()),
            quoted_post_id: Some(QUOTED.into()),
            quoted_author_id: Some("2003".into()),
        };
        assert_eq!(post, expected);
    }

    #[test]
    fn tweet_detail_leaves_unstated_facts_unknown() {
        let parent = read_post(DETAIL.as_bytes(), PARENT);
        let expected = PostInfo {
            id: PARENT.into(),
            text: "parent text".into(),
            author_id: "2001".into(),
            author_screen_name: "fixture_parent".into(),
            conversation_id: PARENT.into(),
            created_at: Some("2018-10-09T10:00:00Z".into()),
            reply_to_post_id: None,
            reply_to_user_id: None,
            quoted_post_id: None,
            quoted_author_id: None,
        };
        assert_eq!(parent, expected);
        // A post inside a conversation module, with no timestamp and a reply to the focal post.
        let child = read_post(DETAIL.as_bytes(), REPLY);
        assert_eq!(child.created_at, None);
        assert_eq!(child.reply_to_post_id.as_deref(), Some(FOCAL));
        assert_eq!(child.reply_to_user_id.as_deref(), Some("2002"));
    }

    #[test]
    fn missing_deleted_and_hidden_posts_are_none() {
        let body = DETAIL.as_bytes();
        for id in ["1900000000000000005", "1900000000000000099"] {
            assert!(parse_tweet_detail(body, id).unwrap().is_none(), "{id}");
        }
        let instructions = "/data/threaded_conversation_with_injections_v2/instructions";
        let empty = edited(DETAIL, instructions, json!([]));
        assert!(parse_tweet_detail(&empty, FOCAL).unwrap().is_none());
        let nothing = parse_tweet_detail(br#"{"data":{}}"#, FOCAL);
        assert!(nothing.unwrap().is_none());
        let with_error = parse_tweet_detail(br#"{"data":{},"errors":[{"message":"x"}]}"#, FOCAL);
        assert_eq!(with_error.unwrap_err(), XError::Malformed);
        let author = format!("{}/core/user_results/result/__typename", result_at(1));
        let hidden = edited(DETAIL, &author, json!("UserUnavailable"));
        assert!(parse_tweet_detail(&hidden, FOCAL).unwrap().is_none());
        let tombstone = format!("{}/__typename", result_at(1));
        let removed = edited(DETAIL, &tombstone, json!("TweetTombstone"));
        assert!(parse_tweet_detail(&removed, FOCAL).unwrap().is_none());
    }

    #[test]
    fn a_missing_post_next_to_errors_is_not_believed() {
        let missing = "1900000000000000099";
        let with_errors = edited(DETAIL, "/errors", json!([{"message": "x"}]));
        assert!(parse_tweet_detail(&with_errors, FOCAL).unwrap().is_some());
        let error = parse_tweet_detail(&with_errors, missing).unwrap_err();
        assert_eq!(error, XError::Malformed);
        let not_a_list = edited(DETAIL, "/errors", json!("boom"));
        assert!(parse_tweet_detail(&not_a_list, missing).is_err());
        let empty_list = edited(DETAIL, "/errors", json!([]));
        assert!(parse_tweet_detail(&empty_list, missing).unwrap().is_none());
    }

    #[test]
    fn unusable_detail_bodies_are_errors() {
        let timeline = "/data/threaded_conversation_with_injections_v2";
        let no_instructions = edited(DETAIL, timeline, json!({}));
        let not_a_list = edited(DETAIL, timeline, json!({"instructions": {}}));
        let no_data = edited(DETAIL, "/data", Value::Null);
        for body in [no_instructions, not_a_list, no_data] {
            let error = parse_tweet_detail(&body, FOCAL).unwrap_err();
            assert_eq!(error, XError::Malformed);
        }
        for body in ["", "not json", "{", "{}", "[]", "null", "123"] {
            let result = parse_tweet_detail(body.as_bytes(), FOCAL);
            assert!(result.is_err(), "{body}");
        }
    }

    #[test]
    fn contradicting_reply_and_quote_facts_become_unknown() {
        let focal = result_at(1);
        let stated = format!("{focal}/legacy/in_reply_to_user_id_str");
        let parent_author = format!("{}/legacy/user_id_str", result_at(0));
        let quoted_author = format!("{focal}/quoted_status_result/result/legacy/user_id_str");
        let quoted_id = format!("{focal}/legacy/quoted_status_id_str");
        let changed = |path: &str, value: Value| {
            let body = edited(DETAIL, path, value);
            read_post(&body, FOCAL)
        };

        for value in [json!("2999"), json!("abc"), json!(""), json!(2001)] {
            let post = changed(&stated, value);
            assert_eq!(post.reply_to_post_id.as_deref(), Some(PARENT));
            assert_eq!(post.reply_to_user_id, None);
        }
        let only_the_parent = changed(&stated, Value::Null);
        assert_eq!(only_the_parent.reply_to_user_id.as_deref(), Some("2001"));
        let unconfirmed_parent = changed(&parent_author, json!("2999"));
        assert_eq!(unconfirmed_parent.reply_to_user_id, None);

        let post = changed(&quoted_author, json!("2999"));
        assert_eq!(post.quoted_post_id.as_deref(), Some(QUOTED));
        assert_eq!(post.quoted_author_id, None);
        let post = changed(&quoted_id, json!("1900000000000000009"));
        assert_eq!(post.quoted_post_id.as_deref(), Some("1900000000000000009"));
        assert_eq!(post.quoted_author_id, None);
        let post = changed(&quoted_id, json!("not-an-id"));
        assert_eq!(post.quoted_post_id, None);
        assert_eq!(post.quoted_author_id, None);
        let post = changed(&format!("{focal}/quoted_status_result"), Value::Null);
        assert_eq!(post.quoted_post_id.as_deref(), Some(QUOTED));
        assert_eq!(post.quoted_author_id, None);
    }

    #[test]
    fn an_unreadable_parent_leaves_only_the_stated_reply_user() {
        let kind = format!("{}/__typename", result_at(0));
        let mut root = load(DETAIL);
        put(&mut root, &kind, json!("TweetTombstone"));
        let post = read_post(&dump(&root), FOCAL);
        assert_eq!(post.reply_to_post_id.as_deref(), Some(PARENT));
        assert_eq!(post.reply_to_user_id.as_deref(), Some("2001"));
        let stated = format!("{}/legacy/in_reply_to_user_id_str", result_at(1));
        put(&mut root, &stated, Value::Null);
        assert_eq!(read_post(&dump(&root), FOCAL).reply_to_user_id, None);
    }

    #[test]
    fn a_post_whose_own_fields_contradict_each_other_is_an_error() {
        let focal = result_at(1);
        let author = format!("{focal}/core/user_results/result");
        let reply_to = format!("{focal}/legacy/in_reply_to_status_id_str");
        let cases = [
            (format!("{focal}/legacy/user_id_str"), json!("2999")),
            (format!("{focal}/legacy/user_id_str"), Value::Null),
            (format!("{focal}/legacy"), Value::Null),
            (format!("{focal}/legacy/full_text"), json!("")),
            (format!("{focal}/legacy/full_text"), Value::Null),
            (format!("{focal}/legacy/conversation_id_str"), Value::Null),
            (format!("{focal}/legacy/conversation_id_str"), json!("12x")),
            (reply_to, json!("abc")),
            (format!("{author}/core/screen_name"), json!("bad name!")),
            (format!("{author}/rest_id"), json!("12x")),
            (format!("{author}/rest_id"), Value::Null),
        ];
        for (path, value) in cases {
            let body = edited(DETAIL, &path, value);
            let error = parse_tweet_detail(&body, FOCAL).unwrap_err();
            assert_eq!(error, XError::Malformed, "{path}");
        }
    }

    #[test]
    fn post_text_prefers_long_form_decodes_once_and_is_bounded() {
        let focal = result_at(1);
        let note = format!("{focal}/note_tweet");
        let long = json!({"note_tweet_results": {"result": {"text": "long &amp;lt; text"}}});
        let body = edited(DETAIL, &note, long);
        assert_eq!(read_post(&body, FOCAL).text, "long &lt; text");

        let full_text = format!("{focal}/legacy/full_text");
        let at_limit = "あ".repeat(MAX_POST_TEXT_CHARS);
        let body = edited(DETAIL, &full_text, json!(at_limit));
        let text = read_post(&body, FOCAL).text;
        assert_eq!(text.chars().count(), MAX_POST_TEXT_CHARS);
        let over_limit = "a".repeat(MAX_POST_TEXT_CHARS + 1);
        let body = edited(DETAIL, &full_text, json!(over_limit));
        assert!(parse_tweet_detail(&body, FOCAL).is_err());
    }

    #[test]
    fn a_visibility_wrapped_post_reads_like_a_plain_one() {
        let focal = result_at(1);
        let plain = read_post(DETAIL.as_bytes(), FOCAL);
        let mut root = load(DETAIL);
        let inner = root.pointer(&focal).unwrap().clone();
        let wrapped = json!({"__typename": "TweetWithVisibilityResults", "tweet": inner});
        put(&mut root, &focal, wrapped);
        assert_eq!(read_post(&dump(&root), FOCAL), plain);
    }

    #[test]
    fn an_unparseable_timestamp_leaves_the_rest_of_the_post_intact() {
        let created = format!("{}/legacy/created_at", result_at(1));
        let plain = read_post(DETAIL.as_bytes(), FOCAL);
        let invalid = [
            json!("Wed Oct 10 20:19:24 +0100 2018"),
            json!("Wed Feb 30 20:19:24 +0000 2018"),
            json!("not a date"),
            json!(""),
            json!(1234),
            Value::Null,
        ];
        for value in invalid {
            let body = edited(DETAIL, &created, value);
            let post = read_post(&body, FOCAL);
            assert_eq!(post.created_at, None);
            assert_eq!(post.text, plain.text);
        }
    }

    #[test]
    fn x_dates_are_parsed_strictly() {
        let valid = [
            ("Wed Oct 10 20:19:24 +0000 2018", "2018-10-10T20:19:24Z"),
            ("Thu Feb 29 00:00:00 +0000 2024", "2024-02-29T00:00:00Z"),
            ("Tue Feb 29 00:00:00 +0000 2000", "2000-02-29T00:00:00Z"),
            ("Fri Dec 31 23:59:59 +0000 1999", "1999-12-31T23:59:59Z"),
        ];
        for (text, expected) in valid {
            assert_eq!(parse_x_date(text).as_deref(), Some(expected), "{text}");
        }
        let invalid = [
            "",
            "not a date",
            "Wed Oct 10 20:19:24 +0100 2018",
            "Wed Oct 10 20:19:24 2018",
            "Wed Oct 10 20:19:24 +0000 2018 extra",
            "Xyz Oct 10 20:19:24 +0000 2018",
            "Wed Foo 10 20:19:24 +0000 2018",
            "Wed Oct 32 20:19:24 +0000 2018",
            "Wed Oct 00 20:19:24 +0000 2018",
            "Wed Oct 1 20:19:24 +0000 2018",
            "Wed Feb 29 12:00:00 +0000 2023",
            "Wed Feb 30 12:00:00 +0000 2024",
            "Mon Feb 29 00:00:00 +0000 1900",
            "Wed Oct 10 24:00:00 +0000 2018",
            "Wed Oct 10 20:60:00 +0000 2018",
            "Wed Oct 10 20:19:60 +0000 2018",
            "Wed Oct 10 20:19 +0000 2018",
            "Wed Oct 10 +0:19:24 +0000 2018",
            "Wed Oct 10 20:19:24 +0000 +018",
            "Wed  Oct 10 20:19:24 +0000 2018",
        ];
        for text in invalid {
            assert_eq!(parse_x_date(text), None, "{text}");
        }
    }

    #[test]
    fn user_profile_keeps_only_stated_facts() {
        let profile = read_profile(USER.as_bytes(), "2002");
        let expected = UserProfile {
            user_id: "2002".into(),
            screen_name: "fixture_user".into(),
            name: Some("Fixture User".into()),
            description: Some("Synthetic bio".into()),
            created_at: Some("2017-01-02T03:04:05Z".into()),
            automated_label: Some(true),
            default_profile_image: Some(false),
            is_protected: Some(false),
            followers_count: Some(12),
            following_count: Some(34),
            posts_count: Some(56),
            relation: UserRelation {
                blocked_by: Some(false),
                blocking: Some(false),
                followed_by: Some(false),
                following: Some(true),
                follow_request_sent: Some(false),
            },
        };
        assert_eq!(profile, expected);
    }

    #[test]
    fn a_sparse_profile_has_no_invented_values() {
        let user = json!({"rest_id": "7", "legacy": {"screen_name": "only_legacy"}});
        let sparse = json!({"data": {"user": {"result": user}}});
        let profile = read_profile(&dump(&sparse), "7");
        let expected = UserProfile {
            user_id: "7".into(),
            screen_name: "only_legacy".into(),
            name: None,
            description: None,
            created_at: None,
            automated_label: None,
            default_profile_image: None,
            is_protected: None,
            followers_count: None,
            following_count: None,
            posts_count: None,
            relation: UserRelation::default(),
        };
        assert_eq!(profile, expected);
    }

    #[test]
    fn profile_fallbacks_and_conflicts() {
        let changed = |tail: &str, value: Value| {
            let body = edited(USER, &at(tail), value);
            read_profile(&body, "2002")
        };
        let profile = changed("/core/name", Value::Null);
        assert_eq!(profile.name.as_deref(), Some("Legacy Name"));
        let profile = changed("/profile_bio", Value::Null);
        assert_eq!(profile.description.as_deref(), Some("legacy bio"));
        let profile = changed("/core/created_at", json!("not a date"));
        assert_eq!(profile.created_at.as_deref(), Some("2018-02-03T04:05:06Z"));

        // Two sources that disagree make the fact unknown, not one of the two.
        let profile = changed("/relationship_perspectives/following", json!(false));
        assert_eq!(profile.relation.following, None);
        assert_eq!(profile.relation.blocking, Some(false));
        let profile = changed("/legacy/blocking", json!(true));
        assert_eq!(profile.relation.blocking, None);
        assert_eq!(profile.relation.following, Some(true));
        let profile = changed("/privacy/protected", json!(true));
        assert_eq!(profile.is_protected, None);
    }

    #[test]
    fn profile_values_of_the_wrong_type_are_unknown() {
        let changed = |tail: &str, value: Value| {
            let body = edited(USER, &at(tail), value);
            read_profile(&body, "2002")
        };
        let profile = changed("/legacy/default_profile_image", json!("false"));
        assert_eq!(profile.default_profile_image, None);
        let profile = changed("/legacy/follow_request_sent", json!(0));
        assert_eq!(profile.relation.follow_request_sent, None);
        let counts = [json!(-1), json!(1.5), json!("12"), Value::Null];
        for value in counts {
            let profile = changed("/legacy/followers_count", value);
            assert_eq!(profile.followers_count, None);
            assert_eq!(profile.following_count, Some(34));
        }
        let profile = changed("/legacy/statuses_count", json!(u64::MAX));
        assert_eq!(profile.posts_count, Some(u64::MAX));
        let profile = changed("/legacy/description", json!("x".repeat(5000)));
        assert_eq!(profile.description.as_deref(), Some("Synthetic bio"));
        let profile = changed("/profile_bio/description", json!("x".repeat(5000)));
        assert_eq!(profile.description, None);
    }

    #[test]
    fn the_automated_label_is_true_only_when_stated() {
        let changed = |value: Value| {
            let body = edited(USER, &at("/affiliates_highlighted_label"), value);
            read_profile(&body, "2002").automated_label
        };
        let label = |kind: &str| json!({"label": {"userLabelType": kind}});
        assert_eq!(changed(Value::Null), None);
        assert_eq!(changed(json!({})), None);
        assert_eq!(changed(json!(true)), None);
        assert_eq!(changed(label("BusinessLabel")), None);
        assert_eq!(changed(label("automatedlabel")), None);
        assert_eq!(changed(label("AutomatedLabel")), Some(true));
    }

    #[test]
    fn unavailable_accounts_are_none_and_other_accounts_are_errors() {
        let user = |body: &str| parse_user_profile(body.as_bytes(), "2002");
        let nothing = r#"{"data":{"user":{"result":null}},"errors":[]}"#;
        for body in [r#"{"data":{"user":{}}}"#, r#"{"data":{}}"#, nothing] {
            assert!(user(body).unwrap().is_none(), "{body}");
        }
        let suspended = edited(USER, &at("/__typename"), json!("UserUnavailable"));
        assert!(parse_user_profile(&suspended, "2002").unwrap().is_none());

        for body in [
            r#"{"data":{"user":{}},"errors":[{"message":"x"}]}"#,
            r#"{"data":{"user":{}},"errors":"boom"}"#,
            r#"{"data":{},"errors":[{"message":"x"}]}"#,
            r#"{"data":null}"#,
            r#"{"data":{"user":[]}}"#,
            "{}",
            "[]",
            "null",
            "not json",
            "",
        ] {
            assert!(user(body).is_err(), "{body}");
        }
        assert!(parse_user_profile(USER.as_bytes(), "2999").is_err());
        let wrong_id = edited(USER, &at("/rest_id"), json!("2999"));
        assert!(parse_user_profile(&wrong_id, "2002").is_err());
        let bad_name = edited(USER, &at("/core/screen_name"), json!("bad name!"));
        assert!(parse_user_profile(&bad_name, "2002").is_err());
    }

    #[test]
    fn favorite_needs_x_confirmation_or_the_already_favorited_code() {
        let like = |body: &str| parse_favorite(body.as_bytes(), "42");
        assert_eq!(like(LIKED).unwrap(), liked(false));
        let with_empty_errors = r#"{"errors":[],"data":{"favorite_tweet":"Done"}}"#;
        assert_eq!(like(with_empty_errors).unwrap(), liked(false));

        let already = [
            r#"{"errors":[{"message":"x","code":139}],"data":{}}"#,
            r#"{"errors":[{"message":"x","extensions":{"code":139}}]}"#,
            r#"{"errors":[{"code":139,"extensions":{"code":139}}],"data":null}"#,
            r#"{"errors":[{"code":139},{"code":139}]}"#,
        ];
        for body in already {
            assert_eq!(like(body).unwrap(), liked(true), "{body}");
        }
        let not_confirmed = [
            r#"{"errors":[{"message":"x","code":144}]}"#,
            r#"{"errors":[{"code":139},{"code":144}]}"#,
            r#"{"errors":[{"code":139,"extensions":{"code":140}}]}"#,
            r#"{"errors":[{"message":"no code"}]}"#,
            r#"{"errors":[{"code":"139"}]}"#,
            r#"{"errors":[139]}"#,
            r#"{"errors":"139"}"#,
            r#"{"data":{}}"#,
            r#"{"data":{"favorite_tweet":""}}"#,
            r#"{"data":{"favorite_tweet":null}}"#,
            r#"{"data":{"favorite_tweet":true}}"#,
            "{}",
            "[]",
            "not json",
            "",
        ];
        for body in not_confirmed {
            assert_eq!(like(body).unwrap_err(), XError::Malformed, "{body}");
        }
    }

    #[test]
    fn follow_is_proved_only_by_the_requested_user_coming_back() {
        let follow = |body: &str| parse_friendship(body.as_bytes(), "42");
        let open = r#"{"id_str":"42","following":true,"follow_request_sent":false}"#;
        assert_eq!(follow(open).unwrap(), followed(Some(true), Some(false)));
        let gated = r#"{"id_str":"42","following":false,"follow_request_sent":true}"#;
        assert_eq!(follow(gated).unwrap(), followed(Some(false), Some(true)));
        let bare = r#"{"id_str":"42"}"#;
        assert_eq!(follow(bare).unwrap(), followed(None, None));
        let nulls = r#"{"id_str":"42","following":null,"follow_request_sent":null}"#;
        assert_eq!(follow(nulls).unwrap(), followed(None, None));
        let unreadable = r#"{"id_str":"42","following":"yes","follow_request_sent":1}"#;
        assert_eq!(follow(unreadable).unwrap(), followed(None, None));

        let unproven = [
            r#"{"id_str":"43","following":true}"#,
            r#"{"id_str":"042"}"#,
            r#"{"id_str":42}"#,
            r#"{"id":42}"#,
            r#"{"user":{"id_str":"42"}}"#,
            r#"{"id_str":"42","errors":[{"code":160,"message":"x"}]}"#,
            r#"{"errors":[{"code":160,"message":"x"}]}"#,
            "{}",
            "[]",
            "not json",
            "",
        ];
        for body in unproven {
            assert_eq!(follow(body).unwrap_err(), XError::Malformed, "{body}");
        }
    }

    #[test]
    fn every_parser_rejects_bodies_that_are_not_objects() {
        for body in ["", "null", "[]", "123", "true", "{", "{}"] {
            let bytes = body.as_bytes();
            let detail = parse_tweet_detail(bytes, FOCAL);
            let user = parse_user_profile(bytes, "2002");
            let favorite = parse_favorite(bytes, "42");
            let friendship = parse_friendship(bytes, "42");
            assert!(detail.is_err(), "detail {body}");
            assert!(user.is_err(), "user {body}");
            assert!(favorite.is_err(), "favorite {body}");
            assert!(friendship.is_err(), "friendship {body}");
        }
    }

    #[test]
    fn partial_action_metadata_is_retried_sooner() {
        let complete = parse_actions(ACTIONS).unwrap();
        assert_eq!(actions_ttl(&complete, TRANSACTION_TTL), TRANSACTION_TTL);
        let catalogue = ACTIONS.replace("FavoriteTweet", "Gone");
        let partial = parse_actions(&catalogue).unwrap();
        assert_eq!(actions_ttl(&partial, TRANSACTION_TTL), PARTIAL_ACTIONS_TTL);
        let none = ActionMetadata::default();
        assert_eq!(actions_ttl(&none, TRANSACTION_TTL), PARTIAL_ACTIONS_TTL);
        let short = Duration::from_secs(1);
        assert_eq!(actions_ttl(&partial, short), short);
        assert_eq!(actions_ttl(&complete, short), short);
    }

    /// An `XApi` whose cached state expires after `ttl`.
    fn expiring(base: &str, ttl: Duration) -> XApi {
        let transaction = Some(fixture::transaction());
        XApi::build(endpoints(base), transaction, Duration::from_secs(5), ttl)
    }

    async fn readiness(api: &XApi) -> [bool; 3] {
        let post = api.has_lookup_post().await;
        let user = api.has_lookup_user().await;
        let like = api.has_like().await;
        [post, user, like]
    }

    /// A local stand-in for the catalogue and the four operations.
    fn action_site(log: &Log) -> Router {
        recording(Arc::clone(log), |seen| match seen.path.as_str() {
            "/catalogue.json" => reply(200, ACTIONS),
            DETAIL_PATH => reply(200, DETAIL),
            USER_PATH => reply(200, USER),
            LIKE_PATH => reply(200, LIKED),
            FOLLOW_PATH => reply(200, FOLLOWED),
            _ => reply(404, "not found"),
        })
    }

    async fn never_answers() -> &'static str {
        tokio::time::sleep(Duration::from_secs(30)).await;
        "late"
    }

    /// The JSON value of a query parameter the client sent.
    fn query_json(seen: &Seen, key: &str) -> Option<Value> {
        use std::collections::HashMap;

        let url = Url::parse(&format!("http://localhost/?{}", seen.query)).unwrap();
        let pairs: HashMap<String, String> = url.query_pairs().into_owned().collect();
        pairs.get(key).map(|raw| serde_json::from_str(raw).unwrap())
    }

    #[tokio::test]
    async fn action_requests_are_built_locally_and_signed() {
        let log = Log::default();
        let server = serve(action_site(&log)).await;
        let api = injected(&server.base);
        let twitter = twitter();

        let meta = api.prepare_actions().await.unwrap();
        assert!(meta.is_complete());
        let post = api.lookup_post(&twitter, &meta, FOCAL).await.unwrap();
        assert_eq!(post.unwrap().author_id, "2002");
        let profile = api.lookup_user(&twitter, &meta, "2002").await.unwrap();
        assert_eq!(profile.unwrap().screen_name, "fixture_user");
        let like = api.like_post(&twitter, &meta, "42").await.unwrap();
        assert_eq!(like, liked(false));
        let result = api.follow_user(&twitter, "2002").await.unwrap();
        let expected = FollowResult {
            user_id: "2002".into(),
            following: Some(false),
            follow_request_sent: Some(true),
        };
        assert_eq!(result, expected);

        let seen = log.lock().unwrap();
        let routes: Vec<String> = seen
            .iter()
            .map(|s| format!("{} {}", s.method, s.path))
            .collect();
        assert_eq!(
            routes,
            [
                "GET /catalogue.json",
                "GET /i/api/graphql/synthetic-detail-1/TweetDetail",
                "GET /i/api/graphql/synthetic-user-1/UserByRestId",
                "POST /i/api/graphql/synthetic-like-1/FavoriteTweet",
                "POST /i/api/1.1/friendships/create.json",
            ]
        );
        for name in CREDENTIAL_HEADERS {
            assert!(!seen[0].headers.contains_key(name), "catalogue {name}");
        }
        for request in &seen[1..] {
            let headers = &request.headers;
            assert_eq!(headers["x-csrf-token"], "CT0-SECRET");
            assert_eq!(headers["cookie"], "auth_token=AUTH-SECRET; ct0=CT0-SECRET");
            assert_eq!(headers["x-twitter-auth-type"], "OAuth2Session");
            assert_eq!(headers["user-agent"], BROWSER_USER_AGENT);
            assert_valid_id(request);
        }

        // Only the query ID, features and field toggles come from the catalogue.
        let variables = query_json(&seen[1], "variables").unwrap();
        assert_eq!(variables["focalTweetId"], FOCAL);
        assert!(variables.get("catalogue_only_variable").is_none());
        let features = query_json(&seen[1], "features").unwrap();
        let flags = json!({"synthetic_detail_feature": true, "synthetic_other_feature": false});
        assert_eq!(features, flags);
        let toggles = query_json(&seen[1], "fieldToggles").unwrap();
        assert_eq!(toggles, json!({"synthetic_detail_toggle": false}));
        assert!(seen[1].body.is_empty());

        let variables = query_json(&seen[2], "variables").unwrap();
        assert_eq!(variables["userId"], "2002");
        assert!(variables.get("catalogue_only_variable").is_none());
        let features = query_json(&seen[2], "features").unwrap();
        assert_eq!(features, json!({"synthetic_user_feature": true}));
        assert!(query_json(&seen[2], "fieldToggles").is_none());

        let content_type = seen[3].headers["content-type"].to_str().unwrap();
        assert_eq!(content_type, "application/json");
        assert!(seen[3].query.is_empty());
        let body: Value = serde_json::from_str(&seen[3].body).unwrap();
        let expected = json!({"queryId": "synthetic-like-1", "variables": {"tweet_id": "42"}});
        assert_eq!(body, expected);

        assert!(seen[4].query.is_empty());
        assert_eq!(
            seen[4].headers["content-type"],
            "application/x-www-form-urlencoded"
        );
        let pairs: Vec<&str> = seen[4].body.split('&').collect();
        assert!(pairs.contains(&"user_id=2002"));
        assert!(pairs.contains(&"skip_status=1"));
        let ids = pairs.iter().filter(|p| p.starts_with("user_id=")).count();
        assert_eq!(ids, 1);
    }

    #[tokio::test]
    async fn production_action_endpoints_are_pinned() {
        let api = XApi::new();
        let meta = parse_actions(ACTIONS).unwrap();
        let operation = meta.tweet_detail.unwrap();
        let variables = json!({"focalTweetId": "1"});
        let url = api.read_url("TweetDetail", &operation, &variables).unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("x.com"));
        assert_eq!(url.port(), None);
        assert_eq!(url.path(), "/i/api/graphql/synthetic-detail-1/TweetDetail");
        assert_eq!(
            api.friendships_create_url(),
            "https://x.com/i/api/1.1/friendships/create.json"
        );
    }

    #[tokio::test]
    async fn prepare_actions_is_anonymous_cached_and_resolves_each_operation_alone() {
        let log = Log::default();
        let server = serve(action_site(&log)).await;
        let api = injected(&server.base);
        let first = api.prepare_actions().await.unwrap();
        let second = api.prepare_actions().await.unwrap();
        assert!(first.is_complete() && second.is_complete());
        assert_eq!(count(&log, "/catalogue.json"), 1, "cached");
        let first_headers = log.lock().unwrap()[0].headers.clone();
        for name in CREDENTIAL_HEADERS {
            assert!(!first_headers.contains_key(name), "{name}");
        }

        let partial_log = Log::default();
        let body = Arc::new(ACTIONS.replace("UserByRestId", "Gone"));
        let respond = move |_: &Seen| reply(200, body.as_str());
        let server = serve(recording(Arc::clone(&partial_log), respond)).await;
        let api = injected(&server.base);
        let meta = api.prepare_actions().await.unwrap();
        assert!(meta.has_lookup_post());
        assert!(meta.has_like());
        assert!(!meta.has_lookup_user());
        api.prepare_actions().await.unwrap();
        assert_eq!(count(&partial_log, "/catalogue.json"), 1);
    }

    #[tokio::test]
    async fn prepare_actions_failures_are_not_cached() {
        let log = Log::default();
        let catalogue = Arc::new(Mutex::new((503, String::new())));
        let served = Arc::clone(&catalogue);
        let app = recording(Arc::clone(&log), move |_| {
            let (status, body) = served.lock().unwrap().clone();
            reply(status, body)
        });
        let server = serve(app).await;
        let api = injected(&server.base);

        let error = api.prepare_actions().await.unwrap_err();
        assert_eq!(error, XError::Status(503));
        *catalogue.lock().unwrap() = (200, LEAK.to_string());
        let error = api.prepare_actions().await.unwrap_err();
        assert_eq!(error, XError::Malformed);
        *catalogue.lock().unwrap() = (200, ACTIONS.to_string());
        assert!(api.prepare_actions().await.unwrap().is_complete());
        assert_eq!(count(&log, "/catalogue.json"), 3);
    }

    #[tokio::test]
    async fn readiness_is_cache_only_and_never_contacts_x() {
        let log = Log::default();
        let server = serve(action_site(&log)).await;
        let api = injected(&server.base);
        assert_eq!(readiness(&api).await, [false, false, false]);
        assert!(
            log.lock().unwrap().is_empty(),
            "nothing is fetched to answer"
        );

        api.prepare_actions().await.unwrap();
        let sent = log.lock().unwrap().len();
        for _ in 0..3 {
            assert_eq!(readiness(&api).await, [true, true, true]);
        }
        assert_eq!(log.lock().unwrap().len(), sent, "readiness asks no one");
    }

    #[tokio::test]
    async fn readiness_reports_each_operation_alone() {
        let cases = [
            ("TweetDetail", [false, true, true]),
            ("UserByRestId", [true, false, true]),
            ("FavoriteTweet", [true, true, false]),
        ];
        for (name, expected) in cases {
            let log = Log::default();
            let body = Arc::new(ACTIONS.replace(name, "Gone"));
            let respond = move |_: &Seen| reply(200, body.as_str());
            let server = serve(recording(Arc::clone(&log), respond)).await;
            let api = injected(&server.base);
            assert_eq!(readiness(&api).await, [false, false, false], "{name}");
            api.prepare_actions().await.unwrap();
            assert_eq!(readiness(&api).await, expected, "{name}");
            assert_eq!(count(&log, "/catalogue.json"), 1, "{name}");
        }
    }

    #[tokio::test]
    async fn readiness_is_false_when_the_catalogue_or_the_signer_fails() {
        let log = Log::default();
        let server = serve(recording(Arc::clone(&log), |_| reply(200, "{}"))).await;
        let api = injected(&server.base);
        api.prepare_actions().await.unwrap();
        assert_eq!(readiness(&api).await, [false, false, false]);

        let server = serve(recording(Log::default(), |_| reply(200, LEAK))).await;
        let api = injected(&server.base);
        assert!(api.prepare_actions().await.is_err());
        assert_eq!(readiness(&api).await, [false, false, false]);

        let server = serve(action_site(&Log::default())).await;
        let api = bootstrapping(&server.base, Duration::from_secs(5));
        assert!(api.prepare_actions().await.is_err());
        assert_eq!(readiness(&api).await, [false, false, false]);
    }

    #[tokio::test]
    async fn readiness_expires_with_the_cache_and_returns_after_a_refresh() {
        let log = Log::default();
        let server = serve(action_site(&log)).await;
        let api = expiring(&server.base, Duration::from_millis(300));
        api.prepare_actions().await.unwrap();
        assert_eq!(readiness(&api).await, [true, true, true]);

        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(readiness(&api).await, [false, false, false]);
        assert_eq!(count(&log, "/catalogue.json"), 1, "expiry fetches nothing");

        // A refresh reads the catalogue again. The stub site has no page to rebuild the expired
        // signer from, so it fails and nothing becomes ready.
        assert!(api.prepare_actions().await.is_err());
        assert_eq!(count(&log, "/catalogue.json"), 2);
        assert_eq!(readiness(&api).await, [false, false, false]);
    }

    #[tokio::test]
    async fn readiness_needs_a_live_signer_too() {
        let log = Log::default();
        let server = serve(action_site(&log)).await;
        let api = expiring(&server.base, Duration::from_millis(500));
        tokio::time::sleep(Duration::from_millis(300)).await;
        api.prepare_actions().await.unwrap();
        assert_eq!(readiness(&api).await, [true, true, true]);

        // The signer was built 600ms ago and has expired; the operations are 300ms old.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(readiness(&api).await, [false, false, false]);
    }

    #[tokio::test]
    async fn readiness_does_not_wait_for_a_refresh_in_flight() {
        let server = serve(Router::new().fallback(never_answers)).await;
        let mut api = injected(&server.base);
        api.action_timeout = Duration::from_millis(600);
        let refresh = api.prepare_actions();
        let probe = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let started = Instant::now();
            let ready = api.has_lookup_post().await;
            (ready, started.elapsed())
        };
        let (refreshed, (ready, waited)) = tokio::join!(refresh, probe);
        assert_eq!(refreshed.unwrap_err(), XError::Transport);
        assert!(!ready);
        assert!(waited < Duration::from_millis(300), "{waited:?}");
    }

    #[tokio::test]
    async fn prepare_actions_needs_the_signer_and_is_bounded_in_time() {
        let log = Log::default();
        let server = serve(action_site(&log)).await;
        let api = bootstrapping(&server.base, Duration::from_secs(5));
        let twitter = twitter();
        // The page that carries the signer is missing, so nothing is prepared or sent.
        let prepared = api.prepare_actions().await;
        let follow = api.follow_user(&twitter, "2002").await;
        assert_eq!(prepared.unwrap_err(), XError::Status(404));
        assert_eq!(follow.unwrap_err(), XError::Status(404));
        let paths: Vec<String> = log.lock().unwrap().iter().map(|s| s.path.clone()).collect();
        let allowed = ["/catalogue.json", "/home"];
        for path in &paths {
            assert!(allowed.contains(&path.as_str()), "{path}");
        }

        let server = serve(Router::new().fallback(never_answers)).await;
        let mut api = injected(&server.base);
        api.action_timeout = Duration::from_millis(200);
        let started = Instant::now();
        let error = api.prepare_actions().await.unwrap_err();
        assert_eq!(error, XError::Transport);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn missing_operations_fail_closed_one_at_a_time_without_sending() {
        let log = Log::default();
        let server = serve(action_site(&log)).await;
        let api = injected(&server.base);
        let twitter = twitter();

        let none = ActionMetadata::default();
        let post = api.lookup_post(&twitter, &none, FOCAL).await;
        let user = api.lookup_user(&twitter, &none, "2002").await;
        let like = api.like_post(&twitter, &none, "42").await;
        assert_eq!(post.unwrap_err(), XError::Malformed);
        assert_eq!(user.unwrap_err(), XError::Malformed);
        assert_eq!(like.unwrap_err(), XError::Malformed);
        assert!(log.lock().unwrap().is_empty());

        // Without the user operation, the others still work.
        let catalogue = ACTIONS.replace("UserByRestId", "Gone");
        let meta = parse_actions(&catalogue).unwrap();
        let user = api.lookup_user(&twitter, &meta, "2002").await;
        assert_eq!(user.unwrap_err(), XError::Malformed);
        let post = api.lookup_post(&twitter, &meta, FOCAL).await.unwrap();
        assert!(post.is_some());
        let like = api.like_post(&twitter, &meta, "42").await.unwrap();
        assert_eq!(like, liked(false));
        assert_eq!(count(&log, USER_PATH), 0);
        assert_eq!(count(&log, DETAIL_PATH), 1);
        assert_eq!(count(&log, LIKE_PATH), 1);

        // Without the like operation, reads and follow still work.
        let catalogue = ACTIONS.replace("FavoriteTweet", "Gone");
        let meta = parse_actions(&catalogue).unwrap();
        let like = api.like_post(&twitter, &meta, "42").await;
        assert_eq!(like.unwrap_err(), XError::Malformed);
        assert_eq!(count(&log, LIKE_PATH), 1);
        let user = api.lookup_user(&twitter, &meta, "2002").await.unwrap();
        assert!(user.is_some());
        assert!(api.follow_user(&twitter, "2002").await.is_ok());
    }

    #[tokio::test]
    async fn invalid_ids_never_reach_the_network() {
        let log = Log::default();
        let server = serve(action_site(&log)).await;
        let api = injected(&server.base);
        let meta = api.prepare_actions().await.unwrap();
        let twitter = twitter();
        let before = log.lock().unwrap().len();
        let too_long = "1".repeat(MAX_USER_ID_LEN + 1);
        let bad = [
            "",
            "abc",
            "12a",
            "1/2",
            "1&user_id=2",
            "../1",
            " 1",
            "1\n2",
            "\u{ff11}\u{ff12}",
            too_long.as_str(),
        ];
        for id in bad {
            let post = api.lookup_post(&twitter, &meta, id).await;
            let user = api.lookup_user(&twitter, &meta, id).await;
            let like = api.like_post(&twitter, &meta, id).await;
            let follow = api.follow_user(&twitter, id).await;
            assert_eq!(post.unwrap_err(), XError::Malformed, "{id:?}");
            assert_eq!(user.unwrap_err(), XError::Malformed, "{id:?}");
            assert_eq!(like.unwrap_err(), XError::Malformed, "{id:?}");
            assert_eq!(follow.unwrap_err(), XError::Malformed, "{id:?}");
        }
        assert_eq!(log.lock().unwrap().len(), before);
        let longest = "1".repeat(MAX_USER_ID_LEN);
        let result = api.lookup_user(&twitter, &meta, &longest).await;
        assert!(result.is_err());
        assert_eq!(count(&log, USER_PATH), 1, "the longest id is sent");
    }

    #[tokio::test]
    async fn read_failures_are_categorised_and_never_echo_the_body() {
        let cases = [
            (401, XError::Unauthorized),
            (403, XError::Forbidden),
            (429, XError::RateLimited),
            (404, XError::Status(404)),
            (500, XError::Status(500)),
            (200, XError::Malformed),
        ];
        let meta = parse_actions(ACTIONS).unwrap();
        for (status, expected) in cases {
            let server = serve(recording(Log::default(), move |_| reply(status, LEAK))).await;
            let api = injected(&server.base);
            let post = api.lookup_post(&twitter(), &meta, FOCAL).await;
            let user = api.lookup_user(&twitter(), &meta, "2002").await;
            assert_eq!(post.unwrap_err(), expected, "post {status}");
            assert_eq!(user.unwrap_err(), expected, "user {status}");
            let shown = format!("{expected} {expected:?}");
            assert!(!shown.contains("SYNTHETIC"));
            assert!(!shown.contains("SECRET"));
        }
    }

    #[tokio::test]
    async fn write_failures_are_certain_only_when_x_refused_the_request() {
        let cases = [
            (401, XError::Unauthorized),
            (403, XError::Forbidden),
            (429, XError::RateLimited),
            (400, XError::Status(400)),
            (404, XError::Status(404)),
            (409, XError::Status(409)),
            (500, XError::Uncertain),
            (502, XError::Uncertain),
            (503, XError::Uncertain),
            (302, XError::Uncertain),
            (200, XError::Uncertain),
        ];
        let meta = parse_actions(ACTIONS).unwrap();
        for (status, expected) in cases {
            let log = Log::default();
            let server = serve(recording(Arc::clone(&log), move |_| reply(status, LEAK))).await;
            let api = injected(&server.base);
            let like = api.like_post(&twitter(), &meta, "42").await;
            let follow = api.follow_user(&twitter(), "2002").await;
            assert_eq!(like.unwrap_err(), expected, "like {status}");
            assert_eq!(follow.unwrap_err(), expected, "follow {status}");
            assert_eq!(count(&log, LIKE_PATH), 1, "{status} is not retried");
            assert_eq!(count(&log, FOLLOW_PATH), 1, "{status} is not retried");
            let shown = format!("{expected} {expected:?}");
            assert!(!shown.contains("SYNTHETIC"));
            assert!(!shown.contains("SECRET"));
        }
        assert!(!XError::Uncertain.is_fatal());
    }

    #[tokio::test]
    async fn an_answer_that_proves_nothing_is_uncertain_not_a_failure() {
        let meta = parse_actions(ACTIONS).unwrap();
        let like_bodies = [
            r#"{"errors":[{"message":"x","code":144}],"data":{}}"#,
            r#"{"data":{}}"#,
            "{}",
            "not json",
        ];
        for body in like_bodies {
            let server = serve(recording(Log::default(), move |_| reply(200, body))).await;
            let api = injected(&server.base);
            let like = api.like_post(&twitter(), &meta, "42").await;
            assert_eq!(like.unwrap_err(), XError::Uncertain, "{body}");
        }
        let follow_bodies = [
            r#"{"id_str":"999","following":true}"#,
            r#"{"errors":[{"code":160,"message":"x"}]}"#,
            "{}",
            "not json",
        ];
        for body in follow_bodies {
            let server = serve(recording(Log::default(), move |_| reply(200, body))).await;
            let api = injected(&server.base);
            let follow = api.follow_user(&twitter(), "2002").await;
            assert_eq!(follow.unwrap_err(), XError::Uncertain, "{body}");
        }
    }

    #[tokio::test]
    async fn an_already_favorited_post_is_a_success() {
        let meta = parse_actions(ACTIONS).unwrap();
        let body = r#"{"errors":[{"message":"x","code":139}],"data":{}}"#;
        for status in [200, 403] {
            let log = Log::default();
            let server = serve(recording(Arc::clone(&log), move |_| reply(status, body))).await;
            let api = injected(&server.base);
            let result = api.like_post(&twitter(), &meta, "42").await.unwrap();
            assert_eq!(result, liked(true), "{status}");
            assert_eq!(count(&log, LIKE_PATH), 1);
        }
    }

    #[tokio::test]
    async fn a_forbidden_like_is_forbidden_unless_x_says_already_favorited() {
        let meta = parse_actions(ACTIONS).unwrap();
        let bodies = [
            LEAK,
            "",
            LIKED,
            r#"{"errors":[{"code":144}]}"#,
            r#"{"errors":[{"code":139},{"code":226}]}"#,
        ];
        for body in bodies {
            let server = serve(recording(Log::default(), move |_| reply(403, body))).await;
            let api = injected(&server.base);
            let like = api.like_post(&twitter(), &meta, "42").await;
            assert_eq!(like.unwrap_err(), XError::Forbidden, "{body}");
        }
    }

    #[tokio::test]
    async fn oversized_action_responses_are_rejected() {
        let size = MAX_RESPONSE_BYTES + 1;
        let app = recording(Log::default(), move |_| reply(200, "a".repeat(size)));
        let server = serve(app).await;
        let api = injected(&server.base);
        let meta = parse_actions(ACTIONS).unwrap();
        let post = api.lookup_post(&twitter(), &meta, FOCAL).await;
        let user = api.lookup_user(&twitter(), &meta, "2002").await;
        let like = api.like_post(&twitter(), &meta, "42").await;
        let follow = api.follow_user(&twitter(), "2002").await;
        assert_eq!(post.unwrap_err(), XError::Malformed);
        assert_eq!(user.unwrap_err(), XError::Malformed);
        assert_eq!(like.unwrap_err(), XError::Uncertain);
        assert_eq!(follow.unwrap_err(), XError::Uncertain);
    }

    #[tokio::test]
    async fn slow_responses_time_out_and_a_slow_write_is_uncertain() {
        let server = serve(Router::new().fallback(never_answers)).await;
        let mut api = injected(&server.base);
        api.action_timeout = Duration::from_millis(200);
        let meta = parse_actions(ACTIONS).unwrap();
        let started = Instant::now();
        let post = api.lookup_post(&twitter(), &meta, FOCAL).await;
        let user = api.lookup_user(&twitter(), &meta, "2002").await;
        let like = api.like_post(&twitter(), &meta, "42").await;
        let follow = api.follow_user(&twitter(), "2002").await;
        assert_eq!(post.unwrap_err(), XError::Transport);
        assert_eq!(user.unwrap_err(), XError::Transport);
        assert_eq!(like.unwrap_err(), XError::Uncertain);
        assert_eq!(follow.unwrap_err(), XError::Uncertain);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn a_refused_connection_is_unsent_but_a_dropped_one_is_uncertain() {
        use tokio::io::AsyncReadExt;

        let meta = parse_actions(ACTIONS).unwrap();
        let closed = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let api = injected(&closed);
        let post = api.lookup_post(&twitter(), &meta, FOCAL).await;
        let like = api.like_post(&twitter(), &meta, "42").await;
        let follow = api.follow_user(&twitter(), "2002").await;
        assert_eq!(post.unwrap_err(), XError::Transport);
        assert_eq!(like.unwrap_err(), XError::Transport);
        assert_eq!(follow.unwrap_err(), XError::Transport);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer).await;
            }
        });
        let server = TestServer { base, task };
        let api = injected(&server.base);
        let post = api.lookup_post(&twitter(), &meta, FOCAL).await;
        let like = api.like_post(&twitter(), &meta, "42").await;
        let follow = api.follow_user(&twitter(), "2002").await;
        assert_eq!(post.unwrap_err(), XError::Transport);
        assert_eq!(like.unwrap_err(), XError::Uncertain);
        assert_eq!(follow.unwrap_err(), XError::Uncertain);
    }

    #[tokio::test]
    async fn action_requests_never_follow_redirects() {
        let sink_log = Log::default();
        let sink = serve(recording(Arc::clone(&sink_log), |_| reply(200, "{}"))).await;
        let location = format!("{}/elsewhere", sink.base);
        let redirect = {
            let location = location.clone();
            move |_: &Seen| (StatusCode::FOUND, [("location", location.clone())]).into_response()
        };
        let server = serve(recording(Log::default(), redirect)).await;
        let api = injected(&server.base);
        let meta = parse_actions(ACTIONS).unwrap();

        let prepared = api.prepare_actions().await;
        assert_eq!(prepared.unwrap_err(), XError::Status(302));
        let post = api.lookup_post(&twitter(), &meta, FOCAL).await;
        let user = api.lookup_user(&twitter(), &meta, "2002").await;
        let like = api.like_post(&twitter(), &meta, "42").await;
        let follow = api.follow_user(&twitter(), "2002").await;
        assert_eq!(post.unwrap_err(), XError::Status(302));
        assert_eq!(user.unwrap_err(), XError::Status(302));
        assert_eq!(like.unwrap_err(), XError::Uncertain);
        assert_eq!(follow.unwrap_err(), XError::Uncertain);
        assert!(sink_log.lock().unwrap().is_empty());
    }

    /// 実際の x.com へ Cookie なしで 1 回だけ bootstrap する手動確認。`cargo test -- --ignored`。
    #[tokio::test]
    #[ignore = "実際の x.com に接続する"]
    async fn live_anonymous_bootstrap_builds_a_generator() {
        let transaction = XApi::new().bootstrap().await.expect("匿名 bootstrap");
        let id = transaction.generate("GET", "/1.1/account/settings.json");
        assert!(id.len() > 60 && !id.contains('='));
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
