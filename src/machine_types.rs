//! Shared data types for the Bot-facing lookup and action operations.
//!
//! These mirror what the Bot reads from X today (see the Bot's `XPost`, `XUserProfile`, `XLikeResult`
//! and `XFollowResult`). Fields X did not report stay `None`/`null`; they are never turned into `false`
//! or guessed from text. Nothing here carries a Cookie, a token or raw upstream bodies.
use serde::Serialize;

/// A post as X reported it. Only IDs the response stated explicitly are filled in.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PostInfo {
    pub id: String,
    pub text: String,
    pub author_id: String,
    pub author_screen_name: String,
    pub conversation_id: String,
    /// UTC ISO 8601, only when X's timestamp could be parsed.
    pub created_at: Option<String>,
    pub reply_to_post_id: Option<String>,
    /// Filled only when it is consistent with `in_reply_to_user_id_str` (or the only one available).
    pub reply_to_user_id: Option<String>,
    pub quoted_post_id: Option<String>,
    /// Filled only when the response embeds the quoted post and its ID matches `quoted_post_id`.
    pub quoted_author_id: Option<String>,
}

/// The viewer's (authenticated account's) relation to another user. `None` = X did not say.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRelation {
    pub blocked_by: Option<bool>,
    pub blocking: Option<bool>,
    pub followed_by: Option<bool>,
    pub following: Option<bool>,
    pub follow_request_sent: Option<bool>,
}

/// Public profile facts. `automated_label` is `Some(true)` only when the profile explicitly says it is
/// an automated account; the absence of a label is never evidence either way, so it is never `false`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserProfile {
    pub user_id: String,
    pub screen_name: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub created_at: Option<String>,
    pub automated_label: Option<bool>,
    pub default_profile_image: Option<bool>,
    pub is_protected: Option<bool>,
    pub followers_count: Option<u64>,
    pub following_count: Option<u64>,
    pub posts_count: Option<u64>,
    pub relation: UserRelation,
}

/// Result of a like. `already_liked` is reported as success: the post is liked either way.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LikeResult {
    pub post_id: String,
    pub already_liked: bool,
}

/// Result of a follow. Protected accounts can leave `following` false with `follow_request_sent` true.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowResult {
    pub user_id: String,
    pub following: Option<bool>,
    pub follow_request_sent: Option<bool>,
}
