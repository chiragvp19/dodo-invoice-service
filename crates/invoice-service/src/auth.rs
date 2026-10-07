//! API keys: `sk_<prefix:8>_<secret:32>`, both parts base62.
//!
//! Stored: the prefix in plaintext (indexed lookup) and SHA-256 of the full key.
//! A fast hash is fine here: the key is ~190 bits of randomness, not a password,
//! so there is nothing for a slow hash to protect against. Comparison is
//! constant-time. Unknown, revoked and malformed keys all get the same 401.

use axum::{extract::FromRequestParts, http::request::Parts};
use rand::{distributions::Alphanumeric, Rng};
use sha2::{Digest, Sha256};
use sqlx::PgExecutor;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{error::ApiError, AppState};

const PREFIX_LEN: usize = 8;
const SECRET_LEN: usize = 32;

/// The authenticated business for this request.
#[derive(Clone, Copy, Debug)]
pub struct Auth {
    pub business_id: Uuid,
}

pub struct NewApiKey {
    pub id: Uuid,
    pub prefix: String,
    /// Plaintext; returned to the caller exactly once.
    pub key: String,
}

pub fn random_base62(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

pub fn hash_key(key: &str) -> Vec<u8> {
    Sha256::digest(key.as_bytes()).to_vec()
}

/// Splits `sk_<prefix>_<secret>`; returns the prefix if the shape is valid.
pub fn parse_key(key: &str) -> Option<&str> {
    let rest = key.strip_prefix("sk_")?;
    let (prefix, secret) = rest.split_once('_')?;
    let valid = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_alphanumeric());
    (valid(prefix, PREFIX_LEN) && valid(secret, SECRET_LEN)).then_some(prefix)
}

pub fn generate_key() -> NewApiKey {
    let prefix = random_base62(PREFIX_LEN);
    let key = format!("sk_{prefix}_{}", random_base62(SECRET_LEN));
    NewApiKey { id: Uuid::now_v7(), prefix, key }
}

/// Inserts a key row for `business_id`. Plain INSERT: a (astronomically unlikely)
/// prefix collision fails loudly instead of touching another business's key.
pub async fn store_key(
    db: impl PgExecutor<'_>,
    business_id: Uuid,
    key: &NewApiKey,
) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO api_keys (id, business_id, prefix, key_hash) VALUES ($1, $2, $3, $4)")
    .bind(key.id)
    .bind(business_id)
    .bind(&key.prefix)
    .bind(hash_key(&key.key))
    .execute(db)
    .await?;
    Ok(())
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let key = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(ApiError::Unauthorized)?;
        let prefix = parse_key(key).ok_or(ApiError::Unauthorized)?;

        let row: Option<(Uuid, Vec<u8>)> = sqlx::query_as(
            "SELECT business_id, key_hash FROM api_keys WHERE prefix = $1 AND revoked_at IS NULL",
        )
        .bind(prefix)
        .fetch_optional(&state.db)
        .await?;

        match row {
            Some((business_id, stored)) if bool::from(stored.ct_eq(&hash_key(key))) => {
                Ok(Auth { business_id })
            }
            _ => Err(ApiError::Unauthorized),
        }
    }
}

/// Guard for the admin bootstrap endpoint (`X-Admin-Token`).
pub struct Admin;

impl FromRequestParts<AppState> for Admin {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let expected = state.config.admin_token.as_deref().ok_or(ApiError::Unauthorized)?;
        let given = parts
            .headers
            .get("x-admin-token")
            .and_then(|v| v.to_str().ok())
            .ok_or(ApiError::Unauthorized)?;
        if bool::from(given.as_bytes().ct_eq(expected.as_bytes())) {
            Ok(Admin)
        } else {
            Err(ApiError::Unauthorized)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_parse() {
        let k = generate_key();
        assert_eq!(parse_key(&k.key), Some(k.prefix.as_str()));
    }

    #[test]
    fn malformed_keys_rejected() {
        for bad in ["", "sk_", "sk_short_x", "pk_abcdefgh_0123456789abcdef0123456789abcdef", "sk_abcdefgh_too-short"] {
            assert_eq!(parse_key(bad), None, "{bad}");
        }
        assert!(parse_key("sk_demo0001_LocalDemoKeyNotForProductionUse0").is_some());
    }
}
