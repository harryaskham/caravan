# GitHub native stacks as an opt-in Caravan backend

Status: proposed main feature (`bd-e3069d`), default-off, owned by MSM-1.

## Decision summary

Add an explicit repository policy axis:

```yaml
stack_type: caravan # default, existing behavior
# stack_type: github # opt-in native GitHub Stack object + atomic stack merge
```

`caravan` remains byte-for-byte the default behavior. `github` keeps Cara as the scheduler and policy authority, but represents each eligible multi-member caravan as a GitHub Stack and lands a contiguous ready prefix through GitHub's asynchronous atomic stack-merge API.

This is a provider adapter, not a replacement for candidacy, priority, compatibility, holds, generation integrity, CI evidence, exact preconditions, or audit receipts.

## Verified provider facts

The design is based on GitHub's public-preview documentation and `github/gh-stack` v0.1.0, released 2026-07-29.

### Server-side Stack resource

GitHub exposes:

- REST stack membership on each pull request;
- `GET /repos/{owner}/{repo}/stacks` and `GET .../stacks/{number}`;
- `POST /repos/{owner}/{repo}/stacks` with two to 100 ordered PR numbers;
- `POST .../stacks/{number}/add` to append only at the top;
- `POST .../stacks/{number}/unstack` to remove unmerged entries (merged, merging, and queued entries remain);
- read-only GraphQL `PullRequest.stack` and `PullRequest.stackEntry` fields.

A Stack is still a linear PR base chain. The first PR targets the stack base; every later PR targets the previous PR's head branch. GitHub rejects Stack creation when those bases do not already match.

### Atomic asynchronous merge

Stacked PRs must use:

- `PUT /repos/{owner}/{repo}/pulls/{number}/merge-async`;
- `GET /repos/{owner}/{repo}/pulls/{number}/merge-async/{uuid}` while pending.

Merging a selected PR includes every unmerged Stack entry below it. Direct stack merge is all-or-nothing: every selected PR lands or none lands. The request accepts `sha` for the selected PR head, `merge_method`, and `merge_action` (`default`, `direct_merge`, or `merge_queue`). It returns `pending`, `merged`, `enqueued`, or `failed`.

`enqueued` is terminal only for submission. Cara must continue observing provider truth; enqueue is not proof that every PR landed. No administrator bypass exists, and native auto-merge is unsupported for Stack entries.

### What GitHub does not replace

`gh stack sync` and `gh stack rebase` are local CLI workflows. They fetch, cascade-rebase local branches, force-with-lease push them atomically, then synchronize PR/Stack metadata. GitHub does automatically rebase and retarget remaining entries after a partial native stack merge, but it does not provide a general server-side pre-merge replacement for every Cara physical rewrite.

Cara must not shell out to `gh stack sync` in its canonical worktree:

- it owns local tracking state separate from provider truth;
- it mutates multiple branches from a checkout;
- divergence can require interactive resolution;
- its rollback and receipts are not Cara's exact generation contract.

Cara should consume the documented REST API directly and retain its own fresh reads and receipts.

## Configuration contract

### `stack_type: caravan` (default)

No behavior change. No Stack endpoint is called, no capability probe is required for mutation, and repositories without GitHub Stacks behave exactly as today.

Existing axes retain their current meanings:

- `rebase_on_join`: virtual or Cara-owned physical chain;
- `sync.head_merge_actor`: Cara direct squash merge or historical provider auto-merge;
- `force_merge`, holds, CI, and repair contracts remain unchanged.

### `stack_type: github` (opt-in)

Initial supported combination:

```yaml
stack_type: github
max_caravan_length: 8 # native Stack batch bound; 2..=100
rebase_on_join: false
sync:
  head_merge_actor: caravan
```

Rationale:

- Native Stack merge cannot use provider auto-merge, so `head_merge_actor: github` is invalid.
- Native Stack mode should start from Cara's virtual base chain. Running Cara physical rewriting and a second stack rebaser creates competing branch writers and discards the main benefit.
- `caravan-force` cannot authorize native Stack merge because GitHub's async merge API does not support admin bypass. A selected group containing force intent returns a typed unsupported-policy refusal.

