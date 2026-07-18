# First use

1. Run `cara status`. This is read-only and reports whether local configuration
   and required labels are ready.
2. If not ready, run `cara init` (or MCP tool `init`). This explicit operation:
   - atomically creates an absent `.caravan/config.yaml` with safe version-1
     defaults, using create-new semantics;
   - preserves every existing config byte and rejects invalid existing config;
   - verifies repository `WRITE` (or stronger) permission, squash auto-merge,
     and protected default-branch check/review policy;
   - creates missing canonical labels and verifies every result.
3. Resolve a reported metadata mismatch manually. Cara never overwrites an
   operator-owned label. Rerun `cara init`; retries and concurrent creation are
   safe and converge by exact re-read.
4. Run `cara status` again, then use `new`, `join`, or `sync`.

Canonical labels:

| Name | Color | Description |
| --- | --- | --- |
| `caravan` | `5319E7` | `Active member of a Caravan PR chain` |
| `caravan-evicted` | `B60205` | `Removed from a Caravan chain pending renew or rejoin` |
| `caravan-force` | `D93F0B` | `Allow configured force handling for known CI failures` |

For repositories created by earlier Caravan versions, the active label
`1D76DB` / `Active member of a Caravan merge chain` is also an exact compatible
definition. Cara preserves it and reports its actual metadata in the
`already_present` receipt; no other metadata variation is accepted.

`status` and `check` never initialize anything. `init` never changes a pull
request label, base, auto-merge state, branch, or commit. On a fully initialized
repository, repeated calls are verification-only no-ops with
`already_present` receipts.
