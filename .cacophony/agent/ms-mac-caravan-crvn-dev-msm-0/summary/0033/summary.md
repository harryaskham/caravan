# Session summary — exact active-binary self-update

## Goal / bead

- `bd-94a009` — ensure successful self-update replaces the Cara binary the shell actually executes, rather than a shadowed hard-coded `~/.local/bin` target.
- Promoted from audited P2 draft after the ready set emptied; no overlap with the separately owned command-output limit slice.

## After state

- CLI and MCP `self-update status|check|run` first resolve and canonicalize the running executable.
- The running executable must be named `cara`, be executable, and equal the first executable `cara` found on `PATH`; missing or shadowed identities fail with typed evidence and safe next actions.
- Existing `~/.cargo/bin/cara` and `~/.local/bin/cara` installations update in place by setting `updatable-cli`'s install directory to the active parent. This provides the migration path for prior cargo-bin installations and avoids writing an inert lower-priority local-bin copy.
- Other intentional user-managed locations require exact absolute `CARA_SELF_UPDATE_INSTALL_DIR`; it must equal the active executable parent and still be first on PATH.
- Renamed/test executables, native and cross-target Cargo `target/.../debug|release/cara`, Nix store, and Homebrew Cellar binaries fail closed.
- Process-entry staged promotion runs only when the same active-install validation succeeds, so a stray `cara_next` cannot overwrite development/package-managed binaries.
- Existing authentication, release asset naming, checksum verification, staging, and atomic promotion remain delegated to `updatable-cli` after local identity validation.
- README, SPEC, embedded help, and MCP descriptions document the exact contract.
- Release-contract smoke now copies the candidate into an isolated first-PATH `~/.cargo/bin/cara`, invokes that executable, and verifies its canonical install target.

## Validation

- Unit tests cover cargo/local targeting, shadowed binary refusal, cross-target development refusal, known package-manager classification, and exact explicit-dir validation.
- CLI tests execute an isolated copied stable binary and prove `installed_path` equals that exact canonical executable; the ordinary Cargo-built binary returns `self_update_development_binary`.
- Release contract passes with isolated active-install proof.
- Full composed validation: 294 library + 12 binary + 10 CLI + 3 parity tests green; strict all-target/all-feature Clippy and rustfmt green; Nix flake checks green after cross-target development-path coverage.

## Commit

- Pre-reintegration implementation commit: `e17d6fe`.
