//! The three tests the brief requires: concurrency, idempotency, PSP failure.

mod common;

use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::task::JoinSet;

/// N concurrent POST /pay (distinct keys) for one invoice: exactly one wins,
/// the PSP charges once, and the final state is consistent.
#[tokio::test]
async fn concurrent_payments_charge_at_most_once() {
    let app = std::sync::Arc::new(common::spawn_app().await);
    let invoice_id = app.open_invoice(4_200).await;

    const N: usize = 20;
    let mut tasks = JoinSet::new();
    for i in 0..N {
        let (app, invoice_id) = (app.clone(), invoice_id.clone());
        tasks.spawn(async move { app.pay(&invoice_id, &format!("concurrent-{i}"), "tok_success").await.status().as_u16() });
    }
    let mut statuses = Vec::new();
    while let Some(status) = tasks.join_next().await {
        statuses.push(status.unwrap());
    }

    let ok = statuses.iter().filter(|s| **s == 200).count();
    let conflict = statuses.iter().filter(|s| **s == 409).count();
    assert_eq!(ok, 1, "exactly one payment succeeds: {statuses:?}");
    assert_eq!(conflict, N - 1, "everyone else gets 409: {statuses:?}");

    let stats = app.psp_stats(&invoice_id).await;
    assert_eq!(stats["charges_created"], 1, "PSP charged once: {stats}");
    assert_eq!(stats["succeeded"], 1, "{stats}");

    let invoice = app.invoice(&invoice_id).await;
    assert_eq!(invoice["status"], "paid");
    assert!(invoice["paid_at"].is_string());
    let attempts = app.attempts(&invoice_id).await;
    assert_eq!(attempts.len(), 1, "losers never created an attempt: {attempts:?}");
    assert_eq!(attempts[0]["status"], "succeeded");
    assert_eq!(app.event_count("invoice.paid", &invoice_id).await, 1);
}

/// Same Idempotency-Key retried: identical response, no second PSP call.
/// Same key with a different body or invoice: 422.
#[tokio::test]
async fn idempotent_retry_replays_without_second_psp_call() {
    let app = common::spawn_app().await;
    let invoice_id = app.open_invoice(1_000).await;

    let first = app.pay(&invoice_id, "idem-key-1", "tok_success").await;
    assert_eq!(first.status(), 200);
    let first_body = first.bytes().await.unwrap();

    let retry = app.pay(&invoice_id, "idem-key-1", "tok_success").await;
    assert_eq!(retry.status(), 200);
    assert_eq!(retry.headers()["idempotent-replayed"], "true");
    assert_eq!(retry.bytes().await.unwrap(), first_body, "byte-identical replay");

    assert_eq!(app.psp_stats(&invoice_id).await["post_calls"], 1, "PSP called exactly once");

    // Same key, different body.
    let reused = app.pay(&invoice_id, "idem-key-1", "tok_card_declined").await;
    assert_eq!(reused.status(), 422);
    let err: Value = reused.json().await.unwrap();
    assert_eq!(err["error"]["type"], "idempotency_key_reused");

    // Same key, different invoice (path is part of the hash).
    let other_invoice = app.open_invoice(1_000).await;
    assert_eq!(app.pay(&other_invoice, "idem-key-1", "tok_success").await.status(), 422);
    assert_eq!(app.invoice(&other_invoice).await["status"], "open", "other invoice untouched");

    // A new key on a paid invoice is a state conflict, and never reaches the PSP.
    let again = app.pay(&invoice_id, "idem-key-2", "tok_success").await;
    assert_eq!(again.status(), 409);
    let err: Value = again.json().await.unwrap();
    assert_eq!(err["error"]["type"], "invalid_state_transition");
    assert_eq!(err["error"]["details"]["current_status"], "paid");
    assert_eq!(app.psp_stats(&invoice_id).await["post_calls"], 1);
}

