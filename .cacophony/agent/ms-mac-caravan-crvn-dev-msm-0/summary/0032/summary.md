# Session summary — resumable semantic grant/revoke persistence

## Goal / bead

- `bd-b9f0f3` — repair two exact partial-persistence states in semantic grant application and revocation.
- Promoted from audited draft to target-v0.0.6 P2 after the ready set was exhausted; scope stayed in managed repair and did not overlap the owned physical-rebase slice.

## After state

- Grant application now explicitly reconciles receipt/index pairs:
  - `applied=false` + exact expected staged OID finalizes the interrupted receipt;
  - `applied=false` + exact baseline OID resumes deterministic application;
  - `applied=true` accepts only the exact expected OID;
  - every other state remains typed fail-closed drift.
- New/existing grant receipts are checkpointed before path writes. All completed paths are marked applied and published in one final manifest write. A path/stage/final-manifest interruption can be retried without trapping `repair continue` behind `repair_grant_incomplete`.
- Existing-path retries no longer incorrectly consume the session's grant limit.
- Revocation validates the complete requested set—including authority, index states and baseline objects—before touching any path.
- Revocation then restores/stages all needed baselines and performs one manifest publication, so later-path preflight errors cannot partially revoke earlier paths.
- An active receipt whose index is already at its baseline is recognized as an interrupted restore and finalized safely.
- Successful revocations leave bounded durable `RepairPathGrantRevocation` receipts. Exact same actor/reason/path retries are idempotent only when the current index still equals the recorded baseline.
- Revocation receipts are exposed in bounded repair status and counted—not expanded—in operation-lock checkpoint evidence. Provider mutation remains false.
- README/SPEC document whole-set preflight, receipt/index recovery, and idempotent revocation evidence.

## Validation

- Added interrupted staged-grant receipt recovery fixture.
- Added two-path revoke fixture proving later authority failure leaves the first grant/index untouched, then simulating baseline restoration before manifest publication, successful mixed finalize/restore, and exact retry.
- Existing revoke fixture now proves durable exact retry.
- Full composed validation: 285 library + 12 binary + 8 CLI + 3 parity tests green; strict all-target/all-feature Clippy and rustfmt green; Nix flake checks green.

## Commit

- Pre-reintegration implementation commit: `8bdd435`.
