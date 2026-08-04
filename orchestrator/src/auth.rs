//! JWT authentication for cloud endpoints

use axum::http::{header::AUTHORIZATION, StatusCode};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use tracing::{error, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    #[serde(rename = "userId")]
    pub user_id: String,
    pub email: Option<String>,
    pub username: Option<String>,
    pub role: Option<String>,
    #[serde(rename = "accountId")]
    pub account_id: Option<String>,
    pub exp: usize,
    pub iat: usize,
    pub aud: Option<String>,
    pub iss: Option<String>,
}

/// Extract and verify Bearer token from headers
pub fn extract_bearer_token(
    headers: &axum::http::HeaderMap,
    jwt_secret: &str,
) -> Result<Claims, StatusCode> {
    if jwt_secret.is_empty() {
        return Ok(Claims {
            user_id: "anonymous".to_string(),
            email: None,
            username: None,
            role: None,
            account_id: None,
            exp: 0,
            iat: 0,
            aud: None,
            iss: None,
        });
    }

    let auth_header = match headers.get(AUTHORIZATION) {
        Some(header) => match header.to_str() {
            Ok(s) => s,
            Err(_) => {
                warn!("Invalid Authorization header encoding");
                return Err(StatusCode::UNAUTHORIZED);
            }
        },
        None => {
            warn!("Missing Authorization header");
            return Err(StatusCode::UNAUTHORIZED);
        }
    };

    if !auth_header.starts_with("Bearer ") {
        warn!("Invalid Authorization header format");
        return Err(StatusCode::UNAUTHORIZED);
    }

    let token = &auth_header[7..];

    let key = DecodingKey::from_secret(jwt_secret.as_ref());
    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_issuer(&["auth-service"]);
    validation.set_audience(&["heyo-app"]);

    match decode::<Claims>(token, &key, &validation) {
        Ok(token_data) => Ok(token_data.claims),
        Err(e) => {
            error!("JWT verification failed: {}", e);
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

/// Validate a raw JWT token string (without extracting from headers).
pub fn extract_bearer_token_raw(token: &str, jwt_secret: &str) -> Result<Claims, StatusCode> {
    if jwt_secret.is_empty() {
        return Ok(Claims {
            user_id: "anonymous".to_string(),
            email: None,
            username: None,
            role: None,
            account_id: None,
            exp: 0,
            iat: 0,
            aud: None,
            iss: None,
        });
    }

    let key = DecodingKey::from_secret(jwt_secret.as_ref());
    let mut validation = Validation::new(Algorithm::HS256);
    validation.set_issuer(&["auth-service"]);
    validation.set_audience(&["heyo-app"]);

    match decode::<Claims>(token, &key, &validation) {
        Ok(token_data) => Ok(token_data.claims),
        Err(e) => {
            warn!("JWT verification failed for raw token: {}", e);
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

pub fn require_internal_api_key(
    headers: &axum::http::HeaderMap,
    expected_api_key: &str,
) -> Result<(), StatusCode> {
    if expected_api_key.is_empty() {
        error!("Internal API key is not configured");
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }

    let auth_header = match headers.get(AUTHORIZATION) {
        Some(header) => match header.to_str() {
            Ok(value) => value,
            Err(_) => return Err(StatusCode::UNAUTHORIZED),
        },
        None => return Err(StatusCode::UNAUTHORIZED),
    };

    if auth_header != format!("Bearer {expected_api_key}") {
        warn!("Invalid internal API key");
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(())
}
