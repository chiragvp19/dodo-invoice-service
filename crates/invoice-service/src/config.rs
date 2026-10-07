use std::{env, time::Duration};

use anyhow::{bail, Context};

/// Runtime configuration, read from environment variables.
#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: String,
    pub psp_url: String,
    /// Hard timeout for a single PSP charge call. Anything slower is "ambiguous".
    pub psp_timeout: Duration,
    /// Enables `POST /v1/businesses` when set.
    pub admin_token: Option<String>,
    pub seed_demo: bool,
    pub demo_api_key: Option<String>,
    pub demo_webhook_url: Option<String>,
    pub demo_webhook_secret: Option<String>,
    pub reconciler_interval: Duration,
    /// Only attempts older than this are reconciled (handler may still be waiting).
    pub reconciler_min_age: Duration,
    /// A PSP 404 for an attempt older than this means "never charged".
    pub psp_not_found_grace: Duration,
    pub webhook_poll_interval: Duration,
    /// Delay before retry N (1-based). Total tries = 1 + len.
    pub webhook_retry_schedule: Vec<Duration>,
    pub webhook_timeout: Duration,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let config = Config {
            database_url: required("DATABASE_URL")?,
            bind_addr: env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()),
            psp_url: env::var("PSP_URL").unwrap_or_else(|_| "http://localhost:4000".into()),
            psp_timeout: millis("PSP_TIMEOUT_MS", 5_000)?,
            admin_token: optional("ADMIN_TOKEN"),
            seed_demo: env::var("SEED_DEMO").map(|v| v == "true").unwrap_or(false),
            demo_api_key: optional("DEMO_API_KEY"),
            demo_webhook_url: optional("DEMO_WEBHOOK_URL"),
            demo_webhook_secret: optional("DEMO_WEBHOOK_SECRET"),
            reconciler_interval: millis("RECONCILER_INTERVAL_MS", 5_000)?,
            reconciler_min_age: millis("RECONCILER_MIN_AGE_MS", 10_000)?,
            psp_not_found_grace: millis("PSP_NOT_FOUND_GRACE_MS", 60_000)?,
            webhook_poll_interval: millis("WEBHOOK_POLL_INTERVAL_MS", 1_000)?,
            webhook_retry_schedule: match optional("WEBHOOK_RETRY_SCHEDULE") {
                Some(s) => parse_schedule(&s)?,
                None => default_retry_schedule(),
            },
            webhook_timeout: millis("WEBHOOK_TIMEOUT_MS", 10_000)?,
        };
        config.validate()?;
        Ok(config)
    }

    /// Timing invariants that correctness depends on. Refuse to start if violated.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.psp_not_found_grace <= self.psp_timeout {
            bail!(
                "PSP_NOT_FOUND_GRACE_MS ({:?}) must be greater than PSP_TIMEOUT_MS ({:?}): \
                 a PSP 404 only proves 'never charged' once our request can no longer be in flight",
                self.psp_not_found_grace,
                self.psp_timeout
            );
        }
        if self.reconciler_min_age < self.psp_timeout {
            bail!(
                "RECONCILER_MIN_AGE_MS ({:?}) must be >= PSP_TIMEOUT_MS ({:?}): \
                 the reconciler must not poll attempts the request handler is still waiting on",
                self.reconciler_min_age,
                self.psp_timeout
            );
        }
        if self.seed_demo
            && (self.demo_api_key.is_none()
                || self.demo_webhook_url.is_none()
                || self.demo_webhook_secret.is_none())
        {
            bail!("SEED_DEMO=true requires DEMO_API_KEY, DEMO_WEBHOOK_URL and DEMO_WEBHOOK_SECRET");
        }
        Ok(())
    }
}

/// 10s, 30s, 2m, 10m, 30m, 1h, 3h, 6h: 1 initial try + 8 retries over ~10.7h.
pub fn default_retry_schedule() -> Vec<Duration> {
    [10, 30, 120, 600, 1_800, 3_600, 10_800, 21_600]
        .into_iter()
        .map(Duration::from_secs)
        .collect()
}

/// Comma-separated seconds, e.g. "2,5,10".
fn parse_schedule(s: &str) -> anyhow::Result<Vec<Duration>> {
    s.split(',')
        .map(|part| {
            part.trim()
                .parse::<u64>()
                .map(Duration::from_secs)
                .with_context(|| format!("invalid WEBHOOK_RETRY_SCHEDULE entry {part:?}"))
        })
        .collect()
}

fn required(name: &str) -> anyhow::Result<String> {
    env::var(name).with_context(|| format!("{name} must be set"))
}

fn optional(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.is_empty())
}

fn millis(name: &str, default: u64) -> anyhow::Result<Duration> {
    match env::var(name) {
        Ok(v) => {
            Ok(Duration::from_millis(v.parse().with_context(|| {
                format!("{name} must be an integer (ms)")
            })?))
        }
        Err(_) => Ok(Duration::from_millis(default)),
    }
}
