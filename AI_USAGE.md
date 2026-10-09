# AI Usage

## Tools and what they did

- **Claude Code (Claude Opus 5.5, in VS Code).** This was the primary tool, used throughout:
  - **Planning.** Read the brief and drafted `PLAN.md`: architecture, schema, state machine, payment flow, failure-mode answers, test plan, and time budget.
  - **Code.** Wrote the implementation step by step, one commit per step: scaffold, auth, invoices and state machine, mock PSP, payments and reconciler, webhooks, tests.
  - **Testing.** Ran the unit and integration tests and a manual end-to-end run (curl, webhook receiver logs). Docker wasn't installed yet, so it started a throwaway user-owned Postgres for testing.
  - **Docs.** Drafted `DESIGN.md`, the OpenAPI spec and the README.
  - **Commits.** Each commit carries a `Co-Authored-By: Claude` trailer.
  - **Demo prep.** Wrote private notes for the video (a demo runbook and an architecture study guide, not in the repo), reviewed a draft recording from extracted frames and a local transcript, and helped tune the microphone setup. The explanations in the video are my own.
- **A second, separate Claude session**, used as a reviewer. I wrote `review.md` together with it: I reviewed the plan, schema and API spec myself and used that session to challenge them. I then decided which findings to act on.

## How the plan was reviewed

`PLAN.md` went through three review passes before any code was written, and the schema and API spec were reviewed again after implementation. All of it is recorded in [`review.md`](review.md). Each pass found real problems in AI-written work, which were fixed before moving on.

## Three decisions I made myself

**1. Review the plan before writing any code.**
- *AI proposal:* Claude drafted a full plan and was ready to start implementing.
- *My choice:* no code until the plan had been through review. I ran three passes with a second Claude session as the reviewer, and decided which findings to accept or reject.
- *Why:* payment bugs are cheapest to fix on paper. The passes found an infinite loop on expired idempotency keys, a Postgres `RETURNING` feature that doesn't exist on PG16, replay bodies that wouldn't be byte-identical, and a contradiction that would have broken 202 replays. All of them were fixed in the plan, before they became code.
- *Rejected findings:* I didn't take every suggestion. I kept Postgres 16 with a `pre_payment_status` column rather than moving to PG18, because the reconciler needs the prior status stored durably anyway. I also kept customer email non-unique (as in Stripe) rather than returning 409 on duplicates.

**2. Make the database enforce the invariants, not just the code.**
- *AI proposal:* the schema gave `payment_attempts` separate foreign keys for `invoice_id` and `business_id`, and relied on the code for most status rules.
- *My choice:* after reviewing the schema, I required:
  - a composite tenant FK `(business_id, invoice_id) → invoices(business_id, id)`, the same pattern already used for customers;
  - CHECK constraints: an attempt is unresolved exactly while `pending`/`unknown`; `succeeded ⇒ psp_ref`; `failed ⇒ failure_code`; `delivered ⇔ delivered_at`; `attempts ≥ 0`; idempotency key length 1–255; a completed key must point at a payment attempt.
- *Why:* this is a payments system. Even if the code has a bug, the database should refuse a cross-tenant attempt or a "succeeded" charge with no PSP reference. I verified that each constraint rejects a bad row and accepts a valid one.

**3. A strict API contract.**
- *AI proposal:* lenient request handling. Unknown query parameters were silently ignored, a wrong HTTP method got an empty 405, and a missing content type or oversized body came back as a generic 400.
- *My choice:* after reviewing the API spec against the server:
  - unknown query parameters return 400;
  - `?state=` is accepted as an alias for `?status=` (the brief says "filterable by state");
  - 405, 413 and 415 come back in the same error envelope;
  - 422 stays reserved for idempotency-key reuse only;
  - the OpenAPI spec was corrected wherever it disagreed with the server.
- *Why:* silently ignoring `?state=paid` returns *every* invoice with a 200, which is a correctness bug for any client. Making the parsing strict also exposed a real pagination bug (`?limit=` returned 400 on two list endpoints), now covered by `tests/api.rs`.

## What the AI got wrong

These are documented in `review.md` and the commit history:

1. **Expired idempotency keys would loop forever (review N1).** The AI-drafted plan skipped expired key rows in the lookup but inserted with `ON CONFLICT DO NOTHING`. That still conflicts with the expired row, so the code would loop. Fixed: take over expired rows with `ON CONFLICT … DO UPDATE … WHERE expires_at < now()`, with the loop capped at 3 tries.
2. **A Postgres feature that doesn't exist (review N2).** The plan used `UPDATE … RETURNING status_before`. Before Postgres 18, `RETURNING` only sees the new row. Fixed: copy the old status in the SET list (`pre_payment_status = status`).
3. **Byte-identical replay that wasn't (review N5).** Response bodies were planned as `JSONB`, which reorders keys. Fixed: `BYTEA`.
4. **A self-contradiction that would have broken replay (review R1).** The shared resolution function would have overwritten a stored 202 with 200 after reconciliation. Fixed: only complete keys that are still `in_progress`.
5. **A mock PSP that couldn't demo the timeout (review H5).** The first plan slept inside the request handler. hyper drops that future when the client disconnects, so the `tok_timeout` charge would never complete. Fixed: settle the charge in a spawned task. Verified by hand: a client timed out at 1 s and the charge still settled at about 30 s.
6. **Broken pagination that no test caught.** Claude wrote the invoice and event list queries with `#[serde(flatten)]`. With URL-encoded query strings this makes numbers fail to parse, so `GET /invoices?limit=5` returned 400. It came to light during a review of the API spec, which also found the other gaps fixed in the same commit:
   - unknown query parameters were silently ignored, so `?state=paid` returned an unfiltered list;
   - a wrong HTTP method got an empty 405;
   - a missing content type got 400 instead of 415, and an oversized body 400 instead of 413;
   - the spec and the server disagreed in places.

   Fixed with strict query structs, and `tests/api.rs` now guards it.
7. **Mistakes caught during coding:**
   - A first draft of the API-key insert used an upsert on the key prefix. A random prefix collision would then have silently taken over another business's key. Changed to a plain insert; only the demo seed upserts.
   - An invalid `$ref` in the OpenAPI spec.

**How correctness was checked:**
- The concurrency test was run 10 times in a row.
- After the test runs, SQL checks confirmed no invoices stuck in `processing`, no unresolved attempts, no idempotency keys left `in_progress`, and no paid invoice without exactly one succeeded attempt and one `invoice.paid` event.

**What I corrected myself:**
- The schema and API findings in decisions 2 and 3 came from my own reviews of the AI-written code. That's where the pagination bug (item 6 above) surfaced.
- Docker initially failed to install on my machine. I fixed that rather than skipping the requirement, then verified `docker compose up` from a fresh clone and ran the README curls as written.
