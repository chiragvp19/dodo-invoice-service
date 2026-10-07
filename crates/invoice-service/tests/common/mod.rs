//! Test harness: runs the invoice service in-process (random port, fast timings)
//! against the Postgres and mock PSP from docker-compose:
//!
//!     docker compose up -d db mock-psp
//!     cargo test -p invoice-service
//!
//! Each test creates its own business, so tests run in parallel safely.

#![allow(dead_code)]

use std::time::{Duration, Instant};

use invoice_service::config::Config;
use serde_json::{json, Value};
use sqlx::PgPool;

pub struct TestApp {
    pub base: String,
    pub psp_url: String,
    pub http: reqwest::Client,
    pub api_key: String,
    pub db: PgPool,
}

const ADMIN_TOKEN: &str = "test-admin-token";

pub async fn spawn_app() -> TestApp {
    invoice_service::telemetry::init();
    let database_url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://dodo:dodo@localhost:55432/dodo".into());
    let psp_url = std::env::var("TEST_PSP_URL").unwrap_or_else(|_| "http://localhost:4000".into());
    let config = Config {
        database_url: database_url.clone(),
        bind_addr: "127.0.0.1:0".into(),
        psp_url: psp_url.clone(),
        // Test timings satisfy the startup invariants: grace > timeout, min_age >= timeout.
        psp_timeout: Duration::from_millis(500),
        reconciler_min_age: Duration::from_millis(600),
        psp_not_found_grace: Duration::from_millis(1_500),
        reconciler_interval: Duration::from_millis(200),
        admin_token: Some(ADMIN_TOKEN.into()),
        seed_demo: false,
        demo_api_key: None,
        demo_webhook_url: None,
        demo_webhook_secret: None,
        webhook_poll_interval: Duration::from_millis(200),
        webhook_retry_schedule: vec![Duration::from_secs(1)],
        webhook_timeout: Duration::from_secs(2),
    };
    config.validate().expect("test config satisfies invariants");
    let (addr, _server) = invoice_service::start(config)
        .await
        .expect("app starts (is `docker compose up -d db mock-psp` running?)");

    let http = reqwest::Client::new();
    let base = format!("http://{addr}/v1");
    let resp: Value = http
        .post(format!("{base}/businesses"))
        .header("x-admin-token", ADMIN_TOKEN)
        .json(&json!({ "name": "Test Business" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let api_key = resp["api_key"]["key"].as_str().expect("api key in bootstrap response").to_owned();
    let db = PgPool::connect(&database_url).await.unwrap();
    TestApp { base, psp_url, http, api_key, db }
}

impl TestApp {
    pub fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.post(format!("{}{path}", self.base)).bearer_auth(&self.api_key)
    }

    pub fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.get(format!("{}{path}", self.base)).bearer_auth(&self.api_key)
    }

    /// Creates a customer and a finalized (open) invoice; returns the invoice id.
    pub async fn open_invoice(&self, cents: i64) -> String {
        let customer: Value = self
            .post("/customers")
            .json(&json!({ "name": "Ada", "email": "ada@example.com" }))
            .send().await.unwrap().json().await.unwrap();
        let invoice: Value = self
            .post("/invoices")
            .json(&json!({
                "customer_id": customer["id"],
                "due_date": "2099-01-01",
                "line_items": [{ "description": "Plan", "quantity": 1, "unit_amount_cents": cents }],
            }))
            .send().await.unwrap().json().await.unwrap();
        let id = invoice["id"].as_str().expect("invoice id").to_owned();
        let finalized = self.post(&format!("/invoices/{id}/finalize")).send().await.unwrap();
        assert_eq!(finalized.status(), 200);
        id
    }

    pub async fn pay(&self, invoice_id: &str, key: &str, token: &str) -> reqwest::Response {
        self.post(&format!("/invoices/{invoice_id}/pay"))
            .header("Idempotency-Key", key)
            .json(&json!({ "card_token": token }))
            .send()
            .await
            .unwrap()
    }

    pub async fn invoice(&self, id: &str) -> Value {
        self.get(&format!("/invoices/{id}")).send().await.unwrap().json().await.unwrap()
    }

    pub async fn attempts(&self, invoice_id: &str) -> Vec<Value> {
        let v: Value = self.get(&format!("/invoices/{invoice_id}/payment_attempts"))
            .send().await.unwrap().json().await.unwrap();
        v["data"].as_array().unwrap().clone()
    }

    /// What the mock PSP saw for this invoice (we send invoice id as `reference`).
    pub async fn psp_stats(&self, invoice_id: &str) -> Value {
        self.http.get(format!("{}/_stats?reference={invoice_id}", self.psp_url))
            .send().await.unwrap().json().await.unwrap()
    }

    pub async fn event_count(&self, event_type: &str, invoice_id: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM webhook_events WHERE type = $1 AND payload->'invoice'->>'id' = $2",
        )
        .bind(event_type)
        .bind(invoice_id)
        .fetch_one(&self.db)
        .await
        .unwrap()
    }

    /// Polls until the invoice reaches `status` or the deadline passes.
    pub async fn wait_for_status(&self, id: &str, status: &str, deadline: Duration) -> Value {
        let start = Instant::now();
        loop {
            let inv = self.invoice(id).await;
            if inv["status"] == status {
                return inv;
            }
            assert!(start.elapsed() < deadline, "invoice {id} still '{}' after {deadline:?}, wanted '{status}'", inv["status"]);
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}
