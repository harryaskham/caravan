# The life of a caravan

This is the decision tree Cara actually walks, written so an operator can locate
their situation in it and know what happens next, and so a scheduler author can
see exactly which states are theirs to act on.

Two rules explain most of the tree:

- **Fleet decisions never depend on your checkout.** Which PR is next, and
  whether a caravan can advance, are properties of the provider graph. Where
  your working tree happens to sit is only used to infer *which PR you mean*
  when you omit `--pr`.
- **Refusals are typed by who can resolve them.** A refusal no rerun can fix is
  not the same as a bounded race, and Cara says which it is rather than making
  the reader guess.

---

## 1. Admission: how a PR becomes a candidate

```
open PR
  ├── draft?                        → not a candidate (drafts are never admitted)
  ├── labelled caravan?             → already a member; see §2
  ├── labelled caravan-evicted?     → evicted; see §5
  ├── labelled caravan-join-skipped?→ skipped this generation; see §1.2
  └── otherwise                     → UNQUEUED: an admission candidate
```

### 1.1 Ordering

Unqueued candidates are ordered by **explicit priority label, then FIFO** on the
provider's immutable `created_at`, with PR number as a deterministic tie-break
when a timestamp is missing.

Priority labels order *automatic* selection only. They are not membership
authority: explicit `cara join --tail-pr N` already carries owner intent, and a
priority label never overrides it.

### 1.2 Skips are generation-bound

A skip is recorded against the **exact head** it was proven against
(`AutoJoinSkipReceipt`). Force-push the branch and the skip no longer applies —
the candidate re-enters the queue on its new generation. A skip is therefore a
statement about one exact set of objects, never a permanent verdict about a PR.

### 1.3 What blocks the front, and what does not

| Condition | Front of queue? | Who resolves |
|---|---|---|
| Mechanically conflicts with the default branch | **skipped, queue advances** | an agent or human, out of band |
| Owner rejected it via `cara check --pr N` | **stays canonical, blocks** | the owner |
| Behind the default branch but compatible | admitted; Cara rebases it | nobody — automatic |
| Your checkout is on a merged/historical branch | irrelevant to selection | nobody — automatic |

The distinction in the first two rows is the important one. A mechanical
conflict is not a decision anyone made, and no rerun resolves it, so holding the
whole queue behind it starves every clean candidate. An owner rejection *is* a
decision, and silently leapfrogging it would discard that decision.

---

## 2. Membership: creating and extending a caravan

```
cara new                     → one-PR caravan; base is the default branch
cara join --tail-pr N        → append after an existing tail
cara renew / cara rejoin     → same, for a previously evicted PR
```

Every one of these physically rebases when `rebase_on_join` is set. `cara new`
is not exempt: a root candidate behind the default branch is brought up to date
as part of creation.

### 2.1 Roots flatten, members replay

A **root** is squash-merged by Cara, so its history is discarded at landing.
Replaying its merge topology therefore proves nothing, and a root whose branch
merged the default branch into itself is **flattened** onto the already-proven
merge tree instead of replayed.

A **member** behind a tail is never flattened: its ancestry must physically
follow the chain, so an unauthorized merge-preserving replay fails closed.

### 2.2 Refusals before any mutation

- Empty source (no effective patch beyond the target) is refused *before* a PR
  is created, so a no-op candidate never becomes a real PR.
- A reused branch name is not identity: a base OID is bound to the resolved
  parent head, so a recycled branch cannot silently reparent a child.

---

## 3. Synchronization: the tick

One `cara sync` tick is bounded and idempotent. It either converges or returns
exactly one structured decision point.

```
discover  → plan rebases → apply under exact leases
          → rediscover   → converge provider state
          → rediscover   → merge the root if it is green
```

Every pass emits one compact receipt naming the verb and the counts it saw, so
"the loop is running and declining to join" is distinguishable from "the loop is
not running at all".

### 3.1 Merging the root

Cara merges the root only when all of these hold:

1. The root's base **is** the default branch.
2. Required checks are green on the **exact current head**.
3. The cumulative merge tree is proven unchanged.
4. The merge commit is an **ancestor of freshly fetched main** afterwards.

An absent proof is never permission. Ordinary waits — pending checks, an
unproven tree, a spent per-tick allowance — are visible no-op steps, not
failures.

### 3.2 Who merges

`sync.head_merge_actor` selects the actor and defaults to `github` so an upgrade
never silently changes who merges. Under `caravan`, Cara performs the merge and
provider-native auto-merge is **not** a precondition anywhere: not for sync, not
for head creation, not for eviction promotion, not for `cara init`.

---

## 4. Failure classification

Every refusal carries a `wake_class` telling a scheduler what to do:

| wake_class | Meaning | Scheduler action |
|---|---|---|
| `none` | healthy tick | nothing |
| `retry_tick` | bounded race; fresh state resolves it | rerun; dispatch nothing |
| `external_decision` | cannot resolve itself | dispatch one agent |
| `operator_action` | config, permission, or checkout work | notify a human |

Deduplicate on `decision_fingerprint`, not the per-emission event id: the
fingerprint is stable while the same decision remains unresolved, so a caravan
stuck for an hour produces one bead rather than one per cron tick.

