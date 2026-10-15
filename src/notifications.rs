//! Local history of received notifications, persisted in SQLite next to the config file.
//!
//! The history lives in `<config>.notifications.sqlite3` (`<config>.<id>.notifications.sqlite3` for
//! accounts other than `default`; mode 0600) and is never pruned.
//! Rows are ordered by a monotonically increasing sequence number, so listing is a keyset
//! scan over that number and never loads the whole history into memory.

use crate::config::{self, NotificationFilter};
use crate::error::{AngelicAngelError, Result};
use crate::tweet_lookup::{TweetLookup, TweetShape};
use regex::Regex;
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: i64 = 3;
const BACKFILL_CHUNK: i64 = 200;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PAGE_LIMIT: usize = 100;
const MAX_PAYLOAD_BYTES: usize = 16 * 1024;
const TRUNCATED_RAW_BYTES: usize = 6 * 1024;
const MAX_TITLE_CHARS: usize = 200;
const MAX_BODY_CHARS: usize = 2000;
const MAX_URL_BYTES: usize = 2048;
const MAX_ID_BYTES: usize = 512;

const LINK_HOSTS: &[&str] = &["x.com", "www.x.com", "twitter.com", "www.twitter.com"];
const ICON_HOSTS: &[&str] = &["pbs.twimg.com", "abs.twimg.com"];

fn schema_sql() -> String {
    format!(
        "
CREATE TABLE IF NOT EXISTS notifications (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    received_at INTEGER NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    url TEXT,
    icon TEXT,
    payload TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'other'
);
PRAGMA user_version = {SCHEMA_VERSION};
"
    )
}

/// 通知の区分。`data.type` から決まるが、`Retweet` は X の `data.type` に無く、投稿の中身から導く。
/// `Quote`・`SelfQuote`・`Reply` は payload だけでは判別できず、[`resolve_kind`] が公開 API への照会で決める。
/// `Quote` は他のアカウントの投稿の引用RT、`SelfQuote` は自分自身の投稿の引用RT。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    Tweet,
    Retweet,
    Quote,
    SelfQuote,
    Reply,
    XchatMessageReceived,
    Other,
}

impl NotificationKind {
    /// X が `data.type` に載せる値。`Retweet`・`Quote`・`SelfQuote`・`Reply` は含まない。
    const DATA_TYPES: [NotificationKind; 2] = [Self::Tweet, Self::XchatMessageReceived];

    fn as_str(self) -> &'static str {
        match self {
            Self::Tweet => "tweet",
            Self::Retweet => "retweet",
            Self::Quote => "quote",
            Self::SelfQuote => "self_quote",
            Self::Reply => "reply",
            Self::XchatMessageReceived => "xchat_message_received",
            Self::Other => "other",
        }
    }

    /// payload の `data.type` を読む。`"retweet"`・`"quote"`・`"self_quote"`・`"reply"` という `data.type` は存在しないので `Other` にする。
    fn from_data_type(value: &str) -> Self {
        Self::DATA_TYPES
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .unwrap_or(Self::Other)
    }

    /// DB の `kind` 列に保存した文字列を戻す。
    fn from_stored(value: &str) -> Self {
        [Self::Retweet, Self::Quote, Self::SelfQuote, Self::Reply]
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .unwrap_or_else(|| Self::from_data_type(value))
    }
}

static RETWEET_URI: LazyLock<Regex> = LazyLock::new(|| compile(r"^/[^/]+/status/([0-9]+)$"));
static RETWEET_TAG: LazyLock<Regex> = LazyLock::new(|| compile(r"^tweet-([0-9]+)$"));

fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).expect("静的パターンは常にコンパイルできる")
}

/// `notification.data.type` を優先し、文字列が無いときだけ外側の `data.type` を読む。
/// `type` が `tweet` で、同じ `data` の `uri`（元ツイート）と `tag`（通知自身）の ID が
/// 食い違うときだけ `Retweet` とする。どちらかが欠けるか形式が違えば `Tweet` のまま残す。
/// 本文や `scribe_target` からは推定しない。
pub fn classify(payload: &Value) -> NotificationKind {
    let Some((data, data_type)) = typed_data(payload) else {
        return NotificationKind::Other;
    };
    match NotificationKind::from_data_type(data_type) {
        NotificationKind::Tweet if is_retweet(data) => NotificationKind::Retweet,
        kind => kind,
    }
}

/// `notification.data` を優先し、そこに文字列の `type` が無いときだけ外側の `data` を読む。
fn typed_data(payload: &Value) -> Option<(&Value, &str)> {
    fn scoped(scope: &Value) -> Option<(&Value, &str)> {
        let data = scope.get("data")?;
        let data_type = data.get("type")?.as_str()?;
        Some((data, data_type))
    }
    payload
        .get("notification")
        .and_then(scoped)
        .or_else(|| scoped(payload))
}

/// `data` の文字列 `key` が `pattern` に合うとき、その最初のキャプチャ（ツイート ID）を返す。
fn captured_id(data: &Value, key: &str, pattern: &Regex) -> Option<String> {
    let text = data.get(key)?.as_str()?;
    Some(pattern.captures(text)?.get(1)?.as_str().to_owned())
}

fn is_retweet(data: &Value) -> bool {
    match (
        captured_id(data, "uri", &RETWEET_URI),
        captured_id(data, "tag", &RETWEET_TAG),
    ) {
        (Some(source), Some(own)) => source != own,
        _ => false,
    }
}

/// 通知自身のツイート ID（`classify` と同じ `data` の `tag` が `tweet-<数字>` のとき）。
fn tweet_id(payload: &Value) -> Option<String> {
    let (data, _) = typed_data(payload)?;
    captured_id(data, "tag", &RETWEET_TAG)
}

