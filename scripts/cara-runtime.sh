#!/usr/bin/env bash
# Resolve and execute Caravan's reviewed rolling system Cara runtime.
# Source-only contract for same-repository pr_cara_join consumers.

set -euo pipefail

source_mode="system"
resolve_only=0
while (($#)); do
  case "$1" in
    --source)
      (($# >= 2)) || { echo 'cara-runtime: --source requires a value' >&2; exit 64; }
      source_mode="$2"
      shift 2
      ;;
    --source=*)
      source_mode="${1#*=}"
      shift
      ;;
    --resolve)
      resolve_only=1
      shift
      ;;
    --)
      shift
      break
      ;;
    *)
      break
      ;;
  esac
done
case "$source_mode" in
  system|installed|auto) ;;
  *)
    printf 'cara-runtime: unsupported source %q; use --source system\n' "$source_mode" >&2
    exit 64
    ;;
esac

json_string() {
  local text="$1"
  text="${text//\\/\\\\}"
  text="${text//\"/\\\"}"
  text="${text//$'\n'/ }"
  text="${text//$'\r'/ }"
  text="${text//$'\t'/ }"
  printf '"%s"' "$text"
}

redact_path() {
  local path="$1"
  if [[ -n "${HOME:-}" && "$HOME" != / && "$path" == "$HOME"/* ]]; then
    local home_marker='~'
    printf '%s/%s' "$home_marker" "${path#"$HOME"/}"
  else
    printf '%s' "$path"
  fi
}

typed_error() {
  local code="$1" message="$2" action="$3" exit_code="$4"
  printf '{"schema_version":1,"status":"error","error":{"code":%s,"message":%s,"details":{"source":"system","required_action":%s}}}\n' \
    "$(json_string "$code")" "$(json_string "$message")" "$(json_string "$action")" >&2
  exit "$exit_code"
}

resolve_path() {
  local path="$1" resolved
  resolved="$(readlink -f "$path" 2>/dev/null || true)"
  if [[ -n "$resolved" ]]; then
    printf '%s' "$resolved"
  elif command -v python3 >/dev/null 2>&1; then
    python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]), end="")' "$path"
  else
    local directory base
    directory="$(cd "$(dirname "$path")" && pwd -P)" || return 1
    base="$(basename "$path")"
    printf '%s/%s' "$directory" "$base"
  fi
}

sha256_file() {
  local path="$1"
  local output
  if command -v sha256sum >/dev/null 2>&1; then
    output="$(sha256sum "$path")"
    printf '%s\n' "${output%% *}"
  elif command -v shasum >/dev/null 2>&1; then
    output="$(shasum -a 256 "$path")"
    printf '%s\n' "${output%% *}"
  elif command -v python3 >/dev/null 2>&1; then
    python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$path"
  else
    return 1
  fi
}

run_bounded() {
  local seconds="$1"
  shift
  if command -v python3 >/dev/null 2>&1; then
    python3 -c '
import os, signal, subprocess, sys
seconds = int(sys.argv[1])
argv = sys.argv[2:]
env = os.environ.copy()
for key in ("NIX_LD", "NIX_LD_LIBRARY_PATH", "LD_LIBRARY_PATH"):
    env.pop(key, None)
process = subprocess.Popen(argv, env=env, start_new_session=True)
forwarded = None
def forward(signum, _frame):
    global forwarded
    forwarded = signum
    if process.poll() is None:
        os.killpg(process.pid, signum)
for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
    signal.signal(signum, forward)
try:
    returncode = process.wait(timeout=seconds)
    raise SystemExit(128 + forwarded if forwarded is not None else returncode)
except subprocess.TimeoutExpired:
    os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
    raise SystemExit(124)
' "$seconds" "$@"
  elif command -v timeout >/dev/null 2>&1; then
    env -u NIX_LD -u NIX_LD_LIBRARY_PATH -u LD_LIBRARY_PATH \
      timeout --signal=TERM --kill-after=5s "${seconds}s" "$@"
  else
    return 125
  fi
}

semver_ge() {
  local actual="$1" required="$2" actual_major actual_minor actual_patch
  local required_major required_minor required_patch
  IFS=. read -r actual_major actual_minor actual_patch <<<"$actual"
  IFS=. read -r required_major required_minor required_patch <<<"$required"
  ((10#$actual_major > 10#$required_major)) && return 0
  ((10#$actual_major < 10#$required_major)) && return 1
  ((10#$actual_minor > 10#$required_minor)) && return 0
  ((10#$actual_minor < 10#$required_minor)) && return 1
  ((10#$actual_patch >= 10#$required_patch))
}

runtime_config="${CARA_RUNTIME_CONFIG:-.caravan/config.yaml}"
[[ -f "$runtime_config" ]] || typed_error \
  cara_runtime_config_missing \
  'Caravan runtime policy file is missing' \
  'run from the Caravan checkout or set CARA_RUNTIME_CONFIG to its reviewed config' \
  78
minimum="0.0.0"
minimum_count=0
while IFS= read -r line; do
  [[ "$line" =~ ^min_cara_version: ]] || continue
  ((minimum_count += 1))
  value="${line#*:}"
  value="${value%%#*}"
  value="${value#"${value%%[![:space:]]*}"}"
  value="${value%"${value##*[![:space:]]}"}"
  quote="${value:0:1}"
  if [[ ("$quote" == '"' || "$quote" == "'") && "${value: -1}" == "$quote" ]]; then
    minimum="${value:1:${#value}-2}"
  else
    minimum="invalid"
  fi
done < "$runtime_config"
((minimum_count <= 1)) || typed_error \
  cara_runtime_config_invalid \
  'min_cara_version appears more than once' \
  'keep one reviewed top-level min_cara_version value' \
  78
[[ "$minimum" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || typed_error \
  cara_runtime_config_invalid \
  'min_cara_version must be a quoted X.Y.Z value' \
  'repair the reviewed Caravan runtime floor' \
  78

probe_timeout="${CARA_RUNTIME_PROBE_TIMEOUT_SECS:-20}"
operation_timeout="${CARA_RUNTIME_TIMEOUT_SECS:-900}"
for value in "$probe_timeout" "$operation_timeout"; do
  if [[ ! "$value" =~ ^[0-9]+$ ]] || ((value < 1 || value > 3600)); then
    typed_error \
      cara_runtime_timeout_invalid \
      'runtime timeout must be an integer from 1 through 3600 seconds' \
      'set CARA_RUNTIME_PROBE_TIMEOUT_SECS and CARA_RUNTIME_TIMEOUT_SECS to bounded values' \
      64
  fi
done

selection="path_discovery"
candidates=()
if [[ -n "${CACO_CARA_BIN:-}" ]]; then
  selection="explicit_binding"
  candidates=("$CACO_CARA_BIN")
else
  IFS=: read -r -a path_entries <<<"${PATH:-}"
  for entry in "${path_entries[@]}"; do
    [[ -n "$entry" ]] || entry=.
    candidates+=("$entry/cara")
  done
fi

resolved=""
version=""
rejections=()
seen=:
for candidate in "${candidates[@]}"; do
  [[ -e "$candidate" && -x "$candidate" && ! -d "$candidate" ]] || {
    [[ "$selection" != explicit_binding ]] || rejections+=("missing_or_not_executable")
    continue
  }
  candidate_resolved="$(resolve_path "$candidate" 2>/dev/null || true)"
  [[ -n "$candidate_resolved" && -x "$candidate_resolved" ]] || {
    rejections+=("not_executable")
    continue
  }
  [[ "$seen" != *:"$candidate_resolved":* ]] || continue
  seen+="$candidate_resolved:"
  set +e
  probe_output="$(run_bounded "$probe_timeout" "$candidate_resolved" --version 2>&1)"
  probe_rc=$?
  set -e
  if [[ $probe_rc -ne 0 || ! "$probe_output" =~ ^cara[[:space:]]+([0-9]+\.[0-9]+\.[0-9]+) ]]; then
    rejections+=("unlaunchable_or_invalid_version")
    [[ "$selection" != explicit_binding ]] || break
    continue
  fi
  resolved="$candidate_resolved"
  version="${BASH_REMATCH[1]}"
  break
done

[[ -n "$resolved" ]] || typed_error \
  cara_runtime_missing \
  'no reviewed system Cara executable could be launched' \
  'install cara on PATH or set CACO_CARA_BIN to an executable Cara binary' \
  69
semver_ge "$version" "$minimum" || typed_error \
  cara_runtime_incompatible \
  "resolved Cara $version is older than required $minimum" \
  'upgrade the reviewed system Cara or lower min_cara_version only through reviewed repository policy' \
  78
fingerprint="$(sha256_file "$resolved" 2>/dev/null || true)"
[[ "$fingerprint" =~ ^[0-9a-fA-F]{64}$ ]] || typed_error \
  cara_runtime_fingerprint_unavailable \
  'could not compute an immutable SHA-256 fingerprint for the selected Cara binary' \
  'install sha256sum, shasum, or python3 before queue execution' \
  69

redacted_binary="$(redact_path "$resolved")"
receipt="${CARA_RUNTIME_RECEIPT:-${CACO_CARA_RESOLUTION_RECEIPT:-}}"
if [[ -n "$receipt" ]]; then
  {
    printf '{"schema_version":1,"status":"success","source":"system"'
    printf ',"selection":%s' "$(json_string "$selection")"
    printf ',"binary":%s' "$(json_string "$redacted_binary")"
    printf ',"version":%s' "$(json_string "$version")"
    printf ',"min_cara_version":%s' "$(json_string "$minimum")"
    printf ',"fingerprint":%s' "$(json_string "sha256:$fingerprint")"
    printf ',"timeout_secs":%s' "$operation_timeout"
    printf ',"candidates":[{"path":%s,"state":"launchable","probe_exit":0,"fingerprint":%s}]' \
      "$(json_string "$redacted_binary")" "$(json_string "sha256:$fingerprint")"
    printf '}\n'
  } > "$receipt.tmp"
  mv "$receipt.tmp" "$receipt"
fi
# `pr_cara_join` consumers parse this reviewed prefix and require an absolute
# binary path. Additional floor/fingerprint fields are additive and ignored by
# older parsers; the JSON receipt keeps the human-facing path home-redacted.
printf 'caco-cara-runtime-receipt source=system binary=%q version=%q version_probe_rc=0 selection=%s min_cara_version=%q fingerprint=sha256:%s timeout_secs=%s\n' \
  "$resolved" "cara $version" "$selection" "$minimum" "$fingerprint" "$operation_timeout" >&2

if ((resolve_only)); then
  printf '%s\n' "$resolved"
  exit 0
fi
(($# > 0)) || typed_error \
  cara_runtime_command_missing \
  'no Cara command followed the runtime wrapper separator' \
  'invoke scripts/cara-runtime.sh --source system -- <cara arguments>' \
  64

set +e
run_bounded "$operation_timeout" "$resolved" "$@"
operation_rc=$?
set -e
if [[ $operation_rc -eq 124 ]]; then
  typed_error \
    cara_runtime_timeout \
    "Cara command exceeded the reviewed ${operation_timeout}s runtime bound" \
    'inspect provider/runtime health and rerun the same idempotent command' \
    124
fi
if [[ $operation_rc -eq 125 ]]; then
  typed_error \
    cara_runtime_bounded_runner_missing \
    'no bounded process runner is available' \
    'install python3 or GNU timeout before queue execution' \
    69
fi
exit "$operation_rc"
