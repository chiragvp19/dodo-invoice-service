# Invoice & Payment Service

A small invoice and payment backend. Businesses create invoices for customers, customers pay them through a (mock) payment processor, and businesses get signed webhooks for state changes.

**Written in Rust** (Axum, sqlx, Tokio) with **PostgreSQL 16**.

- **[DESIGN.md](DESIGN.md):** data model, state machine, payment failure modes, webhooks, API keys
- **[docs/openapi.yaml](docs/openapi.yaml):** API reference, with request/response shapes and the error format
- **[AI_USAGE.md](AI_USAGE.md):** how AI tools were used
- **[PLAN.md](PLAN.md) / [review.md](review.md):** the plan written before coding, and its three review passes

## Demo Video

**▶ TODO: add link (accessible without login)**

Contents: architecture → live demo → state machine walkthrough → failure-mode walkthrough (`tok_timeout` + reconciler).

## Run it

```bash
docker compose up --build
```

This brings up five services:

| Service | Port | What it does |
|---|---|---|
| `app` | `localhost:8080` | The invoice service. It runs migrations at startup, then seeds a demo business. |
| `mock-psp` | `localhost:4000` | The mock payment processor. Outcomes depend on the card token (table below). |
| `webhook-receiver` | `localhost:9000` | A demo merchant endpoint. It verifies signatures and logs every event. |
| `db` | `localhost:55432` | Postgres 16. |

> **Demo credentials are for local use only.** The API key `sk_demo0001_LocalDemoKeyNotForProductionUse0` and the webhook secret in `docker-compose.yml` are public. They are only seeded because compose sets `SEED_DEMO=true`, which defaults to off.

To watch webhooks arrive, run `docker compose logs -f webhook-receiver`, or `curl localhost:9000/received`.

### Card tokens (mock PSP)

| Token | Result |
|---|---|
| `tok_success` | 200, invoice `paid` |
| `tok_card_declined` / `tok_insufficient_funds` | 402 `payment_failed`, invoice back to `open` |
| `tok_timeout` | PSP sleeps 30 s. The API returns **202** after 5 s and the invoice stays `processing`. The reconciler later marks it `paid`. |
| `tok_network_error` | PSP returns 500. The API returns **202**. The reconciler confirms nothing was charged and returns the invoice to `open`. |

## API examples

These use [`jq`](https://jqlang.github.io/jq/) to capture ids.

```bash
API=http://localhost:8080/v1
AUTH="Authorization: Bearer sk_demo0001_LocalDemoKeyNotForProductionUse0"
```

**1. Create a customer**
```bash
CUSTOMER_ID=$(curl -s $API/customers -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name": "Ada Lovelace", "email": "ada@example.com"}' | jq -r .id)
```

**2. Create an invoice, then finalize it (draft → open)**

The server computes the total. Sending `total_cents` is rejected.
```bash
INVOICE_ID=$(curl -s $API/invoices -H "$AUTH" -H 'Content-Type: application/json' -d '{
  "customer_id": "'$CUSTOMER_ID'",
  "due_date": "2099-12-31",
  "line_items": [
    {"description": "Pro plan", "quantity": 2, "unit_amount_cents": 1999},
    {"description": "Setup fee", "quantity": 1, "unit_amount_cents": 500}
  ]}' | jq -r .id)                                   # total_cents = 4498

curl -s -X POST $API/invoices/$INVOICE_ID/finalize -H "$AUTH" | jq .status   # "open"
```

**3. Pay it (success).** `Idempotency-Key` is required.
```bash
curl -s $API/invoices/$INVOICE_ID/pay -H "$AUTH" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: pay-001' -d '{"card_token": "tok_success"}' | jq '.invoice.status'   # "paid"
```
Re-running the same command returns the identical response, with the header `Idempotent-Replayed: true`. The PSP is not charged again.

**4. Pay a second invoice (failure)**

Create and finalize another invoice as in step 2, then:
```bash
curl -s $API/invoices/$INVOICE_ID/pay -H "$AUTH" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: pay-002' -d '{"card_token": "tok_card_declined"}' | jq
# HTTP 402
# {"error": {"type": "payment_failed", "message": "Payment failed: card_declined",
#            "details": {"decline_code": "card_declined", "invoice_status": "open", ...}}}
```

**More:**
- `GET /invoices?status=open` lists invoices by status.
- `GET /invoices/{id}/payment_attempts` lists the attempts on one invoice.
- `GET /events` is the event log for reconciling missed webhooks.
- `POST /invoices/{id}/void` on a paid invoice gets **409 `invalid_state_transition`**.
- To create a new business and key:
  ```bash
  curl -s $API/businesses -H 'X-Admin-Token: local-admin-token' \
    -H 'Content-Type: application/json' -d '{"name": "Acme"}'
  ```

To demo webhook retries, run `RECEIVER_FAIL_FIRST_N=2 docker compose up`. The receiver answers the first two deliveries with 500, and the worker retries them.

## Tests

The integration tests run the service in-process, with short timeouts, against the real Postgres and mock PSP from compose:

```bash
docker compose up -d db mock-psp
cargo test --workspace
```

| Test | Checks |
|---|---|
| `concurrent_payments_charge_at_most_once` | 20 parallel `/pay` → exactly one 200 and nineteen 409s, one PSP charge, invoice `paid`, one `invoice.paid` event |
| `idempotent_retry_replays_without_second_psp_call` | byte-identical replay, a single PSP call, 422 when a key is reused with a different body or invoice |
| `psp_timeout_returns_fast_and_reconciles_to_paid` | `tok_timeout` returns 202 in under 3 s, then the invoice is reconciled to `paid` with one charge (takes about 30 s) |
| `psp_network_error_does_not_leave_invoice_stuck` | `tok_network_error` returns 202, then the invoice is reconciled back to `open` and can be paid |
| `rejected_request_releases_the_key` | a 409 is not cached against the idempotency key |

There are also unit tests for the state-machine transition table, the integer money math (including overflow), API key parsing and webhook signatures.

## Layout

```
crates/invoice-service/   API, payments (pay + resolve), state machine, PSP client,
                          workers/ (reconciler, webhook delivery), tests/
crates/mock-psp/          mock payment processor
crates/webhook-receiver/  demo merchant endpoint (signature verification)
migrations/               SQL schema
docs/openapi.yaml         API reference
```

## Time spent

**TODO:** how long this took, and what's left.