/// A rejected request is not cached: after the invoice becomes payable, the
/// same key works.
#[tokio::test]
async fn rejected_request_releases_the_key() {
    let app = common::spawn_app().await;
    let customer: Value = app.post("/customers")
        .json(&serde_json::json!({ "name": "Bo", "email": "bo@example.com" }))
        .send().await.unwrap().json().await.unwrap();
    let draft: Value = app.post("/invoices")
        .json(&serde_json::json!({
            "customer_id": customer["id"], "due_date": "2099-01-01",
            "line_items": [{ "description": "x", "quantity": 2, "unit_amount_cents": 250 }],
        }))
        .send().await.unwrap().json().await.unwrap();
    let id = draft["id"].as_str().unwrap();
    assert_eq!(draft["total_cents"], 500);

    assert_eq!(app.pay(id, "early-key", "tok_success").await.status(), 409, "draft is not payable");
    assert_eq!(app.post(&format!("/invoices/{id}/finalize")).send().await.unwrap().status(), 200);
    assert_eq!(app.pay(id, "early-key", "tok_success").await.status(), 200, "key was not burned by the rejection");
}

/// tok_network_error: the endpoint answers 202 (outcome unknown), and the
/// reconciler returns the invoice to `open` (PSP confirms no charge) instead of
/// leaving it stuck in `processing`. It can then be paid normally.
#[tokio::test]
async fn psp_network_error_does_not_leave_invoice_stuck() {
    let app = common::spawn_app().await;
    let invoice_id = app.open_invoice(2_500).await;

    let resp = app.pay(&invoice_id, "net-err-1", "tok_network_error").await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["invoice"]["status"], "processing");
    assert_eq!(body["payment_attempt"]["status"], "unknown");

    app.wait_for_status(&invoice_id, "open", Duration::from_secs(10)).await;
    let attempts = app.attempts(&invoice_id).await;
    assert_eq!(attempts[0]["status"], "failed");
    assert_eq!(attempts[0]["failure_code"], "psp_not_charged");
    assert_eq!(app.event_count("invoice.payment_failed", &invoice_id).await, 1);

    assert_eq!(app.pay(&invoice_id, "net-err-2", "tok_success").await.status(), 200);
    assert_eq!(app.invoice(&invoice_id).await["status"], "paid");
}

/// tok_timeout (PSP sleeps 30s): the endpoint does not hang (202 after our 500ms
/// timeout), and the reconciler later finds the charge succeeded and marks the
/// invoice paid. The PSP charged exactly once; a same-key retry still replays 202.
#[tokio::test]
async fn psp_timeout_returns_fast_and_reconciles_to_paid() {
    let app = common::spawn_app().await;
    let invoice_id = app.open_invoice(9_900).await;

    let started = Instant::now();
    let resp = app.pay(&invoice_id, "timeout-1", "tok_timeout").await;
    assert!(started.elapsed() < Duration::from_secs(2), "endpoint must not hang: {:?}", started.elapsed());
    assert_eq!(resp.status(), 202);
    let first_body = resp.bytes().await.unwrap();
    assert_eq!(app.invoice(&invoice_id).await["status"], "processing");

    // Another payer cannot start a second charge while the first is unresolved.
    assert_eq!(app.pay(&invoice_id, "timeout-2", "tok_success").await.status(), 409);

    let invoice = app.wait_for_status(&invoice_id, "paid", Duration::from_secs(40)).await;
    assert!(invoice["paid_at"].is_string());
    assert_eq!(app.attempts(&invoice_id).await[0]["status"], "succeeded");
    assert_eq!(app.psp_stats(&invoice_id).await["charges_created"], 1);
    assert_eq!(app.event_count("invoice.paid", &invoice_id).await, 1);

    // Stable replay by design: the outcome is on the invoice and the webhook.
    let replay = app.pay(&invoice_id, "timeout-1", "tok_timeout").await;
    assert_eq!(replay.status(), 202);
    assert_eq!(replay.bytes().await.unwrap(), first_body);
}
