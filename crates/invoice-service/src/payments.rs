//! Payments: `POST /v1/invoices/{id}/pay`, plus the resolution logic shared with
//! the reconciler.
//!
//! Flow (see DESIGN.md section 3):
//! 1. Idempotency lookup: hash mismatch -> 422; completed -> replay stored bytes;
//!    in progress -> 409.
//! 2. Tx1 (one transaction): claim the key, move the invoice open|uncollectible ->
//!    processing with a status-conditional UPDATE, insert a `pending` attempt.
//!    Any rejection rolls back, which also releases the key (nothing cached).
//! 3. Call the PSP with no transaction or lock held; attempt id = PSP idempotency key.
//! 4. Definitive answer -> `resolve` (guarded). Ambiguous -> `mark_unknown`, 202.
//! 5. The reconciler settles `pending`/`unknown` attempts through the same `resolve`.

use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    auth::Auth,
    error::{error_body, ApiError, ApiJson, ApiPath, ApiResult},
    invoices::{self, Invoice},
    outbox,
    psp::ChargeOutcome,
    state_machine::{self, Action, InvoiceStatus},
    AppState,
};

/// How many times Tx1 may lose the key-claim race before we give up with 409.
const MAX_CLAIM_ATTEMPTS: usize = 3;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayRequest {
    card_token: String,
}

#[derive(Clone, Debug, Serialize, FromRow)]
pub struct PaymentAttempt {
    pub id: Uuid,
    pub invoice_id: Uuid,
    pub status: String,
    pub amount_cents: i64,
    pub psp_ref: Option<String>,
    pub failure_code: Option<String>,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
}

const ATTEMPT_COLUMNS: &str =
    "id, invoice_id, status, amount_cents, psp_ref, failure_code, created_at, resolved_at";

/// A final, storable HTTP response: status plus the exact body bytes.
#[derive(Clone, Debug)]
pub struct StoredResponse {
    pub status: StatusCode,
    pub body: Vec<u8>,
}

impl StoredResponse {
    fn into_response(self, replayed: bool) -> Response {
        let mut resp = (
            self.status,
            [(header::CONTENT_TYPE, "application/json")],
            self.body,
        )
            .into_response();
        if replayed {
            resp.headers_mut()
                .insert("idempotent-replayed", HeaderValue::from_static("true"));
        }
        resp
    }
}

/// The single response builder used by the request handler *and* the
/// reconciler, so a response stored by either one is the same bytes.
pub fn payment_response(invoice: &Invoice, attempt: &PaymentAttempt) -> StoredResponse {
    let (status, body) = match attempt.status.as_str() {
        "succeeded" => (
            StatusCode::OK,
            json!({ "invoice": invoice, "payment_attempt": attempt }),
        ),
        "failed" => {
            let code = attempt.failure_code.as_deref().unwrap_or("declined");
            (
                StatusCode::PAYMENT_REQUIRED,
                error_body(
                    "payment_failed",
                    &format!("Payment failed: {code}"),
                    json!({
                        "decline_code": code,
                        "payment_attempt_id": attempt.id,
                        "invoice_status": invoice.status,
                    }),
                ),
            )
        }
        // pending / unknown: outcome not known yet.
        _ => (
            StatusCode::ACCEPTED,
            json!({ "invoice": invoice, "payment_attempt": attempt }),
        ),
    };
    StoredResponse {
        status,
        body: serde_json::to_vec(&body).expect("json serializes"),
    }
}

