# Session summary — CLI manual external-decision controller

## Goal

Provide a human-operated Cara flow-testing mode without turning production hooks into PTY sessions.

## Bead

- `bd-8b75a9` — Add CLI manual decision mode with an interactive shell.

## After state

- `cara loop --manual [--shell COMMAND]` runs canonical `sync --all` ticks and intervenes only when typed scheduler evidence says `wake_class=external_decision`.
- JSON, MCP, and non-TTY invocation fail closed; ordinary hooks remain unchanged noninteractive machine integration.
- At a decision, Cara writes one complete bounded secret-free JSON file under the repository common Git state `caravan/manual-decisions`, with private directory/file permissions, UUID, timestamp, repository and structured error/evidence. It retains at most 20 files and caps each at one MiB.
- Environment includes `CARA_DECISION_FILE`, code, repository path, event/operation IDs, bounded PR list, and repair session when found.
- Cara chooses an exact affected/repair workspace only when its canonical path is within the repository or common-Git root; otherwise shell cwd is repository root.
- Shell inherits real stdin/stdout/stderr and controlling TTY. Default is `$SHELL -i`; explicit command is supported.
- Operation lock is already released when sync returns the decision. Shell zero does not claim resolution: Cara immediately rediscovers provider state and reruns the exact tick. Nonzero exits with preserved evidence.
- Decision persistence and action are CLI-only; no unbounded manual process is added to MCP.

## Validation

- Fixtures cover JSON/noninteractive refusal, private bounded decision evidence, nested workspace extraction, inherited cwd/context/env and successful shell continuation.
- Full composed suite after repository-root, Plan, P0 membership, scheduler and REST-budget lands: 275 library + 12 binary + 8 CLI + 3 parity tests green.
- Strict all-target/all-feature Clippy, rustfmt, diff checks, and Nix flake check green.

## Diff

- Implementation generation before final rebase: `1306e76`.
- Surfaces: LoopInput, loop tick seam, CLI manual driver/evidence shell, CLI tests, README/SPEC/parity/help.

## Operator takeaway

Hooks stay reliable machine protocols. Manual mode is an explicit human controller: inspect exact evidence in a real shell, repair through first-party commands, exit, and let Cara prove the result from fresh provider facts.
