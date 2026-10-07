mod api_keys;
mod businesses;

use axum::{
    routing::{delete, post},
    Router,
};

use crate::AppState;

pub fn v1() -> Router<AppState> {
    Router::new()
        .route("/businesses", post(businesses::create))
        .route("/api_keys", post(api_keys::create))
        .route("/api_keys/{id}", delete(api_keys::revoke))
}
