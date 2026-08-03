#!/usr/bin/env bash
# Offline contract for Caravan's checkout-owned pr_cara_join runtime wrapper.

set -euo pipefail

root="$(git rev-parse --show-toplevel)"
cd "$root"
workspace="$(mktemp -d "${TMPDIR:-/tmp}/cara-runtime-contract.XXXXXX")"
trap 'rm -rf "$workspace"' EXIT
fake="$workspace/cara"
log="$workspace/args.log"

cat > "$fake" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == --version ]]; then
  printf 'cara %s\n' "${FAKE_CARA_VERSION:-0.0.82}"
  exit 0
fi
if [[ "${1:-}" == spawn-child ]]; then
  (sleep 2; : > "$FAKE_CHILD_MARKER") &
  wait
  exit 0
fi
if [[ "${1:-}" == env-check ]]; then
  [[ -z "${NIX_LD:-}${NIX_LD_LIBRARY_PATH:-}${LD_LIBRARY_PATH:-}" ]]
fi
printf '%s\n' "$@" > "$FAKE_CARA_LOG"
SH
chmod 0755 "$fake"

receipt="$workspace/receipt.json"
resolved="$(
  CACO_CARA_BIN="$fake" \
    CACO_CARA_RESOLUTION_RECEIPT="$receipt" \
    ./scripts/cara-runtime.sh --source system --resolve \
    2>"$workspace/runtime.stderr"
)"
expected_resolved="$(cd "$(dirname "$fake")" && pwd -P)/$(basename "$fake")"
[[ "$resolved" = "$expected_resolved" ]]
grep -q '"source":"system"' "$receipt"
grep -q '"selection":"explicit_binding"' "$receipt"
grep -q '"version":"0.0.82"' "$receipt"
grep -q '"min_cara_version":"0.0.0"' "$receipt"
grep -Eq '"fingerprint":"sha256:[0-9a-f]{64}"' "$receipt"
grep -q '"candidates":\[{"path":' "$receipt"
grep -q '"state":"launchable","probe_exit":0' "$receipt"
grep -q '^caco-cara-runtime-receipt source=system binary=/' "$workspace/runtime.stderr"
grep -q 'version=cara\\ 0.0.82 version_probe_rc=0' "$workspace/runtime.stderr"
grep -Eq 'fingerprint=sha256:[0-9a-f]{64}' "$workspace/runtime.stderr"
first_fingerprint="$(sed -n 's/.*"fingerprint":"\([^"]*\)".*/\1/p' "$receipt")"

FAKE_CARA_LOG="$log" CACO_CARA_BIN="$fake" \
  NIX_LD=bad NIX_LD_LIBRARY_PATH=bad LD_LIBRARY_PATH=bad \
  ./scripts/cara-runtime.sh --source system -- env-check alpha 'two words'
mapfile -t args < "$log"
[[ "${args[*]}" = 'env-check alpha two words' ]]

# The same immutable binary yields the same receipt fingerprint.
CARA_RUNTIME_RECEIPT="$workspace/repeated.json" CACO_CARA_BIN="$fake" \
  ./scripts/cara-runtime.sh --source system --resolve >/dev/null
second_fingerprint="$(sed -n 's/.*"fingerprint":"\([^"]*\)".*/\1/p' "$workspace/repeated.json")"
[[ "$first_fingerprint" = "$second_fingerprint" ]]

set +e
CACO_CARA_BIN="$workspace/missing" ./scripts/cara-runtime.sh --source system -- status \
  >"$workspace/missing.out" 2>"$workspace/missing.err"
missing_rc=$?
set -e
[[ $missing_rc -eq 69 ]]
grep -q '"code":"cara_runtime_missing"' "$workspace/missing.err"

cat > "$workspace/future.yaml" <<'YAML'
version: 1
min_cara_version: "9.0.0"
rebase_on_join: true
YAML
set +e
CARA_RUNTIME_CONFIG="$workspace/future.yaml" CACO_CARA_BIN="$fake" \
  ./scripts/cara-runtime.sh --source system -- status \
  >"$workspace/future.out" 2>"$workspace/future.err"
future_rc=$?
set -e
[[ $future_rc -eq 78 ]]
grep -q '"code":"cara_runtime_incompatible"' "$workspace/future.err"

set +e
CARA_RUNTIME_TIMEOUT_SECS=1 CACO_CARA_BIN="$fake" \
  FAKE_CHILD_MARKER="$workspace/orphan-marker" \
  ./scripts/cara-runtime.sh --source system -- spawn-child \
  >"$workspace/timeout.out" 2>"$workspace/timeout.err"
timeout_rc=$?
set -e
[[ $timeout_rc -eq 124 ]]
grep -q '"code":"cara_runtime_timeout"' "$workspace/timeout.err"
/bin/sleep 2
[[ ! -e "$workspace/orphan-marker" ]] || {
  echo 'timed-out Cara descendant outlived the runtime wrapper' >&2
  exit 1
}

set +e
CACO_CARA_BIN="$fake" ./scripts/cara-runtime.sh --source private -- status \
  >"$workspace/source.out" 2>"$workspace/source.err"
source_rc=$?
set -e
[[ $source_rc -eq 64 ]]
grep -q 'unsupported source' "$workspace/source.err"

# Path discovery works from the Caravan checkout without a sibling repository.
env -u CACO_CARA_BIN PATH="$workspace:$PATH" FAKE_CARA_LOG="$workspace/path.log" \
  ./scripts/cara-runtime.sh --source system -- status --json
mapfile -t path_args < "$workspace/path.log"
[[ "${path_args[*]}" = 'status --json' ]]

echo 'cara runtime contract ok'
