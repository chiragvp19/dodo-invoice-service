use axum::{extract::State, http::StatusCode, Json};
use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::{
    auth::Auth,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    invoices::{self, Invoice, LineItemInput},
    outbox,
    pagination::{List, Page},
    state_machine::{self, Action, InvoiceStatus},
    AppState,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateInvoice {
    customer_id: Uuid,
    due_date: NaiveDate,
    line_items: Vec<LineItemInput>,
}

/// Creates a draft invoice. The total is computed server-side; a client-sent
/// `total` field is rejected by `deny_unknown_fields`.
pub async fn create(
    auth: Auth,
    State(state): State<AppState>,
    ApiJson(req): ApiJson<CreateInvoice>,
) -> ApiResult<(StatusCode, Json<Invoice>)> {
    let (items, total_cents) = invoices::price_line_items(&req.line_items)?;
    if req.due_date < Utc::now().date_naive() {
        return Err(ApiError::validation("due_date must not be in the past (UTC)"));
    }

    let mut tx = state.db.begin().await?;
    let customer_exists: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM customers WHERE id = $1 AND business_id = $2")
            .bind(req.customer_id)
            .bind(auth.business_id)
            .fetch_optional(&mut *tx)
            .await?;
    if customer_exists.is_none() {
        return Err(ApiError::NotFound("Customer"));
    }

    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO invoices (id, business_id, customer_id, status, total_cents, due_date)
         VALUES ($1, $2, $3, 'draft', $4, $5)",
    )
    .bind(id)
    .bind(auth.business_id)
    .bind(req.customer_id)
    .bind(total_cents)
    .bind(req.due_date)
    .execute(&mut *tx)
    .await?;
    for (position, item) in items.iter().enumerate() {
        sqlx::query(
            "INSERT INTO invoice_line_items
                 (id, invoice_id, position, description, quantity, unit_amount_cents, amount_cents)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(Uuid::now_v7())
        .bind(id)
        .bind(position as i32)
        .bind(&item.description)
        .bind(item.quantity)
        .bind(item.unit_amount_cents)
        .bind(item.amount_cents)
        .execute(&mut *tx)
        .await?;
    }
    let invoice = invoices::load(&mut tx, auth.business_id, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("invoice vanished inside its own transaction"))?;
    outbox::enqueue(&mut tx, auth.business_id, outbox::INVOICE_CREATED, json!({ "invoice": invoice })).await?;
    tx.commit().await?;

    tracing::info!(invoice_id = %id, business_id = %auth.business_id, total_cents, "invoice created (draft)");
    Ok((StatusCode::CREATED, Json(invoice)))
}

pub async fn get(
    auth: Auth,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<Invoice>> {
    let mut conn = state.db.acquire().await?;
    invoices::load(&mut conn, auth.business_id, id)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound("Invoice"))
}

#[derive(Deserialize)]
pub struct ListQuery {
    status: Option<String>,
    #[serde(flatten)]
    page: Page,
}

pub async fn list(
    auth: Auth,
    State(state): State<AppState>,
    ApiQuery(q): ApiQuery<ListQuery>,
) -> ApiResult<Json<List<Invoice>>> {
    let limit = q.page.limit()?;
    if let Some(status) = &q.status {
        if InvoiceStatus::parse(status).is_none() {
            return Err(ApiError::validation(format!("unknown status '{status}'")));
        }
    }
    let mut conn = state.db.acquire().await?;
    let rows = invoices::list(&mut conn, auth.business_id, q.status.as_deref(), q.page.starting_after, limit).await?;
    Ok(Json(List::from_rows(rows, limit)))
}

pub async fn finalize(auth: Auth, state: State<AppState>, id: ApiPath<Uuid>) -> ApiResult<Json<Invoice>> {
    apply(auth, state, id, Action::Finalize).await
}

pub async fn void(auth: Auth, state: State<AppState>, id: ApiPath<Uuid>) -> ApiResult<Json<Invoice>> {
    apply(auth, state, id, Action::Void).await
}

pub async fn mark_uncollectible(auth: Auth, state: State<AppState>, id: ApiPath<Uuid>) -> ApiResult<Json<Invoice>> {
    apply(auth, state, id, Action::MarkUncollectible).await
}

/// Applies a manual transition with a status-conditional UPDATE whose guard is
/// derived from the state machine. 0 rows matched means 404 or 409.
async fn apply(
    auth: Auth,
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    action: Action,
) -> ApiResult<Json<Invoice>> {
    let allowed = state_machine::allowed_from(action);
    // These actions have a single target regardless of source state.
    let from = InvoiceStatus::parse(allowed[0]).expect("state machine statuses parse");
    let to = state_machine::transition(from, action).expect("allowed_from is consistent");

    let mut tx = state.db.begin().await?;
    let updated = sqlx::query(
        "UPDATE invoices SET status = $3, updated_at = now()
         WHERE id = $1 AND business_id = $2 AND status = ANY($4)",
    )
    .bind(id)
    .bind(auth.business_id)
    .bind(to.as_str())
    .bind(&allowed)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(invoices::rejection(&mut tx, auth.business_id, id, action.name()).await);
    }
    let invoice = invoices::load(&mut tx, auth.business_id, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("invoice vanished inside its own transaction"))?;
    tx.commit().await?;

    tracing::info!(invoice_id = %id, action = action.name(), to = %to, "invoice transition");
    Ok(Json(invoice))
}
