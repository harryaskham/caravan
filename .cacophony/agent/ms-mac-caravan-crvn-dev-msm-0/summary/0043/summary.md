# Session summary — retry-safe root checks and compact human errors

## Goal / bead

- `bd-ad39f5` — remove false join-root staleness from ordinary CI progress and make operational failures usable in a human terminal.

## Root revalidation

- Live failure compared complete `PullRequestPrecondition`, including checks. Root #2086 changed only `Changed surface admission` from QUEUED to IN_PROGRESS; head/base/state/labels/auto-merge were exact, yet join failed.
- Membership root identity now compares only operation-shaping facts: PR number, state, draft, exact head, exact base, repository/fork identity, labels and auto-merge.
- Checks, provider check state, title, URL and timestamps remain observable but cannot stale membership authority.
- Real drift still returns `join_root_moved_before_apply`, now with compact expected/actual mutation identities, explicit `changed_fields`, `retryable=true`, and same-command retry guidance.
- Fixture proves queued→in-progress check churn succeeds, while head drift fails zero-write with `changed_fields=[head]` and no check arrays in evidence.

## Human CLI evidence

- Human errors now use colored `cara CODE: message` headings.
- Specific compact renderers:
  - root drift: root, changed fields, short expected/actual head/base OIDs, retry action;
  - topology count mismatch: source/rebuilt/dropped/added counts, short commit OIDs, source-rebase/repair action;
  - physical sync budget: required/remaining milliseconds, command slots and config guidance;
  - empty source: concise no-op source/head and zero-mutation statement.
- Other details remain pretty JSON only below 4 KiB. Larger evidence is not dumped; human output says to rerun with `--json` and reports byte count.
- JSON/MCP output is unchanged and complete.

## Validation

- Focused root check-churn and true-drift fixtures green.
- Human root/topology/oversized-evidence fixtures green.
- Strict all-target/all-feature Clippy/rustfmt green.
- Complete CLI-exit and v1 parity suites green.
- Hosted required CI remains the broad delivery gate.

## Commit

- `cc9455f`.