Future support for physical branches in a native Stack requires a separate reviewed contract; it is not inferred by accepting both settings.

Native mode is a bounded batch, not an indefinitely growing queue. `max_caravan_length` defaults to eight under `stack_type: github` and is strictly constrained to GitHub's two-to-100 entry Stack range. At capacity, deterministic admission creates or grows another Caravan rather than extending the full Stack. Sync does not wait merely for a batch to fill: it may land the maximal contiguous ready prefix at any size, while a fully ready batch of up to eight entries lands through one atomic Stack merge. The exact selected prefix is sealed against admission changes while its operation owns the repository lock.

### Rolling compatibility

`stack_type` is optional and defaults to `caravan`. Adding it advances `min_cara_version` only in repositories that opt into `github`. Older readers continue reading configs without the field; they reject opted-in configs instead of silently treating a native Stack as an ordinary caravan. An absent `max_caravan_length` preserves existing unbounded/dynamic-capacity Caravan behavior; the default of eight is applied only after explicit GitHub-backend selection, so existing repositories do not acquire a new admission limit during upgrade.

## Read model

Add provider-neutral typed evidence rather than leaking raw API JSON:

```text
StackBackendStatus
  configured: caravan | github
  capability: not_probed | available | unavailable | unknown
  provider_stack_number?: integer
  provider_stack_id?: string
  base_ref?: string
  base_sha?: CommitOid
  entries: [{ position, pr, head, state }]
  consistency: exact | absent | drifted | ambiguous | truncated
  problems: [...]
```

For `caravan`, capability stays `not_probed` and no provider Stack query is necessary.

For `github`, discovery reads Stack membership through REST (GraphQL may be a bounded fallback/read optimization). It verifies:

1. at most one Stack contains a PR;
2. Stack entries equal the complete Caravan member order;
3. positions are contiguous and bounded;
4. Stack base ref equals the repository default;
5. each PR base ref still equals the prior entry's head ref;
6. exact head OIDs and Stack base SHA match the same discovery generation;
7. merged/queued/merging entries are represented explicitly.

Unknown, partial, or conflicting provider Stack state is never treated as absence.

## Membership mapping

### New root

A one-member caravan has no GitHub Stack object (Stack creation requires at least two PRs). `new` preserves the ordinary PR and Caravan membership receipt and records `provider_stack: absent_singleton`.

### Join

A successful `join` still performs Cara's ordinary full candidate/target/generation/CI preflight and exact PR base update.

Then:

- singleton + new child: create a Stack from `[root, child]`;
- existing exact Stack below `max_caravan_length`: append the child with `/add`;
- a full Stack is not extended; normal deterministic placement considers another non-full compatible Caravan or creates a new singleton batch;
- exact retry: no-op when the Stack already contains the same ordered PR/head generation;
- partial/indeterminate response: rediscover Stack + PR truth before deciding whether to retry.

The Stack mutation receipt binds repository, old/new ordered PRs and heads, Stack number/id, base ref/SHA, actor, operation, provider request identity, and postcondition.

Do not use `gh stack link`: it can push branches, create PRs, and retarget bases before creating the Stack, which is broader than the mutation Cara has authorized.

## Reshape mapping

The initial GitHub API is append-only except for whole-unmerged-Stack `unstack`. It does not expose arbitrary remove/reorder.

Therefore `evict`, `split`, and non-tail reshape use a fenced rebuild:

1. refuse if any affected entry is merging or queued;
2. fresh-read and bind the exact Stack/member/base/head generation;
3. prove the complete desired replacement chain(s) and PR base mutations with zero writes;
4. unstack the unmerged provider Stack;
5. perform existing Cara base/label/auto-merge/physical-unwind mutations under exact preconditions;
6. create zero, one, or multiple replacement Stack objects for chains with at least two members;
7. rediscover and seal the final topology.

This is not provider-atomic. Interrupted receipts name the completed phase and rerun converges from provider truth. Never claim that native Stacks make eviction or split atomic.

