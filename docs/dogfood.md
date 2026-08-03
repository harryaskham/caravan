# Dogfooding Caravan with Caravan

Caravan's own pull requests are the reference deployment for physical chains,
priority/FIFO admission, bounded automatic growth, repair decisions, and the
Cacophony `pr_cara_join` handoff. GitHub remains authoritative. Cacophony owns
worker lifecycle and submits PR generations; Cara owns queue topology and merge
admission.

## Rollout gates

Do not activate the Cacophony `projects.caravan` Cara queue until all of these
are true:

1. Cacophony's managed runtime is pinned and verified as Cara v0.0.7 or newer on
   Darwin and Linux.
2. This repository's `.caravan/config.yaml` and unfiltered stacked-PR CI trigger
   are on `main`.
3. `cara init` reports every exact control/priority label ready, including
   `caravan-join-skipped`.
4. A disposable canary completes `pr_cara_join` → non-main-base CI → `sync
   --all` → merge → bead close, with immutable Cacophony and Cara receipts.

Until the managed v0.0.7 pin is fleet-wide, the config deliberately avoids
raising `min_cara_version`; raise that floor in a follow-up only after old
readers are gone.

## One-time repository convergence

Use a stable v0.0.7+ binary and an authenticated account with repository
administration permission:

```sh
cara --version
cara config check --config .caravan/config.yaml --json
cara status --json
cara init --json
cara status --json
```

`init` is the explicit mutating step. It verifies branch protection and exact
label metadata, then creates only missing labels. Never hand-create or silently
rewrite mismatched labels.

The CI workflow intentionally has no `branches` or `branches-ignore` filter and
listens for `opened`, `synchronize`, `reopened`, `edited`, `labeled`, and
`unlabeled`. A physical child targets its predecessor branch; filtering CI to
`main` would strand that child without current-generation evidence.

## Normal Cacophony worker handoff

After the Cacophony project configuration is activated, Caravan workers use the
mode-less managed lifecycle:

```sh
caco agent reintegrate --id "$CACO_AGENT_ID"
```

The project selects `pr_cara_join`. The worker must not override it to direct,
`pr_review`, or `pr_auto_merge`. A successful handoff creates/reuses one exact
PR generation and atomically asks Cara to append it. The author then moves on;
Cacophony routes CI or Cara failures back to the recorded owner and closes the
bead only after the PR actually merges.

Same-repository consumers execute Caravan's checkout-owned runtime prefix,
`scripts/cara-runtime.sh --source system --`, never a sibling Cacophony script.
The wrapper's rolling minimum-version sentinel, bounded typed failures,
resolution fingerprint, consumer metadata, validation, and rollback contract
are documented in [`cara-runtime.md`](cara-runtime.md). Activating that consumer
remains a separate Cacophony configuration/rollout change.

For an operator-selected PR, explicit membership is authoritative after exact
mechanical checks:

```sh
cara check --pr 123 --tail-pr 120 --json
cara join --pr 123 --tail-pr 120 \
  --reason 'Operator selected this exact PR and tail' --json
```

An explicit join does not need a priority label merely to override automatic
FIFO order. `--priority-label` is used only when deliberately persisting that
PR's future automatic-admission rank.

## Planning and convergence

Review the zero-write plan before consequential or unfamiliar changes:

```sh
cara plan sync --all --json
cara sync --all --json
```

`sync --all` first converges existing chains. Only then may the checked-in
`join_unlabelled_prs` policy consider unqueued, non-draft PRs. Configured
priority labels sort first; ties and unlabelled PRs use immutable GitHub
`createdAt` FIFO. Candidate, mutation, GitHub-request, and wall-clock bounds are
hard limits. An incompatible exact generation receives
`caravan-join-skipped` plus a generation-bound receipt; a changed candidate,
default, tail, config, or heuristic invalidates it.

Treat the structured continuation literally:

- `healthy`, `waiting_ci`, and `held` are normal no-wake states.
- `retry_tick` means rediscover later; do not wake a repair actor or hot-loop.
- `external_decision` means the unchanged evidence needs a human or repair
  choice, not another identical retry.
- Preserve partial branch/provider receipts after interruption and rerun only
  after checking true GitHub state.

The dashboard is an observation and typed-action surface, not separate queue
authority:

```sh
cara web --repo . --listen 127.0.0.1:4774 --open
```

## Holds, repairs, and force intent

Use first-party decision surfaces rather than raw Git surgery:

```sh
cara pause --head-pr 120 --actor "$USER" --reason 'incident investigation'
cara resume --head-pr 120 --actor "$USER"
cara repair start --pr 123
cara repair status --session SESSION
cara repair continue --session SESSION --actor "$USER"
```

`force_merge: true` permits a one-shot admin squash only after exact
head/default compatibility, permission, policy, and audit checks. Force intent
is bound to one head generation and is invalidated by a Cara-owned rewrite. Arm
or revoke it only through the audited typed interface, then let sync consume it:

```sh
cara force --pr 120 --actor "$USER" --reason 'known acceptable CI failure'
cara force revoke --pr 120 --actor "$USER" --reason 'intent withdrawn'
cara sync --all
```

`caravan-forced` is not a recognized label. Force never bypasses a textual
conflict, stale provider fact, hold, ownership check, or permission. Likewise,
change automatic order through `cara priority set|clear`, not raw labels:

```sh
cara priority set --pr 123 --label caravan-priority:high \
  --actor "$USER" --reason 'operator scheduling decision'
cara priority clear --pr 123 --actor "$USER" --reason 'return to FIFO'
```

Priority changes scheduling only and never authorizes membership or bypasses
compatibility. The dashboard exposes these same typed operations with mandatory
actor/reason confirmation.

## Every gap becomes work

Dogfooding is successful only when friction is retained as evidence instead of
normalized into manual workarounds:

1. Preserve the complete Cara/Cacophony error, PR numbers, OIDs, completed
   prefix, scheduler disposition, and exact command.
2. Search all Caravan bead statuses before filing.
3. File the smallest reproducible Caravan bead for Cara behavior, or a
   Cacophony bead for lifecycle/config routing.
4. Record any temporary operator workaround and its rollback.
5. Keep the original bead open until the fix is on `main`; blocked work is
   dependency-linked and unclaimed, never falsely closed.

## Rollback

Rollback is configuration-first and never rewrites branches backward:

1. Stop new `pr_cara_join` handoffs while preserving every existing PR and
   owner/generation receipt.
2. Revert the Cacophony Caravan project to its prior `pr_auto_merge` policy and
   refresh persistent agents from landed source config.
3. Set `sync.actions.join_unlabelled_prs: false` to stop automatic growth.
4. If needed, set `force_merge: false` and `rebase_on_join: false` for future
   operations. Do not force-push old generations back.
5. Drain or explicitly reshape already-enrolled caravans from fresh GitHub
   facts, then document the failed canary and file the responsible gap.
