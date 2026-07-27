#!/bin/sh
# Cara hook: turn one canonical Caravan event into exactly one Cacophony bead.
#
# Contract:
# - Cara runs this with the event JSON on stdin and CARA_* environment values
#   set, from the repository root, under the configured hook timeout.
# - Delivery may repeat after a partial Cara operation, so this script is
#   idempotent: the exact `cara-event:<CARA_EVENT_ID>` label is the dedupe key.
# - It never prints secrets, never mutates GitHub, and never blocks the queue:
#   configure it with `blocking: false` so hook failure cannot roll back
#   completed provider work.
#
# Install: see examples/hooks/README.md.

set -eu

CACO_BIN="${CACO_BIN:-caco}"
PROJECT="${CARA_HOOK_PROJECT:-${CACO_PROJECT:-cacophony}}"
PRIORITY="${CARA_HOOK_PRIORITY:-1}"
EVENT="${CARA_EVENT:-unknown}"
EVENT_ID="${CARA_EVENT_ID:-}"
REPOSITORY="${CARA_REPOSITORY:-unknown}"
CARAVAN_ID="${CARA_CARAVAN_ID:-}"
PRS="${CARA_PRS:-}"

# Bounded read: Cara already caps event payloads at one megabyte.
PAYLOAD="$(head -c 65536)"

if [ -z "$EVENT_ID" ]; then
  echo "cara hook: CARA_EVENT_ID is required for idempotent dispatch" >&2
  exit 64
fi

case "$EVENT" in
  ci_failed|sync_failed|join_failed|eviction_failed|force_merge_attempted) ;;
  *)
    # Informational events need no agent; exit cleanly so the tick stays green.
    exit 0
    ;;
esac

# Not every failed tick is a problem worth dispatching for. Cara classifies each
# one, and the wake class is the load-bearing field:
#
#   none, retry_tick   a healthy tick or a bounded provider race. The next cron
#                      tick rediscovers fresh provider state and resolves it, so
#                      dispatching here spawns work that has already fixed
#                      itself.
#   external_decision  a caravan that cannot resolve itself. Dispatch.
#   operator_action    config, permission, or checkout work no agent can do.
#
# Absent (an older Cara, or an event carrying no scheduler status) is treated as
# dispatchable, because failing open to a human beats silently dropping a stuck
# caravan.
WAKE_CLASS="$(
  printf '%s' "$PAYLOAD" |
    sed -n 's/.*"wake_class"[[:space:]]*:[[:space:]]*"\([a-z_]*\)".*/\1/p' | head -n 1
)"
case "$WAKE_CLASS" in
  retry_tick|none)
    echo "cara hook: ${EVENT} ${EVENT_ID} is ${WAKE_CLASS}; the next tick resolves it" >&2
    exit 0
    ;;
esac

# Dedupe across ticks, not just across redeliveries. CARA_EVENT_ID is unique per
# emission, so a caravan stuck for an hour emits a new id every tick; on a
# one-minute cron that is sixty beads for one problem. Cara therefore publishes
# decision_fingerprint, which is stable for as long as the same decision remains
# unresolved. Prefer it, and fall back to the event id when it is absent.
FINGERPRINT="$(
  printf '%s' "$PAYLOAD" |
    sed -n 's/.*"decision_fingerprint"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1
)"
if [ -n "$FINGERPRINT" ]; then
  DEDUPE_LABEL="cara-decision:$(printf '%s' "$FINGERPRINT" | tr -c 'A-Za-z0-9._-' '-')"
else
  DEDUPE_LABEL="cara-event:${EVENT_ID}"
fi

existing="$(
  "$CACO_BIN" bd list --project "$PROJECT" --label "$DEDUPE_LABEL" --count-only --json 2>/dev/null |
    sed -n 's/.*"count"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' | head -n 1
)"
if [ "${existing:-0}" -gt 0 ]; then
  echo "cara hook: ${EVENT} ${EVENT_ID} already dispatched" >&2
  exit 0
fi

title="cara ${EVENT}: ${REPOSITORY} PR ${PRS:-none}"

description_file="$(mktemp)"
trap 'rm -f "$description_file"' EXIT
{
  printf 'Canonical Caravan event dispatched by a Cara hook.\n\n'
  printf -- '- event: %s\n' "$EVENT"
  printf -- '- event_id: %s\n' "$EVENT_ID"
  printf -- '- operation_id: %s\n' "${CARA_OPERATION_ID:-unknown}"
  printf -- '- repository: %s\n' "$REPOSITORY"
  printf -- '- caravan_id: %s\n' "${CARAVAN_ID:-none}"
  printf -- '- prs: %s\n' "${PRS:-none}"
  printf -- '- wake_class: %s\n' "${WAKE_CLASS:-unknown}"
  printf -- '- dedupe: %s\n' "$DEDUPE_LABEL"
  # shellcheck disable=SC2016 # literal markdown backticks, not substitution
  printf '\nExact bounded Cara evidence:\n\n```json\n%s\n```\n' "$PAYLOAD"
  # shellcheck disable=SC2016 # literal markdown backticks, not substitution
  printf '\nContinuation: rerun the same idempotent `cara sync --all`. '
  printf 'Repair the exact reported PR generation; never hand-merge, force-push, or edit control labels directly.\n'
} > "$description_file"

"$CACO_BIN" bd create \
  --project "$PROJECT" \
  --title "$title" \
  --type bug \
  --priority "$PRIORITY" \
  --labels "caravan,cara-hook,${EVENT},${DEDUPE_LABEL}" \
  --description-file "$description_file" \
  --json > /dev/null

echo "cara hook: filed ${EVENT} ${EVENT_ID} for ${REPOSITORY} ${PRS:-none}" >&2

# Operator action cannot be delegated: an agent cannot change repository
# settings or clean a dirty checkout. File the bead, tell a human, stop.
if [ "$WAKE_CLASS" = "operator_action" ]; then
  "$CACO_BIN" msg broadcast --project "$PROJECT" \
    --body "[cara] ${REPOSITORY} caravan #${CARAVAN_ID:-?} needs operator action: ${EVENT} (${DEDUPE_LABEL})." \
    > /dev/null 2>&1 || true
  exit 0
fi

# Otherwise dispatch one agent for this decision. Failure here is not fatal: the
# bead is already durable and visible, and the next tick re-emits the same
# fingerprint, so nothing is lost.
if [ "${CARA_HOOK_DISPATCH_AGENT:-1}" = "1" ]; then
  "$CACO_BIN" agent new --project "$PROJECT" --type pi --label "$DEDUPE_LABEL" \
    > /dev/null 2>&1 ||
    echo "cara hook: filed ${DEDUPE_LABEL} but could not spawn an agent; dispatch it manually" >&2
fi