pub async fn load_attempt(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<PaymentAttempt> {
    sqlx::query_as(&format!(
        "SELECT {ATTEMPT_COLUMNS} FROM payment_attempts WHERE id = $1"
    ))
    .bind(id)
    .fetch_one(&mut *conn)
    .await
}

pub async fn list_attempts(
    conn: &mut PgConnection,
    invoice_id: Uuid,
) -> sqlx::Result<Vec<PaymentAttempt>> {
    sqlx::query_as(&format!(
        "SELECT {ATTEMPT_COLUMNS} FROM payment_attempts WHERE invoice_id = $1 ORDER BY id DESC"
    ))
    .bind(invoice_id)
    .fetch_all(&mut *conn)
    .await
}

/// Settles an attempt with a definitive PSP answer. Used by the request handler
/// and the reconciler; whoever gets here first wins.
///
/// Guarded: only an attempt still `pending`/`unknown` is updated. Only then do we
/// move the invoice, write the outbox event, and complete the idempotency key,
/// and the key only if it is still `in_progress` (so a stored 202 is never
/// overwritten; a key left in progress by a crashed handler gets the final answer).
///
/// Returns `None` if someone else already resolved the attempt.
pub async fn resolve(
    db: &PgPool,
    attempt_id: Uuid,
    outcome: &ChargeOutcome,
) -> anyhow::Result<Option<StoredResponse>> {
    let (status, psp_ref, failure_code) = match outcome {
        ChargeOutcome::Succeeded { psp_ref } => ("succeeded", Some(psp_ref.as_str()), None),
        ChargeOutcome::Failed { code } => ("failed", None, Some(code.as_str())),
        ChargeOutcome::Ambiguous { .. } => anyhow::bail!("resolve() needs a definitive outcome"),
    };

    let mut tx = db.begin().await?;
    let row: Option<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE payment_attempts
         SET status = $2, psp_ref = $3, failure_code = $4, resolved_at = now()
         WHERE id = $1 AND status IN ('pending', 'unknown')
         RETURNING invoice_id, business_id",
    )
    .bind(attempt_id)
    .bind(status)
    .bind(psp_ref)
    .bind(failure_code)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((invoice_id, business_id)) = row else {
        return Ok(None); // already resolved elsewhere; tx rolls back on drop
    };

    // Move the invoice out of `processing`. On failure it returns to whatever
    // status it had before the payment (open or uncollectible).
    let previous: Option<(String,)> = sqlx::query_as(
        "SELECT pre_payment_status FROM invoices WHERE id = $1 AND status = 'processing'",
    )
    .bind(invoice_id)
    .fetch_optional(&mut *tx)
    .await?;
    let previous = previous
        .and_then(|(s,)| InvoiceStatus::parse(&s))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "invoice {invoice_id} is not processing while attempt {attempt_id} was in flight"
            )
        })?;
    let action = if status == "succeeded" {
        Action::PaymentSucceeded
    } else {
        Action::PaymentFailed { previous }
    };
    let to = state_machine::transition(InvoiceStatus::Processing, action)
        .map_err(|e| anyhow::anyhow!("illegal transition {e:?}"))?;
    let moved = sqlx::query(
        "UPDATE invoices
         SET status = $2, pre_payment_status = NULL, updated_at = now(),
             paid_at = CASE WHEN $2 = 'paid' THEN now() ELSE NULL END
         WHERE id = $1 AND status = 'processing'",
    )
    .bind(invoice_id)
    .bind(to.as_str())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    anyhow::ensure!(
        moved == 1,
        "invoice {invoice_id} left processing concurrently"
    );

    let invoice = invoices::load(&mut tx, business_id, invoice_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("invoice {invoice_id} vanished"))?;
    let attempt = load_attempt(&mut tx, attempt_id).await?;
    let event = if to == InvoiceStatus::Paid {
        outbox::INVOICE_PAID
    } else {
        outbox::INVOICE_PAYMENT_FAILED
    };
    outbox::enqueue(
        &mut tx,
        business_id,
        event,
        json!({ "invoice": invoice, "payment_attempt": attempt }),
    )
    .await?;

    let response = payment_response(&invoice, &attempt);
    complete_key(&mut tx, attempt_id, &response).await?;
    tx.commit().await?;

    tracing::info!(%invoice_id, %attempt_id, from = "processing", to = %to, attempt_status = status, "payment resolved");
    Ok(Some(response))
}

