#!/usr/bin/env bash
# Run the environment-dependent hook acceptance target and report one bounded,
# fingerprinted failure through Caravan's configured feedback webhook.

set -euo pipefail

root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$root"

CARGO_BIN="${CARGO_BIN:-cargo}"
CARA_BIN="${CARA_BIN:-target/debug/cara}"
TEST_TARGET="hook_example"
FEATURE="environmental-hook-acceptance"
TEST_NAMES=(
  "operator_action_notifies_a_human_instead_of_spawning_an_agent"
  "an_unresolvable_caravan_files_one_bead_and_dispatches_one_agent"
)

if [[ ! -x "$CARA_BIN" ]]; then
  echo "hook acceptance: CARA_BIN is not executable: $CARA_BIN" >&2
  exit 64
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/caravan-hook-acceptance.XXXXXX")"
trap 'rm -rf "$work"' EXIT
raw_log="$work/cargo-test.log"
detail_file="$work/feedback-detail.txt"

set +e
"$CARGO_BIN" test --features "$FEATURE" --test "$TEST_TARGET" -- --nocapture \
  2>&1 | tee "$raw_log"
test_status=${PIPESTATUS[0]}
set -e

case "${CARA_HOOK_ACCEPTANCE_FORCE_FAILURE:-false}" in
  1|true|TRUE|yes|YES|on|ON)
    if [[ $test_status -eq 0 ]]; then
      test_status=97
      echo "forced_failure: workflow_dispatch acceptance drill" | tee -a "$raw_log"
    fi
    ;;
esac

if [[ $test_status -eq 0 ]]; then
  echo "hook acceptance: passed; no feedback event filed"
  exit 0
fi

# A failing acceptance must never silently fall back to stderr. Prove the
# authenticated webhook immediately before reporting; the secret-free status
# receipt never contains the bearer token. A missing/misconfigured reporter
# leaves the scheduled/manual workflow visibly red rather than hiding failure.
if ! feedback_status="$($CARA_BIN --json feedback status)"; then
  echo "hook acceptance: feedback status failed; refusing an unreportable failure" >&2
  exit 65
fi
if ! python3 -c '
import json, sys
status = json.load(sys.stdin).get("data", {})
ok = status.get("enabled") is True and status.get("strategy") == "webhook"
raise SystemExit(0 if ok else 1)
' <<<"$feedback_status"; then
  echo "hook acceptance: a configured, authenticated feedback webhook is required" >&2
  printf '%s\n' "$feedback_status" >&2
  exit 65
fi

architecture="${CARA_HOOK_ACCEPTANCE_ARCHITECTURE:-$(uname -m)}"
environment="${CARA_HOOK_ACCEPTANCE_ENVIRONMENT:-github-actions:${RUNNER_NAME:-unknown}:${RUNNER_OS:-unknown}:${RUNNER_ARCH:-unknown}}"
environment_class="${CARA_HOOK_ACCEPTANCE_ENVIRONMENT_CLASS:-github-actions:${RUNNER_OS:-unknown}:${RUNNER_ARCH:-unknown}}"
source_revision="${GITHUB_SHA:-$(git rev-parse HEAD)}"
failure_phase="cargo-test:${TEST_TARGET}"
# This identity deliberately excludes source revision and individual runner
# name: an unchanged architecture/environment-class/phase failure refreshes one
# canonical receiver-side record across equivalent runners and reruns. Exact
# runner identity and source revision remain in the event detail.
fingerprint_suffix="$(printf '%s' "$architecture:$environment_class:$failure_phase" | tr -c 'A-Za-z0-9._:-' '-')"
fingerprint="caravan-hook-acceptance-v1:$fingerprint_suffix"

if command -v sha256sum >/dev/null 2>&1; then
  output_sha256="$(sha256sum "$raw_log" | awk '{print $1}')"
else
  output_sha256="$(shasum -a 256 "$raw_log" | awk '{print $1}')"
fi

{
  printf 'environmental hook acceptance failed\n\n'
  printf 'test_target: %s\n' "$TEST_TARGET"
  printf 'tests:\n'
  printf -- '- %s\n' "${TEST_NAMES[@]}"
  printf 'architecture: %s\n' "$architecture"
  printf 'environment: %s\n' "$environment"
  printf 'environment_class: %s\n' "$environment_class"
  printf 'source_revision: %s\n' "$source_revision"
  printf 'failure_phase: %s\n' "$failure_phase"
  printf 'exit_status: %s\n' "$test_status"
  printf 'output_sha256: %s\n' "$output_sha256"
  printf 'output_tail_bytes: 16384\n\n'
  printf '%s\n' '--- bounded redacted output tail ---'
} > "$detail_file"

# Include enough output to diagnose the process/load failure without shipping
# the whole job log. Replace every credential-shaped environment value plus
# obvious inline assignments; the workflow result still retains the full local
# runner log for its authorized viewers.
python3 - "$raw_log" >> "$detail_file" <<'PY'
import os
import re
import sys

raw = open(sys.argv[1], "rb").read()[-16384:].decode("utf-8", "replace")
for key, value in os.environ.items():
    upper = key.upper()
    if value and any(word in upper for word in ("TOKEN", "SECRET", "PASSWORD")):
        raw = raw.replace(value, "[REDACTED]")
raw = re.sub(
    r"(?i)(token|secret|password)(\s*[:=]\s*)[^\s]+",
    r"\1\2[REDACTED]",
    raw,
)
sys.stdout.write(raw)
if not raw.endswith("\n"):
    sys.stdout.write("\n")
PY

detail="$(<"$detail_file")"
set +e
receipt="$($CARA_BIN --json feedback report \
  --kind error \
  --component hook-acceptance \
  --summary "Caravan environmental hook acceptance failed" \
  --detail "$detail" \
  --severity error \
  --fingerprint "$fingerprint" 2>&1)"
report_status=$?
set -e
printf 'hook acceptance feedback receipt: %s\n' "$receipt"
if [[ $report_status -ne 0 ]]; then
  echo "hook acceptance: feedback delivery failed" >&2
  exit 70
fi

exit "$test_status"
