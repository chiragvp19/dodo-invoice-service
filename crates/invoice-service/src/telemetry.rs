use tracing_subscriber::{fmt, EnvFilter};

/// Pretty logs by default, JSON when LOG_FORMAT=json. Level via RUST_LOG.
pub fn init() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info,sqlx=warn"));
    let builder = fmt().with_env_filter(filter).with_target(false);
    let result = if std::env::var("LOG_FORMAT").as_deref() == Ok("json") {
        builder.json().try_init()
    } else {
        builder.try_init()
    };
    // Tests may initialise more than once; that's fine.
    let _ = result;
}