`src/github/stack_reshape.rs` implements exactly those phases as
`preflighted → unstacked → reshape_applied → rebuilding → rebuilt → verified`.
The sealed plan binds the exact unqueued generation, the complete replacement
partition, and the exact per-PR base/head/control-label/auto-merge
postcondition the existing Cara reshape must establish; `provider_atomic` is a
persisted `false`. Preflight is zero-write, unstack reuses the CRUD adapter's
exact absence proof, the existing reshape receipt is bound only after fresh PR
truth proves every postcondition, replacement Stacks are created one at a time
with exact already-created retries, and final verification proves each
multi-member chain exists exactly once with singleton chains proven by exact
inventory absence.

Tail eviction can use the same rebuild initially. A narrower provider operation may replace it only when GitHub documents one.

## Sync and landing

### Ordinary convergence

Cara remains responsible for:

- graph and Stack consistency;
- compatibility evidence;
- required-run coverage and exact current generation;
- holds and force-intent refusal;
- candidacy and priority;
- webhook/tick scheduling;
- deciding the maximal contiguous ready prefix.

In virtual native-Stack mode, moving the default or a parent changes GitHub's synthetic merge candidates and may rerun CI without rewriting source heads. Cara waits for checks bound to those exact current candidates.

### Merge selection

A tick may submit only a contiguous prefix beginning at the Stack bottom. It chooses the highest entry for which every lower entry is:

- open and non-draft;
- exact and unheld;
- mechanically clean;
- fully covered by successful current-generation required checks;
- free of unsupported force intent or graph problems.

Before submission Cara re-reads the Stack and every selected PR. It binds the top selected `sha` accepted by the API and records every lower exact head even though the current API does not expose per-entry lease fields.

The absence of lower-entry lease parameters was resolved negatively by the 2026-07-31 disposable-repository sandbox. A lower fast-forward that broke ancestry failed all-or-none, but a lower rewind to an ancestor after the provider returned 202 preserved linearity and GitHub merged every selected PR at the changed lower generation. The merge API therefore does not snapshot or lease the complete group, and post-merge `indeterminate` detection is not prevention.

The 2026-08-01 follow-up proved a preventive equivalent: one active repository ruleset with no bypass actors and exact selected refs, containing `update` and `deletion` restrictions, rejected both repository-owner SSH pushes and owner-authenticated REST ref mutations while direct Stack merge succeeded and GitHub rebased the unselected suffix. Cara must acquire and exactly read back that ruleset, re-read the complete Stack, keep the lock through terminal UUID proof, and release only its exact ruleset generation. This path requires Administration(write), remains explicit, and never changes default Caravan permissions; see `docs/validation/github-native-stack-sandbox-2026-07-31.md`.

### Async transaction

1. Persist a pre-submit intent checkpoint.
2. `PUT merge-async` with `merge_method: squash`, configured merge action, and exact top head SHA.
3. Persist `uuid`, expected head, selected entries, and initial status before polling.
4. Poll under the one tick deadline with bounded cadence.
5. On `merged`, fresh-read default, every selected PR, and remaining Stack entries. Prove all selected PRs merged and remaining entries were rebased/retargeted exactly.
6. On `enqueued`, return queue-owned state and observe later; do not claim merge.
7. On `failed`, return the provider message plus unchanged/partial provider proof. The documented direct operation is atomic, but Cara still verifies.
8. On timeout/transport ambiguity, rediscover before any retry. The uuid result lasts 24 hours.

Receipts distinguish `submitted`, `pending`, `enqueued`, `merged`, `failed`, and `indeterminate`.

## Force, holds, and repair

- Holds block any native merge group containing the held caravan.
- `caravan-force` is unsupported for native Stack merge until GitHub documents an audited bypass. Return `github_stack_force_unsupported`; never silently drop force intent or fall back to a legacy merge endpoint.
- Legacy synchronous merge APIs and GraphQL `mergePullRequest` are invalid for Stack entries.
- Repair remains a Cara workflow. A repaired head invalidates prior Stack merge intent and requires fresh Stack/PR/check discovery.

