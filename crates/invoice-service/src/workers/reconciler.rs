//! Settles payment attempts whose outcome we don't know: `unknown` (PSP timed
//! out / errored) and `pending` (the handler crashed mid-flight). It asks the PSP
//! by idempotency key (= attempt id) and resolves through the same guarded
//! `payments::resolve` the request handler uses, so running several replicas, or
//! racing a live handler, can at worst waste a lookup.

use std::time::Duration;

use uuid::Uuid;

use crate::{
    payments,
    psp::{ChargeOutcome, Lookup},
    AppState,
};

const BATCH: i64 = 50;

pub async fn run(state: AppState) {
    let mut tick = tokio::time::interval(state.config.reconciler_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(e) = reconcile_once(&state).await {
            tracing::error!(error = ?e, "reconciler pass failed");
        }
    }
}

pub async fn reconcile_once(state: &AppState) -> anyhow::Result<()> {
    let min_age = state.config.reconciler_min_age.as_secs_f64();
    // Ages are computed by the database clock, the same clock that wrote created_at.
    let due: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT id, (EXTRACT(EPOCH FROM now() - created_at) * 1000)::bigint AS age_ms
         FROM payment_attempts
         WHERE status IN ('pending', 'unknown') AND created_at < now() - $1 * interval '1 second'
         ORDER BY created_at
         LIMIT $2",
    )
    .bind(min_age)
    .bind(BATCH)
    .fetch_all(&state.db)
    .await?;

    for (attempt_id, age_ms) in due {
        let age = Duration::from_millis(age_ms.max(0) as u64);
        let outcome = match state.psp.lookup(attempt_id).await {
            Lookup::Settled(outcome) => outcome,
            // Only safe because our POST can no longer be in flight (grace > PSP
            // timeout, enforced at startup) and the PSP records before acting.
            Lookup::NotFound if age > state.config.psp_not_found_grace => {
                ChargeOutcome::Failed { code: "psp_not_charged".into() }
            }
            other => {
                tracing::debug!(%attempt_id, ?other, age_ms, "attempt not settleable yet");
                continue;
            }
        };
        match payments::resolve(&state.db, attempt_id, &outcome).await? {
            Some(_) => tracing::info!(%attempt_id, ?outcome, age_ms, "reconciler settled attempt"),
            None => tracing::debug!(%attempt_id, "attempt already settled elsewhere"),
        }
    }
    Ok(())
}
