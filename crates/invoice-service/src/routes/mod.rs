mod api_keys;
mod businesses;
mod customers;
mod invoices;

use axum::{
    routing::{delete, get, post},
    Router,
};

use crate::AppState;

pub fn v1() -> Router<AppState> {
    Router::new()
        .route("/businesses", post(businesses::create))
        .route("/api_keys", post(api_keys::create))
        .route("/api_keys/{id}", delete(api_keys::revoke))
        .route("/customers", post(customers::create).get(customers::list))
        .route("/customers/{id}", get(customers::get))
        .route("/invoices", post(invoices::create).get(invoices::list))
        .route("/invoices/{id}", get(invoices::get))
        .route("/invoices/{id}/finalize", post(invoices::finalize))
        .route("/invoices/{id}/void", post(invoices::void))
        .route("/invoices/{id}/mark_uncollectible", post(invoices::mark_uncollectible))
}
