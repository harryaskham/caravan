# Gated CI for Caravan repositories

Use this pattern when expensive PR CI should run only for active Caravan members.
It keeps GitHub branch protection fail-closed for unjoined PRs without making
those PRs inadmissible to Cara.

## Prerequisites

1. Install and pin a Cara release that contains `ci.admission_gate` and
   `cara ci-admission-gate`.
2. Keep exactly one Cara writer. Admission and normal sync share the same
   repository writer/concurrency fence.
3. Keep the existing required aggregate context (for example `build-test`).

## Repository policy

```yaml
ci:
  admission_gate:
    mode: caravan_label
    context: build-test
    member_label: caravan
sync:
  actions:
    join_unlabelled_prs: true
```

Only the exact configured context receives the pre-membership exemption. Every
other terminal or unknown check still refuses admission. As soon as `caravan`
is present, ordinary required-context policy applies.

## Workflow shape

The heavy workflow triggers only code-generation events:

```yaml
on:
  pull_request:
    types: [opened, synchronize, reopened]
```

Do **not** subscribe heavy CI to `labeled` or `unlabeled`. Priority, force,
skip, park, and unpark transitions would create unrelated check generations.

The first job checks out the trusted base SHA—not PR code—and runs:

```bash
cara --json ci-admission-gate \
  --event "$GITHUB_EVENT_PATH" \
  --github-output "$GITHUB_OUTPUT"
```

Heavy jobs use:

```yaml
needs: admission-gate
if: needs.admission-gate.outputs.run-ci == 'true'
```

The required aggregate job always runs. When `deferred-unjoined=true`, it exits
with the emitted sentinel code (78). This blocks direct/manual merge while Cara
continues eligibility, priority/FIFO, generation, capacity, and compatibility
preflight.

After Cara adds membership, sync maps the deferred gate run to one exact
rerequestable check suite and rerequests it under the post-membership PR
precondition. The rerun queries live membership, runs heavy CI on the unchanged
head, and produces ordinary required-check evidence. No label event is needed.

Caravan's checked-in `.github/workflows/ci.yml` is the canonical executable
example and `tests/ci_workflow_contract.rs` prevents broad label triggers or
loss of the fail-closed aggregate.

## Admission and convergence controllers

Copy the reviewed bundle under [`examples/workflows/`](../examples/workflows/):

- `caravan-gate.yml` is the unprivileged exact-head PR suite above;
- `cara-admit.yml` is a trusted default-branch `pull_request_target`, schedule,
  and manual controller which runs exactly one `cara --json admit`;
- `cara-sync.yml` owns ordinary convergence on schedule/manual wake.

Admission never uses the wake PR as its candidate. It hot-discovers the global
priority/FIFO order and returns one of `admitted`, `no_candidate`,
`waiting_for_existing_convergence`, `external_decision`, or
`retryable_provider_race` with an exact cursor. It admits at most one candidate
and cannot merge, promote, park/unpark, evict, repair, reshape, or broadly rerun
fleet CI. Existing fleet work stays with `cara-sync.yml`.

Both writers use the exact `cara-writer-${{ github.repository }}` concurrency
key with `cancel-in-progress: false`. GitHub permits only one running and one
pending member of the group, coalescing event bursts without cancelling an
indeterminate provider write. Both download a versioned x86_64 Linux Cara
archive and verify a hard-pinned SHA-256 before execution. Replace the version
and digest placeholders together after publishing the release containing
`cara admit`; never point the templates at `latest`, a branch, or unverified PR
content. The trusted controller checks out only the provider default branch,
sets `persist-credentials: false`, and never interpolates pull-request fields
into shell commands.

## Adoption sequence for Cacophony and Pi-Daemon

1. Land/pin the required Cara release first; older readers reject the new config.
2. In one reviewed repository PR, add `ci.admission_gate`, convert the heavy
   workflow to the canonical shape, and preserve the existing required aggregate
   context in branch protection.
3. Do not hand-edit labels or live workflow settings.
4. Open one disposable unjoined PR:
   - only admission gate + aggregate should run;
   - aggregate should report deferred failure;
   - no heavy jobs should consume runners.
5. Let canonical Cara admit it:
   - `caravan` is added once;
   - the exact existing suite is rerequested;
   - heavy jobs run once on the same head.
6. Verify priority/force/park/unpark label changes launch no heavy workflow.
7. Verify green member CI merges normally and cleanup uses typed Cara paths.

Any missing suite, ambiguous run-to-suite mapping, stale head/base, incomplete
provider read, config drift, or membership/label disagreement fails open to
running CI and fails closed for queue mutation.