`examples/hooks/caco-bead-dispatch.sh` implements exactly this, and
`tests/hook_example.rs` pins the behaviour.

---

## 5. Eviction and repair

```
cara evict --pr N --reason "..."
  ├── N is the head    → the successor is promoted; it becomes the new root
  ├── N has descendants→ each is replayed after the evicted head, dropping
  │                      exactly that patch and preserving each owner's work
  └── --cascade / --all→ bounded tail-first removals, never a graph rewrite
```

Eviction refuses only if it *introduces* a problem the fleet did not already
have. A tail eviction re-links no edge, so it can never introduce one and is
always allowed.

Every descendant rewrite is proven before any is published: a descendant that
cannot be unwound cleanly leaves the whole stack untouched rather than half
unwound.

---

## Open gaps

Written down because a lifecycle document that only describes the happy path is
how the gaps below survived as long as they did.

1. **Selection does not consult compatibility.** §1.3 describes the intended
   behaviour. Today a mechanically conflicting candidate is *detected and
   reported* but still elected, so it holds the front anyway.

   Exact seam: `read.rs::next_candidate` orders by priority-then-FIFO alone and
   never reads `analysis.fleet.problems`. The rule it must not break sits
   directly above the selection: *"A rejected first attempt remains canonical
   and therefore cannot be silently leapfrogged"* — that protects an **owner's**
   `cara check --pr N` rejection, which is a decision someone made. A mechanical
   conflict against the default branch is not a decision and no rerun resolves
   it, so the two must be separated rather than sharing one blocking rule.

   Correct shape: a proven mechanical conflict persists a generation-bound
   `AutoJoinSkipReceipt` and selection advances; an owner rejection keeps
   blocking exactly as today.
2. **Historical checkout blocks queue advancement.** §"Fleet decisions never
   depend on your checkout" is the intent. Today a checkout left on a merged
   branch can fail closed with `missing_caravan_label` even after the next
   candidate has been computed — and Cara creates that state itself by retiring
   merged heads.

   Exact seam: `GitHubDiscovery::resolve_current_pr` (`src/github.rs`, the call
   to `resolve_historical_current_pr`) propagates
   `DiscoveryError::HistoricalCurrentPullRequest` with `?`, and `src/read.rs`
   converts it to a hard `AppError`. Because `next_candidate()` calls
   `status()`, which calls discovery, resolving the *current branch* fails the
   entire *fleet* read. The operator sees `candidates: [2234]` already computed
   in the same payload that refuses.

   The naive fix looks wrong at first glance. Several of those refusals are
   deliberate safety: `deleted_historical_branch_fails_closed`,
   `closed_unmerged_historical_branch_fails_closed` and
   `exact_open_reuse_refuses_conflicting_historical_membership` encode the
   reused-branch provenance rule, where a recycled branch name must never be
   allowed to reparent a child.

   On inspection the safety property survives, because **`None` is already
   fail-closed at the command layer**. Every PR-scoped command resolves its
   subject as `input.pr.or(status.current_pr).ok_or_else(...)` and refuses with
   `current_pr_not_found` when neither is available (see `read.rs::check`). So a
   discovery that declines to guess yields a refusal to mutate, not a mutation
   of the wrong PR. No command silently substitutes a different subject.

   That means the `DiscoveryOptions` gate is about preserving **diagnostic
   quality**, not safety: without it the operator loses the precise reason
   (`branch_reuse_ambiguous`, `historical_membership_conflict`, ...) and sees
   only "no current PR, pass --pr". Worth keeping for that reason alone, but the
   change is far less dangerous than the fail-closed test names suggest.

   Correct shape: keep the resolution strict for PR-scoped **mutations** and
   lenient for **selection**, gating on `DiscoveryOptions` in the same way
   `allow_unlabelled_historical_pr_creation` already is. Read-only fleet
   commands set it off, get `current_pr: None` plus a retained reason, and any
   command that genuinely needs "which PR do you mean" raises the typed error
   naming that reason. The three tests above then assert "resolves to no current
   PR, and mutation refuses" rather than "discovery errors".
3. **No status-filtered navigation primitive.** There is no supported way to ask
   "what is the next conflicting / evicted / failing PR", so scheduler authors
   cannot compose repair loops without reimplementing selection.

   Agreed shape: one verb, `cara next --status <set>`, JSON-only, replacing
   `cara next-candidate` outright rather than aliasing it. Selection under
   `--status ready` must be byte-identical to what sync admits, or two surfaces
   will give agents different answers.

   `--checkout` is an opt-in convenience on `cara next` and `cara loop
   --manual`: it still returns the same JSON but moves the working tree to the
   selected branch. Two constraints are load-bearing. It must route through
   `navigation::ensure_safe_worktree` and **fail closed on a dirty tree**, so a
   cron can never clobber uncommitted work; and the JSON must carry a receipt of
   what it moved *from* and *to*, so the move is auditable and reversible.

   `--status skipped` must re-derive validity rather than trusting the label,
   because a skip is bound to the exact head it was proven against; a stale
   label would otherwise route agents at PRs that have since been force-pushed.