/// The PSP answer was ambiguous: mark the attempt `unknown` (invoice stays
/// `processing`) and store the 202 on the key in the same transaction.
/// Returns `None` if the attempt was already resolved by the reconciler.
async fn mark_unknown(
    db: &PgPool,
    attempt_id: Uuid,
    reason: &str,
) -> anyhow::Result<Option<StoredResponse>> {
    let mut tx = db.begin().await?;
    let row: Option<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE payment_attempts SET status = 'unknown'
         WHERE id = $1 AND status = 'pending'
         RETURNING invoice_id, business_id",
    )
    .bind(attempt_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((invoice_id, business_id)) = row else {
        return Ok(None);
    };

    let invoice = invoices::load(&mut tx, business_id, invoice_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("invoice {invoice_id} vanished"))?;
    let attempt = load_attempt(&mut tx, attempt_id).await?;
    let response = payment_response(&invoice, &attempt);
    complete_key(&mut tx, attempt_id, &response).await?;
    tx.commit().await?;

    tracing::warn!(%invoice_id, %attempt_id, reason, "psp outcome unknown; invoice stays processing until reconciled");
    Ok(Some(response))
}

async fn complete_key(
    conn: &mut PgConnection,
    attempt_id: Uuid,
    response: &StoredResponse,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE idempotency_keys SET status = 'completed', response_code = $2, response_body = $3
         WHERE payment_attempt_id = $1 AND status = 'in_progress'",
    )
    .bind(attempt_id)
    .bind(response.status.as_u16() as i16)
    .bind(&response.body)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

enum KeyState {
    Absent,
    Completed(StoredResponse),
    InProgress,
}

#[derive(FromRow)]
struct KeyRow {
    request_hash: Vec<u8>,
    status: String,
    response_code: Option<i16>,
    response_body: Option<Vec<u8>>,
}

async fn lookup_key(
    db: &PgPool,
    business_id: Uuid,
    key: &str,
    request_hash: &[u8],
) -> ApiResult<KeyState> {
    let row: Option<KeyRow> = sqlx::query_as(
        "SELECT request_hash, status, response_code, response_body FROM idempotency_keys
         WHERE business_id = $1 AND key = $2 AND expires_at > now()",
    )
    .bind(business_id)
    .bind(key)
    .fetch_optional(db)
    .await?;
    let Some(row) = row else {
        return Ok(KeyState::Absent);
    };
    // Hash check first: a different request with a busy key is still 422, not 409.
    if row.request_hash != request_hash {
        return Err(ApiError::IdempotencyKeyReused);
    }
    match (row.status.as_str(), row.response_code, row.response_body) {
        ("completed", Some(code), Some(body)) => Ok(KeyState::Completed(StoredResponse {
            status: StatusCode::from_u16(code as u16).map_err(anyhow::Error::from)?,
            body,
        })),
        _ => Ok(KeyState::InProgress),
    }
}

fn request_in_progress() -> ApiError {
    ApiError::Conflict {
        kind: "request_in_progress",
        message: "A request with this Idempotency-Key is still being processed; retry later".into(),
    }
}

fn idempotency_key(headers: &HeaderMap) -> ApiResult<String> {
    let key = headers
        .get("idempotency-key")
        .ok_or_else(|| ApiError::validation("Idempotency-Key header is required"))?
        .to_str()
        .map_err(|_| ApiError::validation("Idempotency-Key must be visible ASCII"))?;
    if key.is_empty() || key.len() > 255 || !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(ApiError::validation(
            "Idempotency-Key must be 1-255 visible ASCII characters",
        ));
    }
    Ok(key.to_owned())
}

