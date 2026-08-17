# Session summary — Hermetic watchdog package inputs

## Goal

Unblock the Cacophony v0.0.87 runtime update by making Caravan’s Nix package check environment contain every external executable used by the status watchdog fixture, without changing or skipping the watchdog test.

## Bead(s)

- `bd-6ad422` — Caravan Nix package check omits Python required by watchdog test
- Cross-project blocker: Cacophony `bd-7df6e1`

## Before state

- `flake.nix` supplied only `pkgs.gitMinimal` through `nativeCheckInputs`.
- The watchdog fixture invokes `python3`, so the hermetic package check failed with ENOENT.
- On aarch64-darwin, identity capture also invokes external `ps`; once Python was present, the package check exposed that second missing check input as an empty pre-teardown provider registration.
- Rust production and test logic were already reviewed and were not in scope for modification.

## After state

- `pkgs.python3` is a universal package check input beside `pkgs.gitMinimal`.
- Darwin check environments additionally receive `pkgs.unixtools.ps`; Linux continues using `/proc` and does not acquire the Darwin-only package.
- Exact focused watchdog test passes: 1 passed, 0 failed.
- Exact aarch64-darwin `nix build .#caravan` passes at v0.0.87 with output `/nix/store/12wgjlygz9zim07yc299p1gklh7mv9z7-caravan-0.0.87`.
- Derivation evidence names `git-minimal-2.51.2`, `python3-3.13.12`, and Darwin `ps-adv_cmds-235` as check inputs.
- No Rust/test change, ambient PATH injection, skip, Cacophony workaround, or provider mutation.

## Diff summary

- Code/content commit: `ee0096a` after rebase onto current main; original validated pre-rebase commit was `22cb065`.
- File touched: `flake.nix`
- Tests: exact watchdog test green; exact Nix package build green.
- Behavioural delta: package checks can spawn the Python fixture on every platform and inspect its process tree on Darwin using an explicit hermetic `ps` input.

## Operator-takeaway

The failure was package closure, not watchdog logic. Python was the universal missing dependency; Darwin then revealed its own external `ps` dependency. Both are now explicit and platform-bounded, so Nix reproduces the real watchdog proof without ambient host tools.
