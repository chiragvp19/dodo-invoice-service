# Implementation Plan — Dodo Invoice & Payment Service

> Working plan (v3.1, final; revised after three review passes, see review.md). DESIGN.md is the graded design document. This file records how the work was planned.

## 0. Goals & constraints (from the brief)

- Rust (Axum) + PostgreSQL + migrations; `docker compose up` = app + DB + mock PSP, zero manual steps.
- 4–6 h budget; stop and document remaining work if exceeded (cut order in §12).
- Graded equally: design judgment, core correctness, operational sense, communication.
- Restraint is graded: must-haves only; extras go to DESIGN.md "What I cut".
- Money: `BIGINT` cents in the DB, `i64` in Rust, checked arithmetic. No floats anywhere in the money path.

## 1. Architecture

```
            ┌───────────────────── docker compose ─────────────────────┐
 client ──▶ │ invoice-service (Axum)                                   │
 (curl)     │   ├─ HTTP API  /v1/*                                     │
            │   ├─ webhook worker  (tokio task, leased outbox)  ───────┼──▶ webhook-receiver
            │   └─ reconciler      (tokio task, resolves pending/unknown) │  (verifies sig, logs)
            │          │  HTTP (PSP_TIMEOUT_MS, default 5s)            │
            │          ▼                                               │
            │      mock-psp (Axum)            postgres:16              │
            └──────────────────────────────────────────────────────────┘
```

