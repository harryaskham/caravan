# Caller-reviewed admission (schema 1)

`check --pr N`, `new --pr N` and `join --pr N` accept one JSON string after
`--expected-admission`. MCP accepts the same typed object as `expected_admission`.
This implements the Cacophony caller wire inspected at
`1d94cb3f4bb8e4ff306820968a3add7a53be99e0` in
`crates/caco-daemon/src/cara_join/interactive.rs` and `cara_join.rs`.

```json
{
  "schema_version": 1,
  "repository": "owner/repo",
  "pr": 42,
  "pr_url": "https://github.com/owner/repo/pull/42",
  "head_ref": "source/ref",
  "head_oid": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
  "base_ref": "main",
  "base_oid": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
  "default_ref": "main",
  "default_oid": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
  "unjoined": true
}
```

These are illustrative identities, **not an admission command to execute**.
There is no Cara attestation ID, token, generation, or review-key parameter.
The caller remains responsible for issuing its own authority; this object is a
lease to enforce, never permission to bypass ordinary engine policy.

## Input and output

- Require schema 1, positive exact `--pr`, all identity fields, full 40-hex OIDs,
  bounded nonempty strings and `unjoined: true`. Unknown object fields fail
  closed. CLI parsing and MCP domain validation use the same type and checks.
- Guarded creation, renewal and rejoin are refused. They cannot reinterpret a
  reviewed **existing, unjoined** source as a different operation.
- Compare OIDs by identity without rewriting the supplied strings. JSON object
  order does not matter, but successful echoes preserve every value, including
  uppercase OIDs.
- CHECK data contains `expected_admission` and
  `expected_admission_verified: true` only on success. Ordinary eligibility,
  candidate selection, target choice, required checks and failure codes remain.
- Successful mutation places both fields **inside `join_receipt`**, not merely
  in its outer data. The receipt still binds candidate/source, predecessor,
  result, ancestry, durable membership and configuration, and its hash includes
  the guard. No-force intent serializes as `"none"`, matching Caco; the older
  `"absent"` spelling remains accepted on deserialization.
- Guarded immutable admission proves exact predecessor ancestry with the
  provider commit comparison before writing. Unknown/diverged ancestry is a
  refusal, not a success inferred from clean mergeability. Physical admission
  keeps its existing source/range/tree/rebase proof. Existing native and
  checkout-free Caravan restrictions are unchanged.

## Effect boundaries

1. **Read:** validate the requested binding against complete discovered facts,
   then re-read repository/default identity, branch heads and the candidate before
   returning the live check. Active/evicted/force-labelled or represented native
   membership cannot be called unjoined; truncated native inventory fails closed.
2. **Writer:** retain the existing writer lock/fence and ordinary preflight.
   Validate the same reviewed facts again in the transaction. Unsupported
   provider capabilities fail closed, without affecting unguarded callers.
3. **Optional source publication:** run the guard after slow Git preparation and
   permission preflight, immediately before the existing fenced, leased push.
   Only that operation's exact rewrite receipt can advance the bound source head.
   It does not relax topology checks or introduce a non-force rewrite policy.
4. **Membership:** retain the approved source head and expected base separately
   from provider after-state. Recheck the original base/default leases and exact
   evolving PR before each write. Only the requested retarget and own enrollment
   may advance those expected facts. A provider after-state containing an
   unrequested source change cannot silently authorize its successor. Existing
   adapter preconditions continue to protect each provider command.
5. **Native continuation:** recheck before the native Stack convergence entry;
   existing exact native plan, pending checkpoint and writer fences still own its
   constituent operations. This feature creates no second queue writer.

These are optimistic, fail-closed checks under the existing writer transaction,
not a claim that GitHub offers one atomic cross-resource transaction over PR,
branch refs and native Stacks. A concurrent change or response loss after an
accepted effect is preserved and reported as partial/possible, not rewritten as
zero-effect success.

## Refusal and uncertainty

`expected_admission_invalid`, `expected_admission_operation_unsupported`,
`expected_admission_drift`, `expected_admission_unavailable` and
`expected_admission_ancestry_unproved` refuse the unproved step. A pure initial
lease refusal reports no provider mutation. A completed source push overrides
that no-effect description and retains its rebase receipt.

After completed or possible membership effects, `expected_admission_partial`
retains the operation/provider receipts and reports `provider_mutation: possible`
with no automatic retry. Guarded operations do **not** run the ordinary rollback
and then suggest unconstrained replay. The original source rewrite and any
completed rewrite-comment receipt survive subsequent membership refusal. Native
partial-effect checkpoints and errors remain authoritative.

Do not drop the flag, manufacture a verified echo, reuse a historical attestation,
or treat this source change as deployment or live consumer acceptance. Old
unsupported-flag operations are not replayed by this feature.

## Hermetic evidence

`cargo test --lib expected_admission --no-default-features` covers:

| ID | Executed boundary |
|---|---|
| EA-01 | One-JSON CLI parsing for check/new/join and typed MCP decoding |
| EA-02 | Exact case-preserving CHECK and nested root/join receipt echoes |
| EA-03 | Every binding field changed, malformed/missing/unknown fields, unsupported modes |
| EA-04 | Check-to-writer source/base/default/repository/URL/state/membership races, zero writes |
| EA-05 | Drift after transaction preflight but before the first provider effect |
| EA-06 | Real local bare-Git publication barrier: refusal preserves source ref; verified continuation pushes |
| EA-07 | Only the exact owned rewrite receipt can advance the reviewed source |
| EA-08 | Immutable ancestry unknown refuses; successful root/join proofs remain exact |
| EA-09 | Partial membership, lost response and unrequested provider source successor retain effects without rollback/replay |

These are isolated fixtures, not tests against live Caco admissions. Hosted CI
owns final repository-wide compilation, lint and regression qualification.
