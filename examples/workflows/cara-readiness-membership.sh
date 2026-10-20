#!/usr/bin/env bash
# Reference body for a consumer's required "Caravan admission gate" readiness
# step (bd-06e059). Cacophony's .github/workflows/cara-readiness.yml runs this
# logic after the reusable `cara ci-admission-gate` observation job.
#
# Inputs (environment): GITHUB_EVENT_NAME, GITHUB_EVENT_PATH,
# GITHUB_REPOSITORY, GITHUB_RUN_ID, RUNNER_TEMP, GH_TOKEN, and the observation
# outputs ADMISSION_RESULT, MEMBERSHIP_STATE, ADMISSION_DECISION, RUN_CI,
# DEFERRED_UNJOINED, OBSERVED_HEAD, OBSERVED_BASE, OBSERVED_PR,
# MEMBERSHIP_LABEL (the configured label name, not observed presence).
# Optional READINESS_REPORTING_APP_ID (default 15368, GitHub Actions).
#
# Terminal states, each with one machine-readable line Cara parses from the
# bounded job log:
#   exit 0   current-generation membership proven.
#   exit 78  `cara-deferred-admission ... decision=deferred_unjoined exit=78`:
#            exact unjoined generation; heavy CI waits for membership.
#   exit 78  `cara-admission-unevaluated ... reason=<membership_changed|draft> exit=78`:
#            this run did not evaluate the current generation. The member
#            label changed between the observation and this step's fresh
#            provider read (for example Cara joined the PR mid-run), or the
#            generation is a draft. Cara treats the run as unevaluated, never
#            red, and reruns the exact gate once the PR is a member.
#   exit 1   `cara-readiness-refused predicate=<name> exit=1` on stderr: an
#            identity, provider or reporting predicate failed.
#
# The label checks stay strict: a deferral is never reported while the label
# is present and membership is never reported while it is absent. Only static
# predicate names and the exit status are diagnostic. Never trace commands,
# event or provider bodies, arguments or environment.
set -euo pipefail
predicate=event_kind
work=''
pr=''
head=''
base=''
readiness_exit() {
  status=$?
  trap - EXIT
  if [[ "$status" != 0 && ( "$status" != 78 || ( "$predicate" != deferred_unjoined && "$predicate" != unevaluated_* ) ) ]]; then
    printf 'cara-readiness-refused predicate=%s exit=%s\n' "$predicate" "$status" >&2
  fi
  if [[ -n "$work" ]]; then rm -rf "$work" || true; fi
  exit "$status"
}
trap readiness_exit EXIT
unevaluated() {
  predicate="unevaluated_$1"
  printf 'cara-admission-unevaluated pr=%s head=%s base=%s reason=%s exit=78\n' "$pr" "$head" "$base" "$1"
  exit 78
}
test "$GITHUB_EVENT_NAME" = pull_request
predicate=observer_result
test "$ADMISSION_RESULT" = success
predicate=event_action
jq -e '.action | IN("opened", "synchronize", "reopened", "ready_for_review")' "$GITHUB_EVENT_PATH" >/dev/null
predicate=event_repository
repo="$(jq -er '.repository.full_name' "$GITHUB_EVENT_PATH")"
predicate=event_pr
pr="$(jq -er '.pull_request.number' "$GITHUB_EVENT_PATH")"
predicate=event_head
head="$(jq -er '.pull_request.head.sha' "$GITHUB_EVENT_PATH")"
predicate=event_base
base="$(jq -er '.pull_request.base.sha' "$GITHUB_EVENT_PATH")"
predicate=repository_identity
test "$repo" = "$GITHUB_REPOSITORY"
predicate=event_draft
# A draft is not ready: report it as unevaluated so ready_for_review's run is
# the one that decides, instead of a red refusal that looks like a failure.
if ! jq -e '.pull_request.draft == false' "$GITHUB_EVENT_PATH" >/dev/null; then
  unevaluated draft
