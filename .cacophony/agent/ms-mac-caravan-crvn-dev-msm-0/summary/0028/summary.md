# Session summary — Audited session-level agent repair edits

## Goal

Complete operator-corrected bd-17375b: preserve narrow deterministic conflict/grant repair while authorizing one exact merger agent to make broader repository-content edits after a typed semantic or CI decision.

## After state

- New CLI/JSON/MCP `repair authorize-agent-edits` binds repository, PR, exact head/target, session, config, pre-authorization manifest fingerprint, actor, reason, timestamp, and expiry. It revalidates provider refs and never mutates provider state.
- `repair continue --actor A` permits add/modify/rename/delete only when broad staged paths need it and actor/authority/identity/expiry still match. Existing narrow conflicts and exact semantic grants remain independent and continue to work without broad authority.
- Continue rejects unstaged/untracked residue, unresolved markers, traversal/Git internals, symlink/gitlink modes, secret-like paths, over-64/over-4KiB path scope, over-1MiB diff evidence, identity drift, expiry, and actor mismatch.
- Before commit, Cara records the complete bounded broad path list, path fingerprint, staged object/deletion fingerprint, binary diff fingerprint/bytes, actor/reason, verification time, and fresh-CI requirement. Manifest, bounded status, operation lock, publication, JSON/MCP, and human status carry audit evidence.
- Publication remains exact-parent, ordinary non-force fast-forward under provider head/target rereads. Every publication receipt marks fresh CI required; normal sync resume observes the new generation.
- Exact authorization retries are idempotent. A different unexpired authority conflicts.

## Tests and validation

- Added fixtures for broad modification + new file + deletion, matching/mismatched actor, idempotent authorization, exact publication receipt, and staged `.env` refusal.
- Updated prior scope tests: unrelated changes without broad authority now fail with `repair_agent_edit_authorization_required`; semantic grants still do not authorize a third path.
- Full pre-rebase suite: 259 library + 8 binary + 7 CLI + 3 parity tests, strict all-target/all-feature Clippy/fmt, and Nix flake check green.
- Rebased on scheduler classification main `8af7dd9`; resolved SPEC-only overlap preserving both contracts. Post-rebase strict Clippy, focused authorization fixture, parity, and diff checks green.

## Diff

- Implementation generation: `76b1ccc` after rebase.
- Surfaces: `src/repair.rs`, CLI, MCP router, parity tests, README, SPEC, parity matrix.

## Operator takeaway

The safety boundary for agent repair is now exact session and publication identity—not an inflexible predeclared path list. Cara still fingerprints and bounds every broad edit before non-force publication and fresh CI, while deterministic grants remain available for source-provenance automation.
