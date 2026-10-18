//! Strict parser for the public twitter-openapi operation catalogue. No JavaScript is executed,
//! no catalogue headers or variables are trusted, and the request destination is built locally.
use crate::twitter::XError;
use serde_json::Value;

pub const CATALOGUE_URL: &str = "https://raw.githubusercontent.com/fa0311/twitter-openapi/refs/heads/main/src/config/placeholder.json";

#[derive(Clone, Debug)]
pub struct CreateTweet {
    pub query_id: String,
    pub features: Value,
    pub field_toggles: Value,
}

pub fn parse(source: &str) -> Result<CreateTweet, XError> {
    let root: Value = serde_json::from_str(source).map_err(|_| XError::Malformed)?;
    let operation = root.get("CreateTweet").ok_or(XError::Malformed)?;
    let query_id = operation.get("queryId").and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 128
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')))
        .ok_or(XError::Malformed)?;
    let path = format!("/i/api/graphql/{query_id}/CreateTweet");
    if operation.get("@method").and_then(Value::as_str) != Some("POST")
        || operation.get("@path").and_then(Value::as_str) != Some(path.as_str()) {
        return Err(XError::Malformed);
    }
    let features = operation.get("features").filter(|v| v.is_object())
        .ok_or(XError::Malformed)?;
    if features.as_object().is_none_or(|m| m.is_empty() || m.len() > 256)
        || features.as_object().is_none_or(|m| m.iter().any(|(k, v)| {
            k.len() > 256 || !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || !v.is_boolean()
        })) {
        return Err(XError::Malformed);
    }
    let field_toggles = operation.get("fieldToggles").cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if field_toggles.as_object().is_none_or(|m| m.len() > 64 || m.iter().any(|(k, v)| {
        k.len() > 256 || !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || !v.is_boolean()
    })) {
        return Err(XError::Malformed);
    }
    Ok(CreateTweet { query_id: query_id.to_owned(), features: features.clone(), field_toggles })
}

#[cfg(test)]
mod tests {
    use super::*;
    const FIXTURE: &str = include_str!("../tests/fixtures/machine-metadata.json");

    #[test]
    fn resolves_changed_ids_without_a_fixed_id() {
        assert_eq!(parse(FIXTURE).unwrap().query_id, "synthetic-query-1");
        assert_eq!(parse(&FIXTURE.replace("synthetic-query-1", "rotated_2")).unwrap().query_id, "rotated_2");
    }

    #[test]
    fn missing_or_hostile_metadata_fails_closed() {
        for value in ["{}".to_string(), FIXTURE.replace("POST", "GET"),
            FIXTURE.replace("synthetic-query-1", "../other"),
            FIXTURE.replace("true", "\"not-a-boolean\""),
            FIXTURE.replace("\"features\"", "\"missing\""),
            FIXTURE.replace("/i/api/graphql/", "https://evil.invalid/")] {
            assert!(parse(&value).is_err());
        }
    }
}
