# Historical native prefix required-check rejection (bd-db8384)

## Disposition

The September 23 Stack4058 rejection remains **provider-rejected, root cause
unproved**. The successful head checks in the recorded evidence are not proof
that GitHub's native Stack merge service qualified those same commits. Conversely,
the rejection text is not proof of wrong App identity, unassociated dispatch,
source-branch protection, or missing synthetic checks being the cause.

No historical operation should be retried. The affected work subsequently landed;
this diagnosis does not request another merge, rebase, CI run, protection change,
or recovery of that old Stack. Current protection must be discovered afresh;
`cara-admission` below is a historical context name, not today's policy.

## Evidence and its limits

The recorded prefix was PR4040 at
`46232ed0c7c58d1e3a168f8b87006544a5a0a23d` and PR4046 at
`82654e547aca98fa0a490f9cfcc20c6e3681e28c`. Check-runs107243263310
and107242114391 reported successful `cara-admission`, App15368, before submission.
Recorded provider associations bound each to its PR/head/base. No same-named
legacy status or extra source-branch rule was found. Synthetic merge commits had
no same-named checks. These facts are preserved board evidence, not new live
observations or a complete replayable provider snapshot.

Native operation UUID `5ac93279-4f24-46e4-8409-0b38b9907a7e` failed with
`Required status check "cara-admission" is expected`. Its lock was released and
handoff `native-rejection:fnv1a64:a9a3d12a6ea66154` was non-retrying. The error
alone does not identify the commit/ref/check generation enforced inside GitHub.
A conclusive provider diagnosis would require that evaluated identity and its
then-effective policy, rather than reconstructing it from current mutable state.

## What bd-e81848 fixes—and does not establish

Landed commit `b91fa3c15df969364c16cf605aed6e8d4c67504c` qualifies actual
landing-target requirements by context, App and current head. Optional checks
remain diagnostic; missing, cancelled, failed, wrong-App and wrong-head required
checks cannot qualify. Native pre-submit qualification uses fresh policy and
member facts rather than assuming an unprotected intermediate parent exempts a
child. See `src/required_runs/effective_policy_tests.rs` and
`src/sync/tests/effective_policy.rs`.

The historical-shape regression
`historical_native_prefix_heads_qualify_without_synthetic_check_substitution`
shows both recorded heads qualify under that local policy for pull-request and
dispatch lineage when an actual successful App15368 head check exists. Wrong App,
synthetic-instead-of-head, cancelled and failed checks do not qualify. Unavailable
historical branch/base facts and run IDs in this narrow classifier fixture are
explicit symbolic values; it is not an end-to-end provider reproduction or proof
of workflow-execution authorization. Existing lineage and native transaction
tests retain that separate responsibility.

Thus e81848 addresses real local eligibility defects but **cannot be claimed to
have fixed the historical provider rejection**. Local eligibility and provider
acceptance are separate outcomes.

## Supported continuation boundary

The inspected CLI has no explicit root-only/prefix-length selector for
`plan sync`/`sync --dry-run`; native Stack previews concern representation/rebase.
That is an advertised interface boundary, not proof that internal prefix planning
is impossible. A physical rebase preview is not a merge plan and its force-with-
lease apply path is not authorized for this historical task.

`GitHubStackLandCheckpoint::failure_handoff` in `src/github/stack_land.rs`
preserves a deterministic rejection identity and sets `automatic_retry=false`.
Its root-only revalidation hint, where applicable, is not permission to replay a
failed UUID. Existing checkpoint recovery distinguishes submitting, submitted,
terminal and released phases; response loss must reconcile the existing request,
not issue another attempt. No new selector, fallback or retry mechanism is
introduced by this diagnosis.
