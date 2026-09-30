# History-preserving owner continuation (no force)

This is a source-owner handoff, not another queue writer or an authorization transfer.
Read live help and establish the current repository, source owner/custody, permitted
scope, holds, and operation outcomes first. An actor string is audit/custody binding,
not proof that another owner's PR is yours. Reconcile earlier uncertain publication
or native operations before starting another operation. A later zero-write refusal
does not prove an earlier timed-out writer had no effects.

## Actual supported interface

An authorized owner can prepare an exact, isolated merge with:

```sh
cara repair start --pr "$PR" --target-pr "$PARENT" \
  --non-force --actor "$SOURCE_OWNER" --reason "preserve authored merge history"
cara repair status --session "$SESSION"
cara repair continue --session "$SESSION" --actor "$SOURCE_OWNER" --no-sync \
  --validate "$REVIEWED_TARGETED_CHECK"
```

For MCP, use the distinct `repair_start_non_force` tool with `pr`, optional
`target_pr`, `actor`, and `reason`, then `repair_continue` with the same actor and
`no_sync: true`. An older server must reject that unknown start tool: never fall
back to sending an unfamiliar flag to legacy `repair_start`, which old decoders
may ignore. There is no force switch on the distinct tool.

Omit `--target-pr` to merge the freshly observed default branch. Obtain session and
workspace from the actual start receipt; the example variables are not a plan.
Resolve and stage only authorized conflicts in that workspace. A clean merge needs
no edits. Do not commit, update refs, or push the managed workspace manually.
Semantic grants and broader agent edits still need their existing explicit authority.

The new merge retains the exact original head and target as its two parents; the
old commits, authored merges, and their parent cardinalities remain unchanged.
This is not a rebase, flattening, squash-equivalence rewrite, or guard relaxation.
The publisher uses `git push --no-force` for one verified object/ref, never a plus
refspec or force-with-lease, and never pushes tags or submodule refs.

## Semantic-only correction of an already-contained target

When a reviewed source correction is needed but the source **already contains**
the current default/predecessor, explicitly select the distinct semantic purpose:

```sh
cara repair start --pr "$PR" --non-force --semantic-only \
  --actor "$SOURCE_OWNER" --reason "apply reviewed source correction"
# Use the exact returned session/workspace. Apply reviewed repair grant paths,
# or authorize-agent-edits before staging bounded repository-content changes.
cara repair continue --session "$SESSION" --actor "$SOURCE_OWNER" --no-sync \
  --validate "$REVIEWED_TARGETED_CHECK"
```

Optional `--target-pr` still names the actual freshly observed predecessor; never
invent one to make a merge possible. This mode proves target containment and
starts **no merge**. Explicit semantic grants or existing agent-edit authorization
are still required; the start actor/reason alone grants no content-edit scope.
A nonempty authorized correction becomes a **one-parent** descendant of the exact
old source head. The target is an ancestry/lease binding, not an invented second
parent. Normal no-force publication and all continuation fences below still apply.

MCP callers must use `repair_start_semantic_non_force` (`pr`, optional `target_pr`,
`actor`, `reason`). There is no purpose/force toggle on that tool; never fall back
to `repair_start_non_force` or `repair_start` on an older server. Check the installed
CLI help or server tool inventory before using it: merged source is not installed
capability. Version-3 semantic manifests use `non_force_semantic_v1`, which older
merge-mode readers and cleanup cannot decode.

Before committing, Cara durably records the **authorized tree** after scope/blob
verification. Recovery after a commit/checkpoint interruption requires that exact
nonempty tree and sole original-head parent; a manually created commit with merely
the right parent is not proof of authorization. Changed bytes, empty corrections,
ungranted paths, dirty files, stale generations and failed validation refuse.

There is still one session directory per PR. A previous refused merge start can
leave a preserved `Preparing` record even without a successful start receipt.
A new semantic start must not relabel or overwrite it. Inspect the exact existing
session and preserve unique work; use supported explicit owner-reviewed local
cleanup only when appropriate before starting the new purpose. Never edit manifests,
guess a new generation, or discard an unresolved publication intent.

