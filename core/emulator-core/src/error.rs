//! The JSON error envelope Resource Manager and Key Vault share:
//! `{"error": {"code": "...", "message": "..."}}`.

use axum::response::{IntoResponse, Response};
use http::{header, StatusCode};
use serde_json::json;

#[derive(Clone, Debug)]
pub struct ArmError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
}

impl ArmError {
    pub fn new(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn bad_request(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    pub fn not_found(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, code, message)
    }

    pub fn conflict(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    /// The honest answer for anything the emulator does not implement yet.
    pub fn not_implemented(what: impl std::fmt::Display) -> Self {
        Self::new(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            format!("The emulator does not implement {what} yet."),
        )
    }
}

impl IntoResponse for ArmError {
    fn into_response(self) -> Response {
        let body = json!({"error": {"code": self.code, "message": self.message}});
        (
            self.status,
            [
                (header::CONTENT_TYPE, "application/json; charset=utf-8"),
                (
                    header::HeaderName::from_static("x-ms-error-code"),
                    self.code.as_str(),
                ),
            ],
            body.to_string(),
        )
            .into_response()
    }
}
