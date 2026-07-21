# Session summary — exact open PR precedence on reused branches

## Goal / bead

- `bd-32158c` — allow a genuinely new exact OPEN PR to use branch text that also has older merged history, without weakening branch-reuse ambiguity or membership safeguards.
- Live Cacophony PR #2056 was not mutated.

## Before risk

Cara normally resolves the current PR from one bounded all-open rollup. When that rollup omits a recently opened PR, discovery falls back to branch history. A branch containing old merged history plus the omitted exact open PR was classified as historical before recognizing the current open generation, producing `historical_current_pr_missing_caravan_label`.

## After state

- Branch-history fallback examines OPEN entries before classifying merged history.
- Exactly one open candidate can win only when:
  - it is same-repository and non-fork;
  - its provider head OID equals local `HEAD`;
  - the exact remote branch ref equals that same OID;
  - older history has no `caravan` or `caravan-evicted` membership receipt.
- The selected identity becomes the exact PR number in `current_pr`; all subsequent checks/mutations continue to use PR number plus complete head/base/provider preconditions, never branch text alone.
- Multiple open PRs sharing the branch remain `open_branch_reuse_ambiguous` before OID selection, even if one appears to match.
- Fork-only open reuse, local/provider/remote head mismatch, and conflicting retained Caravan history remain typed fail-closed.
- Draft and unsupported-base policy remains downstream membership/check policy; discovery does not silently grant eligibility.
- Existing retained merged Caravan successor recovery is unchanged.
- README, SPEC, and embedded help document exact reused-branch rules.

## Validation

- Unique open exact head plus older unlabelled merged history resolves to the open PR number.
- Two open PRs reusing one branch fail before OID checks.
- Remote-ref/provider/local mismatch fails exact-open identity.
- Older Caravan-labelled merged history blocks fresh reuse.
- Full composed validation: 306 library + 12 binary + 10 CLI + 3 parity tests green.
- Strict all-target/all-feature Clippy/rustfmt and all Nix flake checks green.

## Commit

- Implementation commit: `1992780`.
