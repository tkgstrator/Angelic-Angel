use crate::error::AngelicAngelError;
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// API failure rendered as `{"error":{"code","message"}}`; messages are always safe to show.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn invalid(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    pub fn busy() -> Self {
        Self::conflict(
            "busy",
            "Another operation is in progress. Try again shortly.",
        )
    }

    pub fn provider(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "provider_failure", message)
    }

    pub fn account_not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "account_not_found",
            "No such account.",
        )
    }

    pub fn is_account_not_found(&self) -> bool {
        self.status == StatusCode::NOT_FOUND && self.code == "account_not_found"
    }

    pub fn invalid_account_id() -> Self {
        Self::invalid(
            "invalid_account_id",
            "An account id uses 1-32 characters of a-z, 0-9, '_' or '-', starting with a letter or digit.",
        )
    }

    pub fn account_exists() -> Self {
        Self::conflict("account_exists", "An account with this id already exists.")
    }

    pub fn history_cleanup_required() -> Self {
        Self::conflict(
            "history_cleanup_required",
            "Notification history files of an earlier account with this id remain next to the \
             config file. Remove them manually before creating the account, so it does not \
             inherit that history.",
        )
    }
}

impl From<AngelicAngelError> for ApiError {
    fn from(error: AngelicAngelError) -> Self {
        // Details (paths, parser output) stay in the log, never in the response.
        tracing::error!(error = %error, "configuration storage failure");
        match error {
            AngelicAngelError::Toml(_) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "config_invalid",
                "The configuration file is invalid.",
            ),
            _ => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "persistence_failed",
                "Failed to read or write the configuration.",
            ),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "error": { "code": self.code, "message": self.message } });
        (self.status, Json(body)).into_response()
    }
}
