# Session summary — Resolve repository root before config and mutation

## Goal

Make every Cara command resolve one exact Git worktree root before default config/state discovery, so nested-directory invocation is identical to root invocation and can never create nested `.caravan` state.

## Bead

- `bd-380ede` — Resolve Git repository root before config discovery and init.

## After state

- `AppContext::load` now runs a bounded, noninteractive `git rev-parse --show-toplevel`, canonicalizes the exact worktree root, and uses it for every domain operation.
- Default `.caravan/config.yaml` remains repository-relative but loads only from `<root>/.caravan/config.yaml`; nested `cara init` therefore writes only at root.
- Relative explicit `--config` retains historical invocation-cwd semantics and is stored as an absolute identity so downstream repository-relative checks cannot reinterpret it. Explicit malformed config retains parse-error precedence before repository discovery.
- Outside Git and bare/non-worktree contexts return typed `repository_not_found` with mutation=false and no filesystem writes.
- Linked worktree behavior continues through Git's own toplevel/common-dir contracts; operation lock tests remain green.
- `mcp tools` remains repository-independent; MCP stdio resolves repository context only when serving operations.
- CLI validates context-independent domain input such as empty eviction reason before root discovery, preserving stable JSON/human error precedence in hermetic/outside-Git use.

## Validation

- Added root/nested path-with-spaces, relative explicit config, and outside-Git zero-write tests.
- Full suite after scheduler/P0 main composition: 264 library + 8 binary + 7 CLI + 3 parity tests green.
- Strict all-target/all-feature Clippy, rustfmt, and diff checks green.
- Nix flake check initially exposed an unrelated loaded-host 10s physical-rebase fixture timeout; rerun then exposed stable outside-Git error precedence, which was fixed. Final Nix flake check green.
- Post-rebase focused nested-root, complete CLI-exit, parity, and strict Clippy gates green.

## Diff

- Exact generation: `93081dc` on true main `3ae715b`.
- Surfaces: `AppContext`, `ConfigError`, CLI context-independent validation, README, SPEC.

## Operator takeaway

Cara's ambient directory is now only where the operator invoked it; repository authority comes from Git's exact worktree root. Nested use cannot fork config, locks, journals, caches, or repair state into accidental subdirectories.
