# Queue monitoring that produces progress

Read with `SKILL.md`, live `cara help --json`, and the target repository's validated policy.
This guide supplies an operator decision loop, not new authority, another scheduler, or an alternative queue implementation.
Non-queue recovery examples are handoffs to separately authorized project/provider actors, not permission for a Cara responder to call raw provider APIs or assume runtime/release authority.
The success criterion is a verified state transition or an actionable, acknowledged disposition—not a running loop, a sent message, or another identical refusal.

## Establish scope and the one writer

- Identify the repository, checkout kind, configured queue/merge actor, source owners, and caller capabilities before acting.
- Apply Cara only to repositories configured for it.
  Other repositories retain their own approved provider/project workflow; monitoring does not enroll them into Cara or enable native auto-merge as an extra actor.
- Reuse the existing scheduler, loop, source owner, and recovery task.
  Check for an existing loop before creating one; do not launch a manual sync beside a scheduled writer merely to make progress visible.
- Preserve drafts, manual skips, parking, submission holds, review requirements, CI enablement, branch protection, and existing work.
  An unavailable owner is not permission to take over, and a capability available to a controller in one project need not exist in another.
- A request to keep monitoring is not approval of an outstanding operator choice, ownership transfer, deployment, force merge, or runtime restart.
  Conversely, do not invent an additional operator hold on ordinary repair already authorized by the actual policy and custody contract.

## Read the exact state, not only the PR list

For each material candidate or blocker, retain a compact evidence record:

| Evidence | What must remain distinct |
|---|---|
| Source | Repository, PR, head ref/OID, intended parent/default ref, actual live parent/default OIDs, ancestry or exact-content proof |
| Provider | PR state, base-ref projection, labels/draft/holds, native Stack identity and membership generation |
| Validation | Run/job/attempt, workflow/event head and base, required contexts, actual failing step, skipped work and missing reporting runs |
| Custody | Canonical owner/generation, accepted queue handoff, repair delegation, immutable versus mutable source, unpublished work |
| Scheduler | Configured actor, request/operation ID, start and terminal outcome, full typed refusal, completed prefix and mutation receipts |
| Decision | Stable fingerprint, first observation/age, unchanged observations, impact, accepted resolver, next bounded action and verification point |

A PR base projection can remain stale while the actual branch tip moves.
A green head-only rollup is not proof for a new base or newly materialized merge candidate.
Use the product's exact-generation/tree-equivalence rules; neither demand an unnecessary rerun when those rules preserve qualification nor carry checks across drift without their proof.
A scheduler process that exists, an exit-zero tick, or an empty failure list is not proof of admission, merge readiness, or required CI execution.
Read the terminal configured scheduler receipt before attributing a failure to its latest tick; an old log tail may describe an earlier execution.

Keep the last successful snapshot if discovery fails, but mark it stale and do not use it to authorize mutation.
Do not turn evidence collection into client-side freshness gates around accepted first-party bead/task CRUD.
Repository or binary versions, paths, and digests are diagnostic/rollout facts, not invented admission gates or excuses to add runtime pins.

## First refusal: find the smallest blocking scope

Identify whether the problem blocks one candidate, the caravan root, a suffix, native Stack recovery, all active fleet convergence, or only new admissions.
Name the qualified prefix and other ready work that cannot proceed because of it, not just the broken PR.
Qualification follows live head/base/tree/check policy, not merely a green PR rollup.
Choose work by queue impact and position, not whichever unrelated failure is easiest to fix.

For a ready prefix with a broken or unavailable tail, compare these routes immediately:

- **Owner repair:** an acknowledged owner can perform a bounded, exact-generation correction under the existing source/custody contract.
- **Supported isolation and prefix landing:** live help/status/plan supplies a sealed partial-prefix, top-eviction, reshape, parking, or other disposition that the exact authorized actor can apply while preserving the blocked work.
- **Explicit decision or product gap:** no supported safe route is available under current authority.
  State which ready PRs are held up, the exact refusal, the required capability/decision, and the smallest proposed resolution.

Do not assume a bad child must keep healthy parents blocked indefinitely.
Do not assume healthy parents authorize a raw merge around a bad child either.
A native Stack recovery preflight may reject the whole member chain before it ever considers landing the prefix; distinguish that from failing parent tests.
A whole-caravan pause may freeze the ready prefix too, so verify the proposed isolation's actual scope rather than treating every hold as a remedy.
Never improvise a detach/rejoin sequence, strip control labels, enable another merge actor, or discard the suffix when the sealed route is unavailable.
The [safe-path canary](safe-path-canary.md) illustrates owner-applied tail eviction, not blanket permission to repeat its commands or operate beside another scheduler.