/// 履歴とフィルターに使う最終的な区分。
/// 公開 API に照会するのは `classify` が `Tweet` とした投稿だけにする。リツイートの ID を照会すると
/// 元ツイートの形が返り、引用RT と誤判定するため。照会できなかったときは `Tweet` のまま残す。
pub async fn resolve_kind(payload: &Value, lookup: &TweetLookup) -> NotificationKind {
    let kind = classify(payload);
    if kind != NotificationKind::Tweet {
        return kind;
    }
    let Some(id) = tweet_id(payload) else {
        return kind;
    };
    match lookup.shape(&id).await {
        Some(TweetShape::Reply) => NotificationKind::Reply,
        Some(TweetShape::Quote) => NotificationKind::Quote,
        Some(TweetShape::SelfQuote) => NotificationKind::SelfQuote,
        Some(TweetShape::Plain) | None => kind,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NotificationItem {
    pub id: String,
    /// Unix time in seconds when the notification was received.
    pub received_at: u64,
    pub title: String,
    pub body: String,
    pub url: Option<String>,
    pub icon: Option<String>,
    pub payload: Value,
    pub kind: NotificationKind,
}

#[derive(Debug, Clone, Serialize)]
pub struct NotificationPage {
    pub items: Vec<NotificationItem>,
    pub next_cursor: Option<String>,
    /// この応答の抽出に実際に適用したフィルター。
    pub notification_filter: NotificationFilter,
}

/// Records a received notification as `kind` in the history of `account_id`. A repeated `id` is
/// ignored.
pub fn append_as_for(
    config_path: &Path,
    account_id: &str,
    id: &str,
    payload: &Value,
    kind: NotificationKind,
) -> Result<()> {
    let db = history_path_for(config_path, account_id);
    append_to(&db, id, payload, kind, now_secs(), BUSY_TIMEOUT)
}

/// Records a received notification as `kind` in the `default` account's history.
#[cfg(test)]
pub fn append_as(
    config_path: &Path,
    id: &str,
    payload: &Value,
    kind: NotificationKind,
) -> Result<()> {
    append_as_for(config_path, config::DEFAULT_ACCOUNT_ID, id, payload, kind)
}

/// payload から `classify` した区分で記録する。
#[cfg(test)]
pub fn append(config_path: &Path, id: &str, payload: &Value) -> Result<()> {
    append_as(config_path, id, payload, classify(payload))
}

/// フィルターを適用せずに一覧を返す。
#[cfg(test)]
pub fn list(config_path: &Path, before: Option<&str>, limit: usize) -> Result<NotificationPage> {
    list_filtered(config_path, before, limit, &NotificationFilter::default())
}

/// Returns up to `limit` (clamped to 1..=100) entries of `account_id`'s history older than the
/// `before` cursor (an entry id returned as `next_cursor`), newest first. Entries whose kind is
/// disabled in `filter` are skipped in SQL before the limit applies; the cursor may be any entry
/// stored for that account, and an id from another account's history is unknown.
pub fn list_filtered_for(
    config_path: &Path,
    account_id: &str,
    before: Option<&str>,
    limit: usize,
    filter: &NotificationFilter,
) -> Result<NotificationPage> {
    list_in(
        &history_path_for(config_path, account_id),
        before,
        limit,
        filter,
        BUSY_TIMEOUT,
    )
}

/// [`list_filtered_for`] for the `default` account.
#[cfg(test)]
pub fn list_filtered(
    config_path: &Path,
    before: Option<&str>,
    limit: usize,
    filter: &NotificationFilter,
) -> Result<NotificationPage> {
    list_with_timeout(config_path, before, limit, filter, BUSY_TIMEOUT)
}

#[cfg(test)]
fn list_with_timeout(
    config_path: &Path,
    before: Option<&str>,
    limit: usize,
    filter: &NotificationFilter,
    busy_timeout: Duration,
) -> Result<NotificationPage> {
    list_in(
        &history_path(config_path),
        before,
        limit,
        filter,
        busy_timeout,
    )
}

fn list_in(
    path: &Path,
    before: Option<&str>,
    limit: usize,
    filter: &NotificationFilter,
    busy_timeout: Duration,
) -> Result<NotificationPage> {
    let limit = limit.clamp(1, MAX_PAGE_LIMIT);
    if !path.try_exists().map_err(io_error)? {
        return match before {
            None => Ok(NotificationPage {
                items: Vec::new(),
                next_cursor: None,
                notification_filter: *filter,
            }),
            Some(_) => Err(unknown_cursor()),
        };
    }

    let mut conn = open(path, busy_timeout)?;
    let tx = conn.transaction().map_err(db_error)?;
    let before_seq = match before {
        None => i64::MAX,
        Some(cursor) => tx
            .query_row(
                "SELECT seq FROM notifications WHERE id = ?1",
                [cursor],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(db_error)?
            .ok_or_else(unknown_cursor)?,
    };

    let mut items = {
        let mut stmt = tx
            .prepare(
                "SELECT id, received_at, title, body, url, icon, payload, kind FROM notifications \
                 WHERE seq < ?1 AND ( \
                     (kind = 'tweet' AND ?3) \
                     OR (kind = 'retweet' AND ?4) \
                     OR (kind = 'quote' AND ?5) \
                     OR (kind = 'reply' AND ?6) \
                     OR (kind = 'xchat_message_received' AND ?7) \
                     OR (kind = 'self_quote' AND ?9) \
                     OR (kind NOT IN ( \
                         'tweet', 'retweet', 'quote', 'self_quote', 'reply', \
                         'xchat_message_received' \
                     ) AND ?8) \
                 ) ORDER BY seq DESC LIMIT ?2",
            )
            .map_err(db_error)?;
        let fetch = i64::try_from(limit + 1).unwrap_or(i64::MAX);
        stmt.query_map(
            params![
                before_seq,
                fetch,
                filter.tweet,
                filter.retweet,
                filter.quote,
                filter.reply,
                filter.xchat_message_received,
                filter.other,
                filter.quote_self
            ],
            |row| {
                let url: Option<String> = row.get(4)?;
                let icon: Option<String> = row.get(5)?;
                let payload: String = row.get(6)?;
                let kind: String = row.get(7)?;
                Ok(NotificationItem {
                    id: row.get(0)?,
                    received_at: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
                    title: row.get(2)?,
                    body: row.get(3)?,
                    url: url.as_deref().and_then(sanitize_link),
                    icon: icon.as_deref().and_then(sanitize_icon),
                    payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
                    kind: NotificationKind::from_stored(&kind),
                })
            },
        )
        .map_err(db_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(db_error)?
    };

    let next_cursor = if items.len() > limit {
        items.truncate(limit);
        items.last().map(|item| item.id.clone())
    } else {
        None
    };
    Ok(NotificationPage {
        items,
        next_cursor,
        notification_filter: *filter,
    })
}

#[cfg(test)]
fn append_at(config_path: &Path, id: &str, payload: &Value, received_at: u64) -> Result<()> {
    let kind = classify(payload);
    append_with_timeout(config_path, id, payload, kind, received_at, BUSY_TIMEOUT)
}

#[cfg(test)]
fn append_with_timeout(
    config_path: &Path,
    id: &str,
    payload: &Value,
    kind: NotificationKind,
    received_at: u64,
    busy_timeout: Duration,
) -> Result<()> {
    append_to(
        &history_path(config_path),
        id,
        payload,
        kind,
        received_at,
        busy_timeout,
    )
}

/// Stores the entry in the history database `db` and returns `Ok` only once the transaction has
/// committed (or the id was already stored). Locked, full, corrupt and unwritable databases are
/// errors, never a no-op.
fn append_to(
    db: &Path,
    id: &str,
    payload: &Value,
    kind: NotificationKind,
    received_at: u64,
    busy_timeout: Duration,
) -> Result<()> {
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return Err(AngelicAngelError::Config(
            "invalid notification id".to_string(),
        ));
    }
    let item = build_item(id, payload, kind, received_at);
    let mut conn = open(db, busy_timeout)?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    insert(&tx, &item).map_err(db_error)?;
    tx.commit().map_err(db_error)
}

/// Inserts `item` unless its id is already stored. Returns whether a row was added.
/// Only a conflict on `id` is tolerated; any other constraint failure surfaces as an error.
fn insert(conn: &Connection, item: &NotificationItem) -> rusqlite::Result<bool> {
    let changed = conn
        .prepare(
            "INSERT INTO notifications \
             (id, received_at, title, body, url, icon, payload, kind) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(id) DO NOTHING",
        )?
        .execute(params![
            item.id,
            i64::try_from(item.received_at).unwrap_or(i64::MAX),
            item.title,
            item.body,
            item.url,
            item.icon,
            item.payload.to_string(),
            item.kind.as_str(),
        ])?;
    Ok(changed == 1)
}

/// Opens the history database, creating the file (mode 0600) and the schema when missing.
fn open(path: &Path, busy_timeout: Duration) -> Result<Connection> {
    ensure_private_file(path).map_err(io_error)?;
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(db_error)?;
    conn.busy_timeout(busy_timeout).map_err(db_error)?;

    let version = user_version(&conn)?;
    if !(0..=SCHEMA_VERSION).contains(&version) {
        return Err(unsupported_schema());
    }
    if version != SCHEMA_VERSION {
        migrate(&mut conn)?;
    }
    Ok(conn)
}

fn user_version(conn: &Connection) -> Result<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(db_error)
}

/// 新規作成と v1→v2、v2→v3 を単一の書き込みトランザクションで行う。
/// 他プロセスの完了を待つ間に状況が変わるため、ロック取得後に版を読み直す。
fn migrate(conn: &mut Connection) -> Result<()> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    match user_version(&tx)? {
        0 => tx.execute_batch(&schema_sql()).map_err(db_error)?,
        // v1→v2 は現在の `classify` で全行を分類し直すので、そのまま最新の版になる。
        1 => migrate_v1_to_v2(&tx)?,
        2 => migrate_v2_to_v3(&tx)?,
        SCHEMA_VERSION => {},
        _ => return Err(unsupported_schema()),
    }
    tx.commit().map_err(db_error)
}

fn unsupported_schema() -> AngelicAngelError {
    AngelicAngelError::Config("notification history has an unsupported schema version".to_string())
}

/// `kind` 列を足し、既存の payload を分類して埋める。行は小分けに読み、履歴全体をメモリに載せない。
/// 不正な JSON や切り詰め済みで復元できない payload は `other` のまま残る。
fn migrate_v1_to_v2(tx: &Connection) -> Result<()> {
    tx.execute_batch("ALTER TABLE notifications ADD COLUMN kind TEXT NOT NULL DEFAULT 'other'")
        .map_err(db_error)?;
    reclassify(tx, None)?;
    set_schema_version(tx)
}

/// `retweet` 区分の導入前に `tweet` として保存された行のうち、リツイートだったものを分類し直す。
/// 切り詰め済みなどで payload から判別できない行は `tweet` のまま残る。
fn migrate_v2_to_v3(tx: &Connection) -> Result<()> {
    reclassify(tx, Some(NotificationKind::Tweet))?;
    set_schema_version(tx)
}

/// `only` を指定すると、その区分で保存済みの行だけを読み、区分が変わるものを更新する。
/// 行は `BACKFILL_CHUNK` 件ずつ読む。
fn reclassify(tx: &Connection, only: Option<NotificationKind>) -> Result<()> {
    let mut last_seq = 0_i64;
    loop {
        let chunk = {
            let mut stmt = tx
                .prepare(
                    "SELECT seq, payload FROM notifications WHERE seq > ?1 \
                     AND (?3 IS NULL OR kind = ?3) ORDER BY seq LIMIT ?2",
                )
                .map_err(db_error)?;
            stmt.query_map(
                params![last_seq, BACKFILL_CHUNK, only.map(NotificationKind::as_str)],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(db_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_error)?
        };
        let Some((newest, _)) = chunk.last() else {
            break;
        };
        last_seq = *newest;
        let mut update = tx
            .prepare("UPDATE notifications SET kind = ?1 WHERE seq = ?2")
            .map_err(db_error)?;
        for (seq, payload) in &chunk {
            let kind = serde_json::from_str::<Value>(payload)
                .map_or(NotificationKind::Other, |value| classify(&value));
            if kind != NotificationKind::Other && Some(kind) != only {
                update
                    .execute(params![kind.as_str(), seq])
                    .map_err(db_error)?;
            }
        }
    }
    Ok(())
}

fn set_schema_version(tx: &Connection) -> Result<()> {
    tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
        .map_err(db_error)
}

/// Creates the file with mode 0600 when absent and tightens an existing file to 0600.
/// SQLite gives its rollback journal the main file's mode, and the default journal mode
/// creates no WAL/SHM files. SQLite owns those sidecar files; they are never deleted here.
fn ensure_private_file(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file.metadata()?.permissions().mode() & 0o777 != 0o600 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
    }
    drop(file);
    Ok(())
}

fn unknown_cursor() -> AngelicAngelError {
    AngelicAngelError::Config("unknown notification cursor".to_string())
}

/// Maps a database failure to a fixed message; the cause is only logged at debug level.
fn db_error(e: rusqlite::Error) -> AngelicAngelError {
    tracing::debug!(error = %e, "notification history database error");
    let message = match e.sqlite_error_code() {
        Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt) => {
            "notification history is unreadable"
        },
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => "notification history is busy",
        _ => "notification history is unavailable",
    };
    AngelicAngelError::Config(message.to_string())
}

fn io_error(e: std::io::Error) -> AngelicAngelError {
    tracing::debug!(error = %e, "notification history file error");
    AngelicAngelError::Config("notification history is unavailable".to_string())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
fn history_path(config_path: &Path) -> PathBuf {
    history_path_for(config_path, config::DEFAULT_ACCOUNT_ID)
}

/// `default` keeps the single-account file name so an existing history is picked up as is.
pub fn history_path_for(config_path: &Path, account_id: &str) -> PathBuf {
    let mut name = config_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "angelic-angel.toml".into());
    if account_id != config::DEFAULT_ACCOUNT_ID {
        name.push(".");
        name.push(account_id);
    }
    name.push(".notifications.sqlite3");
    config_path.with_file_name(name)
}

fn build_item(
    id: &str,
    payload: &Value,
    kind: NotificationKind,
    received_at: u64,
) -> NotificationItem {
    let summary = summarize(payload);
    NotificationItem {
        id: id.to_string(),
        received_at,
        title: summary.title,
        body: summary.body,
        url: summary.url,
        icon: summary.icon,
        kind,
        payload: bound_payload(payload),
    }
}

struct Summary {
    title: String,
    body: String,
    url: Option<String>,
    icon: Option<String>,
}

/// Derives display fields from a payload of the shape
/// `{title, body, icon, data: {url}}`, optionally wrapped in a `notification` object.
/// Anything that does not fit is left empty; the original payload is kept separately.
fn summarize(payload: &Value) -> Summary {
    let mut summary = Summary {
        title: String::new(),
        body: String::new(),
        url: None,
        icon: None,
    };
    let map = match payload {
        Value::Object(map) => map,
        Value::String(text) => {
            summary.body = bounded(text.trim(), MAX_BODY_CHARS);
            return summary;
        },
        Value::Null => return summary,
        other => {
            summary.body = bounded(&other.to_string(), MAX_BODY_CHARS);
            return summary;
        },
    };

    let nested = map.get("notification").and_then(Value::as_object);
    let scopes: Vec<&Map<String, Value>> = nested.into_iter().chain(Some(map)).collect();
    let text = |key: &str| {
        scopes
            .iter()
            .find_map(|scope| scope.get(key).and_then(Value::as_str))
    };

    summary.title = bounded(text("title").unwrap_or_default().trim(), MAX_TITLE_CHARS);
    summary.body = bounded(text("body").unwrap_or_default().trim(), MAX_BODY_CHARS);
    if summary.title.is_empty()
        && summary.body.is_empty()
        && let Some(raw) = map.get("raw").and_then(Value::as_str)
    {
        summary.body = bounded(raw.trim(), MAX_BODY_CHARS);
    }

    summary.icon = scopes
        .iter()
        .filter_map(|scope| scope.get("icon").and_then(Value::as_str))
        .find_map(sanitize_icon);

    let mut links: Vec<&str> = Vec::new();
    for scope in &scopes {
        if let Some(data) = scope.get("data").and_then(Value::as_object) {
            links.extend(
                ["url", "uri"]
                    .iter()
                    .filter_map(|key| data.get(*key))
                    .filter_map(Value::as_str),
            );
        }
    }
    for scope in &scopes {
        links.extend(scope.get("url").and_then(Value::as_str));
    }
    summary.url = links.into_iter().find_map(sanitize_link);

    summary
}

