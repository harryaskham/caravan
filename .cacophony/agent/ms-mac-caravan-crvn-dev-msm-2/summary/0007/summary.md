# Session summary — Separate feedback diagnostics from machine stderr

## Goal

Repair the missing-feedback-token startup contract narrowly: machine JSON commands must keep stderr empty and preserve a single parseable envelope, while operators and explicit feedback commands still receive actionable, secret-free configuration evidence.

## Bead(s)

- `bd-e7aa87` — Fix `json_config_error_keeps_the_machine_envelope` without masking absent feedback tokens.

## Before state

- Running a JSON config-error command with `CACOPHONY_FEEDBACK_TOKEN` absent returned the correct `config_parse_failed` envelope on stdout but also wrote 116 bytes to stderr from `Reporter::from_config` during panic-hook installation.
- The process tests injected a dummy token, hiding the production startup behavior rather than testing it.
- `cara --json feedback status` emitted the same warning twice and misleadingly reported `enabled: true` with destination `disabled`.

## After state

- CLI parsing now determines output mode before panic-feedback installation. When machine mode detects invalid startup feedback configuration, it installs a disabled panic reporter so optional diagnostics cannot pollute stderr.
- Human mode retains exactly one startup warning containing `feedback_config_invalid` and the missing token variable.
- `feedback status` is side-effect-free and reports effective `enabled: false`, destination `disabled`, plus a typed `configuration_error` with code, message, and remediation.
- Explicit CLI and MCP feedback reports fail with typed `feedback_config_invalid` evidence instead of silently claiming a disabled reporter delivered the event.
- Process tests deliberately configure an absent token; no dummy credential masks the contract.

## Diff summary

- Code/content commit: `f26a31c51a3140397741c65793155a9ad7a1a17d`.
- Summary artefact commit: intentionally omitted; this file must not self-reference its own mutable SHA.
- Files touched: `src/lib.rs`, `src/main.rs`, `tests/cli_exit.rs`.
- Tests: CLI process suite expanded from 5 to 7 tests; MCP parity remained green.
- Configured gate: `nix build .#caravan --no-link --print-build-logs` passed with 182 library, 4 binary, 7 CLI process, and 3 parity tests.
- Behavioural delta: machine stdout/stderr stays protocol-clean, human startup remains observable, and explicit status/report surfaces expose effective feedback misconfiguration truthfully.

## Operator-takeaway

The fix does not suppress feedback failures globally or fake a token in tests. It moves optional startup observability to output-mode-appropriate channels and makes the explicit feedback surfaces more truthful when reporting has been disabled by invalid configuration.
