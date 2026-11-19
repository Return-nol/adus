use axum::{
    Json,
    extract::{
        FromRequest, FromRequestParts,
        rejection::{JsonRejection, PathRejection},
    },
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

    pub fn not_found() -> ApiError {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", "not found")
    }

    pub fn conflict(code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::CONFLICT, code, message)
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

/// Seperti `axum::Json`, tetapi penolakan (JSON rusak, field hilang, content-type salah)
/// memakai format `ApiError`. Pakai ini di semua handler yang menerima body JSON.
#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub struct ApiJson<T>(pub T);

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> ApiError {
        let message = rejection.body_text();
        match rejection {
            JsonRejection::JsonDataError(_) => ApiError::validation(message),
            JsonRejection::MissingJsonContentType(_) => ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                message,
            ),
            _ => ApiError::new(StatusCode::BAD_REQUEST, "bad_request", message),
        }
    }
}

/// Seperti `axum::extract::Path`, tetapi path yang tidak valid (mis. bukan UUID) menjadi 404 `not_found`.
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Path), rejection(ApiError))]
pub struct ApiPath<T>(pub T);

impl From<PathRejection> for ApiError {
    fn from(rejection: PathRejection) -> ApiError {
        match rejection {
            // Segmen path tidak cocok tipenya (mis. bukan UUID): sumber daya tidak ada.
            PathRejection::FailedToDeserializePathParams(_) => ApiError::not_found(),
            // Selain itu (MissingPathParams dll.) salah routing di sisi server.
            other => {
                tracing::error!("path extraction failed: {other}");
                ApiError::internal()
            }
        }
    }
}
