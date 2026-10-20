# Session summary — Caravan v0.0.144 release bump for the reliability epic

## Goal

Cut the Caravan release that carries the whole provider-truth reliability epic, so Cacophony can adopt a single new Cara pin. This chunk bumps the version; once it merges, the `v0.0.144` tag on the merge commit triggers the release workflow that builds the assets.

## Bead(s)

- `bd-acf9ca` — Make Cara admission and merge succeed reliably: provider truth over receipt ceremony (epic; all six children are closed)

## Before state

- Failing tests: none.
- main 4009eb2 contained all six fixes (PRs #268, #269, #270, #271, #272, #275) under version 0.0.143. The latest release was v0.0.143 (980f673).

## After state

- Failing tests: none (`cargo test --lib`; the binary reports `cara 0.0.144`).
- Cargo.toml and Cargo.lock are at 0.0.144. After merge, an annotated `v0.0.144` tag on the merge commit starts `.github/workflows/release.yml`.

## Diff summary

- Code/content commits: 9a466d3 (final landed squash SHA comes from the reintegration receipt).
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: Cargo.toml, Cargo.lock.
- Tests: none added; the release contents were validated in their own PRs.
- Behavioural delta: none in this commit. The release adds typed App refusals, `membership-proof`, the v1 receipt contract, `cara-admission-unevaluated`, `failures`/`failure-classify`, Stack quarantine, and queue-owned drift repair.

## Operator-takeaway

Cacophony's adoption includes the `cara-readiness.yml` change from /tmp/lfz-readiness/cara-readiness.diff (held by the Cacophony controller) and the Caco receipt changes (bd-282421/bd-5666aa). On repositories with `mutations_opt_in`, it also turns on queue-owned drift repair; `stack_rollout.manual_drift_repair: true` keeps the old owner-only policy.
