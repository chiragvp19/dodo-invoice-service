//! Local demo data (SEED_DEMO=true, set only by docker-compose): a demo business
//! with a fixed, public API key and the webhook-receiver registered as an
//! endpoint. Every statement is an upsert so restarts are harmless.

use anyhow::Context;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{auth, config::Config};

pub const DEMO_BUSINESS_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_d0d0);
pub const DEMO_ENDPOINT_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_e0e0);

pub async fn run(db: &PgPool, config: &Config) -> anyhow::Result<()> {
    let (Some(key), Some(url), Some(secret)) = (
        config.demo_api_key.as_deref(),
        config.demo_webhook_url.as_deref(),
        config.demo_webhook_secret.as_deref(),
    ) else {
        return Ok(());
    };
    let prefix = auth::parse_key(key)
        .context("DEMO_API_KEY must have the normal sk_<8>_<32> format")?;

    let mut tx = db.begin().await?;
    sqlx::query("INSERT INTO businesses (id, name) VALUES ($1, 'Demo Business') ON CONFLICT (id) DO NOTHING")
        .bind(DEMO_BUSINESS_ID)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO api_keys (id, business_id, prefix, key_hash) VALUES ($1, $2, $3, $4)
         ON CONFLICT (prefix) DO UPDATE
             SET business_id = EXCLUDED.business_id, key_hash = EXCLUDED.key_hash, revoked_at = NULL",
    )
    .bind(Uuid::now_v7())
    .bind(DEMO_BUSINESS_ID)
    .bind(prefix)
    .bind(auth::hash_key(key))
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO webhook_endpoints (id, business_id, url, secret) VALUES ($1, $2, $3, $4)
         ON CONFLICT (id) DO UPDATE SET url = EXCLUDED.url, secret = EXCLUDED.secret, active = true",
    )
    .bind(DEMO_ENDPOINT_ID)
    .bind(DEMO_BUSINESS_ID)
    .bind(url)
    .bind(secret)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    tracing::info!(business_id = %DEMO_BUSINESS_ID, key_prefix = %prefix, webhook_url = %url, "demo data seeded");
    Ok(())
}
