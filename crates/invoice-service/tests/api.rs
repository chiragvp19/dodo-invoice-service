//! API contract: pagination, strict query parameters, and the error envelope
//! for framework-level rejections (405, 413, 415).

mod common;

use serde_json::{json, Value};

fn error_type(body: &Value) -> &str {
    body["error"]["type"].as_str().unwrap_or("<no envelope>")
}

#[tokio::test]
async fn lists_paginate_with_limit_and_cursor() {
    let app = common::spawn_app().await;
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(app.open_invoice(100).await);
    }

    let page1: Value = app
        .get("/invoices?limit=2")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page1["data"].as_array().unwrap().len(), 2, "{page1}");
    assert_eq!(page1["has_more"], true);
    // Newest first.
    assert_eq!(page1["data"][0]["id"], ids[2].as_str());

    let cursor = page1["data"][1]["id"].as_str().unwrap();
    let page2: Value = app
        .get(&format!("/invoices?limit=2&starting_after={cursor}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page2["data"].as_array().unwrap().len(), 1, "{page2}");
    assert_eq!(page2["has_more"], false);
    assert_eq!(page2["data"][0]["id"], ids[0].as_str());

    // `limit` parses on every list endpoint.
    for path in [
        "/customers?limit=1",
        "/events?limit=1",
        "/webhook_endpoints?limit=1",
    ] {
        let resp = app.get(path).send().await.unwrap();
        assert_eq!(resp.status(), 200, "{path}");
    }
    let attempts = app
        .get(&format!("/invoices/{}/payment_attempts?limit=1", ids[0]))
        .send()
        .await
        .unwrap();
    assert_eq!(attempts.status(), 200);
    assert_eq!(
        app.get("/invoices?limit=0").send().await.unwrap().status(),
        400
    );
}

#[tokio::test]
async fn query_parameters_are_strict_and_state_aliases_status() {
    let app = common::spawn_app().await;
    let _unpaid = app.open_invoice(100).await; // must be filtered out
    let paid = app.open_invoice(100).await;
    assert_eq!(
        app.pay(&paid, "strict-q-1", "tok_success").await.status(),
        200
    );

    for param in ["status", "state"] {
        let list: Value = app
            .get(&format!("/invoices?{param}=paid"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let ids: Vec<&str> = list["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec![paid.as_str()], "?{param}=paid filters");
    }

    for path in [
        "/invoices?statuz=paid",
        "/invoices?status=paid&state=open",
        "/invoices?status=bogus",
        "/customers?foo=1",
        "/events?type=invoice.bogus",
    ] {
        let resp = app.get(path).send().await.unwrap();
        assert_eq!(resp.status(), 400, "{path}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(error_type(&body), "validation_error", "{path}");
    }
}

#[tokio::test]
async fn framework_rejections_use_the_error_envelope() {
    let app = common::spawn_app().await;

    let wrong_method = app
        .http
        .put(format!("{}/invoices", app.base))
        .bearer_auth(&app.api_key)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_method.status(), 405);
    assert_eq!(
        error_type(&wrong_method.json().await.unwrap()),
        "method_not_allowed"
    );

    let not_json = app
        .post("/customers")
        .body(r#"{"name":"a","email":"a@b.c"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(not_json.status(), 415);
    assert_eq!(
        error_type(&not_json.json().await.unwrap()),
        "unsupported_media_type"
    );

    let too_big = app
        .post("/customers")
        .header("content-type", "application/json")
        .body(vec![b' '; 3 * 1024 * 1024])
        .send()
        .await
        .unwrap();
    assert_eq!(too_big.status(), 413);
    assert_eq!(
        error_type(&too_big.json().await.unwrap()),
        "payload_too_large"
    );

    // Well-formed JSON of the wrong shape is 400 (422 is reserved for key reuse).
    let wrong_shape = app
        .post("/customers")
        .json(&json!({ "name": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_shape.status(), 400);
    assert_eq!(
        error_type(&wrong_shape.json().await.unwrap()),
        "validation_error"
    );

    let unknown_route = app.get("/nope").send().await.unwrap();
    assert_eq!(unknown_route.status(), 404);
    assert_eq!(
        error_type(&unknown_route.json().await.unwrap()),
        "not_found"
    );
}
