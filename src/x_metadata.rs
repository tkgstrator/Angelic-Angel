//! Strict parser for the public twitter-openapi operation catalogue. No JavaScript is executed,
//! no catalogue headers or variables are trusted, and the request destination is built locally.
use crate::twitter::XError;
use serde_json::{Value, json};

pub const CATALOGUE_URL: &str = "https://raw.githubusercontent.com/fa0311/twitter-openapi/refs/heads/main/src/config/placeholder.json";

const MAX_FEATURES: usize = 256;
const MAX_FIELD_TOGGLES: usize = 64;

#[derive(Clone, Debug)]
pub struct CreateTweet {
    pub query_id: String,
    pub features: Value,
    pub field_toggles: Value,
}

/// A GraphQL read (`TweetDetail`, `UserByRestId`). Only the query ID, the feature flags and the
/// field toggles come from the catalogue; its variables are never used.
#[derive(Clone, Debug)]
pub struct ReadOperation {
    pub query_id: String,
    pub features: Value,
    pub field_toggles: Value,
}

/// `FavoriteTweet` carries no feature flags, so only its query ID comes from the catalogue.
#[derive(Clone, Debug)]
pub struct FavoriteTweet {
    pub query_id: String,
}

/// The operations behind post lookup, user lookup and like. Each one is resolved on its own: an
/// operation that is missing or malformed is `None` and never affects the others.
#[derive(Clone, Debug, Default)]
pub struct ActionMetadata {
    pub tweet_detail: Option<ReadOperation>,
    pub user_by_rest_id: Option<ReadOperation>,
    pub favorite_tweet: Option<FavoriteTweet>,
}

impl ActionMetadata {
    pub fn has_lookup_post(&self) -> bool {
        self.tweet_detail.is_some()
    }

    pub fn has_lookup_user(&self) -> bool {
        self.user_by_rest_id.is_some()
    }

    pub fn has_like(&self) -> bool {
        self.favorite_tweet.is_some()
    }

    pub fn is_complete(&self) -> bool {
        self.has_lookup_post() && self.has_lookup_user() && self.has_like()
    }
}

/// An operation object and its validated query ID.
type Found<'a> = (&'a Value, &'a str);

/// Finds `name`, checks its query ID, and pins its method and path to the local GraphQL form.
fn resolve<'a>(root: &'a Value, name: &str, method: &str) -> Result<Found<'a>, XError> {
    let operation = root.get(name).ok_or(XError::Malformed)?;
    let query_id = operation
        .get("queryId")
        .and_then(Value::as_str)
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        })
        .ok_or(XError::Malformed)?;
    let path = format!("/i/api/graphql/{query_id}/{name}");
    if operation.get("@method").and_then(Value::as_str) != Some(method)
        || operation.get("@path").and_then(Value::as_str) != Some(path.as_str())
    {
        return Err(XError::Malformed);
    }
    Ok((operation, query_id))
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_flag(name: &str, value: &Value) -> bool {
    name.len() <= 256 && name.bytes().all(is_identifier_byte) && value.is_boolean()
}

/// An object of boolean flags with plain identifier names and between `min` and `max` entries.
fn is_flag_map(value: &Value, min: usize, max: usize) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };
    let named = map.iter().all(|(name, flag)| is_flag(name, flag));
    (min..=max).contains(&map.len()) && named
}

/// The feature flags of an operation; at least one is required.
fn feature_flags(operation: &Value) -> Result<Value, XError> {
    let flags = operation.get("features").ok_or(XError::Malformed)?;
    if !is_flag_map(flags, 1, MAX_FEATURES) {
        return Err(XError::Malformed);
    }
    Ok(flags.clone())
}

/// The field toggles of an operation; absent means none.
fn toggle_flags(operation: &Value) -> Result<Value, XError> {
    let found = operation.get("fieldToggles").cloned();
    let toggles = found.unwrap_or_else(|| json!({}));
    if !is_flag_map(&toggles, 0, MAX_FIELD_TOGGLES) {
        return Err(XError::Malformed);
    }
    Ok(toggles)
}

