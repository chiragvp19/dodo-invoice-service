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