## Repeated refusal: change the disposition, not the polling frequency

Prefer Cara's stable decision fingerprint; if absent, record the exact error/generation/config/member facts for comparison, not a fabricated authoritative lease.
Different request IDs with the same causal evidence are one unresolved blocker.

- For a genuine `retry_tick` race, let the existing actor rediscover within its bounded retry policy.
- If identical evidence persists beyond that bound—or recurs on the next scheduled pass with no evidence of convergence—inspect what can actually change it.
  Check whether a source repair, owner decision, missing run, stalled delivery, or unsupported recovery path is being mistaken for a transient provider race.
- Escalate that liveness problem to the existing responsible actor with the fingerprint, elapsed time, ready-prefix impact, and an executable next step or exact capability refusal.
  Do not rewrite the producer's typed disposition, dispatch a repair merely because the retry count grew, or bypass the refusal.
- By the next verification point, obtain an owner acknowledgement, observed execution, or explicit blocked disposition.
  If none exists, diagnose delivery/capacity/authority rather than resending the same request and calling it coordination.
  A request merely awaiting a reply is not an acknowledged blocked disposition; name the concrete refusal, unavailable capability, or required operator decision.
- Search existing open and closed work before recording a product or routing gap.
  Extend the matching task with fresh evidence; do not reopen a completed, different fix or file one duplicate per tick.

Do not hot-loop, increase sync frequency, repeatedly wake an agent, or report unchanged state as progress.
An intentional hold may remain held; an unresolved approval must remain unresolved until the authorized operator decides.
Keep monitoring quietly while that specific decision is outstanding, and continue other work only where policy permits.
When approval arrives, reconcile any existing attempt and route execution through the designated actor with fresh preconditions; do not keep reporting an obsolete approval wait or ask for the same authority again.
Changed scope, ownership or generation may require a new preview or sequencing decision, not blind reuse of an old receipt.

## Close the owner-repair loop

Use the canonical task/ownership response, not an inferred role, branch name, SSH account, stale agent projection, or ambient credentials.
Bind the request to the current PR/head/base, existing owner/generation, failure evidence, allowed source scope, and required return receipt.
Use first-party source repair and normal hooks/checks; accepted immutable queue custody is not permission to push from the old mutable worktree.

Track these as separate states:

`request stored → owner acknowledges → custody/preconditions proven → action starts → exact result verified`

A delivery acceptance or `pending_confirmation` is not proof the owner consumed the instruction.
A capacity acknowledgement is not assignment, exclusive custody, or permission to interrupt the receiver's current task.
Before activation, re-read its current assignment and obtain explicit sequencing with WIP preservation; do not reuse an earlier idle/capacity observation after it changes.
When an owner reports completion, independently read the new exact provider generation and the relevant check/operation receipts before retiring the old blocker.

For an unresponsive owner, compare bounded current runtime evidence with status projections.
A stale status may hide active work; an explicit stalled capture and queued inputs are a different condition.
Keep binding diagnostics, dispatch state, historical launch failures, and actual recent activity separate unless causality is proven.
Do not flush/replay queued input, reconnect/reload the runtime, override heartbeat/delivery policy, overwrite drafts, or start another source writer as a delivery workaround.
Request a supported preservation-fenced disposition from the already-authorized lifecycle/project actor.
A public-PR repair must not accidentally publish held or unvalidated local work; dirty roots and unpublished history must survive any authorized subordinate-checkout or custody operation.
If authority or separation is unsupported, return the exact refusal and one bounded decision rather than spoofing identity or inventing a broader architecture prerequisite.

## Classify CI by the failing step and generation

| Observation | Bounded continuation |
|---|---|
| Admission-only deferral; heavy jobs skipped | Verify exact gate decision and full lineage; preserve it as unevaluated, never green. Let the existing admission actor perform any authorized membership-before-CI trigger once. |
| Required contexts have no reporting run | Distinguish missing execution from passing CI; inspect admission, trigger and concurrency receipts before requesting one supported start. |
| Retargeted PR; old event/base metadata rejected | Route current-base evidence to the existing CI/queue owner. Repeatedly rerunning the stale event is not proof of valid new-base CI. |
| Lock/hash, lint, compile, or assertion failure | Route exact logs and a bounded reproducer to the source owner. Preserve strict checks and invariants; do not mask it with skips, blanket reruns or wider timeouts. |
| Runner/transport/resource preflight failure | Separate it from source failure. Seek an acknowledged infrastructure diagnosis and at most the authorized, generation-bound recovery. |
| Queued workflow/job | Check workflow-level status, concurrency, exact superseded generations and matching runner evidence; PR check rollup alone may omit the pending run. |
| Build passed but artifact upload/handoff failed | Keep artifact transport and device/emulator qualification incomplete. Do not invent artifact hashes, delete storage broadly, or call a successful build device acceptance. |

