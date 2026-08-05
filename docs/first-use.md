# First use

1. Run `cara status`. This is read-only and reports whether local configuration
   and required labels are ready.
2. If not ready, run `cara init` (or MCP tool `init`). This explicit operation:
   - atomically creates an absent `.caravan/config.yaml` with safe version-1
     defaults and the running release as `min_cara_version`, using create-new
     semantics;
   - preserves every existing config byte and rejects invalid existing config;
   - verifies repository `WRITE` (or stronger) permission, squash auto-merge,
     and protected default-branch check/review policy;
   - creates missing canonical labels and verifies every result.
3. If GitHub REST core budget is too low for every bounded label create/reread,
   init returns `github_rest_rate_limit_wait` before provider mutation. Use its
   exact reset/delay and pending/completed-label receipt; do not hot-loop. A
   verification-only init with all exact labels present skips this probe.
4. Resolve a reported metadata mismatch manually. Cara never overwrites an
   operator-owned label. Rerun `cara init`; retries and concurrent creation are
   safe and converge by exact re-read.
5. Run `cara status` again, then use `new`, `join`, or `sync`.

Before merging any config/pin update, run the exact pinned binary as
`cara --config .caravan/config.yaml config check`. This command only reads and
strictly parses the config; its receipt records `provider_mutated: false`. A
policy section introduced by a newer release must raise `min_cara_version` in
the same change that advances the pin. Existing configs without the gate remain
legacy-compatible, while unknown or misspelled policy keys are still rejected.

Canonical labels:

| Name | Color | Description |
| --- | --- | --- |
| `caravan` | `5319E7` | `Active member of a Caravan PR chain` |
| `caravan-evicted` | `B60205` | `Removed from a Caravan chain pending renew or rejoin` |
| `caravan-force` | `D93F0B` | `Allow configured force handling for known CI failures` |
| `caravan-closed` | `6E7781` | `Closed without merge; terminal Caravan provenance outside queue capacity` |
| `caravan-join-skipped` | `6F42C1` | `Generation-bound best-effort automatic admission skip` |

The first four labels are always required. `caravan-join-skipped` is required
and initialized only when `sync.actions.join_unlabelled_prs` is enabled, so an
upgrade with the action disabled does not disrupt existing repositories.

For repositories created by earlier Caravan versions, the active label
`1D76DB` / `Active member of a Caravan merge chain` is also an exact compatible
definition. Cara preserves it and reports its actual metadata in the
`already_present` receipt; no other metadata variation is accepted.

`status` and `check` never initialize anything. `init` never changes a pull
request label, base, auto-merge state, branch, or commit. On a fully initialized
repository, repeated calls are verification-only no-ops with
`already_present` receipts.
