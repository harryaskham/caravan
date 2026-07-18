# Slice summary — bounded structured Actions diagnostics

Partial coherent slice for bd-7667a2 before interrupting for operator P0 bd-17659c.

- Added `ci` module with bounded, secret-free GitHub Actions run/job/failed-step evidence.
- Maximum 10 deduplicated run IDs, 25 failed jobs/run, 25 failed steps/job; no raw log download.
- Preserves run attempt/workflow/check-suite IDs, event/head SHA/branch, immutable PR base/head associations, job IDs/names/conclusions/runner labels, failed step numbers/names/conclusions, and truncation flags.
- Added policy-free `GitHubMutationAdapter::failed_run_diagnostics` after exact PR precondition verification.
- Candidate freshness/classification and rerun-vs-fresh policy remain in-progress; coordinated with bd-021727 owner to consume canonical MergeCandidateIdentity rather than duplicate types.
- Focused tests and strict all-target Clippy pass. Full suite had one known parallel ProcessRunner output-loss recurrence; isolated rerun passed and canonical bd-dfea55 was reopened with evidence. This slice did not touch command.rs/hooks.
