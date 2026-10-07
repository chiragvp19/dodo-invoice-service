//! HTTP client for the PSP, treated as an unreliable external dependency.
//!
//! Rule: only a parseable, terminal `succeeded` or `failed` is definitive.
//! Everything else (timeout, connection refused/reset, 5xx, 4xx, an unparseable
//! body, or a 2xx that still says `processing`) is *ambiguous*: we do not know
//! whether money moved, so we never guess. "Connection refused" provably never
//! reached the PSP and could fail fast, but one code path for every transport
//! error is simpler and still safe, because the reconciler settles it.

use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

#[derive(Clone)]
pub struct PspClient {
    http: reqwest::Client,
    base_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChargeOutcome {
    Succeeded { psp_ref: String },
    Failed { code: String },
    Ambiguous { reason: String },
}

#[derive(Debug)]
pub enum Lookup {
    /// A definitive `Succeeded` or `Failed`.
    Settled(ChargeOutcome),
    /// The PSP knows the charge but has not settled it yet.
    Processing,
    /// The PSP has no charge for this key.
    NotFound,
    /// We could not get an answer; try again later.
    Unavailable(String),
}

#[derive(Deserialize)]
struct PspCharge {
    status: String,
    psp_ref: Option<String>,
    code: Option<String>,
}

impl PspClient {
    pub fn new(base_url: &str, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(2)))
            .build()
            .expect("reqwest client builds");
        PspClient { http, base_url: base_url.trim_end_matches('/').to_owned() }
    }

    /// Creates a charge. `attempt_id` is the PSP idempotency key, so retrying
    /// the same attempt can never charge twice.
    pub async fn charge(&self, attempt_id: Uuid, amount_cents: i64, card_token: &str, reference: Uuid) -> ChargeOutcome {
        let started = std::time::Instant::now();
        let result = self
            .http
            .post(format!("{}/charges", self.base_url))
            .header("Idempotency-Key", attempt_id.to_string())
            .json(&json!({ "amount_cents": amount_cents, "card_token": card_token, "reference": reference }))
            .send()
            .await;

        let outcome = match result {
            Err(e) if e.is_timeout() => ChargeOutcome::Ambiguous { reason: "timeout".into() },
            Err(e) => ChargeOutcome::Ambiguous { reason: format!("transport error: {e}") },
            Ok(resp) if !resp.status().is_success() => {
                ChargeOutcome::Ambiguous { reason: format!("http {}", resp.status().as_u16()) }
            }
            Ok(resp) => match resp.json::<PspCharge>().await {
                Ok(charge) => definitive(charge)
                    .unwrap_or_else(|| ChargeOutcome::Ambiguous { reason: "charge not settled".into() }),
                Err(e) => ChargeOutcome::Ambiguous { reason: format!("unreadable response: {e}") },
            },
        };
        tracing::info!(
            %attempt_id,
            outcome = ?outcome,
            latency_ms = started.elapsed().as_millis() as u64,
            "psp charge call"
        );
        outcome
    }

    /// Looks up a charge by the idempotency key (our attempt id).
    pub async fn lookup(&self, attempt_id: Uuid) -> Lookup {
        let result = self.http.get(format!("{}/charges/{attempt_id}", self.base_url)).send().await;
        match result {
            Err(e) => Lookup::Unavailable(e.to_string()),
            Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => Lookup::NotFound,
            Ok(resp) if !resp.status().is_success() => Lookup::Unavailable(format!("http {}", resp.status())),
            Ok(resp) => match resp.json::<PspCharge>().await {
                Ok(charge) => definitive(charge).map(Lookup::Settled).unwrap_or(Lookup::Processing),
                Err(e) => Lookup::Unavailable(format!("unreadable response: {e}")),
            },
        }
    }
}

fn definitive(charge: PspCharge) -> Option<ChargeOutcome> {
    match (charge.status.as_str(), charge.psp_ref, charge.code) {
        ("succeeded", Some(psp_ref), _) => Some(ChargeOutcome::Succeeded { psp_ref }),
        ("failed", _, code) => Some(ChargeOutcome::Failed { code: code.unwrap_or_else(|| "declined".into()) }),
        _ => None,
    }
}
