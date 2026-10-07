use axum::{extract::State, http::StatusCode, Json};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    auth::{self, Admin},
    error::{ApiError, ApiJson, ApiResult},
    AppState,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBusiness {
    name: String,
}

/// Admin bootstrap: creates a business and its first API key (shown once).
pub async fn create(
    _: Admin,
    State(state): State<AppState>,
    ApiJson(req): ApiJson<CreateBusiness>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let name = req.name.trim();
    if name.is_empty() || name.len() > 200 {
        return Err(ApiError::validation("name must be 1-200 characters"));
    }

    let mut tx = state.db.begin().await?;
    let id = Uuid::now_v7();
    let (created_at,): (DateTime<Utc>,) =
        sqlx::query_as("INSERT INTO businesses (id, name) VALUES ($1, $2) RETURNING created_at")
            .bind(id)
            .bind(name)
            .fetch_one(&mut *tx)
            .await?;
    let key = auth::generate_key();
    auth::store_key(&mut *tx, id, &key).await?;
    tx.commit().await?;

    tracing::info!(business_id = %id, key_prefix = %key.prefix, "business created");
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "business": { "id": id, "name": name, "created_at": created_at },
            "api_key": { "id": key.id, "prefix": key.prefix, "key": key.key },
        })),
    ))
}
