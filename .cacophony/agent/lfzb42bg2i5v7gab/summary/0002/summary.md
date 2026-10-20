# Session summary — readiness gate converges on label/membership races

## Goal

Stop a race between Cara joining a PR and the consumer's readiness reporter from leaving an enrolled member looking failed. The readiness step keeps its strict label checks but reports the race (and drafts) as an unevaluated run, and Cara treats that run as unevaluated and reruns the member's exact gate once.

## Bead(s)

- `bd-06e059` — Readiness gate converges instead of failing on label/membership races
- (parent: `bd-acf9ca` — Make Cara admission and merge succeed reliably: provider truth over receipt ceremony)

## Before state

- Failing tests: none.
- Cacophony PR4575's readiness step exited 1 at `deferred_label_absent`: its observation said deferred while its strict fresh provider read already showed the member label. With no deferral receipt in the log, Cara counted the red gate as a real failure on a member.
- Drafts exited 1 at `event_action_and_draft`, another red that was not a verdict.

## After state

- Failing tests: none (`cargo test --all`, clippy `-D warnings`, fmt clean).
- Cara parses `cara-admission-unevaluated pr= head= base= reason=membership_changed|draft exit=78` with the same exact-tuple proof as the deferral receipt; an enrolled member's gate is rerun once and an unjoined candidate's never refuses admission. The historical deferral wire is unchanged.
- `examples/workflows/cara-readiness-membership.sh` is the reference readiness step, with the strict checks kept and the race and drafts ending as unevaluated exit 78. Identity, provider and reporting failures still exit 1.

## Diff summary

- Code/content commits: 2ca105b (final landed squash SHA comes from the reintegration receipt).
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: src/ci.rs, src/sync/tests/deferred_admission.rs, examples/workflows/cara-readiness-membership.sh (new), flake.nix (jq in the devshell), README.md.
- Tests: +8 (receipt parser, six bash/jq runs of the real step for label-before-gate, label-after-gate, label removed mid-run, draft then ready and identity drift, and a sync-level convergence test).
- Behavioural delta: a mid-run membership change or a draft no longer leaves a red gate on a PR.

## Operator-takeaway

Cacophony's `.github/workflows/cara-readiness.yml` must adopt the reference step together with the Cara release that parses the new receipt; adopting the workflow alone against Cara 0.0.143 would leave the race red exactly as before.
