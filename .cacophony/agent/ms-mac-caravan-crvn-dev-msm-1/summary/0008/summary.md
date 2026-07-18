# Session summary — bounded sync and cancellation-safe operation locks

## Goal

Implement P1 `bd-19ac1a` after the live Cacophony eight-member `sync --all` exceeded a 180-second client window and left a fresh, proven-dead `sync_decision_checkout` lock blocking mutations for the 30-minute stale floor. Coordinate shared discovery timing with `bd-e98eff` and preserve all live-owner/token protections.

## Land-ready generation

- Current code commit before reintegration: `17fa40a`.
- Based on comment/audit main plus `bd-e98eff` deadline/batched-discovery follow-up.

## Lock lifecycle

- `OperationLockOwner` carries an optional compact checkpoint with phase, update time, exact compact receipt/precondition evidence, and provider-indeterminate flag.
- Checkpoints are bounded before write and atomically replace the Unix owner file through a fully synced temporary file; process termination cannot tear the exact owner/token evidence.
- A new mutating acquisition encountering an existing lock now:
  1. reads owner/token;
  2. proves PID dead;
  3. rereads exact owner/token;
  4. proves PID still dead (fencing PID reuse);
  5. removes the orphan and acquires normally.
- Proven-dead owners are recoverable at any age; live, reused-PID, changed-token, malformed, or ambiguous owners remain fail-closed.
- `lock status` exposes `auto_recoverable` without mutating.
- Successful sync output includes exact `lock_recovery`, including the killed owner's checkpoint. Later sync errors also retain this recovery evidence.
- Manual exact-token/stale recovery remains unchanged.

## Sync bounds and receipts

- Whole sync budget is `min(5 × command_timeout_secs, 150s)`; default is 150 seconds, below the observed 180-second client ceiling.
- One absolute deadline is shared across initial `status_with_deadline`, every provider child (still normal 30-second maximum), final status, and decision checkout.
- `SyncTiming` reports total, initial status, provider convergence, final status, and deadline milliseconds.
- Durable lock phases: initial discovery, provider convergence in-flight (provider state indeterminate), provider converged, final discovery, completed.
- Provider checkpoints retain compact exact before/after `PullRequestPrecondition` facts and event IDs/kinds.
- Decision checkout consumes the exact PR snapshot already embedded in the decision and verifies advertised OID, avoiding a third full GitHub inventory.
- Final-status timeout preserves phase/deadline/source evidence plus completed provider receipts.

## Deterministic and live validation

- Real-process SIGTERM canary launches a child holding `sync_decision_checkout`, confirms its checkpoint survives below stale age, and proves the next acquisition returns exact token-verified recovery.
- Unit coverage proves fresh dead cleanup, live-owner refusal, stale live refusal, wrong-token refusal, PID reuse fencing, checkpoint persistence, and later-error recovery evidence.
- Combined post-rebase gate with FIFO/comment lanes: 164 library + 4 binary + 5 CLI + 3 parity tests, strict all-target Clippy, and `nix flake check` all pass.
- Read-only live GitHub canary in a clean Caravan clone:
  - before: dead PID 999999, age 0, stale=false, auto_recoverable=true, exact decision-checkout checkpoint;
  - `sync --all`: success, changed=false, no caravans/provider writes, exact token-verified `lock_recovery` with checkpoint;
  - timing: 13,620ms total (6,160 initial status, 0 provider, 7,378 final) under 150,000ms;
  - after: lock absent, graph healthy.
- `bd-e98eff` independently live-profiled Cacophony status at 24.401s/30s for 30 PRs and seven members with no mutation.
- No Cacophony sync mutation was attempted without controller approval.

## Coordination

- `bd-e98eff` owner exclusively changed command/discovery/read timing and landed `status_with_deadline` plus per-child timeout fix; this lane only composes the API.
- `bd-dc3983` comment lane was integration-reviewed, fixed, fully gated, and landed on main before this final combined gate. Live comment proof remains controller-gated in `bd-477652`.
