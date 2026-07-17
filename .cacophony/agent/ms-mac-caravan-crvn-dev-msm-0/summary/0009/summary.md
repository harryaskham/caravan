# Session summary — Default-branch fleet navigation entry

## Goal

Make `cara van next` enter the first caravan from `main` when there is no current PR, as exposed by Harry's live v1 dogfood.

## Bead(s)

- `bd-d56c0f` — Let fleet navigation enter the first caravan from the default branch.
- Live acceptance parent: `bd-322e38`.

## Before state

- `cara status` on live `main` reported healthy caravan head `#2`.
- Both `cara next` and `cara van next` failed `current_pr_not_found` because destination selection unconditionally required a current PR.

## After state

- `cara van next` from the default branch selects the first deterministic fleet head and reports `from_pr: null`.
- `cara van prev` from the default branch returns typed `navigation_boundary`.
- Chain-level `cara next`/`prev`, and fleet navigation from other non-PR branches, still return `current_pr_not_found`.
- Clean-worktree and exact remote-head checks remain unchanged.

## Validation

- 109 library + 4 binary + 3 integration tests pass.
- Strict all-target clippy passes.
- Hermetic `nix flake check` passes on `aarch64-darwin`.
- Live clean clone of `harryaskham/caravan` on `main` successfully navigated to PR `#2`, branch `dogfood/cara-v1-middle-fixture`, exact head `61e88bf...`, with a clean worktree.

## Diff summary

- Code/content commit: `60a5116`.
- Files: `src/navigation.rs`, `SPEC.md`, `README.md`.
- Tests: +2 navigation regression tests.

## Operator takeaway

Harry's exact command now behaves as expected: after updating to the landed commit, `cara van next` on `main` enters PR #2 while `cara next` remains correctly chain-local.
