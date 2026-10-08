# Review of the API structure (best practices)

Scope:
- `routes/*`, `payments.rs` (the `pay` handler), `error.rs`, `auth.rs`, `pagination.rs`, `lib.rs` (router and middleware)
- `docs/openapi.yaml`

Checked against the brief ("consistent error format", "request and response shapes", "reliable, consistent APIs") and common REST and payments-API practice, using the Stripe API as the reference.

## Verdict

**The API is in good shape, and the core is well designed:**
- **Versioning:** `/v1`.
- **Resources:** nouns with snake_case fields.
- **State changes:** action sub-resources (`/finalize`, `/void`, `/pay`) rather than `PATCH status`. This is the right call for a state machine, and worth one sentence in DESIGN §2.
- **Errors:** one envelope with stable `type` codes.
- **Tenancy:** 404 for other tenants' resources.
- **Lists:** cursor pagination with `has_more`.
- **Idempotency:** `Idempotency-Key` with `Idempotent-Replayed`.
- **Status codes:** 202 for unknown outcomes and 402 for declines.
- **Auth before parsing:** auth runs before body parsing, so unauthenticated junk gets 401 rather than 400.
- **Safe responses:** `card_token` is never echoed back, and webhook secrets are shown exactly once.

The findings below are mostly consistency gaps. The first group affects things the brief grades explicitly: a consistent error format and accurate API docs.

---

## A. Fix before submission (error-format consistency, docs accuracy)

### A1. A wrong HTTP method returns 405 with an empty body, not the error envelope
`lib.rs:41` sets `.fallback(...)`, which only covers paths that match no route. When the path matches but the method doesn't (for example `PUT /v1/invoices` or `DELETE /v1/customers/{id}`), axum returns its default **405 with no body**. That breaks "one error envelope for every failure", and it's an easy thing for a reviewer to try.

**Fix** (axum 0.8.9 is in `Cargo.lock`, which supports this):
```rust
.method_not_allowed_fallback(|| async { ApiError::MethodNotAllowed })
```
Add `MethodNotAllowed` to `ApiError` (405, `method_not_allowed`) and to the OpenAPI error table.

