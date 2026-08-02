#!/usr/bin/env bash
# Deterministic contract for the non-blocking environmental acceptance lane.
# It uses fake cargo/cara executables; the real process/load acceptance remains
# feature-gated in tests/hook_example.rs and runs only in scheduled/manual CI.

set -euo pipefail

root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$root"

require() {
  grep -F -- "$1" "$2" >/dev/null || {
    echo "missing hook-acceptance contract marker '$1' in $2" >&2
    exit 1
  }
}

require 'environmental-hook-acceptance = []' Cargo.toml
require 'required-features = ["environmental-hook-acceptance"]' Cargo.toml
require 'schedule:' .github/workflows/hook-acceptance.yml
require 'workflow_dispatch:' .github/workflows/hook-acceptance.yml
require 'CARAVAN_FEEDBACK_WEBHOOK_URL' .github/workflows/hook-acceptance.yml
require 'CARAVAN_FEEDBACK_WEBHOOK_TOKEN' .github/workflows/hook-acceptance.yml
require 'operator_action_notifies_a_human_instead_of_spawning_an_agent' scripts/run-hook-acceptance.sh
require 'an_unresolvable_caravan_files_one_bead_and_dispatches_one_agent' scripts/run-hook-acceptance.sh
require '--fingerprint' scripts/run-hook-acceptance.sh
if grep -Eq '^[[:space:]]+(push|pull_request):' .github/workflows/hook-acceptance.yml; then
  echo "environmental hook acceptance must not become blocking push/PR CI" >&2
  exit 1
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/caravan-hook-contract.XXXXXX")"
trap 'rm -rf "$work"' EXIT
calls="$work/feedback-calls"

cat > "$work/cara" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
if [[ "$*" == *"feedback status"* ]]; then
  printf '%s\n' '{"data":{"enabled":true,"strategy":"webhook","destination":"webhook test"}}'
  exit 0
fi
if [[ "$*" == *"feedback report"* ]]; then
  printf '%s\n' "$*" >> "$FAKE_CARA_CALLS"
  printf '%s\n' '{"data":{"reported":true,"destination":"webhook test","bead_id":"bd-contract"}}'
  exit 0
fi
echo "unexpected fake cara invocation: $*" >&2
exit 64
SH
chmod 0755 "$work/cara"

cat > "$work/cargo" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' \
  'test operator_action_notifies_a_human_instead_of_spawning_an_agent ... ok' \
  'test an_unresolvable_caravan_files_one_bead_and_dispatches_one_agent ... FAILED' \
  "password=$CARAVAN_FEEDBACK_WEBHOOK_TOKEN"
exit "${FAKE_CARGO_STATUS:-0}"
SH
chmod 0755 "$work/cargo"

export FAKE_CARA_CALLS="$calls"
export CARAVAN_FEEDBACK_WEBHOOK_TOKEN='contract-secret-value'
common=(
  CARGO_BIN="$work/cargo"
  CARA_BIN="$work/cara"
  CARA_HOOK_ACCEPTANCE_ARCHITECTURE='contract-arch'
  CARA_HOOK_ACCEPTANCE_ENVIRONMENT='contract-runner'
  CARA_HOOK_ACCEPTANCE_ENVIRONMENT_CLASS='contract-environment'
  GITHUB_SHA='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
)

env "${common[@]}" FAKE_CARGO_STATUS=0 ./scripts/run-hook-acceptance.sh \
  > "$work/pass.log" 2>&1
[[ ! -e "$calls" ]] || {
  echo "passing acceptance unexpectedly filed feedback" >&2
  exit 1
}

for run in 1 2; do
  set +e
  env "${common[@]}" FAKE_CARGO_STATUS=17 ./scripts/run-hook-acceptance.sh \
    > "$work/fail-$run.log" 2>&1
  status=$?
  set -e
  [[ $status -eq 17 ]] || {
    echo "failing acceptance returned $status instead of preserving test status 17" >&2
    exit 1
  }
done

[[ "$(grep -F -c -- '--json feedback report' "$calls")" -eq 2 ]] || {
  echo "each failed run must emit exactly one feedback event" >&2
  exit 1
}
fingerprint='caravan-hook-acceptance-v1:contract-arch:contract-environment:cargo-test:hook_example'
[[ "$(grep -F -c -- "--fingerprint $fingerprint" "$calls")" -eq 2 ]] || {
  echo "identical reruns did not emit the same stable fingerprint" >&2
  exit 1
}
for marker in \
  'operator_action_notifies_a_human_instead_of_spawning_an_agent' \
  'an_unresolvable_caravan_files_one_bead_and_dispatches_one_agent' \
  'architecture: contract-arch' \
  'environment: contract-runner' \
  'environment_class: contract-environment' \
  'source_revision: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' \
  'failure_phase: cargo-test:hook_example' \
  '[REDACTED]'; do
  require "$marker" "$calls"
done
if grep -F -- 'contract-secret-value' "$calls" >/dev/null; then
  echo "feedback evidence leaked the configured webhook token" >&2
  exit 1
fi
