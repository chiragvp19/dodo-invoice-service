//! Invoice representation, money math, and loading.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection};
use uuid::Uuid;

use crate::error::ApiError;

pub const MAX_LINE_ITEMS: usize = 100;
pub const MAX_QUANTITY: i64 = 1_000_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineItemInput {
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
}

#[derive(Clone, Debug, Serialize, FromRow)]
pub struct LineItem {
    pub description: String,
    pub quantity: i32,
    pub unit_amount_cents: i64,
    pub amount_cents: i64,
}

#[derive(Clone, Debug, Serialize, FromRow)]
pub struct Invoice {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub status: String,
    pub currency: String,
    pub total_cents: i64,
    pub due_date: NaiveDate,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub paid_at: Option<DateTime<Utc>>,
    #[sqlx(skip)]
    pub line_items: Vec<LineItem>,
}

/// Validates line items and computes amounts and the total with checked integer
/// arithmetic. The client never supplies a total.
pub fn price_line_items(items: &[LineItemInput]) -> Result<(Vec<LineItem>, i64), ApiError> {
    if items.is_empty() || items.len() > MAX_LINE_ITEMS {
        return Err(ApiError::validation(format!(
            "line_items must contain 1-{MAX_LINE_ITEMS} items"
        )));
    }
    let overflow = || ApiError::validation("invoice total is too large");
    let mut total: i64 = 0;
    let mut priced = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let description = item.description.trim();
        if description.is_empty() || description.len() > 500 {
            return Err(ApiError::validation(format!(
                "line_items[{i}].description must be 1-500 characters"
            )));
        }
        if !(1..=MAX_QUANTITY).contains(&item.quantity) {
            return Err(ApiError::validation(format!(
                "line_items[{i}].quantity must be between 1 and {MAX_QUANTITY}"
            )));
        }
        if item.unit_amount_cents < 0 {
            return Err(ApiError::validation(format!(
                "line_items[{i}].unit_amount_cents must be >= 0"
            )));
        }
        let amount = item.quantity.checked_mul(item.unit_amount_cents).ok_or_else(overflow)?;
        total = total.checked_add(amount).ok_or_else(overflow)?;
        priced.push(LineItem {
            description: description.to_owned(),
            quantity: item.quantity as i32, // bounded by MAX_QUANTITY above
            unit_amount_cents: item.unit_amount_cents,
            amount_cents: amount,
        });
    }
    if total <= 0 {
        return Err(ApiError::validation("invoice total must be greater than zero"));
    }
    Ok((priced, total))
}

const INVOICE_COLUMNS: &str = "id, customer_id, status, 'usd' AS currency, total_cents, due_date, \
                               created_at, updated_at, paid_at";

/// Loads one invoice (with line items) scoped to the business.
pub async fn load(conn: &mut PgConnection, business_id: Uuid, id: Uuid) -> sqlx::Result<Option<Invoice>> {
    let invoice: Option<Invoice> = sqlx::query_as(&format!(
        "SELECT {INVOICE_COLUMNS} FROM invoices WHERE id = $1 AND business_id = $2"
    ))
    .bind(id)
    .bind(business_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(mut invoice) = invoice else { return Ok(None) };
    invoice.line_items = sqlx::query_as(
        "SELECT description, quantity, unit_amount_cents, amount_cents
         FROM invoice_line_items WHERE invoice_id = $1 ORDER BY position",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(Some(invoice))
}

/// Lists invoices newest first, optionally filtered by status. Returns up to
/// `limit + 1` rows so the caller can compute `has_more`.
pub async fn list(
    conn: &mut PgConnection,
    business_id: Uuid,
    status: Option<&str>,
    starting_after: Option<Uuid>,
    limit: i64,
) -> sqlx::Result<Vec<Invoice>> {
    let mut invoices: Vec<Invoice> = sqlx::query_as(&format!(
        "SELECT {INVOICE_COLUMNS} FROM invoices
         WHERE business_id = $1
           AND ($2::text IS NULL OR status = $2)
           AND ($3::uuid IS NULL OR id < $3)
         ORDER BY id DESC LIMIT $4"
    ))
    .bind(business_id)
    .bind(status)
    .bind(starting_after)
    .bind(limit + 1)
    .fetch_all(&mut *conn)
    .await?;

    // One query for all line items (no N+1).
    let ids: Vec<Uuid> = invoices.iter().map(|i| i.id).collect();
    let rows: Vec<(Uuid, String, i32, i64, i64)> = sqlx::query_as(
        "SELECT invoice_id, description, quantity, unit_amount_cents, amount_cents
         FROM invoice_line_items WHERE invoice_id = ANY($1) ORDER BY invoice_id, position",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    for (invoice_id, description, quantity, unit_amount_cents, amount_cents) in rows {
        if let Some(inv) = invoices.iter_mut().find(|i| i.id == invoice_id) {
            inv.line_items.push(LineItem { description, quantity, unit_amount_cents, amount_cents });
        }
    }
    Ok(invoices)
}

/// After a guarded `UPDATE ... WHERE status = ANY(..)` matched no row, works
/// out whether the invoice is missing (404) or in the wrong state (409).
pub async fn rejection(conn: &mut PgConnection, business_id: Uuid, id: Uuid, action: &str) -> ApiError {
    let current: sqlx::Result<Option<(String,)>> =
        sqlx::query_as("SELECT status FROM invoices WHERE id = $1 AND business_id = $2")
            .bind(id)
            .bind(business_id)
            .fetch_optional(&mut *conn)
            .await;
    match current {
        Ok(Some((current_status,))) => {
            ApiError::InvalidTransition { current_status, action: action.to_owned() }
        }
        Ok(None) => ApiError::NotFound("Invoice"),
        Err(e) => e.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(quantity: i64, unit: i64) -> LineItemInput {
        LineItemInput { description: "x".into(), quantity, unit_amount_cents: unit }
    }

    #[test]
    fn computes_total_in_integer_cents() {
        let (items, total) = price_line_items(&[item(3, 1999), item(1, 1)]).unwrap();
        assert_eq!(items[0].amount_cents, 5997);
        assert_eq!(total, 5998);
    }

    #[test]
    fn rejects_overflow_instead_of_wrapping() {
        assert!(price_line_items(&[item(MAX_QUANTITY, i64::MAX / 2)]).is_err());
        assert!(price_line_items(&[item(1, i64::MAX), item(1, 1)]).is_err());
    }

    #[test]
    fn rejects_invalid_items() {
        assert!(price_line_items(&[]).is_err());
        assert!(price_line_items(&[item(0, 100)]).is_err());
        assert!(price_line_items(&[item(1, -1)]).is_err());
        assert!(price_line_items(&[item(1, 0)]).is_err(), "zero total");
    }
}
