use axum::{extract::State, http::StatusCode, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::FromRow;
use uuid::Uuid;

use crate::{
    auth::{self, Auth},
    error::{ApiError, ApiJson, ApiQuery, ApiResult},
    outbox,
    pagination::{self, List, Page},
    AppState,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateEndpoint {
    url: String,
}

#[derive(Serialize, FromRow)]
pub struct Endpoint {
    id: Uuid,
    url: String,
    active: bool,
    created_at: DateTime<Utc>,
}

/// Registers an endpoint. The signing secret is generated here and returned once.
pub async fn create(
    auth: Auth,
    State(state): State<AppState>,
    ApiJson(req): ApiJson<CreateEndpoint>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let url = reqwest::Url::parse(req.url.trim())
        .map_err(|_| ApiError::validation("url must be an absolute URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err(ApiError::validation("url must be http(s) with a host"));
    }
    let secret = format!("whsec_{}", auth::random_base62(32));
    let endpoint: Endpoint = sqlx::query_as(
        "INSERT INTO webhook_endpoints (id, business_id, url, secret) VALUES ($1, $2, $3, $4)
         RETURNING id, url, active, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(auth.business_id)
    .bind(url.as_str())
    .bind(&secret)
    .fetch_one(&state.db)
    .await?;
    tracing::info!(endpoint_id = %endpoint.id, business_id = %auth.business_id, url = %endpoint.url, "webhook endpoint registered");
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": endpoint.id, "url": endpoint.url, "active": endpoint.active,
            "created_at": endpoint.created_at, "secret": secret,
        })),
    ))
}

pub async fn list(
    auth: Auth,
    State(state): State<AppState>,
    ApiQuery(page): ApiQuery<Page>,
) -> ApiResult<Json<List<Endpoint>>> {
    let limit = page.limit()?;
    let rows = sqlx::query_as(
        "SELECT id, url, active, created_at FROM webhook_endpoints
         WHERE business_id = $1 AND ($2::uuid IS NULL OR id < $2)
         ORDER BY id DESC LIMIT $3",
    )
    .bind(auth.business_id)
    .bind(page.starting_after)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(List::from_rows(rows, limit)))
}

#[derive(Serialize, FromRow)]
pub struct Event {
    id: Uuid,
    #[serde(rename = "type")]
    #[sqlx(rename = "type")]
    event_type: String,
    created_at: DateTime<Utc>,
    #[sqlx(rename = "payload")]
    data: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsQuery {
    #[serde(rename = "type")]
    event_type: Option<String>,
    limit: Option<i64>,
    starting_after: Option<Uuid>,
}

/// Event log, newest first: how a business reconciles webhooks it missed.
pub async fn events(
    auth: Auth,
    State(state): State<AppState>,
    ApiQuery(q): ApiQuery<EventsQuery>,
) -> ApiResult<Json<List<Event>>> {
    let limit = pagination::limit(q.limit)?;
    if let Some(t) = &q.event_type {
        if !outbox::EVENT_TYPES.contains(&t.as_str()) {
            return Err(ApiError::validation(format!(
                "unknown event type '{t}'; expected one of {}",
                outbox::EVENT_TYPES.join(", ")
            )));
        }
    }
    let rows = sqlx::query_as(
        "SELECT id, type, created_at, payload FROM webhook_events
         WHERE business_id = $1
           AND ($2::text IS NULL OR type = $2)
           AND ($3::uuid IS NULL OR id < $3)
         ORDER BY id DESC LIMIT $4",
    )
    .bind(auth.business_id)
    .bind(q.event_type)
    .bind(q.starting_after)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(List::from_rows(rows, limit)))
}
