# Session summary — Make Cara repair builds hermetic under Nix

## Goal

Unblock the first live Cacophony use of the newly landed Cara repair workflow by removing its hidden dependency on ambient Git author/committer configuration and proving the canonical Nix package exposes the repair-capable binary.

## Bead(s)

- `bd-dbb5bb` — Hermetic Nix build lacks Git identity for Cara repair tests.
- Parent repair capability: `bd-cf44d1` — Managed clean repair workspace for Cara sync recovery.

## Before state

- True Caravan main `39e597d` contained `repair start/status/continue/abort`, but `nix run <daemon-checkout>` failed during its sandbox test phase.
- 207 tests passed and all six repair tests failed with `repair_merge_failed` because independent repair clones could not resolve a Git committer identity.
- Installed Cara and the stable wrapper remained version 0.0.1, so caco-merger had no usable supported binary for PR #1962.
- The live repair attempt failed before provider mutation and preserved PR #1962 with auto-merge disabled.

## After state

- Every Cara-owned merge and commit command supplies deterministic command-scoped `user.name=Caravan Repair` and `user.email=caravan-repair@users.noreply.github.com` alongside disabled GPG signing; no global or user Git config is read or modified.
- The committed repair test asserts both author and committer identities on the exact published merge commit.
- All six repair tests pass with global/system Git config disabled and every `GIT_AUTHOR_*` / `GIT_COMMITTER_*` variable removed.
- `nix flake check --no-write-lock-file` completes all 32 aarch64-darwin flake checks and builds `caravan-0.0.2` in the sandbox.
- `nix run . -- --version` reports `cara 0.0.2`; `nix run . -- repair --help` exposes start, continue, status, and abort.
- Provider semantics remain unchanged: exact head/target checks, scoped resolution, exact parents, non-force push, and sync continuation are untouched.

## Diff summary

- Code/content commit: `6773ae9`; final landed squash SHA will come from the reintegration receipt.
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: `src/repair.rs` only.
- Tests: six hermetic repair tests green; strict all-target/all-feature Clippy green; canonical Nix flake and repair CLI/version smoke green.
- Behavioural delta: tool-generated repair merge commits now have deterministic local identity even in an empty Nix sandbox, while GitHub authentication still owns provider publication identity.

## Operator-takeaway

The repair design itself was fail-closed, but its independent clone also isolated away the caller's local Git identity. Supplying identity only on Cara's merge/commit commands makes both tests and live repair hermetic without mutating global config or weakening any exact-generation/provider guard; caco-merger can use the current-checkout Nix app immediately after landing instead of returning to raw Git surgery.
