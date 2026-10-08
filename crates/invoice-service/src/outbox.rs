//! Transactional outbox. Callers pass their open transaction, so an event exists
//! if and only if the state change that caused it committed.

use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

pub const INVOICE_CREATED: &str = "invoice.created";
pub const INVOICE_PAID: &str = "invoice.paid";
pub const INVOICE_PAYMENT_FAILED: &str = "invoice.payment_failed";
pub const EVENT_TYPES: [&str; 3] = [INVOICE_CREATED, INVOICE_PAID, INVOICE_PAYMENT_FAILED];

/// Records the event and one pending delivery per active endpoint of the business.
pub async fn enqueue(
    conn: &mut PgConnection,
    business_id: Uuid,
    event_type: &str,
    data: Value,
) -> sqlx::Result<Uuid> {
    let event_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO webhook_events (id, business_id, type, payload) VALUES ($1, $2, $3, $4)",
    )
    .bind(event_id)
    .bind(business_id)
    .bind(event_type)
    .bind(&data)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO webhook_deliveries (id, event_id, endpoint_id)
         SELECT gen_random_uuid(), $1, id FROM webhook_endpoints WHERE business_id = $2 AND active",
    )
    .bind(event_id)
    .bind(business_id)
    .execute(&mut *conn)
    .await?;
    tracing::info!(%event_id, %business_id, event_type, "event enqueued");
    Ok(event_id)
}
