use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

/// Error API. Bentuk JSON: `{"error":{"code":"...","message":"..."}}`.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn validation(message: impl Into<String>) -> ApiError {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            message,
        )
    }

    pub fn unauthenticated() -> ApiError {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "authentication required",
        )
    }

    pub fn forbidden() -> ApiError {
        ApiError::new(StatusCode::FORBIDDEN, "forbidden", "admin access required")
    }

    pub fn internal() -> ApiError {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "error": { "code": self.code, "message": self.message } });
        (self.status, Json(body)).into_response()
    }
}

/// Detail dicatat di log, klien hanya melihat 500 `internal`.
impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> ApiError {
        tracing::error!("database error: {err}");
        ApiError::internal()
    }
}
