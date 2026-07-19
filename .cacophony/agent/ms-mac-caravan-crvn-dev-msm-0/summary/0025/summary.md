# Session summary — Add audited semantic-path repair grants

## Goal

Provide a fail-closed first-party continuation when a mechanical PR repair is textually conflict-free but semantically incomplete, so operators can restore reviewed source changes to exact paths without broad edit authority or raw Git surgery.

## Bead(s)

- `bd-17375b` — Cara repair cannot grant audited semantic paths for conflict-free regressions.
- Duplicate filed concurrently: `bd-10475d` — Add audited semantic-path grants to Cara repair sessions.

## Before state

- Deployed Caravan `aa8934e` materialized PR #1962 session `pr-1962-05fe82fe80c6` at head `05fe82fe` and target main `90bebd5c` through the canonical cache.
- Mechanical merge reached resolving with no textual conflicts and no provider mutation.
- Staged tree contained 15 PR files but omitted required README.md/SPEC.md shell-safe message-body contracts from reviewed source commit `c915e231`.
- Continue correctly rejected edits outside `conflicting_paths`, but the set was empty, leaving no authorized semantic restoration path.

## After state

- `cara repair grant` accepts a bounded repeated tracked path set, exact reviewed source revision, actor, reason, and expiry after session/config/provider head+target revalidation.
- Cara records source commit+parent, source/base blobs, source patch fingerprint, original staged OID, exact expected result OID, actor/reason/timestamps/expiry/applied state.
- Cara itself computes a three-way result from current path, source parent, and reviewed source blob; only clean reviewed results are hashed, written, and staged. Reruns are idempotent.
- Continue allows only mechanical conflict paths plus unexpired fully applied grants, and every granted path must retain its exact expected result OID. Ungranted third paths, traversal/control paths, symlinks, untracked files, authority changes, expiry, source/result drift, and provider/head/target/config drift fail closed.
- `repair revoke-grant` requires matching grant authority, restores/stages exact pre-grant blobs, removes the receipts, records reason, and never mutates provider state.
- CLI, JSON, MCP, human status, README, SPEC, and parity matrix expose grant and revocation receipts without reclassifying mechanical compatibility.

## Diff summary

- Code/content commit: `37b8f1e`; final landed squash SHA will come from the reintegration receipt.
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: `src/repair.rs`, `src/lib.rs`, `src/main.rs`, `tests/v1_parity.rs`, `README.md`, `SPEC.md`, `docs/v1-parity.md`.
- Tests: exact two-path reviewed source apply/idempotency/continue; ungranted and traversal refusal; authority mismatch; expiry; exact revocation baseline restore; all prior repair safety tests.
- Full 223 library + 6 binary + 7 CLI + 3 parity tests, strict all-target/all-feature Clippy, canonical rustfmt, Nix flake, and packaged repair help all pass.
- Provider publication remains exact-parent, non-force, and separately guarded; grants are local staged-content authority only.

## Operator-takeaway

Mechanical compatibility is not semantic correctness. Semantic grants preserve that distinction: they do not call a clean merge a conflict or permit arbitrary edits; they replay one reviewed source change into explicit paths and bind the exact staged result to a short-lived, attributable receipt that continue can mechanically enforce.
