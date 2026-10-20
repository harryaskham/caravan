# Failure classification fixtures (bd-7e0aa1)

One JSON error envelope per failure class, exercised by
`src/failure_outcome/tests.rs`.

- `caco-force-intent-dead-letter-268.json`: the real Cacophony reintegration
  record for Caravan PR #268 on 2026-10-20. Cara enrolled the PR (label and
  durable membership observed); deployed Caco then refused `force_intent: none`.
  The canonical process-only refusal.
- `cara-app-probe-timeout.json`: Cara's typed App probe timeout (bd-096b1e).
- `caco-graphql-rate-limit.json`: the publication refusal text seen on
  2026-10-20; the envelope code is illustrative, the message is verbatim.
- `cara-graphql-internal-error.json`: GitHub's generic GraphQL internal error.
- `readiness-deferred-label-absent-4575.json`: Cacophony PR4575's readiness
  refusal line (bd-06e059).
- `cara-stack-recovery-backend-unhealthy.json`: the repeated sync refusal that
  starved the queue on 2026-10-12 (bd-4d4621).
- `cara-membership-envelope-rolled-back.json`: bd-420191's rollback code.
- `caco-native-stack-handoff-270.json`: Caravan PR #270, published on PR #269's
  branch; #269 merged first, #270 was retargeted, enrolled and merged, and Caco
  still refused its native Stack handoff. A new process-only class.
- `cara-rebase-conflict.json`, `cara-command-timeout.json`: ordinary conflict
  and timeout classes.

No fixture contains a token or credential.
