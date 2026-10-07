# AI Usage

## Tools and what they did

- **Claude Code (Claude Opus 5.5, in VS Code).** This was the primary tool, used throughout:
  - **Planning.** Read the brief and drafted `PLAN.md`: architecture, schema, state machine, payment flow, failure-mode answers, test plan, and time budget.
  - **Code.** Wrote the implementation step by step, one commit per step: scaffold, auth, invoices and state machine, mock PSP, payments and reconciler, webhooks, tests.
  - **Testing.** Ran the unit and integration tests and a manual end-to-end run (curl, webhook receiver logs). Docker wasn't installed yet, so it started a throwaway user-owned Postgres for testing.
  - **Docs.** Drafted `DESIGN.md`, the OpenAPI spec and the README.
  - **Commits.** Each commit carries a `Co-Authored-By: Claude` trailer.
- **TODO:** who or what wrote `review.md`? Was it you, another AI tool (name it), or both? List any other tools you used.

## How the plan was reviewed

`PLAN.md` went through three review passes before any code was written. They are recorded in [`review.md`](review.md). Each pass found real problems in the AI-drafted plan, and the plan was revised before implementation.

## Three decisions I made myself

> **TODO:** rewrite this section in your own words. For each decision, say what the AI proposed (if anything), what you chose, and why. Pick decisions you can defend on video without notes. Candidates from this project:
>
> - **Payable `uncollectible`.** Claude recommended allowing it, Stripe-style. Did you agree, and why?
> - **Postgres 16 with a `pre_payment_status` column vs Postgres 18 with `RETURNING old.*`.** The review preferred PG18; the PG16 column was chosen because the reconciler needs the prior state stored durably anyway.
> - **Customer email.** Non-unique, as in Stripe, rather than a unique constraint with a 409 on duplicates.
> - **Stable 202 replay vs rewriting it.** The stored 202 is never overwritten, even after the reconciler settles the payment.
> - **No lease for the reconciler.** Guarded resolution already makes duplicate runs harmless.
> - **Explicit `processing` state** instead of keeping the invoice `open` while a payment attempt is pending.
>
> Remove any of these you didn't actually decide.

## What the AI got wrong

These are documented in `review.md` and the commit history:

1. **Expired idempotency keys would loop forever (review N1).** The AI-drafted plan skipped expired key rows in the lookup but inserted with `ON CONFLICT DO NOTHING`. That still conflicts with the expired row, so the code would loop. Fixed: take over expired rows with `ON CONFLICT … DO UPDATE … WHERE expires_at < now()`, with the loop capped at 3 tries.
2. **A Postgres feature that doesn't exist (review N2).** The plan used `UPDATE … RETURNING status_before`. Before Postgres 18, `RETURNING` only sees the new row. Fixed: copy the old status in the SET list (`pre_payment_status = status`).
3. **Byte-identical replay that wasn't (review N5).** Response bodies were planned as `JSONB`, which reorders keys. Fixed: `BYTEA`.
4. **A self-contradiction that would have broken replay (review R1).** The shared resolution function would have overwritten a stored 202 with 200 after reconciliation. Fixed: only complete keys that are still `in_progress`.
5. **A mock PSP that couldn't demo the timeout (review H5).** The first plan slept inside the request handler. hyper drops that future when the client disconnects, so the `tok_timeout` charge would never complete. Fixed: settle the charge in a spawned task. Verified by hand: a client timed out at 1 s and the charge still settled at about 30 s.
6. **Mistakes caught during coding:**
   - A first draft of the API-key insert used an upsert on the key prefix. A random prefix collision would then have silently taken over another business's key. Changed to a plain insert; only the demo seed upserts.
   - An invalid `$ref` in the OpenAPI spec.

**How correctness was checked:**
- The concurrency test was run 10 times in a row.
- After the test runs, SQL checks confirmed no invoices stuck in `processing`, no unresolved attempts, no idempotency keys left `in_progress`, and no paid invoice without exactly one succeeded attempt and one `invoice.paid` event.

**TODO:** add anything you corrected yourself while reviewing the code or recording the video.