pub fn parse(source: &str) -> Result<CreateTweet, XError> {
    let root: Value = serde_json::from_str(source).map_err(|_| XError::Malformed)?;
    let (operation, query_id) = resolve(&root, "CreateTweet", "POST")?;
    Ok(CreateTweet {
        query_id: query_id.to_owned(),
        features: feature_flags(operation)?,
        field_toggles: toggle_flags(operation)?,
    })
}

fn read_operation(root: &Value, name: &str) -> Result<ReadOperation, XError> {
    let (operation, query_id) = resolve(root, name, "GET")?;
    Ok(ReadOperation {
        query_id: query_id.to_owned(),
        features: feature_flags(operation)?,
        field_toggles: toggle_flags(operation)?,
    })
}

fn favorite_tweet(root: &Value) -> Result<FavoriteTweet, XError> {
    let (_, query_id) = resolve(root, "FavoriteTweet", "POST")?;
    Ok(FavoriteTweet {
        query_id: query_id.to_owned(),
    })
}

/// Resolves the lookup and like operations. Only a catalogue that is not JSON fails as a whole; an
/// operation that cannot be trusted is left out so the others stay usable.
pub fn parse_actions(source: &str) -> Result<ActionMetadata, XError> {
    let root: Value = serde_json::from_str(source).map_err(|_| XError::Malformed)?;
    Ok(ActionMetadata {
        tweet_detail: read_operation(&root, "TweetDetail").ok(),
        user_by_rest_id: read_operation(&root, "UserByRestId").ok(),
        favorite_tweet: favorite_tweet(&root).ok(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/machine-metadata.json");
    const ACTIONS: &str = include_str!("../tests/fixtures/machine-actions-metadata.json");

    #[test]
    fn resolves_changed_ids_without_a_fixed_id() {
        assert_eq!(parse(FIXTURE).unwrap().query_id, "synthetic-query-1");
        assert_eq!(
            parse(&FIXTURE.replace("synthetic-query-1", "rotated_2"))
                .unwrap()
                .query_id,
            "rotated_2"
        );
    }

    #[test]
    fn missing_or_hostile_metadata_fails_closed() {
        for value in [
            "{}".to_string(),
            FIXTURE.replace("POST", "GET"),
            FIXTURE.replace("synthetic-query-1", "../other"),
            FIXTURE.replace("true", "\"not-a-boolean\""),
            FIXTURE.replace("\"features\"", "\"missing\""),
            FIXTURE.replace("/i/api/graphql/", "https://evil.invalid/"),
        ] {
            assert!(parse(&value).is_err());
        }
    }

    fn ready(meta: &ActionMetadata) -> [bool; 3] {
        let post = meta.has_lookup_post();
        let user = meta.has_lookup_user();
        let like = meta.has_like();
        [post, user, like]
    }

    #[test]
    fn actions_resolve_ids_features_and_toggles_from_the_catalogue() {
        let meta = parse_actions(ACTIONS).unwrap();
        assert!(meta.is_complete());
        assert_eq!(ready(&meta), [true, true, true]);
        let detail = meta.tweet_detail.unwrap();
        assert_eq!(detail.query_id, "synthetic-detail-1");
        assert_eq!(detail.features["synthetic_detail_feature"], true);
        assert_eq!(detail.features["synthetic_other_feature"], false);
        assert_eq!(detail.field_toggles["synthetic_detail_toggle"], false);
        let user = meta.user_by_rest_id.unwrap();
        assert_eq!(user.query_id, "synthetic-user-1");
        assert_eq!(user.field_toggles, json!({}));
        assert_eq!(meta.favorite_tweet.unwrap().query_id, "synthetic-like-1");
    }

    #[test]
    fn actions_follow_rotated_ids() {
        let rotated = ACTIONS.replace("synthetic-like-1", "rotated_3");
        let meta = parse_actions(&rotated).unwrap();
        assert_eq!(meta.favorite_tweet.unwrap().query_id, "rotated_3");
        let rotated = ACTIONS.replace("synthetic-detail-1", "rotated-4");
        let meta = parse_actions(&rotated).unwrap();
        assert_eq!(meta.tweet_detail.unwrap().query_id, "rotated-4");
    }

    #[test]
    fn a_catalogue_without_these_operations_resolves_none_of_them() {
        let none = [false, false, false];
        let meta = parse_actions(FIXTURE).unwrap();
        assert_eq!(ready(&meta), none);
        assert!(!meta.is_complete());
        let empty = parse_actions("{}").unwrap();
        assert_eq!(ready(&empty), none);
        assert!(parse_actions("not json").is_err());
        assert_eq!(ready(&parse_actions("[]").unwrap()), none);
    }

    #[test]
    fn one_catalogue_serves_posting_and_actions_independently() {
        let posting: Value = serde_json::from_str(FIXTURE).unwrap();
        let extra = posting.as_object().unwrap().clone();
        let mut root: Value = serde_json::from_str(ACTIONS).unwrap();
        root.as_object_mut().unwrap().extend(extra);
        let source = root.to_string();
        assert_eq!(parse(&source).unwrap().query_id, "synthetic-query-1");
        assert!(parse_actions(&source).unwrap().is_complete());
    }

    #[test]
    fn a_broken_operation_only_disables_itself() {
        let cases = [
            ("TweetDetail", "@method", json!("POST")),
            ("TweetDetail", "@path", json!("https://evil.invalid/x")),
            ("TweetDetail", "queryId", json!("../other")),
            ("TweetDetail", "features", json!({})),
            ("TweetDetail", "fieldToggles", json!({"synthetic": "yes"})),
            ("UserByRestId", "@method", json!("POST")),
            ("UserByRestId", "features", json!({"synthetic": "yes"})),
            ("UserByRestId", "queryId", json!("a".repeat(129))),
            ("FavoriteTweet", "@method", json!("GET")),
            ("FavoriteTweet", "queryId", json!("")),
        ];
        for (operation, key, bad) in cases {
            let mut root: Value = serde_json::from_str(ACTIONS).unwrap();
            root[operation][key] = bad;
            let meta = parse_actions(&root.to_string()).unwrap();
            let expected = match operation {
                "TweetDetail" => [false, true, true],
                "UserByRestId" => [true, false, true],
                _ => [true, true, false],
            };
            assert_eq!(ready(&meta), expected, "{operation} {key}");
        }
    }

    #[test]
    fn a_removed_operation_only_disables_itself() {
        for operation in ["TweetDetail", "UserByRestId", "FavoriteTweet"] {
            let mut root: Value = serde_json::from_str(ACTIONS).unwrap();
            assert!(root.as_object_mut().unwrap().remove(operation).is_some());
            let meta = parse_actions(&root.to_string()).unwrap();
            assert_eq!(ready(&meta).iter().filter(|ok| **ok).count(), 2);
            assert!(!meta.is_complete(), "{operation}");
        }
    }

    #[test]
    fn favorite_needs_no_features_but_reads_do() {
        let mut root: Value = serde_json::from_str(ACTIONS).unwrap();
        root["FavoriteTweet"]["features"] = json!({"synthetic": "not-a-flag"});
        assert!(parse_actions(&root.to_string()).unwrap().has_like());
        root["TweetDetail"]["features"] = Value::Null;
        root["UserByRestId"]["fieldToggles"] = Value::Null;
        let meta = parse_actions(&root.to_string()).unwrap();
        assert_eq!(ready(&meta), [false, false, true]);
    }

    #[test]
    fn oversized_flag_sets_are_refused() {
        let mut flags = serde_json::Map::new();
        for index in 0..=MAX_FEATURES {
            flags.insert(format!("flag_{index}"), Value::Bool(true));
        }
        let mut root: Value = serde_json::from_str(ACTIONS).unwrap();
        root["TweetDetail"]["features"] = Value::Object(flags);
        let meta = parse_actions(&root.to_string()).unwrap();
        assert_eq!(ready(&meta), [false, true, true]);
    }
}
