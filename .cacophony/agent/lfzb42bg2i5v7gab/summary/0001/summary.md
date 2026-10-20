# Session summary — provider-truth membership proof and the v1 receipt contract

## Goal

Make provider state, not receipt fields, the authority on whether publication, join, admission or merge happened, and pin the join receipt wire format so a serialization change can never again refuse every completion. Cara gains a read-only `membership-proof` verdict and a CI-enforced, versioned receipt contract shared with Cacophony.

## Bead(s)

- `bd-78d4aa` — Make provider state authoritative for join/admission/merge; demote receipts to advisory
- (parent: `bd-acf9ca` — Make Cara admission and merge succeed reliably: provider truth over receipt ceremony)

## Before state

- Failing tests: none.
- Consumers could only learn a join's outcome from receipt fields. On 2026-10-20 Caco refused seven enrolled Cacophony PRs, and Caravan PR #268 itself, because Cara emits `force_intent: "none"` and the deployed Caco allowlist expected `"absent"`.
- No Caravan-owned receipt schema or fixtures existed; the only real-producer fixtures lived in Cacophony.

## After state

- Failing tests: none (`cargo test --all`, clippy `-D warnings`, fmt clean).
- `cara membership-proof --pr N --expected-head OID [--expected-base-ref REF]` and the `membership_proof` MCP tool make four light provider reads and return enrolled, merged, unjoined, closed_unmerged or inconsistent with one `succeeded` answer. A different head, a fork, base drift or durable `caravan-force` are typed mismatches and never count as success.
- `docs/receipts/README.md`, `docs/receipts/join-receipt.v1.schema.json` and `tests/fixtures/receipts/join-v1/` define the v1 contract. Contract tests prove byte-exact round-trip, the producer hash recipe, the current producer's wire shape and the `force_intent` spelling (emit `none`/`preserved`, accept `absent`, reject anything else).

## Diff summary

- Code/content commits: 30c7b0f (final landed squash SHA comes from the reintegration receipt).
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: src/membership_proof.rs (new), src/membership/tests/receipt_contract.rs (new), src/membership/tests.rs, src/read.rs, src/lib.rs, src/main.rs, docs/receipts/*, tests/fixtures/receipts/join-v1/*.
- Tests: +10 (6 verdict tests, 4 receipt contract tests).
- Behavioural delta: a new read-only command and MCP tool; no existing output changed.

## Operator-takeaway

Caco should call `membership-proof` with the published head whenever its own receipt check fails, and act on `succeeded` instead of dead-lettering. Today's dead letters (including rdl-01a159de for PR #268, which merged anyway) were exactly that case.