/// Keeps the payload verbatim when it fits, otherwise replaces it with a truncation marker
/// carrying a bounded preview of its compact JSON.
fn bound_payload(payload: &Value) -> Value {
    let raw = payload.to_string();
    if raw.len() <= MAX_PAYLOAD_BYTES {
        return payload.clone();
    }
    let mut end = TRUNCATED_RAW_BYTES;
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    json!({
        "truncated": true,
        "original_bytes": raw.len(),
        "raw": &raw[..end],
    })
}

fn bounded(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn sanitize_link(raw: &str) -> Option<String> {
    sanitize_https(raw, LINK_HOSTS)
}

fn sanitize_icon(raw: &str) -> Option<String> {
    sanitize_https(raw, ICON_HOSTS)
}

/// Accepts only `https://<host>[/...]` where `<host>` is exactly one of `hosts`:
/// no userinfo, port, backslash, whitespace, control or non-ASCII characters.
fn sanitize_https(raw: &str, hosts: &[&str]) -> Option<String> {
    let url = raw.trim();
    if url.is_empty() || url.len() > MAX_URL_BYTES || !url.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    if url.contains('\\') {
        return None;
    }
    let (scheme, rest) = url.split_at_checked("https://".len())?;
    if !scheme.eq_ignore_ascii_case("https://") {
        return None;
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.contains(['@', ':']) {
        return None;
    }
    let host = authority.to_ascii_lowercase();
    hosts.contains(&host.as_str()).then(|| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_temp_dir;

    fn config_path() -> PathBuf {
        test_temp_dir().join("c.toml")
    }

    fn db_file(config: &Path) -> PathBuf {
        config.with_file_name("c.toml.notifications.sqlite3")
    }

    #[test]
    fn history_path_for_keeps_the_default_name_and_adds_the_id_for_others() {
        let config = Path::new("/data/angelic-angel.toml");
        assert_eq!(
            history_path_for(config, config::DEFAULT_ACCOUNT_ID),
            Path::new("/data/angelic-angel.toml.notifications.sqlite3")
        );
        assert_eq!(history_path(config), history_path_for(config, "default"));
        assert_eq!(
            history_path_for(config, "work"),
            Path::new("/data/angelic-angel.toml.work.notifications.sqlite3")
        );
        assert_eq!(
            history_path_for(Path::new("c.toml"), "a-1_b"),
            Path::new("c.toml.a-1_b.notifications.sqlite3")
        );
        assert_ne!(history_path_for(config, "a"), history_path_for(config, "b"));
    }

    fn list_for(
        path: &Path,
        account_id: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<NotificationPage> {
        list_filtered_for(
            path,
            account_id,
            before,
            limit,
            &NotificationFilter::default(),
        )
    }

    #[test]
    fn listing_reads_only_the_history_file_of_the_requested_account() {
        let path = config_path();
        let tweet = json!({"title": "t", "data": {"type": "tweet"}});
        for (account, entries) in [
            (config::DEFAULT_ACCOUNT_ID, ["d-1", "d-2"].as_slice()),
            ("work", ["w-1", "w-2", "w-3"].as_slice()),
            ("shop", ["s-1"].as_slice()),
        ] {
            for id in entries {
                append_as_for(&path, account, id, &tweet, NotificationKind::Tweet).unwrap();
            }
        }
        for account in ["work", "shop"] {
            assert!(history_path_for(&path, account).is_file(), "{account}");
        }

        let names = |account: &str| -> Vec<String> {
            list_for(&path, account, None, 10)
                .unwrap()
                .items
                .into_iter()
                .map(|i| i.id)
                .collect()
        };
        assert_eq!(names(config::DEFAULT_ACCOUNT_ID), ["d-2", "d-1"]);
        assert_eq!(names("work"), ["w-3", "w-2", "w-1"]);
        assert_eq!(names("shop"), ["s-1"]);
        // The default account's listing is the file the old single-account code read.
        assert_eq!(
            ids(&list(&path, None, 10).unwrap()),
            ["d-2", "d-1"],
            "default keeps the legacy file"
        );

        // An account with no history yet is empty, and does not create a file by being read.
        let empty = list_for(&path, "idle", None, 10).unwrap();
        assert!(empty.items.is_empty() && empty.next_cursor.is_none());
        assert!(!history_path_for(&path, "idle").exists());
    }

    #[test]
    fn a_cursor_from_one_account_is_unknown_to_every_other_account() {
        let path = config_path();
        let tweet = json!({"title": "t", "data": {"type": "tweet"}});
        for id in ["a-1", "a-2", "a-3"] {
            append_as_for(&path, "one", id, &tweet, NotificationKind::Tweet).unwrap();
        }
        // The same entry id can exist in both histories without the two ever mixing.
        append_as_for(&path, "two", "a-1", &tweet, NotificationKind::Tweet).unwrap();
        append_as_for(&path, "two", "b-1", &tweet, NotificationKind::Tweet).unwrap();

        let page = list_for(&path, "one", None, 2).unwrap();
        assert_eq!(ids(&page), ["a-3", "a-2"]);
        let cursor = page.next_cursor.unwrap();
        assert_eq!(cursor, "a-2");

        // Paging continues inside the same account ...
        let rest = list_for(&path, "one", Some(&cursor), 2).unwrap();
        assert_eq!(ids(&rest), ["a-1"]);
        // ... a cursor that only exists in another account's history is rejected ...
        for other in ["two", config::DEFAULT_ACCOUNT_ID, "idle"] {
            let err = list_for(&path, other, Some("a-2"), 2).unwrap_err();
            assert!(
                err.to_string().contains("unknown notification cursor"),
                "{other}: {err}"
            );
        }
        // ... and an id stored in both histories pages by each account's own order.
        let two = list_for(&path, "two", Some("b-1"), 5).unwrap();
        assert_eq!(ids(&two), ["a-1"]);
        let two = list_for(&path, "two", Some("a-1"), 5).unwrap();
        assert!(two.items.is_empty() && two.next_cursor.is_none());
    }

    #[test]
    fn the_filter_applies_to_each_accounts_listing_separately() {
        let path = config_path();
        let tweet = json!({"data": {"type": "tweet"}});
        let chat = json!({"data": {"type": "xchat_message_received"}});
        for account in ["one", "two"] {
            append_as_for(&path, account, "t", &tweet, NotificationKind::Tweet).unwrap();
            append_as_for(
                &path,
                account,
                "c",
                &chat,
                NotificationKind::XchatMessageReceived,
            )
            .unwrap();
        }
        let chat_only = NotificationFilter {
            tweet: false,
            ..NotificationFilter::default()
        };
        let hidden = list_filtered_for(&path, "one", None, 10, &chat_only).unwrap();
        assert_eq!(ids(&hidden), ["c"]);
        let all = list_for(&path, "two", None, 10).unwrap();
        assert_eq!(ids(&all), ["c", "t"]);
    }

    fn sample(n: usize) -> Value {
        json!({ "title": format!("t{n}"), "body": format!("b{n}") })
    }

    fn ids(page: &NotificationPage) -> Vec<&str> {
        page.items.iter().map(|i| i.id.as_str()).collect()
    }

    fn assert_private(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", path.display());
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    #[test]
    fn lists_newest_first_and_ignores_duplicate_ids() {
        let path = config_path();
        append_at(&path, "c:1", &sample(1), 100).unwrap();
        append_at(&path, "c:2", &sample(2), 200).unwrap();
        append_at(&path, "c:1", &sample(9), 300).unwrap();

        let page = list(&path, None, 50).unwrap();
        assert_eq!(ids(&page), ["c:2", "c:1"]);
        assert_eq!(page.items[1].title, "t1");
        assert_eq!(page.items[1].received_at, 100);
        assert!(page.next_cursor.is_none());

        let value = serde_json::to_value(&page).unwrap();
        let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["items", "next_cursor", "notification_filter"]);
        let mut item_keys: Vec<_> = value["items"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        item_keys.sort();
        assert_eq!(
            item_keys,
            [
                "body",
                "icon",
                "id",
                "kind",
                "payload",
                "received_at",
                "title",
                "url"
            ]
        );
    }

    #[test]
    fn listing_without_history_creates_nothing() {
        let path = config_path();
        let page = list(&path, None, 50).unwrap();
        assert!(page.items.is_empty());
        assert!(page.next_cursor.is_none());
        assert!(!db_file(&path).exists());
        assert!(list(&path, Some("c:1"), 50).is_err());
        assert!(!db_file(&path).exists());
    }

    #[test]
    fn cursor_walks_pages_without_overlap() {
        let path = config_path();
        for n in 1..=5 {
            append_at(&path, &format!("c:{n}"), &sample(n), n as u64).unwrap();
        }
        let first = list(&path, None, 2).unwrap();
        assert_eq!(ids(&first), ["c:5", "c:4"]);
        assert_eq!(first.next_cursor.as_deref(), Some("c:4"));

        let second = list(&path, first.next_cursor.as_deref(), 2).unwrap();
        assert_eq!(ids(&second), ["c:3", "c:2"]);
        assert_eq!(second.next_cursor.as_deref(), Some("c:2"));

        let third = list(&path, second.next_cursor.as_deref(), 2).unwrap();
        assert_eq!(ids(&third), ["c:1"]);
        assert!(third.next_cursor.is_none());

        let exact = list(&path, Some("c:3"), 2).unwrap();
        assert_eq!(ids(&exact), ["c:2", "c:1"]);
        assert!(exact.next_cursor.is_none());
    }

    #[test]
    fn order_follows_arrival_not_received_at() {
        let path = config_path();
        append_at(&path, "c:1", &sample(1), 900).unwrap();
        append_at(&path, "c:2", &sample(2), 100).unwrap();
        assert_eq!(ids(&list(&path, None, 10).unwrap()), ["c:2", "c:1"]);
    }

    #[test]
    fn unknown_cursor_is_a_fixed_config_error() {
        let path = config_path();
        append_at(&path, "c:1", &sample(1), 1).unwrap();
        let err = list(&path, Some("secret-looking-cursor"), 10).unwrap_err();
        assert!(matches!(err, AngelicAngelError::Config(_)));
        assert_eq!(err.to_string(), "config error: unknown notification cursor");
    }

    #[test]
    fn cursor_is_bound_as_a_value_not_sql() {
        let path = config_path();
        append_at(&path, "c:1", &sample(1), 1).unwrap();
        let err = list(&path, Some("x' OR '1'='1"), 10).unwrap_err();
        assert_eq!(err.to_string(), "config error: unknown notification cursor");
    }

    #[test]
    fn limit_is_clamped() {
        let path = config_path();
        let mut conn = open(&history_path(&path), BUSY_TIMEOUT).unwrap();
        let tx = conn.transaction().unwrap();
        for n in 0..120 {
            insert(
                &tx,
                &build_item(
                    &format!("c:{n}"),
                    &sample(n),
                    NotificationKind::Other,
                    n as u64,
                ),
            )
            .unwrap();
        }
        tx.commit().unwrap();

        let page = list(&path, None, 10_000).unwrap();
        assert_eq!(page.items.len(), 100);
        assert_eq!(page.next_cursor.as_deref(), Some("c:20"));
        assert_eq!(list(&path, None, 0).unwrap().items.len(), 1);
    }

    #[test]
    fn history_is_not_capped_and_pages_through_everything() {
        const TOTAL: usize = 2500;
        let path = config_path();
        let mut conn = open(&history_path(&path), BUSY_TIMEOUT).unwrap();
        let tx = conn.transaction().unwrap();
        for n in 0..TOTAL {
            assert!(
                insert(
                    &tx,
                    &build_item(
                        &format!("c:{n}"),
                        &sample(n),
                        NotificationKind::Other,
                        n as u64
                    )
                )
                .unwrap()
            );
        }
        tx.commit().unwrap();
        drop(conn);

        append_at(&path, "c:new", &sample(0), 1).unwrap();

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = list(&path, cursor.as_deref(), 100).unwrap();
            seen.extend(page.items.iter().map(|i| i.id.clone()));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(seen.len(), TOTAL + 1);
        assert_eq!(seen[0], "c:new");
        assert_eq!(seen[1], format!("c:{}", TOTAL - 1));
        assert_eq!(seen[TOTAL], "c:0");
        let unique: std::collections::HashSet<_> = seen.iter().collect();
        assert_eq!(unique.len(), TOTAL + 1);
    }

    #[test]
    fn history_persists_privately_next_to_the_config() {
        let path = config_path();
        append_at(&path, "c:1", &sample(1), 1).unwrap();
        append_at(&path, "c:2", &sample(2), 2).unwrap();

        let file = db_file(&path);
        assert!(file.is_file());
        assert_private(&file);

        let siblings: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .contains("notifications")
            })
            .collect();
        assert_eq!(siblings, std::slice::from_ref(&file));

        let reloaded = list(&path, None, 50).unwrap();
        assert_eq!(ids(&reloaded), ["c:2", "c:1"]);
    }

    #[test]
    fn existing_loose_permissions_are_tightened() {
        let path = config_path();
        let file = db_file(&path);
        std::fs::write(&file, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        append_at(&path, "c:1", &sample(1), 1).unwrap();
        assert_private(&file);
        assert_eq!(ids(&list(&path, None, 10).unwrap()), ["c:1"]);
    }

    #[test]
    fn legacy_json_history_is_left_alone() {
        let path = config_path();
        let legacy = path.with_file_name("c.toml.notifications.json");
        std::fs::write(&legacy, "{\"items\":[]}").unwrap();
        append_at(&path, "c:1", &sample(1), 1).unwrap();
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "{\"items\":[]}");
    }

    #[test]
    fn oversized_payload_is_replaced_by_a_bounded_marker() {
        let path = config_path();
        let huge = json!({
            "title": "T".repeat(5000),
            "body": "あ".repeat(50_000),
            "data": { "url": "https://x.com/i/status/1", "blob": "\"\\".repeat(40_000) },
        });
        append_at(&path, "c:1", &huge, 1).unwrap();

        let item = list(&path, None, 1).unwrap().items.remove(0);
        assert_eq!(item.payload["truncated"], json!(true));
        assert!(item.payload["original_bytes"].as_u64().unwrap() > MAX_PAYLOAD_BYTES as u64);
        assert!(item.payload.to_string().len() <= MAX_PAYLOAD_BYTES);
        assert_eq!(item.title.chars().count(), MAX_TITLE_CHARS);
        assert!(item.title.ends_with('…'));
        assert_eq!(item.body.chars().count(), MAX_BODY_CHARS);
        assert_eq!(item.url.as_deref(), Some("https://x.com/i/status/1"));
    }

    #[test]
    fn payload_at_the_limit_is_kept_verbatim() {
        let filler = "a".repeat(MAX_PAYLOAD_BYTES - json!({ "body": "" }).to_string().len());
        let payload = json!({ "body": filler });
        assert_eq!(payload.to_string().len(), MAX_PAYLOAD_BYTES);
        assert_eq!(bound_payload(&payload), payload);
    }

    #[test]
    fn extracts_nested_and_top_level_fields() {
        let nested = json!({
            "notification": {
                "title": "Nested",
                "body": "Body",
                "icon": "https://pbs.twimg.com/profile_images/1/a.jpg",
                "data": { "url": "https://x.com/i/status/42" },
            },
            "title": "Outer",
        });
        let s = summarize(&nested);
        assert_eq!(s.title, "Nested");
        assert_eq!(s.body, "Body");
        assert_eq!(
            s.icon.as_deref(),
            Some("https://pbs.twimg.com/profile_images/1/a.jpg")
        );
        assert_eq!(s.url.as_deref(), Some("https://x.com/i/status/42"));

        let flat = json!({ "title": "Flat", "data": { "url": "https://twitter.com/a" } });
        let s = summarize(&flat);
        assert_eq!(s.title, "Flat");
        assert_eq!(s.body, "");
        assert_eq!(s.url.as_deref(), Some("https://twitter.com/a"));
    }

    #[test]
    fn unsafe_link_falls_through_to_the_next_candidate() {
        let payload = json!({
            "data": { "url": "javascript:alert(1)", "uri": "https://x.com/i/notifications" },
            "url": "https://www.x.com/home",
        });
        assert_eq!(
            summarize(&payload).url.as_deref(),
            Some("https://x.com/i/notifications")
        );
        let only_unsafe =
            json!({ "data": { "url": "https://evil.example/x" }, "url": "twitter://home" });
        assert!(summarize(&only_unsafe).url.is_none());
    }

    #[test]
    fn malformed_and_primitive_payloads_still_produce_entries() {
        let path = config_path();
        let cases = [
            json!("plain text"),
            json!(42),
            json!(true),
            json!(null),
            json!([1, 2, 3]),
            json!({}),
            json!({ "raw": "not json {" }),
            json!({ "title": 5, "body": ["x"], "icon": 1, "data": "oops", "notification": 7 }),
        ];
        for (n, payload) in cases.iter().enumerate() {
            append_at(&path, &format!("c:{n}"), payload, n as u64).unwrap();
        }
        let page = list(&path, None, 50).unwrap();
        assert_eq!(page.items.len(), cases.len());
        let by_id = |id: &str| page.items.iter().find(|i| i.id == id).unwrap();
        assert_eq!(by_id("c:0").body, "plain text");
        assert_eq!(by_id("c:1").body, "42");
        assert_eq!(by_id("c:3").payload, Value::Null);
        assert_eq!(by_id("c:4").body, "[1,2,3]");
        assert_eq!(by_id("c:6").body, "not json {");
        assert_eq!(by_id("c:6").payload, json!({ "raw": "not json {" }));
        let odd = by_id("c:7");
        assert_eq!((odd.title.as_str(), odd.body.as_str()), ("", ""));
        assert!(odd.url.is_none() && odd.icon.is_none());
        assert_eq!(odd.payload, cases[7]);
    }

    #[test]
    fn link_filter_allows_only_https_x_hosts() {
        for ok in [
            "https://x.com/i/status/1",
            "https://www.x.com",
            "https://twitter.com/a?b=c#d",
            "https://www.twitter.com/a",
            "HTTPS://X.COM/a",
            "  https://x.com/a  ",
        ] {
            assert!(sanitize_link(ok).is_some(), "{ok}");
        }
        for bad in [
            "",
            "javascript:alert(1)",
            "data:text/html,x",
            "twitter://home",
            "/i/notifications",
            "//x.com/a",
            "http://x.com/a",
            "https://evil.com/x.com",
            "https://x.com.evil.com/a",
            "https://evilx.com/a",
            "https://mobile.twitter.com/a",
            "https://x.com@evil.com/a",
            "https://evil.com@x.com/a",
            "https://x.com:443/a",
            "https://x.com:80@evil.com/",
            "https://x.com\\@evil.com/",
            "https://x.com/a b",
            "https://x.com/a\nb",
            "https://x.com/\u{e9}",
            "https://x.com.",
            "https://",
        ] {
            assert!(sanitize_link(bad).is_none(), "{bad:?}");
        }
        let long = format!("https://x.com/{}", "a".repeat(MAX_URL_BYTES));
        assert!(sanitize_link(&long).is_none());
    }

    #[test]
    fn icon_filter_allows_only_twimg_hosts() {
        assert!(sanitize_icon("https://pbs.twimg.com/profile_images/1/a.jpg").is_some());
        assert!(sanitize_icon("https://abs.twimg.com/a.png").is_some());
        for bad in [
            "https://x.com/a.png",
            "https://video.twimg.com/a.png",
            "https://twimg.com/a.png",
            "http://pbs.twimg.com/a.png",
            "https://evil.com/pbs.twimg.com/a.png",
            "https://pbs.twimg.com.evil.com/a.png",
            "data:image/png;base64,AAAA",
        ] {
            assert!(sanitize_icon(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn list_refilters_links_read_from_disk() {
        let path = config_path();
        let conn = open(&history_path(&path), BUSY_TIMEOUT).unwrap();
        conn.execute(
            "INSERT INTO notifications (id, received_at, title, body, url, icon, payload) \
             VALUES ('c:1', 1, '', '', 'javascript:alert(1)', 'https://evil.com/a.png', 'null')",
            [],
        )
        .unwrap();
        drop(conn);
        let item = list(&path, None, 1).unwrap().items.remove(0);
        assert!(item.url.is_none() && item.icon.is_none());
    }

    #[test]
    fn invalid_ids_are_rejected() {
        let path = config_path();
        assert!(append_at(&path, "", &sample(1), 1).is_err());
        assert!(append_at(&path, &"x".repeat(MAX_ID_BYTES + 1), &sample(1), 1).is_err());
        assert!(list(&path, None, 1).unwrap().items.is_empty());
    }

    #[test]
    fn corrupt_database_is_reported_with_fixed_messages_and_kept_intact() {
        let path = config_path();
        let file = db_file(&path);
        let garbage = "this is definitely not a sqlite database ".repeat(40);
        std::fs::write(&file, &garbage).unwrap();

        let read = list(&path, None, 10).unwrap_err();
        assert_eq!(
            read.to_string(),
            "config error: notification history is unreadable"
        );
        let write = append_at(&path, "c:1", &sample(1), 1).unwrap_err();
        assert_eq!(
            write.to_string(),
            "config error: notification history is unreadable"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), garbage);
    }

    #[test]
    fn newer_schema_versions_are_refused() {
        let path = config_path();
        let conn = open(&history_path(&path), BUSY_TIMEOUT).unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
            .unwrap();
        drop(conn);
        let err = list(&path, None, 10).unwrap_err();
        assert!(err.to_string().contains("unsupported schema version"));
        assert!(append_at(&path, "c:1", &sample(1), 1).is_err());
        let conn = Connection::open(db_file(&path)).unwrap();
        assert_eq!(
            user_version(&conn).unwrap(),
            SCHEMA_VERSION + 1,
            "an unknown newer schema must be left untouched"
        );
    }

    #[test]
    fn unusable_storage_location_fails_with_a_fixed_message() {
        let path = config_path();
        std::fs::create_dir(db_file(&path)).unwrap();
        let err = append_at(&path, "c:1", &sample(1), 1).unwrap_err();
        assert_eq!(
            err.to_string(),
            "config error: notification history is unavailable"
        );
        assert!(list(&path, None, 1).is_err());
    }

    #[test]
    fn concurrent_appends_are_serialized_and_deduplicated() {
        let path = config_path();
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for n in 0..15 {
                        append_at(&path, &format!("c:{t}-{n}"), &sample(n), 1).unwrap();
                        append_at(&path, &format!("shared:{n}"), &sample(n), 1).unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let mut count = 0;
        let mut cursor: Option<String> = None;
        loop {
            let page = list(&path, cursor.as_deref(), 100).unwrap();
            count += page.items.len();
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(count, 8 * 15 + 15);
    }

    #[test]
    fn readers_and_writers_do_not_block_each_other() {
        let path = config_path();
        append_at(&path, "c:0", &sample(0), 1).unwrap();
        let writer = {
            let path = path.clone();
            std::thread::spawn(move || {
                for n in 1..=40 {
                    append_at(&path, &format!("c:{n}"), &sample(n), 1).unwrap();
                }
            })
        };
        for _ in 0..40 {
            assert!(!list(&path, None, 10).unwrap().items.is_empty());
        }
        writer.join().unwrap();
        assert_eq!(list(&path, None, 1).unwrap().items[0].id, "c:40");
    }

    const SHORT_TIMEOUT: Duration = Duration::from_millis(50);

    fn lock_exclusively(path: &Path) -> Connection {
        let conn = open(&history_path(path), BUSY_TIMEOUT).unwrap();
        conn.execute_batch("BEGIN EXCLUSIVE").unwrap();
        conn
    }

    #[test]
    fn a_locked_database_is_busy_not_empty_or_duplicate() {
        let path = config_path();
        append_at(&path, "c:1", &sample(1), 1).unwrap();
        let holder = lock_exclusively(&path);

        let read = list_with_timeout(
            &path,
            None,
            10,
            &NotificationFilter::default(),
            SHORT_TIMEOUT,
        )
        .unwrap_err();
        assert_eq!(
            read.to_string(),
            "config error: notification history is busy"
        );
        let read_cursor = list_with_timeout(
            &path,
            Some("c:1"),
            10,
            &NotificationFilter::default(),
            SHORT_TIMEOUT,
        )
        .unwrap_err();
        assert_eq!(
            read_cursor.to_string(),
            "config error: notification history is busy"
        );
        // The id is already stored, but a locked database must still not report success.
        let write = append_with_timeout(
            &path,
            "c:1",
            &sample(1),
            NotificationKind::Other,
            1,
            SHORT_TIMEOUT,
        )
        .unwrap_err();
        assert_eq!(
            write.to_string(),
            "config error: notification history is busy"
        );
        let fresh = append_with_timeout(
            &path,
            "c:2",
            &sample(2),
            NotificationKind::Other,
            1,
            SHORT_TIMEOUT,
        )
        .unwrap_err();
        assert_eq!(
            fresh.to_string(),
            "config error: notification history is busy"
        );

        holder.execute_batch("ROLLBACK").unwrap();
        drop(holder);
        assert_eq!(ids(&list(&path, None, 10).unwrap()), ["c:1"]);
        append_at(&path, "c:2", &sample(2), 1).unwrap();
        assert_eq!(ids(&list(&path, None, 10).unwrap()), ["c:2", "c:1"]);
    }

    #[test]
    fn only_an_id_conflict_counts_as_a_duplicate() {
        let path = config_path();
        let mut conn = open(&history_path(&path), BUSY_TIMEOUT).unwrap();
        let tx = conn.transaction().unwrap();
        let item = build_item("c:1", &sample(1), NotificationKind::Other, 1);
        assert!(insert(&tx, &item).unwrap());
        assert!(!insert(&tx, &item).unwrap());

        let null_title = tx.execute(
            "INSERT INTO notifications (id, received_at, title, body, payload) \
             VALUES ('c:3', 1, NULL, '', 'null') ON CONFLICT(id) DO NOTHING",
            [],
        );
        assert!(
            null_title.is_err(),
            "a NOT NULL violation must fail, not be skipped"
        );
        tx.commit().unwrap();
        assert_eq!(list(&path, None, 10).unwrap().items.len(), 1);
    }

    #[test]
    fn corrupt_database_is_never_recreated_or_treated_as_empty() {
        let path = config_path();
        append_at(&path, "c:1", &sample(1), 1).unwrap();
        let file = db_file(&path);
        let mut bytes = std::fs::read(&file).unwrap();
        bytes[..16].copy_from_slice(b"not a sqlite db!");
        std::fs::write(&file, &bytes).unwrap();

        for _ in 0..2 {
            assert!(list(&path, None, 10).is_err());
            assert!(list(&path, Some("c:1"), 10).is_err());
            assert!(append_at(&path, "c:2", &sample(2), 1).is_err());
        }
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
    }

    #[test]
    fn journal_is_private_while_a_write_is_in_flight() {
        let path = config_path();
        append_at(&path, "c:1", &sample(1), 1).unwrap();
        let mut conn = open(&history_path(&path), BUSY_TIMEOUT).unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        assert!(
            insert(
                &tx,
                &build_item("c:2", &sample(2), NotificationKind::Other, 2)
            )
            .unwrap()
        );

        let journal = path.with_file_name("c.toml.notifications.sqlite3-journal");
        assert!(
            journal.is_file(),
            "expected a rollback journal during the write"
        );
        assert_private(&journal);
        tx.commit().unwrap();
        assert!(!journal.exists());
        assert!(
            !path
                .with_file_name("c.toml.notifications.sqlite3-wal")
                .exists()
        );
        assert!(
            !path
                .with_file_name("c.toml.notifications.sqlite3-shm")
                .exists()
        );
    }

    #[test]
    fn a_hot_journal_left_by_a_crash_is_not_deleted_by_us() {
        let path = config_path();
        append_at(&path, "c:1", &sample(1), 1).unwrap();
        let journal = path.with_file_name("c.toml.notifications.sqlite3-journal");
        // A stray, non-hot journal (zero length) must be left for SQLite to deal with.
        std::fs::write(&journal, b"").unwrap();
        append_at(&path, "c:2", &sample(2), 2).unwrap();
        assert_eq!(ids(&list(&path, None, 10).unwrap()), ["c:2", "c:1"]);
    }

    fn typed(data_type: &str, n: usize) -> Value {
        json!({ "title": format!("t{n}"), "data": { "type": data_type } })
    }

    /// 実際の Web Push と同じ形。`uri` は元ツイート、`tag` はリツイート自身の ID。
    fn tweet_data(uri: &str, tag: &str) -> Value {
        json!({
            "title": "t",
            "body": "b",
            "tag": tag,
            "data": { "type": "tweet", "uri": uri, "tag": tag, "scribe_target": "tweet" },
        })
    }

    fn retweet_payload() -> Value {
        tweet_data(
            "/Gunma_kankou/status/2109479175887245361",
            "tweet-2109480482920673532",
        )
    }

    fn plain_tweet_payload() -> Value {
        tweet_data(
            "/niigata_bic/status/2109197207392141321",
            "tweet-2109197207392141321",
        )
    }

    /// `quote`・`quote_self`・`reply` は有効のまま。この 3 区分を変えるときは構造体を直接作る。
    fn filter(
        tweet: bool,
        retweet: bool,
        xchat_message_received: bool,
        other: bool,
    ) -> NotificationFilter {
        NotificationFilter {
            tweet,
            retweet,
            quote: true,
            quote_self: true,
            reply: true,
            xchat_message_received,
            other,
        }
    }

    /// 判定済みの区分を指定して保存する。
    fn append_kind(path: &Path, id: &str, payload: &Value, kind: NotificationKind, at: u64) {
        append_with_timeout(path, id, payload, kind, at, BUSY_TIMEOUT).unwrap();
    }

    fn list_with(
        path: &Path,
        f: NotificationFilter,
        before: Option<&str>,
        limit: usize,
    ) -> NotificationPage {
        list_filtered(path, before, limit, &f).unwrap()
    }

    #[test]
    fn classify_reads_only_data_type_with_inner_precedence() {
        use NotificationKind::{Other, Retweet, Tweet, XchatMessageReceived};
        let rt_data = json!({ "type": "tweet", "uri": "/a/status/1", "tag": "tweet-2" });
        let same_data = json!({ "type": "tweet", "uri": "/a/status/1", "tag": "tweet-1" });
        let cases = [
            (json!({ "data": { "type": "tweet" } }), Tweet),
            (
                json!({ "data": { "type": "xchat_message_received" } }),
                XchatMessageReceived,
            ),
            (
                json!({ "notification": { "data": { "type": "tweet" } } }),
                Tweet,
            ),
            (
                json!({ "notification": { "data": { "type": "xchat_message_received" } },
                        "data": { "type": "tweet" } }),
                XchatMessageReceived,
            ),
            // 内側が未知の文字列なら、外側が既知でも採用しない。
            (
                json!({ "notification": { "data": { "type": "recommendation" } },
                        "data": { "type": "tweet" } }),
                Other,
            ),
            // 内側に文字列が無いときだけ外側を読む。
            (
                json!({ "notification": { "data": { "type": 5 } }, "data": { "type": "tweet" } }),
                Tweet,
            ),
            (
                json!({ "notification": { "title": "x" }, "data": { "type": "xchat_message_received" } }),
                XchatMessageReceived,
            ),
            (json!({ "data": { "type": "TWEET" } }), Other),
            (json!({ "data": { "type": "" } }), Other),
            (json!({ "data": { "type": 1 } }), Other),
            (json!({ "data": { "type": null } }), Other),
            (json!({ "data": "tweet" }), Other),
            (json!({ "data": {} }), Other),
            (json!({ "type": "tweet" }), Other),
            (
                json!({ "notification": "tweet", "data": { "type": "tweet" } }),
                Tweet,
            ),
            (
                json!({ "title": "tweet", "body": "tweet", "scribe_target": "tweet" }),
                Other,
            ),
            (json!("tweet"), Other),
            (json!(null), Other),
            (json!([{ "data": { "type": "tweet" } }]), Other),
            (
                json!({ "truncated": true, "raw": "{\"data\":{\"type\":\"tweet\"}}" }),
                Other,
            ),
            // リツイート: type が tweet で、uri と tag の ID が食い違うときだけ。
            (retweet_payload(), Retweet),
            (plain_tweet_payload(), Tweet),
            (json!({ "data": rt_data }), Retweet),
            (json!({ "notification": { "data": rt_data } }), Retweet),
            (
                json!({ "notification": { "data": same_data }, "data": rt_data }),
                Tweet,
            ),
            (
                json!({ "notification": { "data": rt_data }, "data": same_data }),
                Retweet,
            ),
            // type を読んだのと同じ階層の uri/tag だけを使う。
            (
                json!({ "notification": { "data": { "type": 5, "uri": "/a/status/1", "tag": "tweet-2" } },
                        "data": same_data }),
                Tweet,
            ),
            (
                json!({ "notification": { "data": { "type": 5 } }, "data": rt_data }),
                Retweet,
            ),
            (
                json!({ "notification": { "data": { "type": "tweet" } }, "data": rt_data }),
                Tweet,
            ),
            (
                json!({ "uri": "/a/status/1", "tag": "tweet-2", "data": { "type": "tweet" } }),
                Tweet,
            ),
            // どちらかが欠けるか、形式が違えば除外側に倒さない。
            (
                json!({ "data": { "type": "tweet", "uri": "/a/status/1" } }),
                Tweet,
            ),
            (
                json!({ "data": { "type": "tweet", "tag": "tweet-2" } }),
                Tweet,
            ),
            (
                json!({ "data": { "type": "tweet", "uri": 1, "tag": "tweet-2" } }),
                Tweet,
            ),
            (
                json!({ "data": { "type": "tweet", "uri": "/a/status/1", "tag": 2 } }),
                Tweet,
            ),
            (
                json!({ "data": { "type": "tweet", "uri": null, "tag": null } }),
                Tweet,
            ),
            (
                json!({ "data": { "type": "tweet", "uri": "/a/status/1", "tag": "retweet-2" } }),
                Tweet,
            ),
            (
                json!({ "data": { "type": "tweet", "uri": "/a/status/1", "tag": "tweet-x" } }),
                Tweet,
            ),
            (
                json!({ "data": { "type": "tweet", "uri": "/a/status/1", "tag": "tweet-" } }),
                Tweet,
            ),
            (
                json!({ "data": { "type": "tweet", "uri": "/a/status/1", "tag": "tweet-2\n" } }),
                Tweet,
            ),
            (
                json!({ "data": { "type": "tweet", "uri": "/a/status/1", "tag": "tweet-2-x" } }),
                Tweet,
            ),
            (
                json!({ "data": { "type": "tweet", "uri": "/a/status/1", "tag": "tweet-２" } }),
                Tweet,
            ),
            (
                json!({ "data": { "type": "tweet", "uri": "/a/status/x", "tag": "tweet-2" } }),
                Tweet,
            ),
            (
                json!({ "data": { "type": "tweet", "uri": "/a/status/1/photo/1", "tag": "tweet-2" } }),
                Tweet,
            ),
            (
                json!({ "data": { "type": "tweet", "uri": "/a/status/1?s=20", "tag": "tweet-2" } }),
                Tweet,
            ),
            (
                json!({ "data": { "type": "tweet", "uri": "/a/status/1\n", "tag": "tweet-2" } }),
                Tweet,
            ),
            (
                json!({ "data": { "type": "tweet", "uri": "/i/web/status/1", "tag": "tweet-2" } }),
                Tweet,
            ),
            (
                json!({ "data": { "type": "tweet", "uri": "//status/1", "tag": "tweet-2" } }),
                Tweet,
            ),
            (
                json!({ "data": { "type": "tweet", "uri": "a/status/1", "tag": "tweet-2" } }),
                Tweet,
            ),
            (
                json!({ "data": { "type": "tweet", "uri": "https://x.com/a/status/1", "tag": "tweet-2" } }),
                Tweet,
            ),
            // tweet 以外の type は uri/tag が食い違っても区分を変えない。
            (
                json!({ "data": { "type": "recommendation", "uri": "/a/status/1", "tag": "tweet-2" } }),
                Other,
            ),
            (
                json!({ "data": { "type": "xchat_message_received", "uri": "/a/status/1", "tag": "tweet-2" } }),
                XchatMessageReceived,
            ),
            // `retweet` は X の data.type には存在しない。
            (
                json!({ "data": { "type": "retweet", "uri": "/a/status/1", "tag": "tweet-2" } }),
                Other,
            ),
            (json!({ "data": { "type": "retweet" } }), Other),
        ];
        for (payload, expected) in cases {
            assert_eq!(classify(&payload), expected, "{payload}");
        }
    }

    #[test]
    fn a_stored_retweet_quote_or_reply_is_restored_but_is_not_a_data_type() {
        for (name, kind) in [
            ("retweet", NotificationKind::Retweet),
            ("quote", NotificationKind::Quote),
            ("self_quote", NotificationKind::SelfQuote),
            ("reply", NotificationKind::Reply),
        ] {
            assert_eq!(NotificationKind::from_stored(name), kind);
            assert_eq!(kind.as_str(), name);
            assert_eq!(
                NotificationKind::from_data_type(name),
                NotificationKind::Other
            );
        }
        for kind in [
            NotificationKind::Tweet,
            NotificationKind::Retweet,
            NotificationKind::Quote,
            NotificationKind::SelfQuote,
            NotificationKind::Reply,
            NotificationKind::XchatMessageReceived,
            NotificationKind::Other,
        ] {
            assert_eq!(NotificationKind::from_stored(kind.as_str()), kind);
        }
        assert_eq!(
            NotificationKind::from_stored("something_new"),
            NotificationKind::Other
        );
    }

    #[test]
    fn kind_is_serialized_as_the_data_type_name() {
        let path = config_path();
        append_at(&path, "c:1", &typed("tweet", 1), 1).unwrap();
        append_at(&path, "c:2", &typed("xchat_message_received", 2), 2).unwrap();
        append_at(&path, "c:3", &typed("something_new", 3), 3).unwrap();
        append_at(&path, "c:4", &retweet_payload(), 4).unwrap();
        append_kind(
            &path,
            "c:5",
            &plain_tweet_payload(),
            NotificationKind::Quote,
            5,
        );
        append_kind(
            &path,
            "c:6",
            &plain_tweet_payload(),
            NotificationKind::Reply,
            6,
        );
        append_kind(
            &path,
            "c:7",
            &plain_tweet_payload(),
            NotificationKind::SelfQuote,
            7,
        );
        let value = serde_json::to_value(list(&path, None, 10).unwrap()).unwrap();
        assert_eq!(value["items"][0]["kind"], "self_quote");
        assert_eq!(value["items"][1]["kind"], "reply");
        assert_eq!(value["items"][2]["kind"], "quote");
        assert_eq!(value["items"][3]["kind"], "retweet");
        assert_eq!(value["items"][4]["kind"], "other");
        assert_eq!(value["items"][5]["kind"], "xchat_message_received");
        assert_eq!(value["items"][6]["kind"], "tweet");
        assert_eq!(
            value["notification_filter"],
            json!({
                "tweet": true,
                "retweet": true,
                "quote": true,
                "quote_self": true,
                "reply": true,
                "xchat_message_received": true,
                "other": true
            })
        );
    }

    #[test]
    fn kind_is_taken_before_the_payload_is_truncated() {
        let path = config_path();
        let huge = json!({
            "title": "T",
            "data": { "type": "xchat_message_received", "blob": "x".repeat(40_000) },
        });
        append_at(&path, "c:1", &huge, 1).unwrap();
        let item = list(&path, None, 1).unwrap().items.remove(0);
        assert_eq!(item.payload["truncated"], json!(true));
        assert_eq!(item.kind, NotificationKind::XchatMessageReceived);
        assert!(
            list_filtered(&path, None, 10, &filter(true, true, false, true))
                .unwrap()
                .items
                .is_empty()
        );
    }

    #[test]
    fn a_retweet_stays_a_retweet_after_its_payload_is_truncated() {
        let path = config_path();
        let mut huge = retweet_payload();
        huge["data"]["blob"] = json!("x".repeat(40_000));
        append_at(&path, "c:1", &huge, 1).unwrap();
        let item = list(&path, None, 1).unwrap().items.remove(0);
        assert_eq!(item.payload["truncated"], json!(true));
        assert_eq!(item.kind, NotificationKind::Retweet);
        assert!(
            list_filtered(&path, None, 10, &filter(true, false, true, true))
                .unwrap()
                .items
                .is_empty()
        );
    }

    #[test]
    fn every_filter_combination_selects_the_matching_kinds() {
        use NotificationKind::{Quote, Reply, SelfQuote};
        let path = config_path();
        append_at(&path, "tweet", &typed("tweet", 1), 1).unwrap();
        append_at(&path, "retweet", &retweet_payload(), 2).unwrap();
        append_kind(&path, "quote", &plain_tweet_payload(), Quote, 3);
        append_kind(&path, "self_quote", &plain_tweet_payload(), SelfQuote, 4);
        append_kind(&path, "reply", &plain_tweet_payload(), Reply, 5);
        append_at(&path, "chat", &typed("xchat_message_received", 6), 6).unwrap();
        append_at(&path, "other", &typed("unknown", 7), 7).unwrap();
        append_at(&path, "missing", &sample(8), 8).unwrap();
        type Shown = fn(&NotificationFilter) -> bool;
        let rows: [(&str, Shown); 8] = [
            ("tweet", |f| f.tweet),
            ("retweet", |f| f.retweet),
            ("quote", |f| f.quote),
            ("self_quote", |f| f.quote_self),
            ("reply", |f| f.reply),
            ("chat", |f| f.xchat_message_received),
            ("other", |f| f.other),
            ("missing", |f| f.other),
        ];

        for bits in 0..128_u8 {
            let f = NotificationFilter {
                tweet: bits & 1 != 0,
                retweet: bits & 2 != 0,
                quote: bits & 4 != 0,
                quote_self: bits & 8 != 0,
                reply: bits & 16 != 0,
                xchat_message_received: bits & 32 != 0,
                other: bits & 64 != 0,
            };
            let expected: Vec<&str> = rows
                .iter()
                .rev()
                .filter(|(_, shown)| shown(&f))
                .map(|(id, _)| *id)
                .collect();
            let page = list_with(&path, f, None, 50);
            assert_eq!(ids(&page), expected, "{f:?}");
            assert_eq!(page.notification_filter, f);
            assert!(page.next_cursor.is_none());
        }
    }

    #[test]
    fn a_page_with_nothing_visible_is_empty_with_no_cursor() {
        let path = config_path();
        for n in 0..5 {
            append_at(&path, &format!("c:{n}"), &typed("tweet", n), n as u64).unwrap();
        }
        let none = NotificationFilter {
            tweet: false,
            retweet: false,
            quote: false,
            quote_self: false,
            reply: false,
            xchat_message_received: false,
            other: false,
        };
        let page = list_with(&path, none, None, 2);
        assert!(page.items.is_empty());
        assert!(page.next_cursor.is_none());
        assert_eq!(page.notification_filter, none);
        // 履歴自体は残っており、設定を戻せばすべて見える。
        assert_eq!(list(&path, None, 50).unwrap().items.len(), 5);

        let missing_db = config_path();
        let empty = list_with(&missing_db, none, None, 10);
        assert!(empty.items.is_empty());
        assert_eq!(empty.notification_filter, none);
    }

    #[test]
    fn many_excluded_rows_never_produce_an_empty_page_or_lose_rows() {
        let path = config_path();
        append_at(&path, "old-tweet", &typed("tweet", 0), 1).unwrap();
        for n in 0..250 {
            append_at(
                &path,
                &format!("chat:{n}"),
                &typed("xchat_message_received", n),
                1,
            )
            .unwrap();
        }
        append_at(&path, "mid-tweet", &typed("tweet", 1), 1).unwrap();
        for n in 0..250 {
            append_at(&path, &format!("noise:{n}"), &typed("unknown", n), 1).unwrap();
        }
        append_at(&path, "new-tweet", &typed("tweet", 2), 1).unwrap();

        let tweets_only = filter(true, false, false, false);
        let first = list_with(&path, tweets_only, None, 1);
        assert_eq!(ids(&first), ["new-tweet"]);
        assert_eq!(first.next_cursor.as_deref(), Some("new-tweet"));

        let second = list_with(&path, tweets_only, first.next_cursor.as_deref(), 1);
        assert_eq!(ids(&second), ["mid-tweet"]);
        assert_eq!(second.next_cursor.as_deref(), Some("mid-tweet"));

        let third = list_with(&path, tweets_only, second.next_cursor.as_deref(), 1);
        assert_eq!(ids(&third), ["old-tweet"]);
        assert!(third.next_cursor.is_none());

        let all = list_with(&path, tweets_only, None, 100);
        assert_eq!(ids(&all), ["new-tweet", "mid-tweet", "old-tweet"]);
        assert!(all.next_cursor.is_none());
    }

    #[test]
    fn a_cursor_may_point_at_a_row_the_filter_hides() {
        let path = config_path();
        append_at(&path, "t1", &typed("tweet", 1), 1).unwrap();
        append_at(&path, "hidden", &typed("xchat_message_received", 2), 2).unwrap();
        append_at(&path, "t2", &typed("tweet", 3), 3).unwrap();

        let tweets_only = filter(true, false, false, false);
        let page = list_with(&path, tweets_only, Some("hidden"), 10);
        assert_eq!(ids(&page), ["t1"]);
        // 設定変更後に、以前の応答にあった cursor を持ち越してもエラーにならない。
        let page = list_with(&path, filter(false, false, false, true), Some("t2"), 10);
        assert!(page.items.is_empty());
        assert!(list_filtered(&path, Some("never-stored"), 10, &tweets_only).is_err());
    }

    fn create_v1(config: &Path, rows: &[(String, String)]) {
        let db = history_path(config);
        ensure_private_file(&db).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE notifications (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                id TEXT NOT NULL UNIQUE,
                received_at INTEGER NOT NULL,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                url TEXT,
                icon TEXT,
                payload TEXT NOT NULL
            );
            PRAGMA user_version = 1;",
        )
        .unwrap();
        for (n, (id, payload)) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO notifications (id, received_at, title, body, payload) \
                 VALUES (?1, ?2, ?3, '', ?4)",
                params![id, n as i64, format!("title-{n}"), payload],
            )
            .unwrap();
        }
    }

    fn snapshot(config: &Path) -> Vec<(i64, String, String)> {
        let conn = Connection::open(history_path(config)).unwrap();
        let mut stmt = conn
            .prepare("SELECT seq, id, payload FROM notifications ORDER BY seq")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn v1_history_is_migrated_in_place_with_classified_kinds() {
        let path = config_path();
        let truncated = json!({ "truncated": true, "original_bytes": 99999,
            "raw": "{\"data\":{\"type\":\"tweet\"}" })
        .to_string();
        let rows: Vec<(String, String)> = vec![
            ("a".into(), typed("tweet", 1).to_string()),
            ("b".into(), typed("xchat_message_received", 2).to_string()),
            ("c".into(), typed("novel", 3).to_string()),
            ("d".into(), "this is {not json".to_string()),
            ("e".into(), truncated),
            (
                "f".into(),
                json!({ "notification": { "data": { "type": "tweet" } } }).to_string(),
            ),
            ("g".into(), json!({ "title": "no type" }).to_string()),
            ("h".into(), "null".to_string()),
        ];
        create_v1(&path, &rows);
        let before = snapshot(&path);

        let page = list(&path, None, 50).unwrap();
        let kinds: Vec<_> = page
            .items
            .iter()
            .rev()
            .map(|i| (i.id.as_str(), i.kind))
            .collect();
        use NotificationKind::{Other, Tweet, XchatMessageReceived};
        assert_eq!(
            kinds,
            [
                ("a", Tweet),
                ("b", XchatMessageReceived),
                ("c", Other),
                ("d", Other),
                ("e", Other),
                ("f", Tweet),
                ("g", Other),
                ("h", Other),
            ]
        );
        assert_eq!(
            snapshot(&path),
            before,
            "seq, id and payload must be preserved"
        );
        let conn = Connection::open(db_file(&path)).unwrap();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);

        // 再度開いても何も変わらず、保存した履歴は移行後もフィルターできる。
        let again = list_with(&path, filter(true, false, false, false), None, 50);
        assert_eq!(ids(&again), ["f", "a"]);
        assert_eq!(snapshot(&path), before);
        append_at(&path, "new", &typed("tweet", 9), 9).unwrap();
        assert_eq!(
            ids(&list_with(
                &path,
                filter(true, false, false, false),
                None,
                50
            ))[0],
            "new"
        );
    }

    #[test]
    fn migration_walks_the_history_in_chunks() {
        let path = config_path();
        let total = (BACKFILL_CHUNK as usize) * 2 + 37;
        let rows: Vec<(String, String)> = (0..total)
            .map(|n| {
                let kind = match n % 3 {
                    0 => "tweet",
                    1 => "xchat_message_received",
                    _ => "novel",
                };
                (format!("r:{n}"), typed(kind, n).to_string())
            })
            .collect();
        create_v1(&path, &rows);

        let conn = open(&history_path(&path), BUSY_TIMEOUT).unwrap();
        let count = |kind: &str| -> usize {
            conn.query_row(
                "SELECT COUNT(*) FROM notifications WHERE kind = ?1",
                [kind],
                |r| r.get::<_, i64>(0),
            )
            .unwrap() as usize
        };
        assert_eq!(count("tweet"), total.div_ceil(3));
        assert_eq!(count("xchat_message_received"), (total + 1) / 3);
        assert_eq!(count("other"), total / 3);
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn concurrent_opens_of_a_v1_history_migrate_exactly_once() {
        let path = config_path();
        let rows: Vec<(String, String)> = (0..300)
            .map(|n| (format!("r:{n}"), typed("tweet", n).to_string()))
            .collect();
        create_v1(&path, &rows);

        let handles: Vec<_> = (0..6)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let conn = open(&history_path(&path), BUSY_TIMEOUT).unwrap();
                    user_version(&conn).unwrap()
                })
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().unwrap(), SCHEMA_VERSION);
        }
        let page = list_with(&path, filter(true, false, false, false), None, 100);
        assert_eq!(page.items.len(), 100);
        assert!(page.next_cursor.is_some());
    }

    #[test]
    fn a_failed_migration_leaves_the_v1_history_intact() {
        let path = config_path();
        create_v1(&path, &[("a".into(), typed("tweet", 1).to_string())]);
        // 列が先に存在すると ALTER が失敗する。途中までの変更が残ってはならない。
        let conn = Connection::open(history_path(&path)).unwrap();
        conn.execute_batch("ALTER TABLE notifications ADD COLUMN kind TEXT")
            .unwrap();
        drop(conn);

        assert!(list(&path, None, 10).is_err());
        let conn = Connection::open(history_path(&path)).unwrap();
        assert_eq!(user_version(&conn).unwrap(), 1);
        assert_eq!(snapshot(&path).len(), 1);
    }

    /// v2（`kind` 列あり、`retweet` 区分なし）の履歴を作る。各行は (id, payload, 保存済みの kind)。
    fn create_v2(config: &Path, rows: &[(String, String, &str)]) {
        let db = history_path(config);
        ensure_private_file(&db).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE notifications (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                id TEXT NOT NULL UNIQUE,
                received_at INTEGER NOT NULL,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                url TEXT,
                icon TEXT,
                payload TEXT NOT NULL,
                kind TEXT NOT NULL DEFAULT 'other'
            );
            PRAGMA user_version = 2;",
        )
        .unwrap();
        for (n, (id, payload, kind)) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO notifications (id, received_at, title, body, payload, kind) \
                 VALUES (?1, ?2, ?3, '', ?4, ?5)",
                params![id, n as i64, format!("title-{n}"), payload, kind],
            )
            .unwrap();
        }
    }

    fn stored_kinds(config: &Path) -> Vec<(String, String)> {
        let conn = Connection::open(history_path(config)).unwrap();
        let mut stmt = conn
            .prepare("SELECT id, kind FROM notifications ORDER BY seq")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn v2_history_turns_only_its_retweets_into_retweets() {
        let path = config_path();
        let truncated = json!({ "truncated": true, "original_bytes": 99999,
            "raw": "{\"data\":{\"type\":\"tweet\"" })
        .to_string();
        let rows: Vec<(String, String, &str)> = vec![
            ("plain".into(), plain_tweet_payload().to_string(), "tweet"),
            ("rt".into(), retweet_payload().to_string(), "tweet"),
            (
                "rt-nested".into(),
                json!({ "notification": retweet_payload() }).to_string(),
                "tweet",
            ),
            ("bare".into(), typed("tweet", 3).to_string(), "tweet"),
            (
                "chat".into(),
                typed("xchat_message_received", 4).to_string(),
                "xchat_message_received",
            ),
            ("novel".into(), typed("novel", 5).to_string(), "other"),
            ("broken".into(), "this is {not json".to_string(), "tweet"),
            ("cut".into(), truncated, "tweet"),
            // tweet として保存された行以外には触れない。
            ("stale-other".into(), retweet_payload().to_string(), "other"),
        ];
        create_v2(&path, &rows);
        let before = snapshot(&path);

        let page = list(&path, None, 50).unwrap();
        let kinds: Vec<_> = page
            .items
            .iter()
            .rev()
            .map(|i| (i.id.as_str(), i.kind))
            .collect();
        use NotificationKind::{Other, Retweet, Tweet, XchatMessageReceived};
        assert_eq!(
            kinds,
            [
                ("plain", Tweet),
                ("rt", Retweet),
                ("rt-nested", Retweet),
                ("bare", Tweet),
                ("chat", XchatMessageReceived),
                ("novel", Other),
                ("broken", Tweet),
                ("cut", Tweet),
                ("stale-other", Other),
            ]
        );
        assert_eq!(
            snapshot(&path),
            before,
            "seq, id and payload must be preserved"
        );
        let conn = Connection::open(db_file(&path)).unwrap();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);

        // 移行後は retweet 単独でも、tweet から RT を外しても絞り込める。
        let only_rt = list_with(&path, filter(false, true, false, false), None, 50);
        assert_eq!(ids(&only_rt), ["rt-nested", "rt"]);
        let no_rt = list_with(&path, filter(true, false, true, true), None, 50);
        assert_eq!(
            ids(&no_rt),
            [
                "stale-other",
                "cut",
                "broken",
                "novel",
                "chat",
                "bare",
                "plain"
            ]
        );
        assert_eq!(snapshot(&path), before);
    }

    #[test]
    fn v2_migration_walks_the_tweet_rows_in_chunks() {
        let path = config_path();
        let total = (BACKFILL_CHUNK as usize) * 2 + 37;
        let rows: Vec<(String, String, &str)> = (0..total)
            .map(|n| match n % 3 {
                0 => (format!("r:{n}"), retweet_payload().to_string(), "tweet"),
                1 => (format!("r:{n}"), plain_tweet_payload().to_string(), "tweet"),
                _ => (
                    format!("r:{n}"),
                    typed("xchat_message_received", n).to_string(),
                    "xchat_message_received",
                ),
            })
            .collect();
        create_v2(&path, &rows);

        let conn = open(&history_path(&path), BUSY_TIMEOUT).unwrap();
        let count = |kind: &str| -> usize {
            conn.query_row(
                "SELECT COUNT(*) FROM notifications WHERE kind = ?1",
                [kind],
                |r| r.get::<_, i64>(0),
            )
            .unwrap() as usize
        };
        assert_eq!(count("retweet"), total.div_ceil(3));
        assert_eq!(count("tweet"), (total + 1) / 3);
        assert_eq!(count("xchat_message_received"), total / 3);
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn v1_history_is_classified_with_retweets_in_a_single_migration() {
        let path = config_path();
        let rows: Vec<(String, String)> = vec![
            ("plain".into(), plain_tweet_payload().to_string()),
            ("rt".into(), retweet_payload().to_string()),
            (
                "chat".into(),
                typed("xchat_message_received", 1).to_string(),
            ),
        ];
        create_v1(&path, &rows);

        let conn = open(&history_path(&path), BUSY_TIMEOUT).unwrap();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        drop(conn);
        assert_eq!(
            stored_kinds(&path),
            [
                ("plain".to_string(), "tweet".to_string()),
                ("rt".to_string(), "retweet".to_string()),
                ("chat".to_string(), "xchat_message_received".to_string()),
            ]
        );
    }

    #[test]
    fn a_v3_history_is_not_reclassified_on_open() {
        let path = config_path();
        append_at(&path, "rt", &retweet_payload(), 1).unwrap();
        let conn = Connection::open(history_path(&path)).unwrap();
        conn.execute("UPDATE notifications SET kind = 'tweet'", [])
            .unwrap();
        drop(conn);
        assert_eq!(
            stored_kinds(&path),
            [("rt".to_string(), "tweet".to_string())]
        );
        assert_eq!(
            list(&path, None, 10).unwrap().items[0].kind,
            NotificationKind::Tweet
        );
    }

    #[test]
    fn append_as_stores_the_given_kind_and_append_classifies() {
        let path = config_path();
        append_as(&path, "q", &plain_tweet_payload(), NotificationKind::Quote).unwrap();
        append(&path, "rt", &retweet_payload()).unwrap();
        append(&path, "plain", &plain_tweet_payload()).unwrap();
        // 同じ ID は区分が違っても無視される。
        append_as(&path, "q", &plain_tweet_payload(), NotificationKind::Reply).unwrap();
        let mut huge = plain_tweet_payload();
        huge["data"]["blob"] = json!("x".repeat(40_000));
        append_as(&path, "cut", &huge, NotificationKind::Reply).unwrap();

        let stored = |id: &str, kind: &str| (id.to_string(), kind.to_string());
        assert_eq!(
            stored_kinds(&path),
            [
                stored("q", "quote"),
                stored("rt", "retweet"),
                stored("plain", "tweet"),
                stored("cut", "reply"),
            ]
        );
        let item = list(&path, None, 1).unwrap().items.remove(0);
        assert_eq!(item.payload["truncated"], json!(true));
        assert_eq!(item.kind, NotificationKind::Reply);
    }

    #[test]
    fn a_v2_migration_leaves_stored_quotes_and_replies_alone() {
        let path = config_path();
        // 移行が読むのは tweet として保存された行だけ。リツイートの形をしていても書き換えない。
        let rows: Vec<(String, String, &str)> = vec![
            ("plain".into(), plain_tweet_payload().to_string(), "tweet"),
            ("quote".into(), plain_tweet_payload().to_string(), "quote"),
            ("reply".into(), plain_tweet_payload().to_string(), "reply"),
            ("quote-rt".into(), retweet_payload().to_string(), "quote"),
            ("reply-rt".into(), retweet_payload().to_string(), "reply"),
            ("rt".into(), retweet_payload().to_string(), "tweet"),
        ];
        create_v2(&path, &rows);

        let conn = open(&history_path(&path), BUSY_TIMEOUT).unwrap();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        drop(conn);
        let stored = |id: &str, kind: &str| (id.to_string(), kind.to_string());
        assert_eq!(
            stored_kinds(&path),
            [
                stored("plain", "tweet"),
                stored("quote", "quote"),
                stored("reply", "reply"),
                stored("quote-rt", "quote"),
                stored("reply-rt", "reply"),
                stored("rt", "retweet"),
            ]
        );
    }

    #[test]
    fn the_id_to_look_up_is_the_notification_own_tag() {
        let wrapped = json!({ "notification": plain_tweet_payload() });
        let outer_only = json!({
            "notification": { "title": "t" },
            "data": { "type": "tweet", "tag": "tweet-77" },
        });
        let cases = [
            (plain_tweet_payload(), Some("2109197207392141321")),
            (retweet_payload(), Some("2109480482920673532")),
            (wrapped, Some("2109197207392141321")),
            (outer_only, Some("77")),
            (
                json!({ "data": { "type": "tweet", "tag": "retweet-2" } }),
                None,
            ),
            (
                json!({ "data": { "type": "tweet", "tag": "tweet-" } }),
                None,
            ),
            (
                json!({ "data": { "type": "tweet", "tag": "tweet-1x" } }),
                None,
            ),
            (json!({ "data": { "type": "tweet", "tag": 5 } }), None),
            (json!({ "data": { "type": "tweet" } }), None),
            // 外側の `tag` だけでは足りない。`classify` と同じ `data` を読む。
            (
                json!({ "tag": "tweet-3", "data": { "type": "tweet" } }),
                None,
            ),
            (json!({ "tag": "tweet-3" }), None),
        ];
        for (payload, expected) in cases {
            assert_eq!(tweet_id(&payload).as_deref(), expected, "{payload}");
        }
    }

    mod lookup {
        use super::*;
        use crate::tweet_lookup::mock::{MockApi, REFUSED_ENDPOINT, http_response, json_response};

        const PLAIN_REQUEST: &str = "GET /tweet-result?id=2109197207392141321&token=0 HTTP/1.1";

        async fn kind_after(response: Vec<u8>, payload: &Value) -> (NotificationKind, Vec<String>) {
            let api = MockApi::start(response).await;
            let lookup = TweetLookup::with_endpoint(&api.url).unwrap();
            (resolve_kind(payload, &lookup).await, api.requests())
        }

        #[tokio::test]
        async fn a_plain_tweet_becomes_a_quote_or_a_reply_by_the_lookup() {
            // 引用元の投稿者が同じなら自己引用、違うか判別できなければ他アカウントの引用。
            let cases = [
                (
                    r#"{"__typename":"Tweet","quoted_tweet":{"id_str":"1"}}"#,
                    NotificationKind::Quote,
                ),
                (
                    r#"{"__typename":"Tweet","user":{"id_str":"7"},"quoted_tweet":{"user":{"id_str":"7"}}}"#,
                    NotificationKind::SelfQuote,
                ),
                (
                    r#"{"__typename":"Tweet","user":{"id_str":"7"},"quoted_tweet":{"user":{"id_str":"8"}}}"#,
                    NotificationKind::Quote,
                ),
                (
                    r#"{"__typename":"Tweet","in_reply_to_status_id_str":"1"}"#,
                    NotificationKind::Reply,
                ),
                (
                    r#"{"__typename":"Tweet","in_reply_to_status_id_str":"1","quoted_tweet":{}}"#,
                    NotificationKind::Reply,
                ),
                (
                    r#"{"__typename":"Tweet","in_reply_to_status_id_str":"1","user":{"id_str":"7"},"quoted_tweet":{"user":{"id_str":"7"}}}"#,
                    NotificationKind::Reply,
                ),
                (r#"{"__typename":"Tweet"}"#, NotificationKind::Tweet),
                (
                    r#"{"__typename":"TweetTombstone"}"#,
                    NotificationKind::Tweet,
                ),
            ];
            for (response, expected) in cases {
                let wrapped = json!({ "notification": plain_tweet_payload() });
                for payload in [plain_tweet_payload(), wrapped] {
                    let (kind, requests) = kind_after(json_response(response), &payload).await;
                    assert_eq!(kind, expected, "{response}");
                    assert_eq!(requests, [PLAIN_REQUEST]);
                }
            }
        }

        #[tokio::test]
        async fn a_failed_lookup_leaves_the_tweet_a_tweet() {
            let quote = r#"{"__typename":"Tweet","quoted_tweet":{}}"#;
            let failures = [
                http_response(
                    "500 Internal Server Error",
                    "application/json",
                    quote.as_bytes(),
                ),
                http_response("404 Not Found", "text/html", b"<html></html>"),
                http_response("200 OK", "text/html", b"<html></html>"),
                http_response("200 OK", "application/json", b"{"),
            ];
            for response in failures {
                let (kind, requests) = kind_after(response, &plain_tweet_payload()).await;
                assert_eq!(kind, NotificationKind::Tweet);
                assert_eq!(requests.len(), 1);
            }
            let refused = TweetLookup::with_endpoint(REFUSED_ENDPOINT).unwrap();
            assert_eq!(
                resolve_kind(&plain_tweet_payload(), &refused).await,
                NotificationKind::Tweet
            );
        }

        #[tokio::test]
        async fn nothing_but_a_plain_tweet_is_looked_up() {
            let quote = json_response(r#"{"__typename":"Tweet","quoted_tweet":{}}"#);
            let api = MockApi::start(quote).await;
            let lookup = TweetLookup::with_endpoint(&api.url).unwrap();
            let cases = [
                // リツイートの ID を照会すると元ツイートが返り、引用RT と誤判定する。
                (retweet_payload(), NotificationKind::Retweet),
                (
                    json!({ "data": { "type": "xchat_message_received", "tag": "tweet-3" } }),
                    NotificationKind::XchatMessageReceived,
                ),
                (
                    json!({ "data": { "type": "novel", "tag": "tweet-4" } }),
                    NotificationKind::Other,
                ),
                (
                    json!({ "data": { "tag": "tweet-5" } }),
                    NotificationKind::Other,
                ),
                (json!({ "title": "t" }), NotificationKind::Other),
                // 自身の ID を取り出せない投稿は、照会せず Tweet のまま。
                (typed("tweet", 1), NotificationKind::Tweet),
                (
                    json!({ "data": { "type": "tweet", "tag": "retweet-2" } }),
                    NotificationKind::Tweet,
                ),
            ];
            for (payload, expected) in cases {
                assert_eq!(resolve_kind(&payload, &lookup).await, expected, "{payload}");
            }
            assert!(api.requests().is_empty());
        }
    }
}