- Cargo workspace, three binaries: `invoice-service`, `mock-psp`, `webhook-receiver`.
- **Runtime sqlx queries** (`sqlx::query_as`, not `query!` macros), so the Docker build needs no DB and no offline-prep step.
- Plain multi-stage Dockerfile first; cargo-chef only if time allows.
- Migrations run at startup (`sqlx::migrate!`); compose healthchecks order the startup.
- Config (env): `DATABASE_URL`, `PSP_URL`, `PSP_TIMEOUT_MS`, `ADMIN_TOKEN`, `SEED_DEMO`, `DEMO_API_KEY`, `DEMO_WEBHOOK_SECRET`, `RECONCILER_INTERVAL_MS`, `RECONCILER_MIN_AGE_MS`, `PSP_NOT_FOUND_GRACE_MS`, `WEBHOOK_RETRY_SCHEDULE`.
- **Timing defaults & invariants** (checked at startup; the app refuses to start if violated):

  | Setting | Default | Test value | Rule |
  |---|---|---|---|
  | `PSP_TIMEOUT_MS` | 5000 | 500 | |
  | `RECONCILER_MIN_AGE_MS` | 10000 | 600 | ≥ `PSP_TIMEOUT_MS` (don't poll attempts the handler still waits on) |
  | `PSP_NOT_FOUND_GRACE_MS` | 60000 | 1500 | > `PSP_TIMEOUT_MS` (a 404 means something only once our POST can't still be in flight) |
  | `RECONCILER_INTERVAL_MS` | 5000 | 200 | |
- Published host ports: Postgres **55432** (a local Postgres already uses 5432 on the dev machine), mock-psp 4000, app 8080, receiver 9000.

### Observability
- `tracing` + `tracing-subscriber` (pretty by default, JSON via env) and `tower-http` `TraceLayer` with a request id (`x-request-id`, generated if absent, echoed back).
- One structured log line per: state transition (`invoice_id, from, to, trigger`), PSP call (`attempt_id, token outcome, latency_ms`), reconciler resolution, webhook delivery attempt (`delivery_id, attempt, status_code, next_attempt_at`).
- Card tokens and API keys are never logged in full (prefix only).

## 2. Data model

PKs are UUIDv7 (time-ordered, so index-friendly, non-enumerable, and they double as the pagination cursor). Every tenant table carries `business_id`.

| Table | Columns (key ones) | Constraints / indexes |
|---|---|---|
| businesses | id, name, created_at | |
| api_keys | id, business_id, prefix, key_hash, created_at, revoked_at | unique(prefix) |
| customers | id, business_id, name, email | **unique(business_id, id)** (target of the tenant FK). Email is *not* unique (Stripe allows duplicates; the brief doesn't require it), so duplicates have no error path |
| invoices | id, business_id, customer_id, status, **pre_payment_status** (NULL unless processing), total_cents BIGINT, due_date, created_at, updated_at, paid_at | **FK (business_id, customer_id) → customers(business_id, id)** (tenant-safe); CHECK total_cents > 0; idx(business_id, status, id) |
| invoice_line_items | id, invoice_id, position, description, quantity INT, unit_amount_cents BIGINT, amount_cents BIGINT | CHECK quantity BETWEEN 1 AND 1_000_000, unit ≥ 0 |
| payment_attempts | id, invoice_id, business_id, status (pending/unknown/succeeded/failed), amount_cents, card_token, psp_ref, failure_code, created_at, resolved_at | partial unique(invoice_id) WHERE status IN ('pending','unknown'); partial unique(invoice_id) WHERE status='succeeded'; idx(status, created_at) for the reconciler |
| idempotency_keys | business_id, key, request_hash, status (in_progress/completed), response_code, **response_body BYTEA** (exact bytes sent; JSONB would reorder keys and break byte-identical replay), payment_attempt_id, created_at, expires_at | PK(business_id, key) |
| webhook_endpoints | id, business_id, url, secret, active | |
| webhook_events | id, business_id, type, payload JSONB, created_at | outbox, written in the same tx as the state change |
| webhook_deliveries | id, event_id, endpoint_id, status (pending/delivered/dead), attempts, next_attempt_at, last_status_code, last_error | idx(status, next_attempt_at) |

- Create validation: 1–100 line items, quantity 1..1e6, non-empty description, `total_cents > 0`, `due_date` ≥ today **(UTC)**. Totals use `checked_mul`/`checked_add`, and overflow is 400. Request structs use `deny_unknown_fields`, so a client-supplied `total` is rejected.
- `due_date` is informational: no automatic overdue transition (dunning is a production gap).

## 3. Invoice state machine

| From | Trigger | To |
|---|---|---|
| (new) | POST /invoices | draft (emits `invoice.created`) |
| draft | POST /finalize | open |
| draft | POST /void | void |
| open | POST /pay (attempt starts) | processing |
| uncollectible | POST /pay (late payment, Stripe semantics) | processing |
| processing | PSP succeeded (handler or reconciler) | paid |
| processing | PSP failed / confirmed not charged | the state it came from (open or uncollectible) |
| open | POST /void | void |
| open | POST /mark_uncollectible | uncollectible |

- Terminal states: `paid`, `void`. `uncollectible` is non-terminal on purpose: it can still be paid.
- Reversible: only `processing → open/uncollectible` on payment failure. Every other edge moves forward.
- Rejected: void or mark_uncollectible while `processing` (money may be in flight); pay on draft, processing, paid or void.
- Enforcement: a pure `fn transition(from, action) -> Result<to, TransitionError>` (unit-tested table), plus a DB guard `UPDATE … WHERE id=$1 AND business_id=$2 AND status = ANY($allowed_from)`. 0 rows → 409 `invalid_state_transition` with `{current_status, action}`.
- To "return to the previous state" on failure, Tx1 records it on the invoice in the same statement: `SET status='processing', pre_payment_status = status` (right-hand sides see the old row; `RETURNING` only sees the new one before PG 18). Resolution restores it and clears it. It's durable, so the reconciler can use it after a crash.

## 4. Payment flow: `POST /v1/invoices/{id}/pay`

Body `{ "card_token": "tok_success" }` (non-empty). `Idempotency-Key` header required (400 if missing; ≤ 255 chars). `request_hash = sha256("POST" + path + canonical body)`, so the invoice id is part of the hash.

1. **Idempotency lookup**: `SELECT` by (business_id, key), treating expired rows as absent (24 h TTL, lazy expiry).
   - Exists, hash differs: **422 `idempotency_key_reused`** (checked before in_progress).
   - Exists, `completed`: replay the stored code and body byte-for-byte. No PSP call.
   - Exists, `in_progress`: **409 `request_in_progress`**.
2. **Tx1, a single transaction**:
   - `INSERT idempotency_keys (in_progress) ON CONFLICT (business_id, key) DO UPDATE SET <reset to in_progress, new hash, clear response> WHERE idempotency_keys.expires_at < now() RETURNING 1`. This takes over expired rows. If 0 rows, a live row exists (or a concurrent same-key request just committed): roll back and go back to step 1. **At most 3 loops, then 409** (guards against a future bug spinning forever).
   - `UPDATE invoices SET status='processing', pre_payment_status=status WHERE id=$1 AND business_id=$2 AND status IN ('open','uncollectible') RETURNING total_cents`. If 0 rows, **roll back** (the key is released, nothing cached), then one `SELECT status … WHERE id AND business_id` decides 404 (absent) or 409 `invalid_state_transition` (with current_status).
   - `INSERT payment_attempts (pending)` and link it to the key. Commit.
3. **PSP call** outside any transaction: `POST psp/charges`, `Idempotency-Key: <attempt_id>`, timeout `PSP_TIMEOUT_MS`.
4. **Resolution (Tx2)**: the same guarded function the reconciler uses:
   `UPDATE payment_attempts SET status=$new … WHERE id=$1 AND status IN ('pending','unknown')`. **Only if 1 row** do we also update the invoice, insert the outbox event, and complete the idempotency key **only if it is still in progress**: `UPDATE idempotency_keys SET status='completed', response_code=…, response_body=… WHERE business_id=$1 AND key=$2 AND status='in_progress'`. So the reconciler never overwrites a stored 202 (timeout case), but does store the final 200/402 when the handler crashed (case c). If 0 rows, the reconciler won the race: the handler returns **the key's stored response** (never builds a fresh one), so the first answer and every replay are identical.
   - **PSP response mapping** (`psp.rs`): only a parseable terminal `succeeded` or `failed` is definitive. Everything else (transport error, timeout, 5xx, 4xx, unparseable 2xx, 2xx with `processing`) is **ambiguous**, which goes to step 5. Never guess.
   - **One response builder** (`payment_response(invoice, attempt) -> (status, bytes)`) is shared by the handler and the reconciler, so both produce identical 200/402 bodies.
   - succeeded: attempt succeeded, invoice paid, `invoice.paid`, **200** `{invoice, payment_attempt}`.
   - failed (decline): attempt failed, invoice back to its previous status, `invoice.payment_failed`, **402** `{error: {type: payment_failed, details: {decline_code, payment_attempt_id}}}`.
5. **Ambiguous result** (timeout, connection reset or refused, 5xx). *Deliberate:* "connection refused" provably never reached the PSP and could fail fast, but treating every transport error as `unknown` keeps one code path and is still safe: attempt `unknown` (guarded), invoice stays `processing`, **202** `{payment_attempt_id, status: "processing"}`. The `pending → unknown` update and storing the 202 as the key's completed response happen in **one transaction**, so a crash can't leave an `unknown` attempt with an `in_progress` key. The 202 stays stable on replay *by design*: a same-key retry keeps returning 202, and the outcome is on the invoice and the webhook. The final outcome comes via webhook or `GET /invoices/{id}`.
6. **Reconciler**: every `RECONCILER_INTERVAL_MS`, attempts in `pending`/`unknown` older than `RECONCILER_MIN_AGE_MS` → `GET psp/charges/{attempt_id}`:
   - succeeded or failed: resolve through the step 4 function. If the linked key is still `in_progress` (handler crashed), store the final response on it.
   - 404 and older than `PSP_NOT_FOUND_GRACE_MS`: resolve as failed (`psp_not_charged`).
   - processing or PSP unreachable: leave it and try next tick.
   - Multiple replicas are safe: guarded resolution means a duplicate run only wastes one PSP lookup.

### Failure-mode answers (DESIGN.md §3)
- **(a) Concurrent pays**: in Postgres, a conditional `UPDATE` re-checks its `WHERE` after waiting on the row lock. Exactly one transaction moves open→processing, and the rest match 0 rows and get 409. The partial unique indexes back this up at the schema level. Alternatives: `SELECT FOR UPDATE` works but takes more statements for the same effect; holding a lock across the PSP call blocks; advisory locks leave no durable state behind; SERIALIZABLE forces retry loops.
- **(b) tok_timeout**: we get 202 after `PSP_TIMEOUT_MS`, with the attempt `unknown` and the invoice `processing`. The reconciler sees the PSP charge succeed at about 30 s, then the invoice becomes paid and `invoice.paid` fires.
- **(c) Crash after PSP success**: the attempt is still `pending` and the key `in_progress` (a retry gets 409). The reconciler looks up `attempt_id`, finds succeeded, marks the invoice paid and completes the key. No second charge is possible: the invoice is still `processing`, so no new attempt can start, and the PSP is idempotent on attempt_id.
- **(d) Key reused with a different body or invoice**: 422, and the original stored response is untouched.
- **(e) Pay on paid**: new key gives 409 `invalid_state_transition` (current_status=paid), no PSP call. The original key replays the original 200.
- **Stated assumption (M4)**: "PSP 404 after grace means never charged" holds for our mock (it records before acting) but is not guaranteed by real PSPs. In production: void/cancel by idempotency key, or wait out the PSP idempotency window before reopening. This goes under production gaps.

## 5. Mock PSP (separate binary, in-memory)

- `POST /charges` with `{amount_cents, card_token}` and `Idempotency-Key`. It records the charge first, then:
  - tok_success: about 100 ms, then succeeded with `psp_ref` (uuid).
  - tok_insufficient_funds / tok_card_declined: about 100 ms, then failed with code.
  - tok_timeout: store `processing` and **`tokio::spawn`** a task that marks it succeeded after 30 s, independent of the caller. The handler then sleeps 30 s and returns success if the caller is still connected.
  - tok_network_error: **does not record**; returns 500 (`--drop-connection` mode optional).
  - unknown token: failed with `invalid_token`.
  - Repeated idempotency key: return the stored charge, no new charge.
- `GET /charges/{idempotency_key}`: status lookup, 404 if unknown. A small extension beyond the brief, modelled on real PSPs; justified in DESIGN.md.
- `GET /_stats`: charge count per key, so tests can assert "no double charge".
- Limitation (documented): state is lost on restart, which the reconciler would then read as "not charged".

## 6. Webhooks

- Events: `invoice.created` (on create, as draft), `invoice.paid`, `invoice.payment_failed`. `invoice.voided` is cut.
- Outbox: the event row and one delivery row per active endpoint are written in the same transaction as the state change.
- Worker (**lease, no lock held during HTTP**):
  `UPDATE webhook_deliveries SET next_attempt_at = now() + interval '60s' WHERE id IN (SELECT id … WHERE status='pending' AND next_attempt_at <= now() ORDER BY next_attempt_at FOR UPDATE SKIP LOCKED LIMIT 20) RETURNING …`. Commit, POST concurrently (10 s timeout), then write each result. If the worker crashes mid-flight, the lease expires and the delivery is retried. That gives at-least-once delivery, so receivers dedupe on `event.id`.
- Signature: `Dodo-Signature: t=<unix>,v1=<hex(HMAC-SHA256(secret, "{t}.{raw_body}"))>`, re-signed on each attempt with a fresh `t`. The receiver rejects `|now - t| > 300 s`.
- Retry: **1 initial attempt + 8 retries = 9 total** at +10 s, 30 s, 2 m, 10 m, 30 m, 1 h, 3 h, 6 h (about 10.7 h) with ±20 % jitter, then `dead`. The schedule can be overridden by env (short in the demo and tests).
- Missed events: business reconciles via `GET /v1/events` (optional, see §12) or by re-fetching invoices.
- **Demo seeding:** with `SEED_DEMO=true`, the seed also registers `http://webhook-receiver:9000/webhooks` for the demo business, with secret `DEMO_WEBHOOK_SECRET`. Compose passes the same variable to the receiver, so `docker compose up` alone produces verified deliveries. Normal `POST /webhook_endpoints` still generates secrets server-side.
- **Result handling per attempt:** 2xx marks it `delivered`. Otherwise `attempts += 1` and `next_attempt_at = now() + schedule[attempts] ± 20 %`, or `dead` after the last retry. A lease expiry after a worker crash re-sends **without** incrementing `attempts`.
- Endpoint secret is generated server-side and returned once at creation. Stored plaintext because HMAC needs it; encryption at rest and SSRF filtering of URLs are production gaps.
- webhook-receiver: a small Axum app that verifies the signature with `DEMO_WEBHOOK_SECRET`, logs `✔ verified invoice.paid evt_…`, **dedupes on `event.id`** (logs `↺ duplicate evt_…`), and can be configured to fail the first N calls to demo retries.

## 7. API keys & auth

- Bootstrap:
  - (a) `SEED_DEMO` defaults to **false** in the app; only compose sets it to true. The seed is an **upsert**, so restarts with changed env values don't fail. It creates a demo business with the fixed dev key from `DEMO_API_KEY` plus the receiver webhook endpoint (§6). `DEMO_API_KEY` must match the normal `sk_<8>_<32 base62>` format and goes through the normal auth path (no special case). README curls work right after `docker compose up`. README states: "demo credentials are for local use only" (they're public in the repo).
  - (b) `POST /v1/businesses` with `X-Admin-Token` returns a business plus its key, shown once. Tests use this, one business per test.
- Format: `sk_<prefix 8>_<secret 32 base62>`. Stored: `prefix` plus SHA-256(full key). A fast hash is fine because the key has 190+ bits of entropy. Constant-time compare.
- `Authorization: Bearer <key>`. Unknown, revoked or malformed keys all get the same 401.
- `DELETE /v1/api_keys/{id}` revokes. Rotation means bootstrapping or creating a new key, then revoking the old one. The `POST /api_keys` endpoint is the first thing cut if time runs short.
- Leaked-key blast radius: one business. Forensics (`last_used_at`, audit log) is a production gap.

## 8. API surface (`/v1`) & errors

```
POST   /businesses                        (admin)
POST   /api_keys   DELETE /api_keys/{id}
POST   /customers  GET /customers  GET /customers/{id}
POST   /invoices   GET /invoices?status=  GET /invoices/{id}
POST   /invoices/{id}/finalize | /void | /mark_uncollectible | /pay
GET    /invoices/{id}/payment_attempts
POST   /webhook_endpoints  GET /webhook_endpoints
GET    /events                            (optional)
GET    /healthz
```
- Error envelope: `{"error": {"type": "...", "message": "...", "details": {...}}}`.
- Status rule: **400** malformed or invalid input (including overflow and a missing Idempotency-Key), **401** auth, **404** not found (including other tenants' resources), **409** state conflict, request in progress, or the idempotency retry bound being hit, **402** payment declined, **422** only `idempotency_key_reused`.
- Lists: `?limit=` (default 20, max 100) and `starting_after=<id>`, ordered by id.
- `docs/openapi.yaml` written **before** the pay handler, with exact 200/202/402/409/422 bodies.

## 9. Tests

- The test harness starts the app **in-process** on a random port with the test timings from §1 (500 / 600 / 1500 / 200 ms, all satisfying the invariants). It points at compose's Postgres and mock-psp via **published ports and localhost URLs** (`postgres://…@localhost:55432/…`, `http://localhost:4000`), set in `.env.example` and the README. Each test creates its own business via the admin endpoint, so tests can run in parallel.
- Run: `docker compose up -d db mock-psp && cargo test` (documented in README).

1. **Concurrency**: 20 parallel `/pay` (distinct keys, tok_success). Expect exactly one 200 and 19 × 409, PSP charge count 1, invoice paid, one succeeded attempt, one `invoice.paid` event.
2. **Idempotency**: same key twice gives byte-identical responses and a PSP count of 1. Same key to a different invoice or token gives 422.
3. **PSP failure**: `tok_timeout` returns 202 in under 1 s, then the invoice becomes paid within a **40 s** deadline (the mock's 30 s delay is fixed). `tok_network_error` ends with the invoice back to open and the attempt failed. Neither stays in `processing`.
- Plus a fast unit test of the transition table, with no I/O.

## 10. DESIGN.md outline (~1,400 words)

| § | Content | Words |
|---|---|---|
| 1 Data model | table list, PK strategy (UUIDv7), tenant FK, indexes; per-table "at 100x" (time-partition deliveries/idempotency, TTL purge, read replica for lists, outbox → queue, reconciler sharding) | 280 |
| 2 State machine | Mermaid diagram, triggers, terminal states, reversibility, how invalid transitions are rejected, uncollectible deliberation | 170 |
| 3 Payment correctness | (a)–(e) as in §4, named mechanism plus alternatives, PSP-404 assumption | 450 |
| 4 Webhooks | HMAC scheme, replay window, 1+8 retries with numbers, dead letters, reconciliation, lease-based decoupling | 200 |
| 5 API keys | format, prefix + SHA-256, Bearer, rotation, revocation, blast radius | 110 |
| 6 Cut | refunds/partial payments, `invoice.voided`, manual redelivery, draft editing, dunning/overdue, rate limiting, `last_used_at` | 110 |
| 7 Prod gaps | observability/alerting on attempts stuck in `unknown`/`processing` past X min (PSP permanently unreachable keeps the key at 409) and on dead webhooks; rate limiting; audit log + key forensics; PSP void-by-key; SSRF and secret encryption | 100 |

## 11. Repo layout

```
dodo-invoice-service/
├── Cargo.toml                 (workspace)
├── crates/
│   ├── invoice-service/       src/{main,config,error,auth,state_machine,psp,routes/*,db/*,workers/*}.rs, tests/
│   ├── mock-psp/
│   └── webhook-receiver/
├── migrations/
├── docs/openapi.yaml
├── Dockerfile  docker-compose.yml  .env.example
├── README.md  DESIGN.md  AI_USAGE.md  PLAN.md
└── notes/                     (gitignored: setup notes, AI decision log)
```

## 12. Build order, time, and cut order

| # | Step | Est. | Commit |
|---|---|---|---|
| 1 | Workspace, Dockerfile, compose, migrations, tracing, /healthz | 40 m | `chore: scaffold workspace, compose, migrations` |
| 2 | Error envelope, seed and admin bootstrap, auth, key revoke | 25 m | `feat: api key auth` |
| 3 | Customers, invoices, line items, state machine | 55 m | `feat: customers and invoices with state machine` |
| 4 | Mock PSP | 20 m | `feat: mock psp` |
| 5 | OpenAPI pay shapes, /pay, idempotency, reconciler | 70 m | `feat: idempotent payments with reconciliation` |
| 6 | Outbox, webhook worker, receiver | 40 m | `feat: signed webhooks with retry` |
| 7 | Tests | 40 m | `test: concurrency, idempotency, psp failure` |
| 8 | README, OpenAPI, DESIGN.md, AI_USAGE.md | 55 m | `docs: …` |
| 9 | Clean-clone check (`down -v`, fresh clone, `up`) | 10 m | |
| | **Total** | **≈ 5.9 h, optimistic** | (toolchain install and video not included) |

Realistically, scope grew in v2/v3 while the estimates shrank. Assume cut items 1–3 below will happen. **Hard rule: start step 8 (docs) by the 4.5 h mark** whatever the state of steps 5–7, because DESIGN.md is the primary deliverable. Keep notes for DESIGN.md while building so step 8 is mostly editing.

**Cut order if running long** (each one moves into DESIGN §6): 1. `GET /events` → 2. `POST /api_keys` → 3. receiver's fail-first-N mode → 4. `starting_after` pagination (keep `limit`) → 5. `mark_uncollectible` + uncollectible state (also removes the `pre_payment_status` logic, because failure always returns to `open`). Never cut: idempotency, guarded resolution, reconciler, outbox, the three tests, DESIGN.md.

## 13. Delivery

- Public GitHub repo; commits carry the `Co-Authored-By: Claude` trailer, consistent with AI_USAGE.md.
- Demo video uploaded as YouTube *Unlisted* or Google Drive "anyone with link" (Loom free is capped at about 5 min). Check the link in incognito. It goes under `## Demo Video` in README.
- Video order: architecture (1–2 m) → live demo: customer, invoice, finalize, pay `tok_success`, pay `tok_card_declined`, receiver logs showing verified webhooks (2–3 m) → state machine, unscripted (1–2 m) → failure mode `tok_timeout` + reconciler, walking through the code (1–2 m).
