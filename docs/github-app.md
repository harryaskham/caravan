# GitHub App deployment contract

Caravan supports a GitHub App identity as an **opt-in deployment mode**. It does
not replace local execution or ambient/stored `gh` authentication. The
machine-readable permission baseline is
[`github-app-policy.json`](github-app-policy.json).

## Modes and credential broker

Ambient mode is the default, including when `.caravan/config.yaml` is absent.
App identity must first be authorized by non-secret repository policy:

```yaml
github_auth:
  mode: app_installation
  app_slug: caravan
  installation_id: 12345
```

Ambient policy must not declare App identity fields. App policy requires both;
unknown modes/fields and invalid slugs/IDs fail strict config parsing. Config
validation remains offline and never needs credentials.

At production startup, repository policy is bound to all four deployment
settings. A broker path by itself does not activate App identity:

```sh
export CARA_GITHUB_AUTH_MODE=app_installation
export CARA_GITHUB_APP_CREDENTIAL_COMMAND=/absolute/path/to/reviewed-broker
export CARA_GITHUB_APP_SLUG=caravan
export CARA_GITHUB_APP_INSTALLATION_ID=12345
```

Mode, slug, and installation must exactly match repository policy. This check
runs for normal CLI/MCP context and every explicit `cara web --repo`; one web
process therefore cannot silently mix installations. Missing, mismatched, or
ambient-vs-App runtime selection fails before provider discovery.

The broker is one executable, not a shell command. Cara passes
`CARA_GITHUB_APP_HOST` and `CARA_GITHUB_APP_REPOSITORY` to it. On stdout it must
return exactly one JSON object with no additional fields:

```json
{
  "token": "<short-lived installation token>",
  "app_slug": "caravan",
  "installation_id": 12345,
  "repository": "owner/name",
  "expires_unix_secs": 1893456000
}
```

Never put that response, an App private key, or a token in repository config,
argv, a URL, a generated helper script, logs, receipts, or checkpoints. The
broker may use a protected key file, KMS, workload identity, or a hosted token
service. Cara validates exact repository, slug, installation, and expiry; pins
the first principal for the process lifetime; caches only a usable short-lived
token; single-flights refresh; and retries one authentication failure. Invalid
or incomplete App mode fails closed and never falls back to a human account.

## Repository permissions

Install the App only on repositories Caravan operates. Start with this baseline:

| Repository permission | Level | Why |
|---|---:|---|
| Metadata | Read | Resolve exact repository and installation identity. |
| Contents | Write | Fetch and lease-protected update of managed source branches. |
| Pull requests | Write | Read/update bases, merge, and manage PR state. |
| Issues | Write | PR comments and labels use issue-compatible endpoints. |
| Checks | Read | Read required check runs and suites. |
| Commit statuses | Read | Read required legacy status contexts. |
| Actions | Read | Read workflow/check state. |

Two upgrades are conditional, not baseline permissions:

- **Actions: write** only when the deployment explicitly enables workflow reruns.
- **Workflows: write** only when managed source branches are allowed to change
  `.github/workflows/*`. Without it, such a push must fail rather than broaden
  the installation silently.

Do not grant Administration, Members, organization administration, repository
or organization secrets, or blanket default-branch bypass. Permission changes
require review of `github-app-policy.json` and its contract test.

## Git transport and branch rules

API and Git use the same cached installation principal. Remote Git accepts one
exact HTTPS repository. Cara supplies `x-access-token` credentials through a
static environment-configured credential helper whose text contains no token.
It clears other helpers, prompts, hooks, Git/cURL tracing, insecure TLS flags,
and injected config parameters for that child. It rejects SSH, plaintext HTTP,
local or mismatched remotes, URL credentials, multiple origin URLs, `pushurl`,
`url.*.insteadOf`, and URL-specific TLS overrides before credential resolution.
All existing force-with-lease OID checks remain authoritative.

Repository rules should let the App force-update **only Caravan-managed PR
source branches**. Do not permit force-push or bypass on the default branch.
Restrict branch naming/ownership using the same reviewed policy that authorizes
physical rewrites. Installation is identity, not mutation authority: Cara's
manual-decision, label, lease, result-tree, budget, and lock checks still apply.

## Webhooks

`cara web` already provides a loopback GitHub webhook receiver:

```sh
export CARA_GITHUB_WEBHOOK_SECRET='<from secret manager>'
cara web --repo /srv/caravan/repository \
  --github-webhook-secret-env CARA_GITHUB_WEBHOOK_SECRET \
  --github-installation-id 12345 \
  --webhook-sync
```

Put a TLS reverse proxy or private tunnel in front of the loopback listener. The
receiver verifies `X-Hub-Signature-256`, bounds payload/header sizes, binds the
installation and explicit repository, durably deduplicates
`X-GitHub-Delivery`, coalesces bursts, and keeps fallback polling. Events are
wake hints only; every job re-reads provider state. Current wake subscriptions
are `push`, `pull_request`, `pull_request_review`, `check_run`, `check_suite`,
`workflow_run`, and `status`. Future hosted tenancy also needs `installation`
and `installation_repositories` lifecycle events.

## Attribution

GitHub API writes and HTTPS branch updates should appear as
`<app-slug>[bot]`. The operator-authorized live proof is tracked by
`bd-71e444`; documentation is not that proof. Rewritten commits use the stable
metadata `Caravan <caravan@localhost.invalid>`. This identifies generated
objects but is neither GitHub App attribution nor a cryptographic signature.

## Local and hosted writers

Local and future hosted execution may coexist in the product, but exactly one
is the configured writer for a repository. The broker and guard contract is in
[`remote-writer-lease.md`](remote-writer-lease.md), and `writer.mode:
remote_fenced` activates it. The other may read and plan. The machine-local
operation lock alone cannot fence two hosts, so cross-host safety requires that
remote compare-and-swap lease with repository identity, owner, operation ID,
expiry, heartbeat, and monotonically fenced token, rechecked before every
provider or branch write. Lease-service failure fails closed; takeover is
explicit and audited. Automatic failover remains unavailable by design.

## Hosted worker mode (`cara web --hosted`)

`cara web --hosted` runs the existing dashboard/webhook receiver under a strict
deployment contract over **pre-provisioned** repository checkouts. It adds no
second receiver: HMAC verification, delivery dedup, per-repository serialized
action queues, fallback polling, authoritative provider reread, operation
budgets, and remote lease fencing are the same code paths local mode uses.

Startup refuses unless all of the following hold:

- every `--repo` is an existing Git worktree the operator already provisioned;
- a signed webhook secret env and the exact `--github-installation-id` are set;
- `--webhook-sync` is on and `--read-only` is off;
- each repository sets `github_auth.mode: app_installation` pinned to that same
  installation, `writer.mode: remote_fenced`, and exact `repository: owner/name`.

Mixed installations, ambient auth, `local_only`, a missing slug, or a missing
remote-lease broker/host/writer identity all fail closed before serving. The
listener stays loopback-only; expose it through an operator-owned TLS reverse
proxy or tunnel, and supply the webhook secret from a secret manager as an
environment variable name, never a literal.

Dashboard state reports `hosted` plus each repository's non-secret auth/writer
policy, so a misconfigured member is visible rather than silently ambient.

Explicitly **not** included: automatic installation onboarding, tenancy
management, clone/checkout provisioning or garbage collection, and automatic
failover. Exactly one worker may be the configured writer per repository; a
second host may read and plan. Those remain separate, later work.