## Capability and rollout

### Capability detection

Read-only `GET /stacks?per_page=1` under configured `stack_type: github`:

- success: available;
- documented feature-unavailable 404: unavailable;
- auth/rate/transport/partial response: unknown, not unavailable.

No mutation is used as a capability probe.

### Phases

1. **Schema and read-only status** — optional enum, default preservation, capability and Stack discovery, drift diagnostics.
2. **Stack create/add** — membership adapter with exact idempotent receipts; no merge.
3. **Async merge preview** — plan and preflight only; expose selected prefix and lower-head lease gap.
4. **Sandbox direct merge** — completed with a negative lower-lease result; atomic failure, success, partial merge, squash results, remaining-entry rebase, and both fast-forward/rewind lower-head races are recorded in `docs/validation/github-native-stack-sandbox-2026-07-31.md`.
5. **Opt-in merge** — permitted only after the proven exact-ref no-bypass ruleset lock is fully wired, durably checkpointed, and exposed as an explicit Administration(write) opt-in; allowlisting alone is insufficient.
6. **Reshape rebuild** — unstack/recreate for evict and split.
7. **Merge-queue adapter** — separate acceptance; `enqueued` is not direct atomic landing proof.

Each phase leaves `stack_type: caravan` untouched.

## Compatibility matrix

| Concern | `caravan` | `github` initial contract |
|---|---|---|
| Default | yes | no, explicit opt-in |
| Candidate priority | Cara | Cara |
| Compatibility policy | Cara | Cara |
| PR base chain | Cara | Cara, then provider Stack verifies |
| Routine source rebase | optional Cara physical mode | disabled initially; virtual merge candidates |
| Stack metadata | labels/base refs | GitHub Stack + Cara labels/base refs |
| Root merge | existing Cara/provider actor | Cara invokes async Stack merge |
| Multi-PR merge | serial bounded roots | provider atomic contiguous prefix (direct action) |
| Admission batch bound | existing dynamic mutation-budget capacity | default 8 (`max_caravan_length`, 2..=100) |
| Auto-merge | existing historical option | unsupported |
| Force/admin bypass | audited Cara policy | unsupported initially |
| Holds | Cara | Cara, before submit |
| Evict/split atomicity | Cara fenced mutations | unstack/rebuild, resumable but not atomic |
| Provider queue | existing configured lane | separate future adapter; enqueue != landed |
| Repositories without Stacks | unaffected | typed unavailable refusal |

## Sandbox acceptance cases

Use disposable same-repository branches and never production Caravan PRs.

1. Create two-, three-, and eight-entry Stacks; verify REST/GraphQL/webhook identity and that a ninth candidate is routed to another batch.
2. Repeat create/add after ambiguous response; prove idempotent convergence.
3. Move a lower head between preflight and async submit; prove all-or-none behavior and determine whether group generation is snapshot-bound.
4. Move the selected top head; verify `sha` rejects.
5. Make one lower PR red, held, draft, conflicting, or branch-rule-incomplete; prove none merge.
6. Direct-squash merge the full Stack; verify every PR and default commit/tree receipt.
7. Merge only a prefix; verify remaining PRs are rebased/retargeted and checks become stale/fresh as documented.
8. Timeout after submit, recover by uuid and provider truth without duplicate submission.
9. Enqueue through merge queue; prove terminal enqueue handling and later final-state observation.
10. Unstack with open entries and with a queued/merging entry; verify retained entries and rebuild refusal.
11. Repository without feature: read-only unavailable, zero writes.
12. Run the full existing `stack_type: caravan` suite with the field absent and explicit to prove no behavior drift.

## References

- GitHub Stacked PRs public preview announcement, 2026-07-30.
- `github/gh-stack` v0.1.0 release notes.
- GitHub Stacks REST, GraphQL, and async Merge API references published with `github/gh-stack`.
- `gh stack link`, `sync`, `unstack`, and `merge` command contracts.
