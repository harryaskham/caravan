# Session summary — Versioned hooks and foreground cara loop

## Goal

Complete Caravan's event delivery and foreground automation layer over canonical sync without adding a second queue authority: execute configured hooks with strict blocking/best-effort semantics, expose bounded delivery status, and make `cara loop` a signal-aware sequence of fresh `sync --all` ticks while keeping the unbounded process out of MCP.

## Bead(s)

- `bd-7691a8` — Implement versioned hooks and lightweight cara loop.
- `bd-322e38` — Dogfood cara against the Caravan repository until v1 works end to end (ongoing acceptance track).
- Parent: `bd-caab31` — Implement Caravan v1 agent-in-the-loop merge queue.

## Before state

- Hook configuration parsed but no command executed it; canonical `CaravanEvent` values from sync/CI/force had no delivery layer.
- `cara loop` still returned the old `not_implemented` scaffold error.
- Subprocess requests could not provide explicit stdin or non-secret event context to a bounded child.
- Membership and reshape outputs exposed mutation receipts but no canonical event or hook delivery status.

## After state

- Hook commands receive one v1 `CaravanEvent` JSON object on stdin plus non-secret `CARA_*` context and run through the timeout-aware subprocess runner.
- Delivery output contains only state, blocking mode, exit code, and bounded byte counts—never hook output content. Best-effort failure is reported without rollback; blocking failure returns typed `hook_failure`, including timeout category when appropriate.
- Sync dispatches its exact canonical event objects, including the same IDs preserved in CI decision evidence; generic decision failures emit `sync_failed`. Membership emits `caravan_created`/`pr_joined`, reshape emits `evicted`/`split`, and outputs include bounded hook status.
- `cara loop --once --json` performs one canonical `sync --all` tick. The foreground loop repeats from fresh GitHub state, streams human progress, stops on decision/error after hook delivery, and exits cleanly on SIGINT/SIGTERM. The unbounded loop is explicitly absent from MCP.
- Repeated event delivery deliberately has no internal cursor: external coordinators own their lock/dedupe record.

## Diff summary

- Code/content commits: `d5d6b33771d784c3601eb405cb3cc09d7d0f8196`, `362f06879a7587d72152bcd2cbee9b1ddb56328c`, `36a9825ed26a2304b51715dd428a471cf0588c33`, `2a64dfc82fc02aa5437e3f93a5bbb6977bcb0803`, `ceca316880eca58c13a7a309716108ced0a901bd`.
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: `Cargo.toml`, `Cargo.lock`, `README.md`, `src/command.rs`, `src/hooks.rs`, `src/lib.rs`, `src/loop_runner.rs`, `src/main.rs`, `src/membership.rs`, `src/reshape.rs`, `src/sync.rs`, `tests/cli_exit.rs`.
- Tests: full suite green (105 library, 4 binary, 3 process-level integration tests); all-target warning-denied clippy and `nix build .#caravan --no-link` green.
- Live acceptance: disposable unqueued PR #4 produced `ready_pr_unqueued` through two real `cara loop --once` calls. Both hook deliveries succeeded, while an external mkdir lock allowed only the first event ID to enter the coordinator log; PR/branch were then closed and deleted.
- Behavioural delta: agents and schedulers can now drive one bounded tick, operators can run a clean foreground loop, and every configured hook result is explicit without Caravan claiming external coordination authority.

## Operator-takeaway

Hooks and loops now operate on the exact same canonical events as sync/CI rather than inventing a parallel event model. The live external-lock no-op proves the intended at-least-once contract. Dogfooding also found the remaining P1 stale operation-lock recovery gap at `bd-b52180`; no worker raw-deleted the canonical lock.
