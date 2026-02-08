use crate::api::error::ApiError;
use crate::app::AppState;
use axum::http::HeaderMap;

pub fn require_sensitive_access(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(expected_token) = state.api_token.as_deref() else {
        return Ok(());
    };

    let Some(provided_token) = extract_token(headers) else {
        return Err(ApiError::forbidden(
            "missing API token; send Authorization: Bearer <token> or X-Chief-Token",
        ));
    };

    if provided_token == expected_token {
        Ok(())
    } else {
        Err(ApiError::forbidden("invalid API token"))
    }
}

fn extract_token(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("x-chief-token") {
        if let Ok(token) = value.to_str() {
            let trimmed = token.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }

    let auth = headers.get("authorization")?;
    let auth = auth.to_str().ok()?;
    let token = auth.strip_prefix("Bearer ")?.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::extract_token;
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn extracts_x_chief_token() {
        let mut headers = HeaderMap::new();
        headers.insert("x-chief-token", HeaderValue::from_static("abc123"));
        assert_eq!(extract_token(&headers).as_deref(), Some("abc123"));
    }

    #[test]
    fn extracts_bearer_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer super-secret"),
        );
        assert_eq!(extract_token(&headers).as_deref(), Some("super-secret"));
    }

    #[test]
    fn returns_none_when_no_supported_header() {
        let headers = HeaderMap::new();
        assert!(extract_token(&headers).is_none());
    }
}
