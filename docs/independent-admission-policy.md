# Independent new-caravan admission: policy review

Status: **review boundary and diagnostics; no independence bypass enabled**.
This is not an instruction to create another caravan, increase capacity, alter a
hold, or operate a second scheduler. Historical PR numbers in the review request
are reproduction context, not live scopes.

## Current behavior and read/write trace

`GraphProblemKind::Incompatible` describes an existing member's mechanical
conflict and blocks the fleet. It differs from `CandidateIncompatible`, which is
local to an unadmitted candidate. `GraphAnalysis::problem_blocks_active_fleet`
already excludes problems wholly owned by parked caravans; an unparked member or
unattributed/global problem remains blocking.

The paths are deliberately coupled:

| Surface | Actual policy path |
|---|---|
| `check --pr` without a target | `check_analysis_with_recommendation` first checks the canonical available join target, then a coherent new-caravan fallback; it does not search arbitrary compatible tails. |
| Explicit `new` / `renew` | `check_requested_action_analysis` proves the requested new operation, without turning a recommendation into different mutation intent. |
| Membership application | `membership::policy::preflight_eligibility` calls that same requested-action path, after generation integrity; repository policy and exact provider preconditions remain separate gates. |
| Automatic admission | The existing sync actor owns convergence and admission order. Native backend/graph work may stop the tick before candidate membership. It also checks root and member capacity. A candidate-local read is not authorization to bypass those scheduler gates. |

The new-caravan proof checks candidate-to-default, candidate-to-each-unparked-tail,
and each-unparked-head-to-candidate compatibility. JOIN separately proves the
exact selected tail and cross-caravan attachment. Existing active-fleet problems
are seeded into both paths. Thus a candidate can have all clean own reports and
still be ineligible because an unrelated active edge is incompatible.

This review adds `blocked_by_existing_fleet` to the existing `admission_note`
when that is the observed scope. An exclusively inherited mechanical conflict
is not misreported as evidence of a defect in the candidate's source. If candidate
or global failures also exist, the note says so. Original problems, PR identities,
compatibility reports, action and eligibility remain unchanged. The note is
**diagnostic, never a permission, waiver, or fresh generation lease**.

## Mechanical isolation versus global safety

An exact unrelated active edge is conceptually separable from global topology:
it can be associated with a single derived caravan, bounded members and exact
head/base facts. A global cycle, branching/duplicate/reused provenance,
ambiguous generation, unknown provider state or incomplete membership inventory
cannot safely establish what is independent. Merely filtering out all
`Incompatible` problems is therefore not a policy.

Any future opt-in independent-NEW implementation must prove **all** of these in
the same fresh snapshot and again at mutation preflight:

1. **Scope:** only explicitly classified mechanical active-edge failures, wholly
   contained in another exact caravan, are isolated. Their source, topology,
   controls and operation receipts remain untouched. Empty/unknown scope,
   cross-caravan/global structural failure, and incomplete evidence fail closed.
2. **Candidate:** open ready same-repository source, required policy/checks,
   valid generation, exact current default/candidate compatibility and all
   cross-caravan attachment proofs. Clean mergeability is not independence.
3. **Dependencies:** no actual base-chain or other policy-recognized dependency
   reaches the isolated chain. Existing explicit-owner ordering exceptions do
   not supply this proof: ordinary explicit NEW can currently be compatible
   while still listing active dependency PRs. A future isolation path must inspect
   those dependencies, not reinterpret ordinary eligibility as independence.
4. **Capacity and order:** preserve `sync.max_caravans`, max-member capacity,
   priority/FIFO and explicit-intent semantics, parked/held policy, and candidate
   selection. Spare capacity does not waive an incompatible edge; a pending owner
   response does not create capacity or make the candidate independent.
5. **Read/write parity:** the same scoped decision and exact generation must be
   consumed by check, explicit-new, auto-admission and membership application.
   Scheduler/native preflight must support the same isolation deliberately;
   changing only `check` would produce an eligible-but-unwritable lie.
6. **Writer and provider:** preserve existing writer leases, incomplete provider
   refusal, native Stack identity/order/ancestry, source-preserving publication,
   protection, required CI, custody, holds and response-loss reconciliation.
   Do not use this policy to activate a competitor while another operation is
   uncertain or to replay a terminal failed operation.
7. **JOIN is unchanged:** a bad selected tail cannot be bypassed by silently
   choosing a different caravan. Forming a new caravan must remain explicit
   requested intent or the existing canonical automatic decision.

This review does not approve or implement that opt-in; it names the required
contract and the code boundaries that a separate policy change must satisfy.
The current conservative refusal remains authoritative. Ordinary owned source
repair and qualified existing landing do not wait on this discussion.

## Executable controls

`src/read/tests/independent_admission.rs` executes the actual read paths for a
clean candidate plus an unrelated conflicting active chain, explicit JOIN,
candidate/default and both cross-caravan conflict directions, malformed/unknown
global problems, unresolved unjoined dependencies, ordinary active dependencies,
and capacity. The scope-note tests refuse to label empty, unknown or candidate
rows as another caravan's repair.

`membership::tests::independent_admission_new_write_preflight_matches_read_refusal_without_effects`
executes membership with a hermetic provider and asserts that the read refusal is
also a write refusal with unchanged PRs and no audit/membership writes. The
existing parked-repair independent-admission control remains intact. These are
policy tests, not live provider acceptance or permission for historical PR work.
