//! Errors as a caller of the HTTP server sees them.

use std::fmt::Display;

use axum::Json;
use axum::http::StatusCode;
use axum::response::sse::Event;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::DbError;

const SERVER_FAULT: &str = "agentdb failed while handling this request. The fault is on the server, not in the call: retry it, and if it keeps failing the operator will find the cause in the server log.";
const MODEL_UNAVAILABLE: &str = "ask() could not get an answer from the language model. find() works without the model. If this keeps happening, the operator will find the cause in the server log.";

/// An error as the caller sees it: `{"error": {"code": ..., "message": ...}}`.
#[derive(Debug)]
pub(super) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    pub(super) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    /// A failure inside the server. The detail goes to the server's log and
    /// the caller is told only that the server is at fault: the detail can
    /// name files, SQL and other things a caller has no business knowing.
    pub(super) fn internal(detail: impl Display) -> Self {
        Self::logged("internal", &detail, SERVER_FAULT)
    }

    /// An error whose detail is for the operator: it goes to the log, and
    /// the caller reads `message` in its place.
    fn logged(code: &'static str, detail: &dyn Display, message: &str) -> Self {
        eprintln!("agentdb: {code} error: {detail}");
        Self::new(status_of(code), code, message)
    }

    fn body(&self) -> Value {
        json!({"error": {"code": self.code, "message": self.message}})
    }

    /// The error as the last event of a change feed that cannot go on.
    pub(super) fn event(&self) -> Event {
        Event::default()
            .event("error")
            .data(self.body().to_string())
    }
}

impl From<DbError> for ApiError {
    fn from(error: DbError) -> Self {
        match error.code() {
            code @ ("storage" | "internal") => Self::logged(code, &error, SERVER_FAULT),
            code @ "model_unavailable" => Self::logged(code, &error, MODEL_UNAVAILABLE),
            code => Self::new(status_of(code), code, error.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body())).into_response()
    }
}

/// The HTTP status for an error code: 404 when what the call names does not
/// exist, 409 when the call is valid but what is stored stands in its way,
/// 410 for history that is gone, 413 for more than one request may carry,
/// 5xx when the fault is not the caller's, and 400 for a call that has to
/// change before it can work.
fn status_of(code: &str) -> StatusCode {
    match code {
        "not_found" | "unknown_table" => StatusCode::NOT_FOUND,
        "version_conflict" | "duplicate_value" | "duplicates_exist" | "still_referenced"
        | "table_exists" | "field_exists" | "table_referenced" | "enum_value_in_use"
        | "index_required" | "would_destroy" => StatusCode::CONFLICT,
        "changes_trimmed" | "since_ahead" => StatusCode::GONE,
        "batch_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
        "missing_api_key" => StatusCode::SERVICE_UNAVAILABLE,
        "model_unavailable" => StatusCode::BAD_GATEWAY,
        "storage" | "internal" | "newer_format" | "wrong_key" | "empty_key" => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        _ => StatusCode::BAD_REQUEST,
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::ApiError;
    use crate::DbError;

    #[test]
    fn what_went_wrong_with_the_model_stays_in_the_log() {
        let error = ApiError::from(DbError::Jev(
            "dns lookup of model.internal:443 failed".to_owned(),
        ));
        assert_eq!(
            (error.status, error.code),
            (StatusCode::BAD_GATEWAY, "model_unavailable")
        );
        assert_eq!(
            error.message,
            "ask() could not get an answer from the language model. find() works without the model. If this keeps happening, the operator will find the cause in the server log."
        );
    }
}
