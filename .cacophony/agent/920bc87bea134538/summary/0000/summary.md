# Session summary — Cumulative native Stack tree repair

## Goal

Stop queue-owned native Stack repair from evicting clean merge-synced members because replaying their history reintroduces main-side release conflicts. Preserve exact-generation publication guards and leave live queue operations to their existing owners.

## Bead(s)

- `bd-93b5ba` — Native Stack repair replays merge-synced PR history and evicts clean PRs on main-side-only conflicts.

## Before state

- Native Stack repair used the generic merge-preserving sequencer, including its simulated-parent upstream override.
- The hermetic reproduction proves a clean final merge tree but fails intermediate replay on `CHANGELOG.md`, `Cargo.lock`, and `Cargo.toml`, none of which the member net-changes against current main.
- Prior merge-resolution support runs only after sequencing succeeds; root-only flattening is not enabled on this path.
- Atomic prepared verification lacked the live remote-target reread already present in single-head apply.

## After state

- Queue-owned repair proves one unique three-way tree and commits it with both exact source and predecessor heads retained. No source commit sequence is replayed.
- An already-containing head, including an empty source range, remains unchanged and needs no source push or fresh CI.
- Plans and receipts retain merge-base/source/target/result-tree evidence. Real final-tree conflicts and ambiguous bases fail closed; source and target movement stop the atomic batch.
- 82 focused hermetic tests pass: 44 physical-rebase, 13 native-stack-rebase, 17 non-force-repair, 7 stale-base-refresh, and 1 native force-intent refusal.
- Formatting and diff checks pass. Local Rust 1.99 Clippy diagnostics have no new-file findings after corrections but retain existing warnings; pinned hosted CI remains the final gate.
- No live Cacophony queue, cron, sync, or PR actions were taken. Release, deployment, and live acceptance are not part of this source delivery.

## Diff summary

- Code commit: `2c5e7b8` — `bd-93b5ba: prove cumulative trees for queue-owned native Stack repair`.
- `src/physical_rebase/cumulative.rs`: queue-only tree proof, ancestry no-op, merge materialization, and exact receipt evidence.
- `src/physical_rebase.rs`: strategy selection after existing preflight, optional serialized proof, and remote-target revalidation for atomic apply.
- `src/native_stack_rebase.rs`: opt into cumulative preparation only for queue-owned repair, preserving explicit owner behavior.
- `src/physical_rebase/tests/cumulative.rs`: seven hermetic fixtures covering release-import replay, semantic/binary/mode preservation, genuine conflict, source/target races, ambiguity, and no-op publication.
- Existing receipt fixtures in membership and sync gained an absent optional proof field; README documents the behavior.
- Reflection deduplicated against `bd-a743fa`/`bd-17b606` for native provider transaction test seams and `bd-8095c7` for local-toolchain lint drift; context was sent to the controller rather than filing duplicates.

## Operator-takeaway

A clean final tree is the relevant repair proof, not whether Git can replay every historical merge without an intermediate conflict. Queue-owned repair now uses that proof directly while retaining both input histories and exact publication fences. This source still needs the configured queue's CI and a separate release/deployment before it changes live behavior.
