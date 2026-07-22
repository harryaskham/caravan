# Session summary — persistent Saloon disclosure and actionable topology refusal

## Goal / bead

- `bd-3a9068` — fix two live operator UX regressions without weakening safety.

## Saloon disclosure state

- Root cause: the dashboard rebuilds Saloon `<details>` nodes every five-second status render and reapplied hard-coded defaults, forcing Saddling Up open and Other/Bounty closed.
- Each repository/group now has an exact local key: `caravan.saloon.<repo-id>.<group>`.
- Native `toggle` events persist `open`/`closed` independently for Ready, Saddling, Other, and Bounty.
- Render reads retained state; only first-ever state uses defaults (Ready/Saddling open, Other/Bounty closed).
- Synthetic browser proof toggled Saddling closed and Other/Bounty open, waited beyond a polling cycle, and observed `{ready:true, saddling:false, other:true, bounty:true}`.

## Topology refusal guidance

- Safety remains unchanged: merge-preserving replay still requires one rebuilt commit per exact source-range commit and never forces an unproven topology.
- Count mismatch now explains that Git rebuilt a different commit count, commonly because patches already exist on current default/tail, Git pruned duplicate/empty commits, or merge topology cannot replay one-for-one.
- Structured evidence now contains source/rebuilt/dropped/added counts, exact source/rebuilt commit OIDs, likely causes, `mutated=false`, and a safe next action: inspect/rebase source to remove landed patches or use reviewed Cara repair for intentional topology change.
- Focused fixture proves two source commits → one rebuilt commit reports one dropped commit and actionable source-rebase guidance.

## Documentation and validation

- README/SPEC document persistent disclosure state and the expanded topology decision.
- JavaScript syntax and strict all-target/all-feature Clippy/rustfmt green.
- Focused embedded-assets and topology-diagnostic fixtures green.
- Hosted required CI remains the broad delivery gate under active project policy.

## Commit

- `c005273`.
