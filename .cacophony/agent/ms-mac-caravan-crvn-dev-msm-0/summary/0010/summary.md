# Session summary — Remove landed live dogfood fixtures

## Goal

Remove fixture-only files intentionally merged during live force-merge acceptance before the v1 milestone release.

## Bead(s)

- `bd-d2f303` — Remove landed live dogfood fixture before v0.0.2.
- Acceptance source: `bd-322e38`.

## Before state

- `main@2da4fe7` retained `.caravan/dogfood/head.md` and `.caravan/dogfood/middle.md` from the disposable PR chain.
- GitHub PRs and remote dogfood branches were already fully torn down.

## After state

- Both fixture-only files are removed; `.caravan/config.yaml` is the only repository-local Caravan configuration file.
- GitHub reports zero open PRs and `git ls-remote` reports zero `dogfood/*` heads.
- The exact live acceptance evidence remains durable as attachment `att-019f7251-43bf-7d52-ad27-0551fc5dfebe` on `bd-322e38`.

## Validation

- `nix flake check --no-write-lock-file` passes on aarch64-darwin.
- Git diff check passes.
- No product behavior changed.

## Diff summary

- Code/content commit: `0f51530`.
- Files removed: `.caravan/dogfood/head.md`, `.caravan/dogfood/middle.md`.

## Operator takeaway

Live runtime parity remains fully evidenced, while release main no longer ships disposable dogfood content.
