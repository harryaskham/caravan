# Session summary — authenticated GitHub webhook wakes

## Goal / bead

- `bd-4f4250` — replace high-frequency blind dashboard polling with authenticated, coalesced GitHub event wakes while preserving exact provider rediscovery.

## After state

- `cara web` optionally serves `POST /api/v1/webhooks/github` when given `--github-webhook-secret-env ENV` and exact `--github-installation-id ID`.
- The receiver remains loopback-only (public ingress requires an operator TLS tunnel/reverse proxy), bounds payload/header/concurrency, verifies GitHub `sha256` HMAC in constant time, checks installation and exact explicit repository, and accepts no secret in config/status/logs.
- Default-branch push, PR lifecycle/review, check run/suite, status, and workflow-run events are wake hints. Unknown/non-relevant events mutate nothing.
- Delivery IDs are validated, durably retained/deduplicated under private common-Git `caravan/webhooks` state, bounded to 1,000 entries and bounded reads.
- By default an accepted wake invalidates cache and refreshes canonical status. `--webhook-sync` enqueues one ordinary bounded typed `sync --all`; bursts and events during an action coalesce into at most one pending follow-up tick.
- Every wake goes through ordinary provider rediscovery, operation locking, budgets, preconditions, and receipts. Payload facts never authorize mutation.
- Webhook mode automatically stretches the unchanged default 15-second polling interval to a five-minute reconciliation fallback; explicit poll intervals remain honored.
- Schema v4 exposes secret-free enabled/action mode, accepted/deduped/rejected counts and latest event/delivery/time. Dashboard overview renders this telemetry.
- README/SPEC/help document setup, event subscriptions, read-only GitHub App delivery permissions, secret handling, and the separate ordinary Cara provider credential used for sync mutation.

## Validation

- New tests cover strict CLI/config combinations, exact HMAC and event classification, private durable delivery dedupe, and burst coalescing behind an active action.
- Full composed validation: 283 library + 12 binary + 8 CLI + 3 parity tests green; strict all-target/all-feature Clippy and rustfmt green; Nix flake check (11 checks) green.

## Commit

- Pre-reintegration implementation commit: `9c33f0f`.