## Boundaries and refusals

- The durable session binds the exact actor/reason, repository/PR/ref, source and
  target generations, provider creation/update evidence, control metadata, config,
  workspace, and source-edit grants. Fresh source/parent/default observations,
  exact live ref checks, purpose-specific parent verification, and the existing local/remote
  writer guards remain mandatory. Changed/reopened/draft/foreign/unknown facts
  refuse; a blocked read is not permission to publish.
- `continue` requires the same actor **and `--no-sync`**, including saved-output
  replay. This intentionally leaves queue/topology/CI work to the existing actor.
  Fresh CI is required; a source publication is not membership, readiness, or merge.
  `--no-sync` limits this invocation only: it does not pause or reconfigure another
  scheduled writer. Establish the permitted sequencing with the existing actor;
  this session creates no implicit queue hold or no-force policy for other actors.
- A publication-attempt marker and validation receipts are persisted before push.
  After response loss, only the exact observed prepared successor can confirm the
  historical source postcondition without another push, even if the default has
  subsequently advanced. Readback reuses stored validation; it cannot attest new
  validation commands or current queue readiness. The unchanged old ref is **not** proof
  of no prior write: `repair_non_force_publication_unresolved` preserves evidence
  and refuses retry and `repair abort`. There is no automatic clear-intent shortcut;
  return the original writer/provider evidence to the existing responsible actor.
- Legacy sessions retain their recorded force-with-lease semantics. Starting with
  `--non-force` cannot relabel or upgrade an existing legacy/other-owner generation.
  `repair status` exposes the actual stored `non_force` policy and attempt marker.
  Version-2 manifests use an explicit wire-state discriminator so older readers,
  including cleanup paths, refuse rather than ignoring the policy. Never edit a
  manifest or replace it with status/API JSON to work around that refusal.
- Without `--semantic-only`, an already-contained target still returns
  `repair_non_force_already_contains_target` and preserves the workspace. No merge
  is needed. Return stale topology evidence to the designated actor; only an actual
  reviewed semantic correction justifies the explicit semantic purpose above.
  Do not invent a source rewrite to clear a stale provider representation.
- After a parent changes, rediscover the child and its actual current parent.
  Neither a handoff fingerprint nor an old parent's successful receipt is a lease
  for the next child. Preserve held/unpublished work and independent operation history.

## What remains forceful or separately authorized

**Explicit native `rebase-apply` and legacy `repair continue` publish
force-with-lease.** `rebase_on_join: false` is not a global no-force switch for
those explicit paths. They are not a non-force fallback; first-party names do
not change their effect.

Ordinary sync no longer invokes `auto_apply_from_status`: the automatic native
source-rebase publisher was removed, not wrapped in another receipt or approval
gate. Native ancestry drift preserves source heads and returns the existing
backend diagnosis to the source owner. This source correction does not prove an
older installed runtime has changed, erase earlier effects, or authorize replay
of a historical operation. Normal landing and membership recovery remain separate.

**There is no standalone sealed tail-eviction preview CLI.** `cara plan` exposes
sync/concat, not an eviction preview, and `cara evict` has no dry-run flag. Some
partial-prefix decisions carry a sealed reshape plan. Only that exact fresh
receipt, actual authority, and the typed evict path's internal preflight/checkpoint
can authorize its separate mutation. Tail eviction preserves source but may still
refuse native generation, lock, or authority checks. Never run evict merely to
inspect, reuse a historical canary scope, discard descendants, or invent raw
label/base/split operations to free a prefix.

If neither authorized owner continuation nor a currently supported queue-only
route is available, return the precise refusal, affected ready prefix, custody or
capability gap, and one bounded decision to the existing actor. Do not replace that
refusal with unchanged polling, another controller, or a force/replay workaround.
