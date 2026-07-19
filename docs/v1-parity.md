# Caravan v1 SPEC-to-surface parity matrix

This matrix audits the normative command contract in `SPEC.md` against the
human CLI, the stable `--json` envelope, and bounded MCP tools. The automated
contract is `tests/v1_parity.rs`; real-provider receipts are tracked on
`bd-322e38`. GitHub remains the acceptance environment—no fake GitHub service
is required or used.

| SPEC operation | Human CLI | `--json` | MCP tool | Checked behavior |
|---|---|---|---|---|
| repository inspection | `cara status` | envelope | `status` | live discovery, graph, PR/check facts, initialization, canonical priority-then-FIFO attempts |
| event journal | `cara log` / `cara log -f` | bounded envelope / NDJSON follow | bounded `log` only | common-Git storage, filters, exact IDs, hook receipts, locking, rotation, torn-tail recovery |
| next admission | `cara next-candidate` | envelope | `next_candidate` | nonmutating canonical first attempt; membership preflight remains required and rejection cannot leapfrog; invalid labels fail closed |
| eligibility / remote preflight | `cara check [--pr N] [--tail-pr N\|--head-pr N]` | envelope | `check` | exact provider candidate receipt, active/new/join modes, canonical fail-closed selection, stale-head rejection, next action; target forms exclusive |
| create caravan | `cara new [--create-pr]` | envelope | `new` | live preflight, label/base, squash auto-merge |
| renew evicted | `cara renew [--create-pr]` | envelope | `renew` | evicted/force labels removed only after preflight |
| join tail | `cara join [...]` | envelope | `join` | explicit/dynamic tail, resumable exact receipts |
| rejoin evicted | `cara rejoin [...]` | envelope | `rejoin` | same target contract plus eviction cleanup |
| current caravan | `cara show` | envelope | `show` | whole chain and highlighted position |
| chain navigation | `cara next`, `cara prev` | envelope | `next`, `prev` | clean-worktree/exact-head guarded checkout |
| fleet list/navigation | `cara van list/next/prev` | envelope | `van_list/next/prev` | deterministic PR-number head order |
| synchronize | `cara sync [--all] [--rerun-failed]` | envelope | `sync` | idempotence, rolling head, exact root/tail/member scheduler generations, healthy/waiting/held no-wake status, retry-vs-external-decision wake class, safe affected-PR checkout, exact-run rerun, intentional hold skips |
| managed repair | `cara repair start/grant/revoke-grant/status/continue/abort` | envelope | `repair_start`, `repair_grant`, `repair_revoke_grant`, `repair_status`, `repair_continue`, `repair_abort` | dirty caller isolation, object-cache/exact-provider materialization, persistent manifest, typed conflict + audited semantic source scope, non-force publication, interruption-safe sync resume |
| incident hold | `cara pause/resume --head-pr N --actor A [...]` | envelope | `pause`, `resume` | bounded metadata, exact disable/enable preconditions, expiry warning without auto-resume, stale-fact closure |
| foreground ticks | `cara loop [--once]` | one-shot envelope only | intentionally absent | canonical sync events and bounded hook delivery |
| eviction | `cara evict [--pr N] --reason TEXT` | envelope | `evict` | safe gap closure, exact receipts and success/failure events |
| split | `cara split [--pr N]` | envelope | `split` | only non-heads; both resulting fleets validated |
| operation lock | `cara lock status/recover` | envelope | `lock_status/recover` | exact-token, age, and dead-owner guarded recovery |
| repository init | `cara init` | envelope | `init` | atomic config with ordered priority defaults, repository-policy preflight, idempotent fixed/configured label ensure receipts |
| agent help | `cara help` | envelope | `help` | resumable operating loop and recovery guidance |
| MCP metadata/server | `cara mcp tools/stdio` | metadata is JSON | n/a | all bounded domain inputs and outputs have schemas |
| self update | `cara self-update status/check/run` | envelope | `self_update_*` | release-asset updater contract |
| feedback | `cara feedback status/report` | envelope | `feedback_*` | shared structured feedback contract |

## Machine-contract checks

- Every bounded v1 command is invoked in an isolated non-repository directory;
  it must return a versioned success/error envelope and may never return
  `not_implemented`.
- Every bounded MCP operation has non-empty description, input schema, and
  output envelope schema. `loop` is intentionally excluded because it is
  unbounded.
- Config read/parse/validation failures honor `--json` rather than escaping as
  human-only stderr.
- `join_failed` and `eviction_failed` are canonical version-1 events attached
  to the same typed error exposed to JSON/MCP and then dispatched under the
  configured blocking/best-effort policy.

## Live-only acceptance boundary

The current real-repository receipts prove status/check, membership,
navigation, no-op sync, merged-head advancement, queued-CI waiting, loop
one-shot delivery, and external hook deduplication. The owner of `bd-322e38`
exclusively controls disposable live PRs and is completing reshape, terminal
CI/rerun, force-squash, and teardown receipts. This audit does not mutate those
fixtures. As of the audit, terminal Actions evidence and the `v0.0.1` release
remain dependent on registering the intended self-hosted runner; that exact
infrastructure condition is recorded on `bd-322e38`.
