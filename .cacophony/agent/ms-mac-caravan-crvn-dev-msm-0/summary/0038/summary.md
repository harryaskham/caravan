# Follow-up summary — patch-equivalent main subtraction

## Bead

- `bd-35e822` reopened after auditing its merged duplicate requirements.
- Initial land `36a491b` provided stale-root guards, source provenance, historical source boundaries, result-tree verification, durable failures, and literal-empty no-op.
- Follow-up closes the remaining distinct-OID/already-landed patch gap.

## Added contract

- Join runs bounded `git cherry <current-default> <source-head> <source-parent>` and records every exact source commit plus those whose stable patch identity is already represented on current main.
- It independently computes `merge-tree(current-default, source-head)` and diffs current default to that tree. `JoinSourceReceipt` now stores both original source-range and effective-not-on-main patch fingerprints.
- If the effective patch is empty—even when source/main commit OIDs differ—the operation returns durable `join_empty_source_noop`, complete stable-patch evidence, and zero provider/branch mutation.
- Mixed source ranges are safe: physical linear replay uses the exact selected tail as Git's upstream, allowing Git to omit patch-equivalent commits already on the tail/current-main ancestry and replay only unique commits. The separately retained source merge-base remains plan authority.
- The precomputed physical result tree must still equal independent source/tail merge-tree proof before push; source/range/default/head/tail leases are revalidated at apply.
- Non-empty source content with no bounded stable patch classification fails closed as ambiguous.

## Regression proof

- Sibling source/main commits with identical release content but different OIDs produce one source commit classified already-landed, an empty effective fingerprint, `noop=true`, and unchanged provider state.
- A two-commit source containing an equivalent release patch plus one unique source change, with the equivalent release independently committed on current main, produces a child tree containing base/release/tail/source exactly once. Receipt records 2 source commits, 1 already-landed commit, distinct original/effective fingerprints, and exact result tree; provider state remains unchanged during planning.
- Literal empty-commit source remains zero-mutation no-op.
- Existing simulated multi-member chain apply remains valid; apply-time remote-range revalidation is skipped only where a simulated parent is expected to advance that same branch.

## Validation

- 315 library + 12 binary + 10 CLI + 3 parity tests green.
- Strict all-target/all-feature Clippy and rustfmt green.
- All Nix flake checks green.

## Commit

- Follow-up implementation commit: `a9d4db9`.
