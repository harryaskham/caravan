# Session summary — Durable interrupted Native Stack receipts

## Goal

Make interrupted or failed Caravan syncs identifiable and inspectable without relying on truncated scheduler output, and reconcile uncertain native merges without issuing a second merge request. Work was limited to Caravan source and hermetic fixtures; the live Cacophony queue, cron schedules, and running sync were untouched.

## Bead(s)

- `bd-6a588d` — Native Stack merge can end indeterminate with no operation ID or readable receipt.

## Before state

- Sync generated its receipt identity late, after some provider writes could already have occurred. Native merge failures did not expose a top-level operation identity to the scheduler.
- Landing checkpoints existed locally, but had no supported operation-inspection command and were not consistently linked to the invoking sync.
- Released indeterminate UUIDs were polled through the active-lock path, which re-required the intentionally deleted lock and forced terminal evidence back to indeterminate.
- The current scheduler's read-only source confirmed that restart recovery accepts complete JSON root records containing `/operation_id`, not prose-only markers.

## After state

- Sync persists its ID before writes and emits a complete structured startup receipt on stderr, compatible with the scheduler's merged output spool. Final envelopes and failure summaries retain the ID and inspection command.
- `cara operation --operation-id ID --json` and the MCP `operation` tool read durable local receipts independently of provider access, writer-lock cleanup, or CLI repository-policy validity.
- Write-ahead subprocess markers distinguish known pre-write failures from possible effects. Sealed native plans and reconciliation evidence remain archived when active scheduling cursors retire.
- Resumed submitting checkpoints are never blindly resubmitted. Released UUIDs are reconciled without re-requiring the deleted lock; UUID-less submissions resolve only with exact merge provenance and otherwise remain unknown.
- Focused diagnostics passed: six interruption/receipt tests including a real killed child; 47 native Stack adapter tests; the existing resumed-landing regression; seven sync CLI/unit summary tests; the operation CLI regression; and the receipt-filtered library tests. Formatting and diff checks passed. Hosted CI owns the final broad gate.
- Ambient Rust 1.99 reports unrelated pre-existing lint/deprecation warnings. No source suppressions or unrelated cleanup were added. Reflection evidence was routed against existing drafts `bd-8095c7` and `bd-0a950d` rather than duplicated.

## Diff summary

- Code/content commit: `6a898d1db30976eadb652b01fb43c3b86df59d88`.
- `src/sync/receipt.rs`: durable invocation ledger, native receipt archive, read-only inspection, and failure identity attachment.
- `src/command.rs`, `src/stack_checkpoint.rs`, `src/sync.rs`: pre-spawn write markers, atomic synced storage, early identity, and resumed checkpoint linkage.
- `src/github/stack_land.rs`, `src/github/stack_merge.rs`: released-UUID and UUID-less read-only reconciliation, preserving exact evidence and no-replay semantics.
- `src/main.rs`, `src/lib.rs`: CLI/MCP inspection and scheduler-facing receipt identity.
- `src/sync/tests/interrupted_receipt.rs`, `src/sync/tests.rs`, `tests/cli_exit.rs`, and adapter/unit tests: ten tests added, none removed, including the killed-child fixture helper.
- `docs/native-sync-receipts.md` and `README.md`: inspection, spool compatibility, uncertain outcomes, and existing-writer reconciliation runbook.
- Summary metadata is intentionally outside product history; final landing identity comes from the lifecycle receipt.

## Operator-takeaway

An unknown provider outcome must retain its own identity and evidence, not borrow confidence from a later successful sync. This change makes the receipt available before an interrupt can destroy the caller and fixes the deleted-lock reconciliation trap, while keeping uncertain merges non-replayable. Receipts are local Git common-directory metadata, so preserving that metadata remains necessary when removing or relocating a checkout.