### A2. Content-type and body-size failures are all reported as 400
`From<JsonRejection>` maps every rejection to `Validation`, so all of these come back as 400:
- a missing `Content-Type: application/json` (axum's status is **415**)
- a body over the default 2 MB limit (**413**)
- a syntax error (400)

The envelope is right, but the status is less precise than it should be. Keep the rejection's own status:
```rust
ApiError::Validation { status: rejection.status(), message: rejection.body_text() }
```
Alternatively, add `UnsupportedMediaType` and `PayloadTooLarge` variants. Either way, add both statuses to the OpenAPI table. Clients sending curl with `-d` but no `-H 'Content-Type: application/json'` will hit this, so it's worth a line in the README too.

### A3. Unknown query parameters are silently ignored, including `?state=`
The brief says invoices are "filterable by **state**", and the API uses `status`. A reviewer who tries `GET /v1/invoices?state=paid` gets an **unfiltered** list with 200, which looks like the filter is broken. Typos such as `?limt=5` are also ignored. That's inconsistent with the strict `deny_unknown_fields` on every request body.

`deny_unknown_fields` doesn't work together with `#[serde(flatten)] page: Page`. So either:
- **Preferred:** list the fields explicitly in each query struct (`status`, `limit`, `starting_after`) and add `#[serde(deny_unknown_fields)]`. Unknown parameters then return 400, for example "unknown field `state`, expected one of `status`, `limit`, `starting_after`".
- Or also accept `state` as an alias: `#[serde(alias = "state")]` on `status`.

Doing both is reasonable: the alias matches the brief's wording, and strictness catches everything else.

### A4. The OpenAPI spec disagrees with the server in a few places
| Where | Spec says | Server does | Fix |
|---|---|---|---|
| `/pay` `card_token` | `enum` of the 5 test tokens | Accepts any string from 1 to 255 chars; an unknown token gives 402 `invalid_token` | Drop the `enum`, keep `minLength: 1, maxLength: 255`, and list the test tokens in `description`/`examples` |
| `/pay` replay header | `Idempotent-Replayed` on **200** only | Sent on any replayed response (200, 202 and 402) | Add the header to the 202 and 402 responses |
| `/events` `type` | `enum` of 3 types | Accepts any string; an unknown type returns an empty list (whereas `/invoices?status=bogus` returns 400) | Validate it like `status` (400 on unknown), so the behaviour matches the spec and the sibling endpoint |
| `/webhook_endpoints` GET, `/invoices/{id}/payment_attempts` | `{data, has_more}` | Unpaginated; `has_more` is always `false` and `limit` is not accepted | State "not paginated (bounded per business/invoice)" in both descriptions, or accept `limit`/`starting_after` like the other lists |
| Response schemas | No `required` lists | Every field except the `*_at` timestamps documented as nullable is always present | Add `required:` to `Invoice`, `Customer`, `PaymentAttempt`, `Event`, `LineItem`, so generated clients don't make every field optional |
| Error table | Has no 405, 413 or 415 | (after A1 and A2) | Add the three rows |

After editing, lint it: `npx @redocly/cli lint docs/openapi.yaml`, or paste it into editor.swagger.io.

---

## B. Recommended (cheap, improves operability)

**B1. Put `request_id` in error bodies.** The `x-request-id` header is already set and echoed back. Adding `"request_id": "…"` inside `error` (especially on 500, whose details are "logged, not returned") lets a client quote an id that matches your logs. Read it from the `RequestId` extension in `IntoResponse`, or attach it in a small middleware. Mention it in DESIGN under observability.

**B2. Add `WWW-Authenticate: Bearer` on 401** (RFC 6750). Also, the `Bearer` scheme match is case-sensitive (`strip_prefix("Bearer ")`), but RFC 7235 says schemes are case-insensitive. `bearer sk_…` currently gets 401. A small fix makes both right.

**B3. Revoking the last active key locks the business out.** `DELETE /api_keys/{id}` lets a business revoke its only key, including the one making the request. After that, there's no API path back, because the admin endpoint creates new businesses, not keys for existing ones. Either reject with 409 `last_active_key` when it would leave zero active keys, or document it as intended. There's also no way to *list* keys (id, prefix, created_at, revoked_at), so a business that lost a creation response can't find a key id to revoke. A `GET /api_keys` endpoint is about 15 lines; otherwise, list it under cuts.

**B4. Webhook endpoints can't be deactivated.** There's an `active` column, but no endpoint changes it. A business with a dead URL can't stop deliveries, which will retry for about 10.7 h per event and then go `dead`. Add `DELETE /webhook_endpoints/{id}` (sets `active = false`, 200 with the endpoint) or name it in DESIGN §6. Either is fine; just don't leave it unmentioned.

**B5. `/healthz` doesn't touch the database.** It always returns `ok`. For a liveness probe that's fine, but compose and reviewers usually read `/healthz` as "ready". A `SELECT 1` with a short timeout (returning 503 on failure) makes `docker compose ps` honest. If the compose healthcheck for the app uses it, the app will then show healthy only once the DB is reachable.

**B6. Add a `Location` header on `201 Created`.** For example `Location: /v1/invoices/{id}`, and on the `/pay` **202** as well (pointing at the invoice), plus `Retry-After` on 202 and on 409 `request_in_progress`. These are standard hints that cost one line each, and they make the 202 contract ("poll the invoice") self-describing.

---

## C. Optional / talking points (fine to leave; mention in DESIGN or the video)

- **Idempotency only on `/pay`.** `POST /customers` and `POST /invoices` aren't idempotent, so a client retrying after a network error creates duplicates. Stripe accepts `Idempotency-Key` on every POST. Your table and code would generalise, but extending it is scope creep. Put "idempotency on all POSTs" in DESIGN §6 or §7.
- **Repeated transitions return 409.** For example `finalize` on an already-`open` invoice. That's defensible (strict state machine) and consistent. Say in DESIGN §2 that the state machine has no idempotent no-op transitions, and that clients use the 409's `current_status` to tell "already done" from "not allowed".
- **402 for declines.** This matches Stripe. Some argue that a processed request should return 200 with `status: failed`. Your choice is consistent and documented; be ready to defend it in the video ("the caller's intent, payment, failed").
- **Validation errors have no machine-readable field.** Messages contain `line_items[0].quantity`, which is good for humans. Adding `details: {"param": "line_items[0].quantity"}` (Stripe's `param`) would help clients. Low priority.
- **Malformed UUID in the path returns 400.** That's fine and consistent. Some APIs return 404 so that ids are opaque. Either way works; no change needed.
- **IDs are plain UUIDs.** Stripe-style prefixed ids (`in_…`, `cus_…`, `evt_…`) help with debugging, but they're cosmetic. Skip them.
- **No filters beyond `status`.** `customer_id` and date-range filters on invoices aren't required, so leave them for "at 100x / later".

---

## Suggested order

1. A1 (405 envelope), A2 (415/413 status), A3 (strict query params plus a `state` alias): one small commit, `fix(api): consistent errors for method, media type and unknown params`. Add a quick test hitting `PUT /v1/invoices` and `?state=`.
2. A4: OpenAPI corrections plus a lint pass. Commit `docs(openapi): align spec with server behaviour`.
3. B1–B6 as time allows. If you skip any, list B3 and B4 in DESIGN §6 so they read as deliberate cuts rather than oversights.

---
---

# Review of migrations/0001_init.sql (schema)

Compared against the brief's domain model, PLAN.md v3, and the queries the code actually runs (`payments.rs`, `outbox.rs`, `workers/*`, `routes/*`).

## Verdict

**The schema meets every requirement in the brief. Nothing is blocking.**

| Brief requirement | Schema |
|---|---|
| Business has many Customers and Invoices | ✅ `business_id` FKs |
| Invoice belongs to a Customer | ✅ Composite tenant-safe FK |
| Invoice has line items, a total, a state and a due date | ✅ |
| Payment attempt records success, failure or pending | ✅ Plus `unknown` |
| API keys scoped to a business | ✅ |
| Integer cents, no floats | ✅ `BIGINT` everywhere, `CHECK total_cents > 0` |
| Postgres migrations | ✅ |

The strongest parts, and worth pointing at in the video:
- The partial unique indexes (`one_in_flight`, `one_success`).
- The cross-column CHECKs on `invoices`: `processing ⇔ pre_payment_status`, `paid ⇔ paid_at`.
- `amount_cents = quantity * unit_amount_cents` enforced in the DB.
- `BYTEA` response bodies.
- The tenant FK.

The suggestions below make the database enforce a few more invariants the code already keeps. They're cheap, and they strengthen DESIGN §1's "why this shape" answer.

## Recommended changes

### S1. `payment_attempts` can point at another business's invoice
`invoice_id` and `business_id` are independent FKs, so the DB would accept an attempt whose `business_id` differs from its invoice's. `invoices → customers` already uses the composite pattern; apply it here too:
```sql
-- invoices
UNIQUE (business_id, id),
-- payment_attempts: replace `invoice_id UUID NOT NULL REFERENCES invoices (id)` with
invoice_id    UUID NOT NULL,
...
FOREIGN KEY (business_id, invoice_id) REFERENCES invoices (business_id, id)
```
Tx1 inserts both values from the same guarded UPDATE, so no code changes.

### S2. Attempt status doesn't enforce its result fields
`resolve()` always sets `resolved_at`, sets `psp_ref` on success (`psp.rs:132` requires it) and sets `failure_code` on failure. Make the schema say so:
```sql
CHECK ((status IN ('pending', 'unknown')) = (resolved_at IS NULL)),
CHECK (status <> 'succeeded' OR psp_ref IS NOT NULL),
CHECK (status <> 'failed'    OR failure_code IS NOT NULL)
```

### S3. Delivery status doesn't enforce `delivered_at` or a non-negative attempt count
This mirrors the `paid ⇔ paid_at` check on invoices. `workers/webhooks.rs:112` already sets both together.
```sql
CHECK ((status = 'delivered') = (delivered_at IS NOT NULL)),
CHECK (attempts >= 0)
```

### S4. `idempotency_keys` could carry two more backstops
```sql
CHECK (length(key) BETWEEN 1 AND 255),                     -- matches the 400 rule in the API
CHECK (status = 'in_progress' OR payment_attempt_id IS NOT NULL)
```
The second CHECK holds because pre-execution rejections roll back, so every completed key (200, 202 or 402) has an attempt. It's still compatible with Tx1 inserting the key before the attempt, and with the expired-row takeover that resets `payment_attempt_id = NULL`, since both rows are `in_progress`.

## Optional (low value; skip if short on time)

- **`webhook_events.type`**: `CHECK (type IN ('invoice.created', 'invoice.paid', 'invoice.payment_failed'))`. This fits with the text-plus-CHECK choice for status columns.
- **`customers`**: `CHECK (length(name) BETWEEN 1 AND 200)` and `CHECK (length(email) <= 254)`. These match `routes/customers.rs:36–40` and the existing check on `businesses.name`.
- **`webhook_endpoints.url`**: `CHECK (url ~ '^https?://')`.
- **`api_keys.key_hash`**: `CHECK (octet_length(key_hash) = 32)` (SHA-256).
- **`invoice_line_items.position`**: `CHECK (position >= 0)`.
- **Header comment vs code**: the header says PKs are "UUIDv7 generated by the application", but `outbox.rs:31` inserts deliveries with `gen_random_uuid()`, which is v4 and made by the DB. That's harmless, since delivery ids are never cursors. Either generate them in Rust or add "(except webhook_deliveries)" to the comment, so a reviewer doesn't spot the inconsistency.

## Not needed (considered and rejected)

- **An index on `invoices (business_id, customer_id)` for the FK.** Customers are never deleted and there's no list-by-customer endpoint. Mention it under "100x / when we add X".
- **An `expires_at` index for purging idempotency keys.** Expiry is lazy, and a purge job is a production gap.
- **A `(business_id, type, id)` index for `GET /events?type=`.** `(business_id, id)` is enough at this scale; it's a 100x note.
- **A `currency` column.** The brief is single-currency USD. Leaving it out is restraint; mention it under DESIGN §6 or the 100x notes rather than adding it.
- **A trigger for `updated_at`.** Every invoice UPDATE in the code sets `updated_at = now()` (`payments.rs`, `routes/invoices.rs:175`). A trigger would be implicit behaviour for no gain.

## How to apply

The repo hasn't been submitted, so **edit `0001_init.sql` in place** rather than adding `0002_*.sql`. A single clean init migration reads better.

sqlx stores a checksum of each applied migration, so any database that already ran the old file will refuse to start with "migration 1 was previously applied but has been modified". Afterwards, run `docker compose down -v && docker compose up --build` and re-run `cargo test`. All four changes S1–S4 only formalise what the code already does, so the tests should pass unchanged. If one fails, it has found a real bug.

---
---

# Review of PLAN.md (v3)

Compared against the brief (`.docx`), `dodo-requirement.txt`, and the v1 and v2 reviews (the v2 review is kept below; it includes the v1 status table).

## Verdict

**Ready to build.** Every v2 finding (N1–N10 and the small notes) is resolved, and the fixes are the right ones:
- the SET-list trick for `pre_payment_status`
- `BYTEA` response bodies
- startup-checked timing rules
- the seeded receiver secret
- the hard rule to start the docs by the 4.5 h mark

This pass found nothing that breaks correctness. The four items below are clarifications. Fold them in while writing the code for step 5; they don't need another planning round.

## 1. Status of v2 findings

| v2 | Topic | Status |
|---|---|---|
| N1 | Expired-key loop | ✅ `ON CONFLICT … DO UPDATE … WHERE expires_at < now()`, at most 3 loops, then 409 (also added to the status rule) |
| N2 | `RETURNING status_before` | ✅ `SET status='processing', pre_payment_status = status`; stays on PG 16. Durable, so the reconciler can use it after a crash |
| N3 | Demo webhook secret | ✅ Seed registers the receiver with `DEMO_WEBHOOK_SECRET`, shared through compose |
| N4 | Handler vs reconciler response | ✅ Returns the key's stored response; one shared `payment_response` builder. ⚠️ One gap, see R1 |
| N5 | JSONB bytes | ✅ `response_body BYTEA` |
| N6 | Timing rules | ✅ Table of defaults, test values (500/600/1500/200) satisfy both rules, startup check |
| N7 | Host ports | ✅ 55432 / 4000 / 8080 / 9000, localhost URLs in `.env.example` |
| N8 | Duplicate emails | ✅ Constraint dropped; Stripe parity stated |
| N9 | Time risk | ✅ Marked optimistic, assumes cuts 1–3 happen, hard 4.5 h docs rule |
| N10 | review.md committed | ✅ Plan header references it, so commit it as a record of the process |
| Small notes | UTC due_date, 404-vs-409 lookup, connection-refused rationale, per-attempt webhook result handling, 40 s test deadline, cut 5 removing `pre_payment_status` | ✅ All present |

## 2. Remaining clarifications

### R1. Does the reconciler overwrite a stored 202? (medium; pick one rule)
The plan contradicts itself:
- Step 4's shared resolution function says it "completes the idempotency key with the response bytes" whenever its guard matches.
- Step 5 says the stored **202** "stays stable on replay".
- Step 6 says the reconciler stores a response only "if the linked key is still `in_progress`".

If the shared function writes unconditionally, a replay after the reconciler resolves a `tok_timeout` attempt returns 200 instead of the original 202.

**Rule to state:** the key write inside resolution is `UPDATE idempotency_keys SET status='completed', response_code=…, response_body=… WHERE business_id=$1 AND key=$2 AND status='in_progress'`. Then:
- in the timeout case the 202 stays (the handler already completed the key), and
- in the crash case (c), the reconciler's 200 or 402 is stored.

Also say that the handler's `pending → unknown` update and its storing of the 202 happen in **one transaction**. That keeps a crash between the two from leaving an `unknown` attempt with an `in_progress` key. The reconciler would still recover that case, but it's needless.

DESIGN §3(b) gets one sentence from this: "a same-key retry keeps returning 202 by design; the outcome is on the invoice and the webhook".

### R2. Some PSP responses aren't mapped (low)
Step 4 maps `succeeded` and `failed`, and step 5 maps transport errors and 5xx. Not covered:
- a 2xx with an unparseable body
- a 2xx whose status is `processing` (the mock returns the stored charge on a repeated key)
- a 4xx

Rule: **anything that isn't a parseable terminal `succeeded` or `failed` is ambiguous**, so the attempt goes to `unknown` and the request returns 202. That's one line in `psp.rs` and keeps the "never guess" stance consistent.

### R3. The demo key is a public secret (low)
`DEMO_API_KEY` and `DEMO_WEBHOOK_SECRET` will sit in `.env.example` / `docker-compose.yml` in a public repo. That's fine for a demo, but:
- `SEED_DEMO` defaults to **false** in the app. Only compose sets it to true.
- The demo key must match the `sk_<8>_<32 base62>` format, so it goes through the normal auth path rather than a special case.
- The seed is an upsert, so restarts with changed env values don't fail on unique constraints.
- Mention it in one line in README: "demo credentials are for local use only".

### R4. Small notes (optional)
- The demo receiver should **dedupe on `event.id`** and log duplicates (`↺ duplicate evt_…`). The plan tells receivers to do this, and the demo can show it when fail-first-N triggers a retry.
- An `in_progress` key whose attempt the reconciler can't resolve (PSP permanently unreachable) returns 409 indefinitely. That's correct behaviour. Add "alert on attempts in `unknown` longer than X" to §7 production gaps (already listed as alerting on stuck `processing`, so just make it explicit).
- AI_USAGE.md: the review history gives you concrete material. For example, "AI review caught the expired-key loop (N1) and the PG16 `RETURNING` limitation (N2)", or a suggestion you *rejected*, such as choosing Stripe-style non-unique emails over the 409 option. Make sure `notes/ai-log.md` records which choices were yours.

## 3. Next step

Plan is final. Start step 1 (scaffold). The toolchain items in `notes/setup.md` (Rust via rustup, Docker + compose) come first and are outside the time budget.

---
---

# Review of PLAN.md (v2)

Compared against the brief (.docx), `dodo-requirement.txt`, and the v1 review.

## Verdict

v2 addresses every v1 finding, and most of the fixes are good. The payment section now reads like a design someone has thought through, not a sketch. This pass found **3 new issues that would break something** (N1–N3) and a handful of smaller ones. None of them changes the architecture. Fix N1–N4 in the plan, then start building.

---

## 1. Status of v1 findings

| v1 | Topic | Status in v2 |
|---|---|---|
| H1 | Hash must include the invoice | ✅ `sha256("POST" + path + body)` |
| H2 | Guarded resolution | ✅ One shared function, `WHERE status IN ('pending','unknown')`, side effects only if 1 row |
| H3 | Idempotency key lifecycle | ✅ Key insert merged into Tx1; rollback releases the key; hash checked before in_progress. ⚠️ See N1 (expired keys) and N4 (who answers when the reconciler wins) |
| H4 | Tenant-safe customer FK | ✅ Composite FK |
| H5 | Mock `tok_timeout` cancellation | ✅ `tokio::spawn` |
| M1 | Webhook lock during HTTP | ✅ Lease-based claim. Gives at-least-once delivery, which is stated |
| M2 | Logging | ✅ Observability subsection, with redaction |
| M3 | Test harness | ✅ In-process app, short timings, one business per test. ⚠️ See N5 |
| M4 | PSP-404 assumption | ✅ Stated, listed as a production gap |
| M5 | DESIGN.md outline | ✅ About 1,420 words with a budget per section. All 7 sections covered |
| M6 | AI log | ✅ `notes/ai-log.md`, gitignored |
| M7 | Demo bootstrap | ✅ `SEED_DEMO` + `DEMO_API_KEY`. ⚠️ See N3 (the webhook endpoint isn't seeded) |
| Time | | ✅ 5.9 h with an explicit cut order. ⚠️ See N9 |
| DECIDE items | | ✅ All resolved; personal details scrubbed |
| L1–L13 | | ✅ All addressed (zero totals, status-code rule, 1+8 retries, created-on-draft, unknown token, due_date, secrets/SSRF, TTL, replicas, `tok_card_declined` in the video, response shapes up front) |

---

## 2. New issues

### N1. Expired idempotency keys can cause an infinite loop (high)
Step 1 ignores expired rows. Tx1 then runs `INSERT … ON CONFLICT DO NOTHING`, which **does** conflict with the expired row because it's still under the same PK. That returns 0 rows, so "go back to step 1", which ignores the row again, and the loop never ends.

**Fix:** let Tx1 take over expired rows:
```sql
INSERT INTO idempotency_keys (...) VALUES (...)
ON CONFLICT (business_id, key) DO UPDATE
  SET request_hash = EXCLUDED.request_hash, status = 'in_progress',
      response_code = NULL, response_body = NULL, payment_attempt_id = NULL,
      created_at = now(), expires_at = EXCLUDED.expires_at
  WHERE idempotency_keys.expires_at < now()
RETURNING 1;
```
If this returns 0 rows, a live row exists, so going back to step 1 is correct. Also put a bound on the retry loop (for example 3 tries, then 409) so a future bug can't spin forever.

### N2. `RETURNING status_before` isn't valid on Postgres 16 (high, small fix)
`UPDATE … RETURNING` returns the **new** row, so there's no pre-update status to return. Two options:
- **Postgres 18**, which supports `RETURNING old.status`. Bump the image to `postgres:18`. This is the cleanest option and keeps the mechanism a single conditional UPDATE.
- **Postgres 16**: copy the old value in the SET list, because right-hand sides see the old row: add `invoices.pre_payment_status` and use `SET status='processing', pre_payment_status = status … RETURNING pre_payment_status`. Then `payment_attempts.invoice_status_before` isn't needed.

Avoid adding a `SELECT … FOR UPDATE` first. It works, but your DESIGN §3(a) answer would then need to name a row lock as the mechanism, not the conditional update alone.

### N3. The demo can't verify webhook signatures as planned (high for the demo)
§6 says the endpoint secret is "generated server-side, returned once". §6 and §13 also have the webhook-receiver verifying with "the shared secret". The receiver container can't know a secret that's generated at runtime. Nothing in the plan registers the receiver's endpoint either, so `docker compose up` alone delivers no webhooks, which breaks the "no manual steps" demo.

**Fix:** have `SEED_DEMO=true` also register `http://webhook-receiver:PORT/webhooks` for the demo business, with its secret taken from `DEMO_WEBHOOK_SECRET`. Compose passes the same env var to both containers. The normal `POST /webhook_endpoints` path still generates secrets server-side.

### N4. Handler and reconciler can both answer for the same key (medium)
In step 4, if resolution updates 0 rows, the plan says to "read and return the current state". The reconciler may already have stored a final response on the key. The handler should then **return the key's stored response** rather than building a new one. Otherwise the first response and the replay can differ, and the idempotency test (byte-identical) becomes flaky.

Related: the reconciler has to produce exactly the bodies the handler would (200 or 402). Put the response construction in one function used by both.

### N5. JSONB doesn't preserve bytes, so "byte-for-byte" replay won't hold (medium)
`response_body JSONB` normalises key order and whitespace. The first response (serialized by serde) and the replay (read back from JSONB) can differ byte-wise, and test 2 asserts byte-identical responses.

**Fix:** store `response_body BYTEA` (or `TEXT`) holding the exact bytes sent. Alternatively, always build the first response from the stored value, but that's more fragile.

### N6. Timing settings aren't checked against each other (medium)
Correctness relies on two relationships between settings that the plan doesn't state:
- `PSP_NOT_FOUND_GRACE_MS` must be well above `PSP_TIMEOUT_MS`. A 404 is only meaningful once our POST can no longer be in flight.
- `RECONCILER_MIN_AGE_MS` should be at least `PSP_TIMEOUT_MS`. If it's lower, the reconciler polls attempts the handler is still waiting on. Guarded resolution keeps that safe, but it's wasted work and confusing in the logs. The test config (min age 300 ms against a 500 ms timeout) violates this.

**Fix:** document the defaults (for example 5 s timeout, 10 s min age, 60 s grace, 5 s interval), check `grace > timeout` at startup and refuse to start if it fails, and use test values that respect both rules (for example 500 / 600 / 1500 / 200 ms).

### N7. The test harness needs host-reachable ports (low)
Tests run on the host but talk to compose's `db` and `mock-psp`. Both services must publish ports (`5432`, and for example `4000`), and the test config must use `localhost` URLs rather than compose service names. Put both in `.env.example` and the README.

### N8. Duplicate customer emails need a defined response (low)
`unique(business_id, email)` turns a duplicate create into a DB error. Map it to **409 `customer_exists`** in the error envelope and the OpenAPI spec. Alternatively, drop the constraint, since the brief doesn't require uniqueness. Either way, decide before writing the handler.

### N9. Estimates shrank while the scope grew (risk)
Scope added in v2: tracing, demo seeding, lease-based delivery, a unit-test table, OpenAPI written first, configurable timings. Over the same period, the per-step estimates dropped (390 → 355 min). The 5.9 h figure is optimistic. Plan on cut items 1–3 (`GET /events`, `POST /api_keys`, fail-first-N) being likely, and start the docs step no later than the 4.5 h mark, since DESIGN.md is the primary deliverable.

### N10. review.md will be committed (low)
`.gitignore` covers `notes/` but not `review.md`. Either move this file into `notes/` or commit it on purpose as evidence of process. If you commit it, it pairs well with AI_USAGE.md ("AI review found H1–H5; I accepted X, rejected Y because…").

---

## 3. Small notes (no action needed unless trivial)

- §2: say `due_date ≥ today` is evaluated **in UTC**.
- §4 step 2: a 0-row UPDATE doesn't tell you whether it should be 404 or 409. Follow it with one `SELECT status` scoped by business.
- §4 step 5: "connection refused" means the request never reached the PSP, so it could fail right away. Treating it as `unknown` is still safe and simpler. Keep that, but say it was deliberate.
- §6: write down the per-attempt result update: success marks `delivered`; failure sets `attempts += 1` and `next_attempt_at = now() + schedule[attempts] ± jitter`, or `dead`. A crash-lease retry doesn't increment `attempts`. Say that too.
- §9: test 3 with `tok_timeout` takes at least 30 s because the mock's delay is fixed. That's fine, but give the deadline some slack (about 40 s).
- §12 cut item 5 (`uncollectible`): cutting it also removes the "return to previous state" logic, so N2 goes away. Keep it in mind if time gets tight.

---

## 4. Suggested edits to PLAN.md

1. §4 Tx1: `ON CONFLICT … DO UPDATE … WHERE expires_at < now()`, with a bounded retry loop (N1).
2. §1/§4: `postgres:18` with `RETURNING old.status`, or a `pre_payment_status` column set in the SET list (N2).
3. §6/§7: seed the receiver endpoint with `DEMO_WEBHOOK_SECRET` shared through compose (N3).
4. §4 step 4: when resolution updates 0 rows, return the key's stored response. Use one response builder for both paths (N4).
5. §2: `response_body BYTEA` (N5).
6. §1/§9: list the default timings, the two ordering rules with a startup check, and test values that respect them (N6).
7. §9/README: published ports and localhost URLs for tests (N7). §8: 409 `customer_exists` (N8).
