# Creating the Caravan GitHub App

This is the operator click-path for producing the three non-secret values and
one broker that `docs/github-app.md` requires. Cara never receives a private key
or a token directly: it invokes a broker executable that mints a short-lived
installation token on demand.

At the end you will have:

| Value | Example | Where it goes |
|---|---|---|
| App slug | `caravan` | `github_auth.app_slug` + `CARA_GITHUB_APP_SLUG` |
| Installation ID | `12345` | `github_auth.installation_id` + `CARA_GITHUB_APP_INSTALLATION_ID` |
| Broker path | `/opt/caravan/bin/caravan-token-broker` | `CARA_GITHUB_APP_CREDENTIAL_COMMAND` |

## 1. Create the App

GitHub UI: **Settings → Developer settings → GitHub Apps → New GitHub App**
(personal), or **Organization settings → Developer settings → GitHub Apps → New
GitHub App** (organization-owned, recommended for shared repositories).

- **GitHub App name**: `Caravan` (the slug is derived from it, for example
  `caravan`; if the name is taken, note the slug GitHub actually assigns).
- **Homepage URL**: any stable URL, for example the Caravan repository.
- **Webhook**: uncheck **Active** unless you are deploying `cara web --hosted`.
  If you do enable it, set the URL to your receiver and generate a strong secret
  (see `docs/github-app.md` for the `X-Hub-Signature-256` contract).
- **Where can this App be installed**: "Only on this account" unless you
  deliberately want it public.

## 2. Set repository permissions

Set exactly the baseline in [`github-app-policy.json`](github-app-policy.json):

| Permission | Level |
|---|---|
| Metadata | Read-only |
| Contents | Read and write |
| Pull requests | Read and write |
| Issues | Read and write |
| Checks | Read-only |
| Commit statuses | Read-only |
| Actions | Read-only |

Conditional upgrades, each only if you actually need it:

- **Actions: Read and write** — only if the deployment authorizes workflow
  reruns.
- **Workflows: Read and write** — only if managed source branches may modify
  `.github/workflows/*`.
- **Administration: Read and write** — only for a reviewed native GitHub Stack
  deployment that needs the exact-ref Stack merge lock ruleset. This is not a
  baseline permission; do not grant it for ordinary Caravan mode.

Do not grant Members, Organization administration, or any secrets permission.

Subscribe to events only if webhooks are enabled: `push`, `pull_request`,
`pull_request_review`, `check_run`, `check_suite`, `workflow_run`, `status`.

Click **Create GitHub App**.

## 3. Record the slug and generate a private key

On the App settings page:

- The **App slug** is the last path segment of the App URL, for example
  `https://github.com/apps/caravan` → `caravan`. Record it.
- Under **Private keys**, click **Generate a private key**. A `.pem` file
  downloads. Store it where only the broker can read it, for example
  `chmod 600` under a service-owned directory, or in KMS/a secret manager. It
  never goes into repository config, environment variables Cara reads, argv, or
  any log.
- Record the numeric **App ID** shown at the top; the broker needs it to sign
  its JWT.

## 4. Install the App and record the installation ID

Click **Install App**, choose the account, and select **Only select
repositories** with the exact repositories Caravan manages.

After installing, the browser URL is:

```
https://github.com/settings/installations/12345
```

That trailing number is the **installation ID**. For an organization it is
`https://github.com/organizations/<org>/settings/installations/12345`.

You can also read it back with an already-authenticated `gh`:

```sh
gh api /repos/OWNER/REPO/installation --jq '.id'
```

## 5. Write the broker

The broker is one executable. Cara sets `CARA_GITHUB_APP_HOST` and
`CARA_GITHUB_APP_REPOSITORY` in its environment, and it must print exactly one
JSON object on stdout:

```json
{
  "token": "<short-lived installation token>",
  "app_slug": "caravan",
  "installation_id": 12345,
  "repository": "owner/name",
  "expires_unix_secs": 1893456000
}
```

A minimal reference implementation using `gh` and a local key file:

