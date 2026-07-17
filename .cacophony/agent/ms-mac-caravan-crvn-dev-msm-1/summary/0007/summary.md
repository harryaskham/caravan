# Session summary — large wrapped GitHub JSON status blocker

## Goal

Fix P1 `bd-f7c3bc`: installed Cara 0.0.1 failed `--json status` on the Cacophony repository even though the exact wrapped `gh pr list` command emitted valid 181,484-byte JSON.

## Root cause

`ProcessRunner` applied one 64 KiB capture cap to both stdout and stderr. On truncation it appended `\n...[truncated]`. The Cacophony `gh pr list` output exceeded 64 KiB and was cut inside a JSON string; the inserted newline was therefore reported by serde as an illegal control character at line 2. Stdout and stderr were already piped separately—the observed failure was deterministic in-stream truncation, not wrapper-stream mixing.

## Changes

- Use separate bounded capture limits:
  - stdout: 32 MiB for bounded provider JSON queries;
  - stderr: 64 KiB diagnostic evidence.
- JSON decoding continues to consume stdout only.
- `DiscoveryError::InvalidJson` now carries boxed, bounded first/last stdout and stderr excerpts without inflating the common error path.
- Structured `github_discovery_failed` details expose stage, command, serde message, separate stdout/stderr, `streams_combined=false`, and retry guidance.
- Deterministic tests cover:
  - valid JSON stdout larger than the historical 64 KiB cap;
  - control/diagnostic bytes on stderr remaining separate;
  - genuinely control-contaminated stdout failing closed;
  - nonzero exit preserving provider stderr without parsing valid-looking stdout;
  - machine-visible separate stream evidence.

## Exact live reproduction and local proof

Against a clean public Cacophony clone under the managed `gh` shim with `GH_REPO=harryaskham/cacophony`:

- Installed `~/.cargo/bin/cara 0.0.1 --json status` reproduced the exact failure: exit 1, `github_discovery_failed`, line-2 control-character InvalidJson; envelope 1,016 bytes, process stderr 0.
- Rebuilt fixed `target/debug/cara --json status` through the same wrapped path exited 0 with schema 1, repository `harryaskham/cacophony`, default `main`, healthy graph, 29 PRs / 29 ready unqueued, zero graph problems, zero compatibility failures; envelope 115,640 bytes, process stderr 0.

Helsinki's installed `~/.cargo/bin/cara` proof remains to run immediately after canonical landing/install.

## Validation

- 119 library tests passed.
- 4 binary tests passed.
- 5 CLI tests passed.
- 3 parity tests passed.
- Strict all-target Clippy passed.
- `nix flake check` passed.

## Follow-up

Draft `bd-75e83c` tracks a first-class typed output-limit error if a future provider response exceeds the new 32 MiB bound; this is not a current v1 blocker.
