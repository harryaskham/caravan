# Session summary — pinned workflow lint toolchain

## Goal / bead

- `bd-c64f10` — make GitHub Actions and shell validation available from Caravan's pinned development environment without ad hoc nixpkgs downloads.
- Promoted from P3 draft after all higher-priority ready work closed; scope remained flake/tooling only.

## After state

- The default Nix development shell includes pinned `actionlint` and `shellcheck` from Caravan's existing nixpkgs input.
- `scripts/check-workflows.sh` deterministically validates every `.github/workflows/*.yml` file with the repository `.github/actionlint.yaml`, then runs ShellCheck across `scripts/*.sh` and `tests/*.sh`.
- The script resolves the repository when Git metadata exists but also runs from a copied source tree in the Nix sandbox.
- `.github/actionlint.yaml` declares the real custom `nix` and `x86_64-linux` self-hosted runner labels in addition to existing Azure/Darwin labels, avoiding false positives without suppressing other diagnostics.
- `checks.<system>.workflow-lint` runs the same script in a sandbox with only pinned actionlint/shellcheck inputs; `nix flake check` now gates workflows and scripts.
- README/SPEC document the one local/CI command.

## Validation

- `nix develop --no-write-lock-file --command ./scripts/check-workflows.sh` passes.
- `nix flake check --no-write-lock-file` evaluates and builds three checks, including `caravan-workflow-lint`, successfully.
- Diff checks pass. No Rust runtime surface changed.

## Commit

- Pre-rebase implementation commit: `1cf4c0a`.
