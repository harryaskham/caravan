# Caravan

Caravan is an agent-in-the-loop merge queue for GitHub pull requests. The `cara`
CLI represents a queue as one or more labelled PR chains, performs deterministic
compatibility and CI checks, and returns typed decision points when semantic
judgment or code repair is required.

The normative behavior is in [`SPEC.md`](SPEC.md).

## Core model

- A caravan head targets the repository default branch.
- Each later PR targets its predecessor's branch without routinely rewriting
  history.
- Every member has the `caravan` label.
- Only the head has squash auto-merge enabled.
- The current head PR number is the caravan ID.
- `cara sync` is idempotent: it converges mechanically or stops with evidence
  and recovery actions for a user or external agent.

## Foundation status

This initial slice establishes the command tree and ecosystem plumbing. Queue
operations deliberately return a structured `not_implemented` error until their
domain beads land; they never claim to have changed GitHub.

Working surfaces:

```sh
nix develop
cargo test
cargo run -- help
cargo run -- mcp tools
cargo run -- mcp stdio
cargo run -- self-update status
cargo run -- feedback status
```

Planned domain surface:

```text
cara status
cara check [--tail-pr N | --head-pr N]
cara new [--create-pr]
cara join [--tail-pr N | --head-pr N] [--create-pr]
cara renew | cara rejoin
cara show | cara next | cara prev
cara sync [--all] | cara loop
cara evict [--pr N] --reason TEXT
cara split [--pr N]
cara van list | cara van next | cara van prev
```

Use `cara help` for the agent operating loop and recovery rules. Use `--json`
for stable `mcp-cli` envelopes.

## Ecosystem

The binary uses:

- [`mcp-cli`](https://github.com/harryaskham/mcp-cli) for shared CLI/MCP typed
  tools and structured envelopes;
- [`updatable-cli`](https://github.com/harryaskham/updatable-cli) for GitHub
  release self-update tools;
- [`feedback-cli`](https://github.com/harryaskham/feedback-cli) for configurable
  structured reporting.

The Nix flake pins Harry's current FlakeHub nixpkgs revision. The three public
ecosystem repositories are also pinned as `flake = false` source inputs and
patched into Cargo, so sandbox builds neither evaluate their independent flakes
nor fetch Git dependencies at build time.

## Configuration

Repository policy and hooks will live at `.caravan/config.yaml`:

```yaml
version: 1
force_merge: false
loop:
  interval_secs: 60
hooks:
  sync_failed:
    command: ./scripts/on-caravan-sync-failed
    timeout_secs: 30
    blocking: false
```

Hooks receive versioned JSON on stdin. Long-running coordinators own external
deduplication/locking; repeated sync ticks remain safe and observable.