A proposed infrastructure retry requires fresh exact head/run/attempt evidence, owner/custody agreement, and reconciliation of competing or uncertain attempts.
Do not carry an old retry authorization onto a new source generation.
For an obsolete run whose ordinary cancellation was accepted but never terminalized, the existing authorized CI owner may review the provider's documented escalation, such as GitHub's exact-run `force-cancel`.
This is not a standing cancellation loop: through that actor's approved provider interface, reverify that only the obsolete run is targeted, preserve its real failures, issue at most the approved operation, and read back terminal state.
An HTTP acceptance, cancellation request, or conflict response is not terminal cancellation or a successful current run.
Do not cancel other work or reprioritize runners to manufacture a result.
A runner inventory without a match is bounded evidence, not proof that all organization/fleet capacity is absent; once a matching runner starts work, retire the obsolete missing-capacity diagnosis.
No source or infrastructure symptom authorizes weakening protection, changing CI enablement, registering/restarting services, or broad cleanup without the appropriate separate authority.

## Apply once and reconcile uncertainty

Use the exact typed preview/plan and its operation identity, current generation, policy/config fingerprint, mutation scope and rollback/preservation evidence.
A clean plan, lock absence, node-local pause or idle checkout does not reserve a cross-clone writer or establish exclusive source custody.
Honor writer-lock contention and ownership fences; use the returned bounded continuation rather than deleting a lock or switching hosts to race it.

A timeout or failed caller receipt can follow a successful provider mutation.
Before retrying, reconcile provider state, durable operation journal, accepted membership/publication receipts, source refs and completed prefix using first-party status/recovery surfaces.
Resume only the supported uncompleted continuation; do not repeat successful joins, pushes, merges, CI triggers, cancellation requests or lifecycle completion.
If a first-party operation refuses, preserve the exact error; generic shell access and credentials are not an alternate authorization path.

After application, verify the actual postcondition and generation, then update the blocker record and inform the existing owner.
Submitted, accepted, queued, executing and terminal are different outcomes.
A monitor must not mistake a routed proposal for an applied recovery or a process exit for managed lifecycle completion.

## Manual intervention and supersession

If the operator lands or changes a PR manually, independently verify the exact merge and true-main ancestry/content.
Credit it as operator intervention, not queue automation success.
Invalidate stale plans and parent-repair requests, preserve still-held work, and have the existing writer rediscover before further mutation.
A concurrent tick's subsequent stale-ref refusal may be caused by that intervention; it is not necessarily the original cause of the stall.
Revalidate pending approval scope and all application preconditions; a manual parent merge does not itself approve a child takeover.

For a genuinely superseded standalone PR, obtain owner/coordinator disposition and prove the actual integration using the project's exact-content and ancestry evidence.
A similar title or patch-id alone is not a complete integration proof.
Use the supported close/supersession route only within current authority, preserving refs, source history and unfinished feature work.
Closed-as-superseded is not merged, CI-green, delivered to main, or permission to close an unfinished feature task.

## Report outcomes and finish the right task

Report material transitions or actionable blockers only, with the affected ready work and next decision made explicit.
A useful blocked report says: exact blocker/fingerprint; scope and age; acknowledged owner or missing capability; attempted safe route and exact refusal; smallest required decision; mutations performed; next verification point.
A useful progress report says what actually changed and which acceptance remains unproven.

Keep source merge, current-base CI qualification, immutable release, artifact/checksum verification, installed bytes, running service identity, live/device acceptance and fleet rollout separate.
Use existing release/install actors and declarative routes; a source fix does not authorize a release dispatch or manual binary installation.
Keep completed classifier/release work closed when a new metadata, topology, delivery, runner or artifact problem appears.
Do not create new runtime pins, cleanup jobs or secondary merge actors to paper over those separate failures.

Retain a compact continuation checkpoint with current generations, holds, owner acknowledgements, unresolved choices, uncertain operations and evidence locations.
Re-read it after compaction before resuming; never replay old actions merely because the conversation was shortened.
For product rules and typed surfaces, consult [lifecycle](../../../../docs/lifecycle.md), [dogfood operation](../../../../docs/dogfood.md), and the live help rather than copying this guide into a scheduler or treating historical examples as current authority.
