# Follow-up summary — exact Saloon PR root admission

## Live regression

Every dashboard “New caravan” action sent `{ pr: N }`, but `CreateInput` had no `pr` field. Serde ignored the extra value, `membership::new` always passed `candidate_pr=None`, and the long-lived web server checkout was on `main`. Source-provenance preflight therefore bound `main` to itself and returned a misleading `join_empty_source_noop` rather than admitting the selected Saloon PR.

## Fix

- `CreateInput` now has exact `pr: Option<u64>` with `--pr`/`--create-pr` mutual exclusion, matching join/rejoin.
- `new` and `renew` forward the exact remote PR into checkout-free membership execution.
- Dashboard `new` action JSON now deserializes and proves `pr=42` in the typed action fixture.
- CLI parser fixture proves `cara new --pr 43` and rejects `--pr` with `--create-pr`.
- Physical membership validates operation shape before source provenance:
  - without `--pr`, `--create-pr`, or one unique current PR, return `current_pr_not_found` with Saloon `new --pr` guidance;
  - `--create-pr` from default branch returns `create_pr_on_default_branch`;
  - default branch is never interpreted as an empty membership source.
- README, SPEC and embedded help now document checkout-free `new/renew --pr N`.

## Validation

- Focused source-shape, CLI parser, typed web action and v1 parity tests green.
- Strict all-target/all-feature Clippy/rustfmt green.
- Hosted required CI remains the broad delivery gate per active project policy.

## Commit

- `a7830fe`.
