//! Webhook delivery worker, decoupled from the API through the outbox table.
//!
//! Claiming uses a lease instead of holding row locks during HTTP: a short
//! statement pushes `next_attempt_at` 60s into the future for up to 20 due
//! deliveries (FOR UPDATE SKIP LOCKED, so replicas don't collide) and commits.
//! We then POST concurrently and record each result. If the worker dies
//! mid-flight, the lease expires and the delivery is retried without counting an
//! attempt. Delivery is therefore at-least-once; receivers dedupe on event id.

use std::time::Duration;

use chrono::{DateTime, Utc};
use rand::Rng;
use serde_json::{json, Value};
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::{webhooks, AppState};

const BATCH: i64 = 20;
const LEASE_SECS: i64 = 60;

#[derive(sqlx::FromRow)]
struct Due {
    delivery_id: Uuid,
    attempts: i32,
    event_id: Uuid,
    event_type: String,
    payload: Value,
    created_at: DateTime<Utc>,
    url: String,
    secret: String,
}

pub async fn run(state: AppState) {
    let http = reqwest::Client::builder()
        .timeout(state.config.webhook_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client builds");
    let mut tick = tokio::time::interval(state.config.webhook_poll_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(e) = deliver_due(&state, &http).await {
            tracing::error!(error = ?e, "webhook pass failed");
        }
    }
}

async fn deliver_due(state: &AppState, http: &reqwest::Client) -> anyhow::Result<()> {
    let due: Vec<Due> = sqlx::query_as(
        "WITH due AS (
             SELECT id FROM webhook_deliveries
             WHERE status = 'pending' AND next_attempt_at <= now()
             ORDER BY next_attempt_at
             LIMIT $1
             FOR UPDATE SKIP LOCKED
         )
         UPDATE webhook_deliveries d
         SET next_attempt_at = now() + $2 * interval '1 second'
         FROM due, webhook_events e, webhook_endpoints ep
         WHERE d.id = due.id AND e.id = d.event_id AND ep.id = d.endpoint_id
         RETURNING d.id AS delivery_id, d.attempts, e.id AS event_id, e.type AS event_type,
                   e.payload, e.created_at, ep.url, ep.secret",
    )
    .bind(BATCH)
    .bind(LEASE_SECS as f64)
    .fetch_all(&state.db)
    .await?;

    let mut tasks = JoinSet::new();
    for d in due {
        let (state, http) = (state.clone(), http.clone());
        tasks.spawn(async move {
            if let Err(e) = deliver_one(&state, &http, d).await {
                tracing::error!(error = ?e, "recording webhook result failed");
            }
        });
    }
    while tasks.join_next().await.is_some() {}
    Ok(())
}

async fn deliver_one(state: &AppState, http: &reqwest::Client, d: Due) -> anyhow::Result<()> {
    let body = serde_json::to_vec(&json!({
        "id": d.event_id,
        "type": d.event_type,
        "created_at": d.created_at,
        "data": d.payload,
    }))?;
    let timestamp = Utc::now().timestamp();
    let result = http
        .post(&d.url)
        .header("content-type", "application/json")
        .header(
            "Dodo-Signature",
            webhooks::signature_header(&d.secret, timestamp, &body),
        )
        .header("Dodo-Event-Id", d.event_id.to_string())
        .header("Dodo-Event-Type", &d.event_type)
        .body(body)
        .send()
        .await;

    let attempt = d.attempts + 1;
    let (code, error) = match result {
        Ok(resp) if resp.status().is_success() => {
            sqlx::query(
                "UPDATE webhook_deliveries
                 SET status = 'delivered', attempts = $2, last_status_code = $3, last_error = NULL,
                     delivered_at = now()
                 WHERE id = $1",
            )
            .bind(d.delivery_id)
            .bind(attempt)
            .bind(resp.status().as_u16() as i16)
            .execute(&state.db)
            .await?;
            tracing::info!(delivery_id = %d.delivery_id, event_id = %d.event_id, event_type = %d.event_type,
                attempt, status_code = resp.status().as_u16(), "webhook delivered");
            return Ok(());
        }
        Ok(resp) => (
            Some(resp.status().as_u16() as i16),
            format!("http {}", resp.status().as_u16()),
        ),
        Err(e) => (None, e.to_string()),
    };

    // Failure: schedule retry N after `schedule[N-1]` (±20% jitter), or give up.
    let schedule = &state.config.webhook_retry_schedule;
    match schedule.get(d.attempts as usize) {
        Some(base) => {
            let delay = jittered(*base);
            sqlx::query(
                "UPDATE webhook_deliveries
                 SET attempts = $2, last_status_code = $3, last_error = $4,
                     next_attempt_at = now() + $5 * interval '1 second'
                 WHERE id = $1",
            )
            .bind(d.delivery_id)
            .bind(attempt)
            .bind(code)
            .bind(&error)
            .bind(delay.as_secs_f64())
            .execute(&state.db)
            .await?;
            tracing::warn!(delivery_id = %d.delivery_id, event_id = %d.event_id, attempt, %error,
                retry_in_secs = delay.as_secs_f64(), "webhook failed; will retry");
        }
        None => {
            sqlx::query(
                "UPDATE webhook_deliveries
                 SET status = 'dead', attempts = $2, last_status_code = $3, last_error = $4
                 WHERE id = $1",
            )
            .bind(d.delivery_id)
            .bind(attempt)
            .bind(code)
            .bind(&error)
            .execute(&state.db)
            .await?;
            tracing::error!(delivery_id = %d.delivery_id, event_id = %d.event_id, attempt, %error,
                "webhook retries exhausted; marked dead (recoverable via GET /v1/events)");
        }
    }
    Ok(())
}

fn jittered(base: Duration) -> Duration {
    base.mul_f64(rand::thread_rng().gen_range(0.8..=1.2))
}
