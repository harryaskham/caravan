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
| A required check has **failed** | **skipped, queue advances** | the PR's owner |
| A required check is still **running** | admitted; the tick then waits | nobody — automatic |
| Owner rejected it via `cara check --pr N` | **stays canonical, blocks** | the owner |
| Behind the default branch but compatible | admitted; Cara rebases it | nobody — automatic |
| Your checkout is on a merged/historical branch | irrelevant to selection | nobody — automatic |
| Forge reports `mergeStateStatus: BLOCKED` | **irrelevant to admission** | see §1.4 |

The distinction in the first two rows is the important one. A mechanical
conflict is not a decision anyone made, and no rerun resolves it, so holding the
whole queue behind it starves every clean candidate. An owner rejection *is* a
decision, and silently leapfrogging it would discard that decision.

A skipped candidate appears in `admission.skipped` with the exact reason, so
"skipped" is never indistinguishable from "forgotten".

### 1.4 Red bars admission; pending does not; the forge verdict is not consulted

A **failed** required check bars admission outright. Queueing behind red is
guaranteed rework: the failure has to be fixed, fixing it rewrites the head, and
every member stacked behind is rebased anyway. Admitting a red candidate buys
nothing and costs the whole tail a re-stitch, which is the cost the queue exists
to avoid. `caravan-force` remains the audited override.

A **running** check is not a failure. Refusing on pending would stall every
candidate for the duration of its own CI.

A **missing or empty** conclusion is `Unknown`, and Unknown is grouped with
failures rather than successes. An absent result is not a passing result.

The forge's own `mergeStateStatus` is deliberately **not** consulted for
admission, and that is measured rather than assumed. On live data a clean,
correctly-elected candidate reported `BLOCKED` identically to a red one, because
the forge returns `BLOCKED` whenever branch protection is unsatisfied — including
required checks that are merely still running. Gating admission on it would
refuse every candidate whose CI had not finished. It is consulted only at merge
time (§3.1), where the checks are already proven green, so a forge that still
declines knows something Cara does not.

A hold that is not expressed as pull request state is not a hold. Draft status
and the `caravan-join-skipped` label are visible to Cara; an intention is not.

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
4. The forge does not itself refuse the merge.
5. The merge commit is an **ancestor of freshly fetched main** afterwards.

Condition 4 is where `mergeStateStatus` is consulted, and the only place it is.
By this point the required checks are already proven green, so `BLOCKED`,
`DIRTY` or `DRAFT` here means an unsatisfied protection rule, a required review,
or a rule added since discovery — something Cara does not otherwise know.
Attempting the merge anyway just produces a confusing provider error. The
refusal is typed `forge_refuses_merge`.

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
`tests/hook_example.rs` pins the behaviour. Because that target exercises real
shell/process boundaries, Cargo feature `environmental-hook-acceptance` keeps it
out of ordinary Nix installs and blocking pull-request/release checks. The
scheduled/manual `Environmental hook acceptance` workflow runs the assertions
unchanged and reports one bounded failure event through the configured feedback
webhook. Identical runs carry the same stable fingerprint; shared receiver-side
collapse is tracked by Cacophony `bd-52cf42`.

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

## Resolved, and how they were found

Every gap this document originally listed was found by an operator hitting it in
normal use, not by a test. That is the point of writing the tree down.

### The failure mode worth remembering

Six separate defects here were one mistake repeated: **a rule applied at one site
and not at its siblings**, with every test still green because each site was
tested alone against inputs the test constructed itself.

The worst instance: the producer moved from `Incompatible` to
`CandidateIncompatible`, and the consumer guard kept comparing against the old
variant. The head-of-line skip silently stopped working for four releases. Its
unit test kept passing because it *injected* a problem carrying the old kind, so
it exercised a path production could no longer reach.

Three things that actually caught these, in order of usefulness:

1. **Test the composition, not the unit.** A test that builds its own inputs can
   certify a disconnected path indefinitely. The bootstrap failure was found by
   writing a test for the operator's real shape — empty fleet, several unqueued
   PRs, the leading one conflicting — rather than the one-clean-candidate shape
   the existing test used.
2. **Make the compiler ask the question.** Classification now lives in exhaustive
   `match` (`GraphProblemKind::blocks_fleet`), so a new variant fails to build
   until someone decides what it means. Note this only helps at call sites that
   *use* the classifier: a `_ =>` wildcard is the compiler being told not to
   help, and one such wildcard was the fifth site.
3. **Verify the mechanism in situ, not in isolation.** A mechanism reproduced
   standalone may be unreachable in place. One reported bug was real in a
   scratch script and impossible in the actual hook, because an input bound
   several lines earlier made it so.

And one for measurements: a number taken from a degraded machine describes the
machine, not the platform. A worktree timing that "proved" macOS could not
physically rebase failed to reproduce by 3-22x once the node was healthy.

- **Selection ignored compatibility.** A candidate proven unable to merge was
  still elected, holding the whole queue. Detection had been added without
  wiring it into selection, so Cara named the problem and then ignored it.
  Fixed: a proven conflict is skipped with its exact reason and the queue
  advances, while an owner's explicit rejection still blocks.
- **An explicit skip looked inert.** Admission excluded a `caravan-join-skipped`
  PR correctly, but the detection loop kept re-proving and re-reporting it, so
  the label appeared to do nothing.
- **A historical checkout blocked the queue.** Fleet reads failed closed because
  the local branch was merged or ambiguous — a state Cara produces itself by
  retiring merged heads, so the tool penalised its own happy path. Fixed by
  making `current_pr` resolution strict only for PR-scoped mutations. That is
  not a safety relaxation: every such command resolves its subject as
  `input.pr.or(current_pr).ok_or(current_pr_not_found)`, so an unresolved branch
  refuses to act rather than acting on a guess.

## Composing repair loops

A scheduler that cannot host one long-lived `cara loop` reconstructs its routing
with `cara queue`:

```sh
cara --json queue --status conflict,evicted,skipped
```

It reports the first match across the requested positions, in the order asked
for, plus every match so a caller can plan more than one step. Positions are
`ready` (the canonical admission candidate, identical to what sync admits),
`skipped`, `conflict`, and `evicted`.

**Nothing matching is an ordinary payload, never an error.** A cron must be able
to tell an empty queue from a provider outage; an error envelope for both makes
an outage look like quiet success.

`--checkout` additionally moves the working tree to the selected branch and
returns the same JSON plus a receipt naming the move. It routes through
`ensure_safe_worktree` and **refuses on a dirty tree**, because a scheduler that
clobbers uncommitted work is worse than one that does nothing.

`conflict` is read from proven graph evidence rather than a label, so a stale
label can never route an agent at a PR that has since been force-pushed onto a
clean generation.

The verb is `queue`, not `next`: `cara next` already means "check out the next PR
toward the current caravan tail", and overloading it with queue selection would
give one word two unrelated meanings.
