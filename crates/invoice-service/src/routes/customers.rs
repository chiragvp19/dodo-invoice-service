use axum::{extract::State, http::StatusCode, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::{
    auth::Auth,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    pagination::{List, Page},
    AppState,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCustomer {
    name: String,
    email: String,
}

#[derive(Serialize, FromRow)]
pub struct Customer {
    id: Uuid,
    name: String,
    email: String,
    created_at: DateTime<Utc>,
}

pub async fn create(
    auth: Auth,
    State(state): State<AppState>,
    ApiJson(req): ApiJson<CreateCustomer>,
) -> ApiResult<(StatusCode, Json<Customer>)> {
    let name = req.name.trim();
    let email = req.email.trim();
    if name.is_empty() || name.len() > 200 {
        return Err(ApiError::validation("name must be 1-200 characters"));
    }
    // Deliberately minimal: we never send email, so only a sanity check.
    if email.len() > 254 || !email.contains('@') || email.starts_with('@') || email.ends_with('@') {
        return Err(ApiError::validation("email must be a valid email address"));
    }
    let customer: Customer = sqlx::query_as(
        "INSERT INTO customers (id, business_id, name, email) VALUES ($1, $2, $3, $4)
         RETURNING id, name, email, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(auth.business_id)
    .bind(name)
    .bind(email)
    .fetch_one(&state.db)
    .await?;
    Ok((StatusCode::CREATED, Json(customer)))
}

pub async fn get(
    auth: Auth,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<Customer>> {
    sqlx::query_as("SELECT id, name, email, created_at FROM customers WHERE id = $1 AND business_id = $2")
        .bind(id)
        .bind(auth.business_id)
        .fetch_optional(&state.db)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound("Customer"))
}

pub async fn list(
    auth: Auth,
    State(state): State<AppState>,
    ApiQuery(page): ApiQuery<Page>,
) -> ApiResult<Json<List<Customer>>> {
    let limit = page.limit()?;
    let rows = sqlx::query_as(
        "SELECT id, name, email, created_at FROM customers
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