/// `POST /v1/invoices/{id}/pay`
pub async fn pay(
    auth: Auth,
    State(state): State<AppState>,
    ApiPath(invoice_id): ApiPath<Uuid>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<PayRequest>,
) -> ApiResult<Response> {
    let key = idempotency_key(&headers)?;
    if req.card_token.trim().is_empty() || req.card_token.len() > 255 {
        return Err(ApiError::validation("card_token must be 1-255 characters"));
    }
    // The path (and so the invoice id) is part of the hash: reusing a key for a
    // different invoice is a different request.
    let mut hasher = Sha256::new();
    hasher.update(format!("POST\n/v1/invoices/{invoice_id}/pay\n").as_bytes());
    hasher.update(serde_json::to_vec(&req).expect("json serializes"));
    let request_hash = hasher.finalize().to_vec();

    let business_id = auth.business_id;
    let mut claimed = None;
    for _ in 0..MAX_CLAIM_ATTEMPTS {
        // Step 1: idempotency lookup.
        match lookup_key(&state.db, business_id, &key, &request_hash).await? {
            KeyState::Completed(stored) => {
                tracing::info!(%invoice_id, idempotency_key = %key, "idempotent replay");
                return Ok(stored.into_response(true));
            }
            KeyState::InProgress => return Err(request_in_progress()),
            KeyState::Absent => {}
        }
        // Step 2: Tx1.
        if let Some(c) = start_attempt(
            &state.db,
            business_id,
            invoice_id,
            &key,
            &request_hash,
            &req.card_token,
        )
        .await?
        {
            claimed = Some(c);
            break;
        }
        // Lost the claim race (a concurrent same-key request committed first): look again.
    }
    let Some((attempt_id, amount_cents)) = claimed else {
        return Err(request_in_progress());
    };

    // Step 3: PSP call, no transaction or lock held.
    let outcome = state
        .psp
        .charge(attempt_id, amount_cents, &req.card_token, invoice_id)
        .await;

    // Step 4: settle (or park as unknown).
    let settled = match &outcome {
        ChargeOutcome::Ambiguous { reason } => mark_unknown(&state.db, attempt_id, reason).await?,
        definitive => resolve(&state.db, attempt_id, definitive).await?,
    };
    match settled {
        Some(response) => Ok(response.into_response(false)),
        // The reconciler won the race; it stored the final response on our key.
        None => match lookup_key(&state.db, business_id, &key, &request_hash).await? {
            KeyState::Completed(stored) => Ok(stored.into_response(false)),
            _ => Err(request_in_progress()),
        },
    }
}

/// Tx1: claim the idempotency key, move the invoice to `processing`, and insert
/// a `pending` attempt, atomically. Returns `None` if the key is held by a live
/// row (caller re-checks it). Any rejection rolls everything back, including the
/// key claim, so rejections are never cached.
async fn start_attempt(
    db: &PgPool,
    business_id: Uuid,
    invoice_id: Uuid,
    key: &str,
    request_hash: &[u8],
    card_token: &str,
) -> ApiResult<Option<(Uuid, i64)>> {
    let mut tx = db.begin().await?;

    // Claim the key; takes over an expired row, never a live one. A concurrent
    // same-key INSERT blocks here until the first transaction finishes.
    let claimed = sqlx::query(
        "INSERT INTO idempotency_keys (business_id, key, request_hash, status, expires_at)
         VALUES ($1, $2, $3, 'in_progress', now() + interval '24 hours')
         ON CONFLICT (business_id, key) DO UPDATE
             SET request_hash = EXCLUDED.request_hash, status = 'in_progress',
                 response_code = NULL, response_body = NULL, payment_attempt_id = NULL,
                 created_at = now(), expires_at = EXCLUDED.expires_at
             WHERE idempotency_keys.expires_at < now()",
    )
    .bind(business_id)
    .bind(key)
    .bind(request_hash)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if claimed == 0 {
        return Ok(None);
    }

    // The concurrency guard: in Postgres a conditional UPDATE re-checks its WHERE
    // after waiting for the row lock, so exactly one concurrent payer wins.
    let row: Option<(i64,)> = sqlx::query_as(
        "UPDATE invoices
         SET status = 'processing', pre_payment_status = status, updated_at = now()
         WHERE id = $1 AND business_id = $2 AND status = ANY($3)
         RETURNING total_cents",
    )
    .bind(invoice_id)
    .bind(business_id)
    .bind(state_machine::allowed_from(Action::Pay))
    .fetch_optional(&mut *tx)
    .await?;
    let Some((amount_cents,)) = row else {
        let err = invoices::rejection(&mut tx, business_id, invoice_id, Action::Pay.name()).await;
        tx.rollback().await?;
        return Err(err);
    };

    let attempt_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO payment_attempts (id, invoice_id, business_id, status, amount_cents, card_token)
         VALUES ($1, $2, $3, 'pending', $4, $5)",
    )
    .bind(attempt_id)
    .bind(invoice_id)
    .bind(business_id)
    .bind(amount_cents)
    .bind(card_token)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE idempotency_keys SET payment_attempt_id = $3 WHERE business_id = $1 AND key = $2",
    )
    .bind(business_id)
    .bind(key)
    .bind(attempt_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    tracing::info!(%invoice_id, %attempt_id, amount_cents, to = "processing", "payment attempt started");
    Ok(Some((attempt_id, amount_cents)))
}
