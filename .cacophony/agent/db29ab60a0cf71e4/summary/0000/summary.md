# Session summary — Unproven readiness receipts in Cara 0.0.145

## Goal

Recognize readiness reports whose membership observation is unproven without fabricating a membership race, treating the run as green, or weakening the exact-generation admission checks. Prepare a versioned correction that Cacophony can adopt after normal merge and release verification.

## Bead(s)

- `bd-0d5e4b` — Recognize run_unproven and unknown_membership unevaluated readiness reasons.
- Related consumer work: Cacophony `bd-b172e8`; its controller confirmed the initial adoption remains on 0.0.144 and a subsequent task will adopt the corrective release.

## Before state

- Starting source: `66e0d0a`, Caravan 0.0.144.
- The unevaluated-reason parser accepted only `membership_changed` and `draft`. Valid `run_unproven` and `unknown_membership` markers were discarded, allowing the existing failure classification to treat these gate runs as source failures.
- Exact tuple, native App/job, bounded log, and actual process exit validation already existed and needed to remain intact.

## After state

- `DeferredAdmissionReason` preserves both new reasons as distinct snake-case wire values. Existing reasons and historical unjoined serialization remain unchanged.
- Proven gate-only reports remain waiting/unevaluated, never membership success or green CI. Independently enrolled members refresh the exact gate once and reuse an active run.
- Wrong PR/head/base/job/App, incomplete evidence, exit 1, duplicate/malformed/truncated markers, and independent source failures retain their existing vetoes.
- Package and lockfile versions are 0.0.145; both Nix packages derive their version from the manifest. Release notes describe the post-merge continuation.
- Pinned devshell validation: 16 CI diagnostic/parser/readiness tests, 8 deferred-admission sync tests, and 10 source-precedence tests passed (34 total). `cargo fmt --all --check`, `cargo clippy --lib --tests -- -D warnings`, version consistency, and `git diff --check` passed.
- No full local final-CI duplicate was run; hosted required CI remains the queue's final gate. No tag or release assets were published in this session.

## Diff summary

- Code/content commit: `be4b97e` — `bd-0d5e4b: recognize unproven readiness reasons in v0.0.145`. The final landed squash SHA comes from the queue receipt.
- Files: `src/ci.rs`, `src/sync/tests/deferred_admission.rs`, `Cargo.toml`, `Cargo.lock`, `README.md`, `docs/gated-ci.md`, and `docs/release/v0.0.145-rollout.md`.
- Tests: +2 / -0; existing parser, sync recovery, identity, App, and source-failure matrices were expanded to cover both reasons.
- Production behavior changes only the typed reason allowlist; no validation guard, membership writer, CI trigger, or queue policy changed.
- Initial fixture assumptions were corrected during development: absent provider associations correctly classify stale, and exact gate reruns are recorded as `RerunChecks`. Final assertions retain those distinctions.
- The host's unpinned Rust 1.97 Clippy reported unrelated newer lints; the repository-pinned Rust 1.91 devshell passed strictly without suppressions or unrelated source changes.
- Session reflection found no additional actionable friction worth filing as a draft.

## Operator-takeaway

Unknown membership is not a membership race or success. Cara 0.0.145 recognizes these exact, independently validated readiness receipts as unevaluated while preserving source-failure and identity vetoes. The Cacophony controller agreed to route post-merge tagging and asset verification: use `just release-tag v0.0.145 <exact-landed-commit>`, then verify the immutable release and all platform assets with `just release-pin-rows`. Only then should a consumer follow-up advance the pin and enable these reporter paths. This one-shot parks at accepted queue handoff rather than becoming another release or merge actor.
