# Session summary — Historical native required-check rejection

## Goal

Determine whether the landed effective-policy fix explains the historical Stack4058 rejection, without repeating its failed operation or inventing a live recovery route.

## Bead(s)

- `bd-db8384` — Diagnose native prefix admission-check rejection despite exact-head validation.

## Before state

The original prefix had successful App15368 head checks but a native provider rejection. The work subsequently landed. Prior board hypotheses included synthetic identity, dispatch association and branch policy, none proven. bd-e81848 had since corrected local landing-target/App/head eligibility.

## After state

A historical-shaped regression demonstrates the recorded heads qualify locally for PR and dispatch lineage with real matching successful checks. Wrong-App, wrong-head, cancelled and failed checks do not qualify. It passed in a focused test run. The diagnosis explicitly does not claim a provider reproduction or root cause: the provider's evaluated identity/policy is unavailable. No policy, workflow, native transaction or live queue mutation was added.

## Diff summary

- Commit: `5292860`.
- `src/required_runs/effective_policy_tests.rs`: one historical-head classifier test covering both heads/events and negative controls.
- `docs/native-prefix-required-check-diagnosis.md`: evidence limits, e81848 scope, unsupported root-selector boundary and terminal operation disposition.
- Formatting and diff whitespace checks passed; broad validation belongs to hosted CI.
- Earlier unrelated lifecycle cleanup preserved both histories and closed duplicate PR234 only after matching its patch to merged PR232. No source from bd647b33 is republished here.

## Operator-takeaway

Local eligibility now checks the correct required identity, but that is not evidence of which identity GitHub's native merge service enforced during an old rejection. Preserve that uncertainty rather than manufacture a workaround or replay a terminal operation.
