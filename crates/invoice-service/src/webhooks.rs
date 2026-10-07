//! Webhook signing. Header: `Dodo-Signature: t=<unix seconds>,v1=<hex>` where
//! `v1 = HMAC-SHA256(endpoint_secret, "{t}.{raw_body}")`. Receivers recompute it
//! over the raw body, compare in constant time, and reject `|now - t| > 300s`
//! (replay window). Each delivery attempt is re-signed with a fresh `t`.

use hmac::{Hmac, Mac};
use sha2::Sha256;

pub fn signature(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(format!("{timestamp}.").as_bytes());
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

pub fn signature_header(secret: &str, timestamp: i64, body: &[u8]) -> String {
    format!("t={timestamp},v1={}", signature(secret, timestamp, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_depends_on_secret_timestamp_and_body() {
        let base = signature("whsec_a", 100, b"{}");
        assert_eq!(base.len(), 64);
        assert_eq!(base, signature("whsec_a", 100, b"{}"));
        assert_ne!(base, signature("whsec_b", 100, b"{}"));
        assert_ne!(base, signature("whsec_a", 101, b"{}"));
        assert_ne!(base, signature("whsec_a", 100, b"{ }"));
    }
}
