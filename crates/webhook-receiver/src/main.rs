use axum::{routing::get, Router};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(false).init();
    let port = std::env::var("PORT").unwrap_or_else(|_| "9000".into());
    let app = Router::new().route("/healthz", get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await.unwrap();
    tracing::info!(%port, "webhook-receiver listening");
    axum::serve(listener, app).await.unwrap();
}
