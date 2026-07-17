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
- `cara sync` is idempotent: pending CI waits, failed CI returns exact check/run
  evidence, and `--rerun-failed` reruns only verified runs for the selected PR
  and head SHA.
- `caravan-force` keeps a known acceptable failure in-chain. A forced head is
  admin-squashed only when repository policy permits it, the exact head/default
  compatibility proof is still current, and GitHub reports admin permission.

## Implementation status

The core read, membership, navigation, reshape, and synchronization commands are
implemented and exercised against live disposable PRs on this repository. CI
failure/force handling, hooks, loop behavior, and final parity continue to use
the same typed command contracts while their acceptance lanes land.

Development surfaces:

```sh
nix develop
cargo test
cargo run -- help
cargo run -- mcp tools
cargo run -- mcp stdio
cargo run -- self-update status
cargo run -- feedback status
```

Domain surface:

```text
cara status
cara check [--tail-pr N | --head-pr N]
cara new [--create-pr]
cara join [--tail-pr N | --head-pr N] [--create-pr]
cara renew | cara rejoin
cara show | cara next | cara prev
cara sync [--all] [--rerun-failed] | cara loop
cara evict [--pr N] --reason TEXT
cara split [--pr N]
cara van list | cara van next | cara van prev
```

`cara van next` on the default branch enters the first caravan head; ordinary
`cara next` remains chain-local and requires the current branch to map to an
open caravan PR.

Use `cara help` for the agent operating loop and recovery rules. Use `--json`
for stable `mcp-cli` envelopes.

## Releases and self-update

Version tags publish update-compatible `cara` binaries for `x86_64-linux`,
`aarch64-linux`, and `aarch64-darwin`. Each target produces:

```text
cara-<version>-<target>.tar.gz
cara-<version>-<target>.sha256
```

The archive contains `cara-<version>-<target>/cara`, matching
`updatable-cli`'s default asset strategy. The release workflow tests the tagged
source, packages each binary, verifies native archives by executing `--version`,
and uploads both assets to the GitHub Release. Public ecosystem source inputs
are fetched anonymously over HTTPS; the workflow needs no SSH deploy secret.

Use the bounded helper to keep `Cargo.toml`, `Cargo.lock`, and `flake.nix`
aligned and create the tag:

```sh
./scripts/release.sh patch --dry-run   # inspect the next version
./scripts/release.sh patch --no-push   # commit + tag for local review
./scripts/release.sh patch             # atomically push branch + tag
```

An explicit version such as `./scripts/release.sh 0.2.0` is also accepted.
Run `./tests/release_contract.sh target/debug/cara` after building to exercise
the asset/checksum layout and `cara self-update status` with an isolated home;
it never stages or installs an update over the developer binary.

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
command_timeout_secs: 30
loop:
  interval_secs: 60
hooks:
  sync_failed:
    command: ./scripts/on-caravan-sync-failed
    timeout_secs: 30
    blocking: false
```

`command_timeout_secs` is the hard deadline for each `git` or `gh` child;
timeouts terminate and reap the child process group and return a structured,
resumable error with the command stage and bounded output evidence.

Hooks receive one versioned `CaravanEvent` JSON object on stdin plus non-secret
`CARA_EVENT`, `CARA_EVENT_ID`, `CARA_OPERATION_ID`, `CARA_REPOSITORY`,
`CARA_CARAVAN_ID`, and `CARA_PRS` context. Delivery output reports only bounded
state/exit/byte counts, never hook output content. Best-effort failures remain in
the command output; blocking failures return typed `hook_failure` and never roll
back provider mutations already recorded by the event.

Caravan intentionally owns no cross-process hook dedupe. A long-running hook can
use an external lock and return success while that lock exists; repeated ticks
then become visible no-ops rather than duplicate coordination.

```sh
cara loop --once --json     # one bounded sync --all tick for agents/schedulers
cara loop                   # foreground human stream until SIGINT/SIGTERM
```

The unbounded loop is deliberately not an MCP tool. Every tick starts from fresh
GitHub state, and a decision-point/error tick fires its configured hook and stops
instead of inventing an agent decision.