fi
predicate=observed_pr
test "$OBSERVED_PR" = "$pr"
predicate=observed_head
test "$OBSERVED_HEAD" = "$head"
predicate=observed_base
test "$OBSERVED_BASE" = "$base"
predicate=scratch_create
work="$(mktemp -d "$RUNNER_TEMP/cara-ready.XXXXXX")"
predicate=provider_read
gh api --method GET "repos/$repo/pulls/$pr" > "$work/provider.json"
predicate=provider_draft
if jq -e '.draft == true' "$work/provider.json" >/dev/null; then
  unevaluated draft
fi
predicate=provider_tuple
jq -e --slurpfile event "$GITHUB_EVENT_PATH" '
  $event[0] as $e | .number == $e.pull_request.number and .state == "open" and .draft == false
  and .head.sha == $e.pull_request.head.sha and .head.ref == $e.pull_request.head.ref
  and .head.repo.full_name == $e.repository.full_name
  and .base.repo.full_name == $e.repository.full_name
  and .base.sha == $e.pull_request.base.sha and .base.ref == $e.pull_request.base.ref
' "$work/provider.json" >/dev/null
# Bind the reporting context to this workflow's actual GitHub Actions check,
# not a similarly named check from another App or generation.
predicate=reporting_jobs_read
gh api --method GET "repos/$repo/actions/runs/$GITHUB_RUN_ID/jobs?filter=latest&per_page=100" > "$work/jobs.json"
predicate=reporting_job_identity
check_id="$(jq -er --arg head "$head" --argjson run "$GITHUB_RUN_ID" '
  [.jobs[] | select(.name == "Caravan admission gate" and .head_sha == $head and .run_id == $run)]
  | if length == 1 then .[0].check_run_url | split("/")[-1] | tonumber else error("ambiguous reporting job") end
' "$work/jobs.json")"
predicate=reporting_check_read
gh api --method GET "repos/$repo/check-runs/$check_id" > "$work/check.json"
predicate=reporting_check_identity
jq -e --arg head "$head" --argjson id "$check_id" '
  .id == $id and .name == "Caravan admission gate" and .head_sha == $head
' "$work/check.json" >/dev/null
predicate=reporting_app
jq -e --argjson app "${READINESS_REPORTING_APP_ID:-15368}" '.app.id == $app' "$work/check.json" >/dev/null
predicate=reporting_status
jq -e '.status == "in_progress"' "$work/check.json" >/dev/null
predicate=membership_label
test -n "$MEMBERSHIP_LABEL"
label_present=false
if jq -e --arg label "$MEMBERSHIP_LABEL" 'any(.labels[]; .name == $label)' "$work/provider.json" >/dev/null; then
  label_present=true
fi
if [[ "$MEMBERSHIP_STATE" == deferred && "$DEFERRED_UNJOINED" == true && "$ADMISSION_DECISION" == deferred_unjoined ]]; then
  predicate=deferred_label_absent
  # Strict: never report an unjoined deferral while the label is present. The
  # observation predates the label (membership landed mid-run): unevaluated.
  if [[ "$label_present" == true ]]; then
    unevaluated membership_changed
  fi
  predicate=deferred_unjoined
  printf 'cara-deferred-admission pr=%s head=%s base=%s decision=deferred_unjoined exit=78\n' "$pr" "$head" "$base"
  exit 78
fi
predicate=member_state
test "$MEMBERSHIP_STATE" = member
predicate=member_label
# Strict: never report membership without the label in this fresh read. The
# label was removed after the observation (eviction mid-run): unevaluated.
if [[ "$label_present" != true ]]; then
  unevaluated membership_changed
fi
predicate=member_decision
test "$ADMISSION_DECISION" = run_member
predicate=member_run_ci
test "$RUN_CI" = true
predicate=member_not_deferred
test "$DEFERRED_UNJOINED" = false
predicate=member_success_report
echo 'Current ready-generation membership proven; existing source CI is reused without starting another source suite.'
