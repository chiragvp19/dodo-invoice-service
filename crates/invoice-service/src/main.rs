use invoice_service::{config::Config, telemetry};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    telemetry::init();
    let config = Config::from_env()?;
    invoice_service::run(config).await
}
