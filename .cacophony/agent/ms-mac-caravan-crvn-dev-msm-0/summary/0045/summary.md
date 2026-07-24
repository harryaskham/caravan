# Session summary — physical rebase fixture timeout resilience

## Goal / bead

- `bd-7325ae` — stop Nix switch/full-suite load from spuriously failing physical-rebase Git fixtures.

## Live evidence

- Operator's Nix build reported failures in:
  - `rewrites_under_exact_lease_without_touching_caller_worktree`
  - `five_member_plan_reuses_exact_simulated_parent_generations`
- On fresh current main, the exact rewrite fixture passed in 2.94s.
- The exact five-member fixture failed after 114.53s because one local bare-remote fetch of `refs/heads/d` exceeded a hard-coded 10,000ms `RebaseExecutionBudget`.
- The test validates cumulative simulated-parent generations, not timeout behavior. Parallel Nix test load can starve these real local Git subprocesses.

## Change

- Added one documented 60s `TEST_REBASE_BUDGET` for physical-rebase fixtures that exercise real fetch/rebase/merge subprocesses.
- Replaced the six incidental 10s budgets in non-timeout planning, conflict, lease-race, default-race, and stale-snapshot fixtures.
- Production operation budgets and timeout-specific behavior are unchanged.
- Exact remote/head/tree, no-write, and stale-lease assertions remain intact.

## Validation

- Exact rewrite fixture: green, 2.07s.
- Exact five-member cumulative fixture: green, 11.28s.
- `cargo clippy --lib --all-features -- -D warnings`: green.
- `git diff --check`: green.
- Hosted CI remains the broad delivery gate.

## Commit

- `898ed68`.
