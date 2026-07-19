# Session summary — Replace cold repair clone with resumable exact-object materialization

## Goal

Finish live acceptance of the PR #1962 repair path after two correctly bounded 180-second retries proved that a full provider SSH clone repeatedly disconnects. Replace retransmission with a canonical object-cache seed plus minimal exact provider fetch while preserving every head/target/config/provider and non-force safety guard.

## Bead(s)

- `bd-9264d0` — Reopened: Cara repair cannot recover from clone timeout/provider sideband disconnect.
- Merged duplicate evidence: `bd-b4fab9` — Cold clone cannot survive provider SSH sideband disconnect.

## Before state

- Deployed Caravan `35dffd1` correctly persisted/reaped two ~180-second clone attempts for session `pr-1962-05fe82fe80c6`.
- Both fresh start and exact resume failed `fetch-pack: unexpected disconnect while reading sideband packet` while retransferring a full cold SSH clone.
- Manifest remained `preparing/cloning`, provider mutation remained false, and PR #1962 stayed open at exact head with auto-merge disabled.
- Timeout/resume semantics worked, but retrying the same full transfer was not a recovery strategy.

## After state

- Repair records the canonical caller checkout and Git-common identity as a content-addressed object-cache source.
- New workspaces are seeded by a fast local shared/no-checkout clone with a `cache` remote; explicit provider `origin` is bound separately and remains the only ref/publication authority.
- Head and target materialization use protocol v2, exact refs, `--filter=blob:none`, command-scoped SSH batch/connect/keepalive settings, pre/post provider OID checks, and exact commit-object verification.
- A valid partial repository survives interruption and is resumed in place, reusing already received objects. Invalid partial state is removed only after canonical manifest/path validation.
- Object-cache Git identity drift fails closed. Provider sideband disconnect is separately classified as `repair_provider_transport_disconnect` with in-place resume, bounded stderr, elapsed/budget/process-group/partial path, and provider-mutated=false evidence.

## Diff summary

- Code/content commit: `1a176a0`; final landed squash SHA will come from the reintegration receipt.
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: `src/repair.rs`, `README.md`, `SPEC.md`.
- Tests: 11 focused repair tests, including cache/provider remote separation under dirty/internal-origin caller, cache identity drift refusal, sideband classification, valid partial-repository reuse, no-workspace resume, timeout group receipt, scope/head/parent/non-force/abort guards.
- Full 218 library + 6 binary + 7 CLI + 3 parity tests, strict all-target/all-feature Clippy, canonical rustfmt, Nix flake, and packaged Cara 0.0.2/repair-help smoke all pass.
- Provider semantics are unchanged: every head/target is re-read from explicit provider before and after materialization/publication; local cache never proves freshness or permits force.

## Operator-takeaway

The 180-second timeout fix proved the state machine, then exposed transport behavior rather than a larger budget need. Seeding from the already-verified canonical checkout and fetching only exact missing objects converts repeated full-clone failure into bounded resumable transfer without trusting daemon-internal refs or touching the dirty caller worktree.
