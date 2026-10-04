//! Errors as a caller of the HTTP server sees them.

use std::fmt::Display;

use axum::Json;
use axum::http::StatusCode;
use axum::response::sse::Event;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::DbError;

const SERVER_FAULT: &str = "agentdb failed while handling this request. The fault is on the server, not in the call: retry it, and if it keeps failing the operator will find the cause in the server log.";

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
        Self::server_fault("internal", &detail)
    }

    fn server_fault(code: &'static str, detail: &dyn Display) -> Self {
        eprintln!("agentdb: {code} error: {detail}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, code, SERVER_FAULT)
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
            code @ ("storage" | "internal") => Self::server_fault(code, &error),
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
        "changes_trimmed" => StatusCode::GONE,
        "batch_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
        "missing_api_key" => StatusCode::SERVICE_UNAVAILABLE,
        "model_unavailable" => StatusCode::BAD_GATEWAY,
        "newer_format" | "wrong_key" | "empty_key" => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    }
}
