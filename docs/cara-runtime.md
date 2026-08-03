# Caravan system runtime wrapper

Caravan owns the runtime prefix used when a same-repository Cacophony
`pr_cara_join` integration executes from a Caravan checkout:

```text
scripts/cara-runtime.sh --source system -- <cara arguments>
```

This is a source contract, not a rollout action. It does not edit live
Cacophony configuration, select a queue backend, mutate a provider, or run a
canary.

## Resolution and compatibility

The wrapper resolves exactly one rolling system Cara:

1. `CACO_CARA_BIN`, when set, is an authoritative explicit binding. A missing,
   non-executable, unlaunchable, or malformed binary fails closed; it never
   falls back to `PATH`.
2. Otherwise each `PATH` entry is inspected in order and the first executable
   whose bounded `cara --version` probe returns a semantic release is selected.

Loader overrides (`NIX_LD`, `NIX_LD_LIBRARY_PATH`, and `LD_LIBRARY_PATH`) are
removed from both probes and operations. `system`, `installed`, and `auto` are
accepted source spellings during consumer rollout; receipts always normalize
the source to `system`.

The reviewed floor is the single quoted top-level `min_cara_version` in
`.caravan/config.yaml`. Caravan currently uses the neutral rolling sentinel
`"0.0.0"`: version is recorded but does not pin upgrades. If repository policy
later raises the floor, the wrapper rejects an older system binary with typed
`cara_runtime_incompatible` before running a Cara command. Missing, duplicate,
or malformed floor policy is also a typed refusal.

## Bounded execution and receipts

The launchability probe defaults to 20 seconds. Cara operations default to 900
seconds, matching this repository's bounded sync window. Reviewers may lower
those values with `CARA_RUNTIME_PROBE_TIMEOUT_SECS` and
`CARA_RUNTIME_TIMEOUT_SECS`; both must be integers from 1 through 3600. The
wrapper requires Python 3 or GNU `timeout` and never launches an unbounded
operation. The Python runner forwards TERM/INT/HUP from an outer Cacophony
process-group deadline into Cara's child process group, so nested Git/provider
commands cannot outlive the wrapper. Timeout returns exit 124 with typed
`cara_runtime_timeout` evidence.

Every successful resolution writes a bounded stderr receipt containing:

- normalized source and selection mode;
- selected absolute binary path required by the Cacophony provenance parser;
- semantic version and reviewed minimum;
- immutable `sha256:` binary fingerprint;
- operation timeout.

Set `CARA_RUNTIME_RECEIPT` (or the consumer-compatible
`CACO_CARA_RESOLUTION_RECEIPT`) to write the JSON twin atomically. Its binary
path is home-redacted and includes the selected candidate plus fingerprint.
`--resolve` prints the selected absolute path on stdout without
running a queue operation, so a config/materialization consumer can record the
exact reviewed `runtime_bin` separately from human-visible diagnostics.

Typed failure codes include `cara_runtime_missing`,
`cara_runtime_incompatible`, `cara_runtime_config_invalid`,
`cara_runtime_fingerprint_unavailable`, `cara_runtime_bounded_runner_missing`,
and `cara_runtime_timeout`. Error bodies contain a bounded required action and
never include environment contents or provider credentials.

## Consumer metadata

A same-repository integration review should bind these facts together:

| Field | Reviewed value |
|---|---|
| wrapper | `scripts/cara-runtime.sh` from the exact Caravan source revision |
| argv prefix | `--source system --` |
| runtime bin | exact `--resolve` output/materialized system binary |
| runtime version/fingerprint | JSON resolution receipt |
| repository | `harryaskham/caravan` |
| base | `main` (or the exact reviewed same-repository base) |
| policy | `.caravan/config.yaml`, retaining `rebase_on_join: true` |

The consumer must keep its own command/provider deadline outside the wrapper as
a second bound. No force, direct-merge, label, Aviator, or sibling-checkout
fallback is authorized when resolution or execution fails.

## Validation and rollback

Run the offline contract from a Caravan checkout:

```sh
./tests/cara_runtime_contract.sh
```

It covers explicit system resolution, `PATH` resolution from this checkout,
argument passthrough, loader sanitization, deterministic fingerprints, missing
runtime, incompatible floor, timeout, and unsupported source refusal. No live
provider or Cacophony config is touched.

Rollback is source-only: stop new same-repository handoffs, restore the previous
reviewed wrapper/config consumer revision, and preserve every existing PR and
provider ref. Do not move tags, remove membership labels, switch queue actors,
or mutate a live node to compensate for a wrapper refusal.