```sh
#!/usr/bin/env bash
# /opt/caravan/bin/caravan-token-broker
set -euo pipefail

APP_ID=111111
APP_SLUG=caravan
INSTALLATION_ID=12345
KEY_FILE=/etc/caravan/caravan.private-key.pem

jwt="$(
  python3 - "$APP_ID" "$KEY_FILE" <<'PY'
import base64, json, sys, time
import jwt as pyjwt  # pip install pyjwt cryptography
app_id, key_file = sys.argv[1], sys.argv[2]
now = int(time.time())
payload = {"iat": now - 60, "exp": now + 540, "iss": app_id}
print(pyjwt.encode(payload, open(key_file).read(), algorithm="RS256"))
PY
)"

response="$(
  curl -fsS -X POST \
    -H "Authorization: Bearer ${jwt}" \
    -H "Accept: application/vnd.github+json" \
    "https://api.github.com/app/installations/${INSTALLATION_ID}/access_tokens"
)"

token="$(printf '%s' "$response" | jq -r '.token')"
expires="$(printf '%s' "$response" | jq -r '.expires_at')"

jq -nc \
  --arg token "$token" \
  --arg app_slug "$APP_SLUG" \
  --argjson installation_id "$INSTALLATION_ID" \
  --arg repository "${CARA_GITHUB_APP_REPOSITORY}" \
  --argjson expires_unix_secs "$(date -u -d "$expires" +%s 2>/dev/null || python3 -c "
import datetime,sys;print(int(datetime.datetime.strptime(sys.argv[1],'%Y-%m-%dT%H:%M:%SZ').replace(tzinfo=datetime.timezone.utc).timestamp()))" "$expires")" \
  '{token: $token, app_slug: $app_slug, installation_id: $installation_id, repository: $repository, expires_unix_secs: $expires_unix_secs}'
```

Requirements Cara enforces on the response: exact repository, exact slug, exact
installation ID, a future expiry, and no additional fields. Anything else fails
closed rather than falling back to a human account.

Make it executable and owned by the service account:

```sh
sudo install -m 0755 caravan-token-broker /opt/caravan/bin/caravan-token-broker
```

## 6. Wire it up

Repository policy (`.caravan/config.yaml`), non-secret:

```yaml
github_auth:
  mode: app_installation
  app_slug: caravan
  installation_id: 12345
```

Deployment environment:

```sh
export CARA_GITHUB_AUTH_MODE=app_installation
export CARA_GITHUB_APP_CREDENTIAL_COMMAND=/opt/caravan/bin/caravan-token-broker
export CARA_GITHUB_APP_SLUG=caravan
export CARA_GITHUB_APP_INSTALLATION_ID=12345
```

Mode, slug, and installation must match repository policy exactly, or Cara
refuses before any provider read. A repository using native GitHub Stacks also
needs its reviewed rollout declaration; selecting the backend alone is never
enough:

```yaml
min_cara_version: "0.0.65"
stack_type: github
stack_rollout:
  mutations_opt_in: true
  reviewed_by: "operator/change-ticket"
rebase_on_join: false
sync:
  head_merge_actor: caravan
```

## 7. Verify

```sh
# The broker alone, outside Cara:
CARA_GITHUB_APP_HOST=github.com \
CARA_GITHUB_APP_REPOSITORY=owner/repo \
  /opt/caravan/bin/caravan-token-broker | jq 'del(.token)'

# Then Cara, which reports the resolved principal:
cara status
```

`cara status` shows the auth source it actually used. App attribution is proven
when provider timeline entries for Caravan-owned mutations are authored by the
App principal rather than a human account.

## Troubleshooting

| Symptom | Cause |
|---|---|
| Config parse error naming App fields | ambient policy declared App identity, or App policy omitted one of slug/installation |
| Startup refusal before discovery | environment mode/slug/installation disagrees with repository policy |
| Broker rejected | extra JSON fields, wrong repository/slug/installation, or past expiry |
| `Resource not accessible by integration` | the App is not installed on that repository, or a required permission was not granted |
| Permission change not taking effect | organization owners must approve newly requested permissions before they apply |
