use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chief::storage::db_reset_required_from_anyhow;
use serde::Serialize;
use serde_json::Value;
use std::fmt;

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
    code: Option<String>,
    details: Option<Value>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Value>,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            code: None,
            details: None,
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
            code: None,
            details: None,
        }
    }

    pub fn unprocessable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            message: message.into(),
            code: None,
            details: None,
        }
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
            code: None,
            details: None,
        }
    }

    pub fn chief_yaml_missing(config_path: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: ".chief/chief.yaml is required before starting a run for this project"
                .to_owned(),
            code: Some("chief_yaml_missing".to_owned()),
            details: Some(serde_json::json!({
                "config_path": config_path.into(),
                "hint": "create .chief/chief.yaml (run `chief init` or copy .chief/chief.example.yaml)",
            })),
        }
    }

    pub fn classify_store_error(error: anyhow::Error) -> Self {
        let message = error.to_string();
        if message.contains("not found") {
            Self::not_found(message)
        } else if message.contains("already exists") {
            Self::unprocessable(message)
        } else {
            Self::internal(error)
        }
    }

    pub fn internal(error: anyhow::Error) -> Self {
        if let Some(reset) = db_reset_required_from_anyhow(&error) {
            return Self {
                status: StatusCode::CONFLICT,
                message:
                    ".chief/chief.db is inconsistent for this project. Reset is required before continuing."
                        .to_owned(),
                code: Some("db_reset_required".to_owned()),
                details: Some(serde_json::json!({
                    "db_path": reset.db_path.display().to_string(),
                    "reason": reset.reason,
                })),
            };
        }
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
            code: None,
            details: None,
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        Self::internal(value)
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(code) = &self.code {
            write!(f, "{} ({code})", self.message)
        } else {
            write!(f, "{}", self.message)
        }
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
                code: self.code,
                details: self.details,
            }),
        )
            .into_response()
    }
}
