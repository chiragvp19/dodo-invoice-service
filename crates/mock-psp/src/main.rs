//! Mock payment processor. Outcomes are driven by the card token:
//!
//! | token                    | behaviour                                              |
//! |--------------------------|--------------------------------------------------------|
//! | tok_success              | ~100 ms, then `succeeded` with a psp_ref               |
//! | tok_insufficient_funds   | ~100 ms, then `failed` / insufficient_funds            |
//! | tok_card_declined        | ~100 ms, then `failed` / card_declined                 |
//! | tok_timeout              | 30 s, then `succeeded`                                 |
//! | tok_network_error        | HTTP 500, nothing recorded (the charge never happened) |
//! | anything else            | ~100 ms, then `failed` / invalid_token                 |
//!
//! Like real PSPs it is idempotent on the `Idempotency-Key` header and exposes a
//! status lookup (`GET /charges/{idempotency_key}`). Both are small extensions to
//! the brief; without them "did the PSP charge?" cannot be answered after a
//! timeout or crash.
//!
//! A charge is recorded *before* the simulated delay, and the delay runs in a
//! spawned task, so a charge completes even if the caller disconnects (hyper drops
//! the handler future on disconnect). State is in memory and lost on restart.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::oneshot;
use uuid::Uuid;

#[derive(Clone, Serialize)]
struct Charge {
    id: String,
    status: &'static str, // processing | succeeded | failed
    #[serde(skip_serializing_if = "Option::is_none")]
    psp_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'static str>,
    amount_cents: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    reference: Option<String>,
}

#[derive(Default, Clone, Copy, Serialize)]
struct Stats {
    post_calls: u64,
    charges_created: u64,
}

#[derive(Default)]
struct Psp {
    /// Keyed by the caller's Idempotency-Key.
    charges: Mutex<HashMap<String, Charge>>,
    /// Keyed by the caller-supplied `reference` (the invoice id), for tests.
    stats: Mutex<HashMap<String, Stats>>,
}

type AppState = Arc<Psp>;

#[derive(Deserialize)]
struct ChargeRequest {
    amount_cents: i64,
    card_token: String,
    reference: Option<String>,
}

enum Outcome {
    Succeeded,
    Failed(&'static str),
}

fn behaviour(token: &str) -> Option<(Duration, Outcome)> {
    let fast = Duration::from_millis(100);
    Some(match token {
        "tok_success" => (fast, Outcome::Succeeded),
        "tok_insufficient_funds" => (fast, Outcome::Failed("insufficient_funds")),
        "tok_card_declined" => (fast, Outcome::Failed("card_declined")),
        "tok_timeout" => (Duration::from_secs(30), Outcome::Succeeded),
        "tok_network_error" => return None,
        _ => (fast, Outcome::Failed("invalid_token")),
    })
}

async fn create_charge(State(psp): State<AppState>, headers: HeaderMap, Json(req): Json<ChargeRequest>) -> Response {
    let Some(key) = headers.get("idempotency-key").and_then(|v| v.to_str().ok()).map(str::to_owned) else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "Idempotency-Key header required" }))).into_response();
    };
    let reference = req.reference.clone().unwrap_or_default();
    psp.stats.lock().unwrap().entry(reference.clone()).or_default().post_calls += 1;

    // Idempotent replay: return the existing charge as it is right now.
    if let Some(existing) = psp.charges.lock().unwrap().get(&key).cloned() {
        tracing::info!(%key, status = existing.status, "replayed existing charge");
        return Json(existing).into_response();
    }

    let Some((delay, outcome)) = behaviour(&req.card_token) else {
        tracing::warn!(%key, "tok_network_error: returning 500, no charge recorded");
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "simulated network error" }))).into_response();
    };

    // Record first, then act.
    {
        let mut charges = psp.charges.lock().unwrap();
        if let Some(existing) = charges.get(&key).cloned() {
            return Json(existing).into_response(); // lost a race with a concurrent same-key call
        }
        charges.insert(
            key.clone(),
            Charge {
                id: format!("ch_{}", Uuid::new_v4().simple()),
                status: "processing",
                psp_ref: None,
                code: None,
                amount_cents: req.amount_cents,
                reference: req.reference.clone(),
            },
        );
    }
    psp.stats.lock().unwrap().entry(reference).or_default().charges_created += 1;
    tracing::info!(%key, token = %req.card_token, amount_cents = req.amount_cents, delay_ms = delay.as_millis() as u64, "charge accepted");

    // Complete in a detached task so the charge settles even if the caller hangs up.
    let (tx, rx) = oneshot::channel();
    let psp_bg = psp.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let settled = {
            let mut charges = psp_bg.charges.lock().unwrap();
            let charge = charges.get_mut(&key).expect("charge recorded above");
            match outcome {
                Outcome::Succeeded => {
                    charge.status = "succeeded";
                    charge.psp_ref = Some(Uuid::new_v4().to_string());
                }
                Outcome::Failed(code) => {
                    charge.status = "failed";
                    charge.code = Some(code);
                }
            }
            charge.clone()
        };
        tracing::info!(%key, status = settled.status, "charge settled");
        let _ = tx.send(settled); // receiver is gone if the caller disconnected
    });

    match rx.await {
        Ok(charge) => Json(charge).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn get_charge(State(psp): State<AppState>, Path(key): Path<String>) -> Response {
    match psp.charges.lock().unwrap().get(&key).cloned() {
        Some(charge) => Json(charge).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "no such charge" }))).into_response(),
    }
}

#[derive(Deserialize)]
struct StatsQuery {
    reference: String,
}

/// Test helper: how many times the PSP was called / actually charged for a reference.
async fn stats(State(psp): State<AppState>, Query(q): Query<StatsQuery>) -> Json<serde_json::Value> {
    let s = psp.stats.lock().unwrap().get(&q.reference).copied().unwrap_or_default();
    let succeeded = psp
        .charges
        .lock()
        .unwrap()
        .values()
        .filter(|c| c.reference.as_deref() == Some(q.reference.as_str()) && c.status == "succeeded")
        .count();
    Json(json!({ "post_calls": s.post_calls, "charges_created": s.charges_created, "succeeded": succeeded }))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(false).init();
    let port = std::env::var("PORT").unwrap_or_else(|_| "4000".into());
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/charges", post(create_charge))
        .route("/charges/{key}", get(get_charge))
        .route("/_stats", get(stats))
        .with_state(AppState::default());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await.unwrap();
    tracing::info!(%port, "mock-psp listening");
    axum::serve(listener, app).await.unwrap();
}
