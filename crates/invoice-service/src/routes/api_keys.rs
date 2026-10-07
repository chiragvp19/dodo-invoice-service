use axum::{extract::State, http::StatusCode, Json};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    auth::{self, Auth},
    error::{ApiError, ApiPath, ApiResult},
    AppState,
};

/// Creates an additional key for the calling business (rotation: create new,
/// deploy it, then revoke the old one).
pub async fn create(
    auth: Auth,
    State(state): State<AppState>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let key = auth::generate_key();
    auth::store_key(&state.db, auth.business_id, &key).await?;
    tracing::info!(business_id = %auth.business_id, key_prefix = %key.prefix, "api key created");
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": key.id, "prefix": key.prefix, "key": key.key })),
    ))
}

/// Revokes a key. Idempotent: revoking an already-revoked key returns its
/// original revocation time.
pub async fn revoke(
    auth: Auth,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<Value>> {
    let row: Option<(String, DateTime<Utc>)> = sqlx::query_as(
        "UPDATE api_keys SET revoked_at = COALESCE(revoked_at, now())
         WHERE id = $1 AND business_id = $2
         RETURNING prefix, revoked_at",
    )
    .bind(id)
    .bind(auth.business_id)
    .fetch_optional(&state.db)
    .await?;
    let (prefix, revoked_at) = row.ok_or(ApiError::NotFound("API key"))?;
    tracing::info!(business_id = %auth.business_id, key_prefix = %prefix, "api key revoked");
    Ok(Json(
        json!({ "id": id, "prefix": prefix, "revoked_at": revoked_at }),
    ))
}
