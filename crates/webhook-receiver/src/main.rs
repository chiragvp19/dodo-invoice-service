//! Demo webhook receiver: shows what a merchant's endpoint should do.
//! - verifies `Dodo-Signature` (HMAC-SHA256 over "{t}.{raw_body}") in constant time
//! - rejects timestamps outside a 300s window (replay protection)
//! - dedupes on event id (delivery is at-least-once)
//! - FAIL_FIRST_N=n answers the first n requests with 500, to demo retries

use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use subtle::ConstantTimeEq;

const TOLERANCE_SECS: i64 = 300;

struct Receiver {
    secret: String,
    fail_first_n: u64,
    requests: AtomicU64,
    seen: Mutex<HashSet<String>>,
    received: Mutex<Vec<Value>>,
}

type AppState = Arc<Receiver>;

fn verify(secret: &str, header: &str, body: &[u8], now: i64) -> Result<(), &'static str> {
    let mut t = None;
    let mut v1 = None;
    for part in header.split(',') {
        match part.split_once('=') {
            Some(("t", value)) => t = value.parse::<i64>().ok(),
            Some(("v1", value)) => v1 = hex::decode(value).ok(),
            _ => {}
        }
    }
    let (t, v1) = (t.ok_or("missing t")?, v1.ok_or("missing v1")?);
    if (now - t).abs() > TOLERANCE_SECS {
        return Err("timestamp outside tolerance");
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(format!("{t}.").as_bytes());
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    if bool::from(expected.as_slice().ct_eq(&v1)) {
        Ok(())
    } else {
        Err("signature mismatch")
    }
}

async fn webhook(State(rx): State<AppState>, headers: HeaderMap, body: Bytes) -> (StatusCode, Json<Value>) {
    let n = rx.requests.fetch_add(1, Ordering::SeqCst) + 1;
    if n <= rx.fail_first_n {
        tracing::warn!(request = n, "✘ simulating failure (FAIL_FIRST_N={})", rx.fail_first_n);
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "simulated failure" })));
    }

    let Some(sig) = headers.get("dodo-signature").and_then(|v| v.to_str().ok()) else {
        tracing::warn!("✘ rejected: missing Dodo-Signature");
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "missing signature" })));
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    if let Err(reason) = verify(&rx.secret, sig, &body, now) {
        tracing::warn!(reason, "✘ rejected: bad signature");
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": reason })));
    }

    let Ok(event) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid json" })));
    };
    let id = event["id"].as_str().unwrap_or_default().to_owned();
    let kind = event["type"].as_str().unwrap_or_default().to_owned();
    let invoice = &event["data"]["invoice"];
    if !rx.seen.lock().unwrap().insert(id.clone()) {
        tracing::info!(event_id = %id, event_type = %kind, "↺ duplicate, ignored");
        return (StatusCode::OK, Json(json!({ "duplicate": true })));
    }
    tracing::info!(
        event_id = %id,
        event_type = %kind,
        invoice_id = %invoice["id"].as_str().unwrap_or("-"),
        invoice_status = %invoice["status"].as_str().unwrap_or("-"),
        "✔ verified"
    );
    rx.received.lock().unwrap().push(event);
    (StatusCode::OK, Json(json!({ "received": true })))
}

/// Everything received so far (handy in the demo and in tests).
async fn received(State(rx): State<AppState>) -> Json<Value> {
    Json(json!({ "data": *rx.received.lock().unwrap() }))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(false).init();
    let port = std::env::var("PORT").unwrap_or_else(|_| "9000".into());
    let state = Arc::new(Receiver {
        secret: std::env::var("WEBHOOK_SECRET").expect("WEBHOOK_SECRET must be set"),
        fail_first_n: std::env::var("FAIL_FIRST_N").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
        requests: AtomicU64::new(0),
        seen: Mutex::new(HashSet::new()),
        received: Mutex::new(Vec::new()),
    });
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/webhooks", post(webhook))
        .route("/received", get(received))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await.unwrap();
    tracing::info!(%port, "webhook-receiver listening");
    axum::serve(listener, app).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &str, t: i64, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("{t}.").as_bytes());
        mac.update(body);
        format!("t={t},v1={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn verifies_and_rejects() {
        let body = br#"{"id":"evt"}"#;
        assert!(verify("s", &sign("s", 1000, body), body, 1000).is_ok());
        assert_eq!(verify("s", &sign("other", 1000, body), body, 1000), Err("signature mismatch"));
        assert_eq!(verify("s", &sign("s", 1000, body), b"{}", 1000), Err("signature mismatch"));
        assert_eq!(verify("s", &sign("s", 1000, body), body, 1000 + 301), Err("timestamp outside tolerance"));
    }
}
