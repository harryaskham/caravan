# Native root CI waits (bd-4eccb7)

A native Stack whose root cannot qualify has no ready prefix. Its wait is a
completed sync decision, not permission to repeat convergence or replay
membership in the same tick.

## Bounded convergence

The native prefix pass assesses required checks against the actual landing
branch, in member order. It stops expensive CI reads at the first unqualified
member and preserves the ordinary native planner and checkpoint reconciliation.
An existing submitted/locked operation still uses its persisted transaction;
a wait never manufactures a new merge or changes a source ref.

The pass visits independent caravans too, including provider-proven stackless
singletons. A blocked root therefore does not prevent another ready root from
landing. Once a provider write or native transaction occurs, the existing
return-and-rediscover boundary still ends the pass.

Previously, a zero-write native wait was discarded. Ordinary convergence then
read the same members again, followed by automatic admission with an almost
exhausted request budget. That could strand an unrelated membership mutation
and its rollback. Now the tick retains the wait and stops before that admission
phase; it does not raise the 256-request limit or relax mutation preconditions.

The successful terminal receipt includes:

- `scheduler_status.native_stack_waits`: Stack number, caravan, blocking PR,
  typed native blockers and deferred members;
- the original CI disposition (`waiting_ci`, or a required-run/source decision),
  rather than claiming that an unchanged prefix advanced;
- `auto_admission.continuation=requires_converged_fleet` when sync-all admission
  was configured but deferred;
- no passing CI assessment for unobserved descendants.

The native wait is also retained in the bounded operation checkpoint. The
ordinary read budget and operation deadline still bound discovery and each
provider call; this change removes duplicate and useless convergence work,
not the need to acquire the initial complete graph.

## Fresh CI is a separate requirement

This change does **not** turn an exact-head `workflow_dispatch` success into a
PR-associated required check. It does not synthesize checks, close/reopen a live
Stack member, retarget it to trigger an event, or rewrite source to make CI run.
The existing current-generation infrastructure rerun and required-run recovery
policies remain in effect.

A rerun replays the original PR payload. If a trusted workflow materializes the
current base, its admission checks must bind both the live base SHA and base
ref, while retaining the original exact head and repository fences. Replaying
only a new SHA alongside an old stack-branch ref still fails admission.
Cacophony tracks that workflow/recovery fix in `bd-ba6c82`. Until it is delivered,
repeatedly rerunning the same stale event is not a recovery.

`required_runs::REVIEWED_MANUAL_RECOVERY` documents close/immediate-reopen as a
reviewed manual fallback, not a native-Stack-fenced automatic command. This
patch does not perform or broaden that operation.

## Log ownership

Cara's event journal already has `journal.max_bytes` and `journal.max_archives`.
The separately appended `cara-sync-cacophony.log` belongs to the Cacophony cron
runner, not this journal. Its size/age rotation and terminal-receipt retention
are tracked in Cacophony `bd-a1a5ae` and `bd-626170`; no daemon log or runtime
configuration is modified by this source change.

## Regression coverage

`src/sync/tests/native_root_wait.rs` covers pending, cancelled and failed roots
with 2-, 5- and 25-member stacks: convergence performs the same root-only CI
reads, does not mutate membership/source, and never enters descendant CI.
It also checks the serialized terminal wait/admission disposition, preserves
an existing landing checkpoint despite changed root CI, and confirms that an
independent ready singleton can land without losing the first Stack's wait
evidence.
