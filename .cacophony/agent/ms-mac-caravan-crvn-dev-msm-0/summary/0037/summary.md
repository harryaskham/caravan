# Session summary — stale-tail/source-only join integrity

## Goal / bead

- `bd-35e822` — prevent join from replaying current-main/release content into a child when the selected caravan root/tail is stale; bind exact source-only provenance and make empty source zero-mutation.
- Sole ownership was atomically claimed before edits.
- Cacophony PR #2080 was never mutated by this implementation. Operator later ran ordinary `sync --all`, which independently rebuilt #2079/#2080 onto current main; the original provider evidence remains the motivating audit record.

## Before risk

`join --create-pr` created the provider PR against the selected tail before physical planning. That changed the candidate's provider base from current main to the tail. Physical range selection then treated the stale tail as the source boundary and could replay main commits already landed under distinct OIDs, contaminating the child patch/title.

## After state

- Before PR creation/update, branch rewrite, push, or membership mutation, join requires the selected caravan root's provider base name/OID to equal the exact discovered current default. `join_root_stale_default` gives `sync --all` guidance and `mutated=false`.
- Root PR preconditions are refetched immediately after source planning and again before branch apply; provider drift returns `join_root_moved_before_apply` before branch push.
- Source planning binds exact repository, branch, remote/provider head, one source/default merge-base parent, source tree, binary source-only patch fingerprint, source commit title, selected predecessor branch/head, and independent expected result tree.
- The source parent can be a retained older default generation. New `HistoricalSourceBranch` range authority binds both that parent and current default, verifies ancestry, and revalidates both at apply.
- Physical planning uses the source receipt's parent rather than the newly created PR's tail base, so current-main/release changes outside the source-only range are not replayed.
- Before push, the physical plan must exactly equal source head/parent, selected tail, and expected result tree. Apply rechecks source/range/default refs, old-head lease, target and retained object.
- An empty source-only binary patch returns typed `join_empty_source_noop` with complete source/tail/tree evidence, `noop=true`, and zero provider/branch mutation.
- Successful physical root/join receipts include optional `JoinSourceReceipt`; physical membership requires it and includes source tree/result-tree checks in `ancestry_verified`.
- Early stale-root, root-drift, source-noop and source-plan refusals emit `join_failed` events whose durable metadata includes complete bounded structured error/source evidence. Successful events retain join/provider/rebase receipts.
- Existing virtual/non-rebase join remains backward-compatible with absent source provenance.

## Regression proof

- Stale root base is rejected with no provider mutation and durable exact error metadata.
- Root provider drift after preview fails without mutation.
- Empty source branch produces exact no-op receipt and unchanged fake-provider state.
- Source branch from an older main plus an unrelated release already on current main produces a physical plan whose tree contains base, release, tail and source files exactly once; plan old-base equals the source merge-base, result tree equals independent merge-tree, and no provider mutation occurs.
- Existing nonlinear/simulated-chain tests remain green after apply-time range revalidation was scoped to remote plans (simulated child ranges legitimately observe their parent branch advance).

## Validation

- 314 library + 12 binary + 10 CLI + 3 parity tests green.
- Strict all-target/all-feature Clippy and rustfmt green.
- All Nix flake checks green.

## Commit

- Rebasing implementation commit: `2f35c80`.
