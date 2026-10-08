pub mod auth;
pub mod config;
pub mod error;
pub mod invoices;
pub mod outbox;
pub mod pagination;
pub mod payments;
pub mod psp;
mod routes;
pub mod seed;
pub mod state_machine;
pub mod telemetry;
pub mod webhooks;
pub mod workers;

use std::{net::SocketAddr, sync::Arc};

use anyhow::Context;
use axum::{http::Request, routing::get, Json, Router};
use serde_json::json;
use sqlx::{postgres::PgPoolOptions, PgPool};
use tokio::{net::TcpListener, task::JoinHandle};
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, RequestId, SetRequestIdLayer},
    trace::TraceLayer,
};

use crate::{config::Config, error::ApiError, psp::PspClient};

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub config: Arc<Config>,
    pub psp: PspClient,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .nest("/v1", routes::v1())
        .fallback(|| async { ApiError::NotFound("route") })
        .method_not_allowed_fallback(|| async { ApiError::MethodNotAllowed })
        .with_state(state)
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(TraceLayer::new_for_http().make_span_with(|req: &Request<_>| {
            let request_id = req
                .extensions()
                .get::<RequestId>()
                .and_then(|id| id.header_value().to_str().ok())
                .unwrap_or("-")
                .to_owned();
            tracing::info_span!("request", %request_id, method = %req.method(), path = %req.uri().path())
        }))
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

/// Connects, migrates, and serves until the process is stopped.
pub async fn run(config: Config) -> anyhow::Result<()> {
    let (addr, server) = start(config).await?;
    tracing::info!(%addr, "invoice-service listening");
    server.await?
}

/// Starts the server and background workers and returns the bound address.
/// Used by `run` and by the integration tests (bind to port 0).
pub async fn start(config: Config) -> anyhow::Result<(SocketAddr, JoinHandle<anyhow::Result<()>>)> {
    let db = PgPoolOptions::new()
        .max_connections(20)
        .connect(&config.database_url)
        .await
        .context("connecting to database")?;
    sqlx::migrate!("../../migrations")
        .run(&db)
        .await
        .context("running migrations")?;

    if config.seed_demo {
        seed::run(&db, &config).await.context("seeding demo data")?;
    }

    let psp = PspClient::new(&config.psp_url, config.psp_timeout);
    let state = AppState {
        db,
        config: Arc::new(config),
        psp,
    };
    tokio::spawn(workers::reconciler::run(state.clone()));
    tokio::spawn(workers::webhooks::run(state.clone()));
    let listener = TcpListener::bind(&state.config.bind_addr)
        .await
        .with_context(|| format!("binding {}", state.config.bind_addr))?;
    let addr = listener.local_addr()?;
    let app = router(state);
    let server =
        tokio::spawn(async move { axum::serve(listener, app).await.context("server error") });
    Ok((addr, server))
}
