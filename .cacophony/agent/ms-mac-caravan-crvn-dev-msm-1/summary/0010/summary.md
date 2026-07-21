# Session summary — scoped force-head reconciliation

## Goal

Make an exact `caravan-force` head reach Cara's audited force policy when provider-native auto-merge is disabled, without weakening fleet graph safety or allowing an unrelated repairable force-head invariant to block a targeted caravan sync.

## Bead(s)

- `bd-1f3ff4` — Cara force heads are rejected by the auto-merge invariant before force handling.
- Follow-up filed: `bd-e89b4b` — restore exact force intent when physical rewrite publication fails.

## Before state

- A force-labelled head with native auto-merge disabled appeared as `auto_merge_invariant` before CI observation and force validation when another invalid force caravan was encountered first.
- Targeted sync validated the whole fleet both before execution and after final rediscovery, so an unrelated exact force head could block the selected caravan.
- Cara already had the safe mutation primitive: a generation-bound audit comment followed by direct administrator squash, without enabling native auto-merge.
- Live Cacophony evidence involved force-labelled #2079→#2080; the separate physical rewrite path also removed #2080's force intent before a later publication failure.

## After state

- Initial normal and physical-rebase graph validation defer only an unselected, open, non-draft, force-labelled head whose native auto-merge is disabled and only when `force_merge: true`.
- Selected force heads remain deterministically repairable and reach exact CI, default-head, compatibility, label, permission, audit-comment, and administrator-squash validation.
- Final rediscovery accepts only an unobserved unrelated force-head gap (or a forced successor intentionally deferred without an attempt); selected non-force drift and all structural, ordinary, enabled, or non-squash auto-merge problems still fail closed.
- The direct force path never arms native auto-merge. Post-merge replay is provider-write-free.
- Required hosted CI remains authoritative for the final workspace test/lint gate.

## Diff summary

- Code/content commit: `7d4c60c` (`bd-1f3ff4: scope force head graph repair`); final landed squash SHA will come from the reintegration receipt.
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: `SPEC.md`, `src/sync.rs`, `src/sync/plan.rs`, `src/sync/tests.rs`.
- Tests: +3 regression fixtures, plus the existing one-shot force-chain fixture strengthened to start with head auto-merge disabled.
- Focused validation: the three new exact sync tests, existing stale-force-label behavior, existing one-shot child advancement, `cargo fmt --check`, `cargo check --quiet`, and `git diff --check` passed.
- Behavioural delta: a safe disabled force head can reach direct audited administrator squash even when another force caravan has the same repairable invariant; ordinary graph errors remain global blockers.

## Operator-takeaway

The force primitive itself was already safe; decision ordering and completion scoping prevented Cara from reaching it. This change opens only the exact disabled force-head state and leaves every broader graph invariant strict. Physical rewrite intent restoration is deliberately separated into `bd-e89b4b` because its publication certainty contract is materially different.
