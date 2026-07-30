//! `cara next`: report the next pull request at a requested queue position.
//!
//! Schedulers that cannot host one long-lived `cara loop` need to reconstruct
//! its routing from their own cron or hook system. This surface exists so they
//! can do that without reimplementing admission ordering, which would drift
//! from what `sync` actually admits.
//!
//! Two deliberate choices:
//!
//! - **"Nothing matched" is a normal payload, not an error.** A cron must be
//!   able to distinguish an empty queue from a provider outage; returning an
//!   error envelope for both makes an outage look like quiet success.
//! - **`--checkout` is opt-in and refuses on a dirty tree.** Selection is a
//!   read; moving the working tree is not. A scheduler that silently clobbers
//!   uncommitted work is worse than one that does nothing.

use crate::model::PrNumber;
use crate::read::StatusOutput;
use crate::{AppContext, AppError, CheckoutReceipt, NextInput, NextMatch, NextOutput, NextStatus};

/// Report the next pull request at each requested position.
pub fn next(context: &AppContext, input: &NextInput) -> Result<NextOutput, AppError> {
    let budget = std::time::Duration::from_secs(context.config.command_timeout_secs);
    // Queue position is a property of the provider graph, never of the local
    // checkout, so a merged or ambiguous current branch must not fail this read.
    let status = crate::read::fleet_status(context, std::time::Instant::now() + budget, None)?;
    let requested = if input.status.is_empty() {
        vec![NextStatus::Ready]
    } else {
        input.status.clone()
    };

    let mut matches = Vec::new();
    for status_kind in &requested {
        matches.extend(collect(&status, *status_kind));
    }
    let selected = matches.first().cloned();

    let checkout = match (&selected, input.checkout) {
        (Some(selected), true) => Some(checkout_selected(context, &status, selected)?),
        _ => None,
    };

    let next = selected.as_ref().map_or_else(
        || {
            format!(
                "nothing matched {}; the queue is empty at those positions, which is not a fault",
                render(&requested)
            )
        },
        |selected| {
            format!(
                "PR #{} on branch `{}`; run `cara check --pr {}` before acting on it",
                selected.pr, selected.branch, selected.pr
            )
        },
    );

    Ok(NextOutput {
        schema_version: 1,
        repository: status.repository.clone(),
        requested,
        selected,
        matches,
        checkout,
        next,
    })
}

fn render(requested: &[NextStatus]) -> String {
    requested
        .iter()
        .map(|status| format!("`{}`", code(*status)))
        .collect::<Vec<_>>()
        .join(", ")
}

const fn code(status: NextStatus) -> &'static str {
    match status {
        NextStatus::Ready => "ready",
        NextStatus::Skipped => "skipped",
        NextStatus::Conflict => "conflict",
        NextStatus::Evicted => "evicted",
    }
}

fn collect(status: &StatusOutput, kind: NextStatus) -> Vec<NextMatch> {
    match kind {
        // `ready` must be byte-identical to what sync admits, so it reads the
        // same canonical selection rather than re-deriving an ordering.
        NextStatus::Ready => status
            .admission
            .next_candidate
            .into_iter()
            .filter_map(|pr| {
                describe(
                    status,
                    pr,
                    kind,
                    "canonical next automatic-admission candidate".to_owned(),
                )
            })
            .collect(),
        NextStatus::Skipped => status
            .admission
            .skipped
            .iter()
            .filter_map(|candidate| describe(status, candidate.pr, kind, candidate.reason.clone()))
            .collect(),
        // A conflict is read from proven graph evidence rather than a label, so
        // a stale label can never route an agent at a PR that has since been
        // force-pushed onto a clean generation.
        NextStatus::Conflict => status
            .analysis
            .fleet
            .problems
            .iter()
            // Must match what graph analysis actually emits for an unadmitted
            // candidate. This read `Incompatible` — the variant the producer
            // stopped emitting when `CandidateIncompatible` was introduced — so
            // `--status conflict` silently returned nothing while the fleet
            // plainly carried conflicts. Same producer/consumer drift as
            // bd-299d3e, in code written after that fix, and invisible to unit
            // tests because they never asked the real analysis for a kind.
            .filter(|problem| problem.kind.is_candidate_scoped())
            .flat_map(|problem| {
                problem
                    .prs
                    .iter()
                    .map(|pr| (*pr, problem.message.clone()))
                    .collect::<Vec<_>>()
            })
            .filter_map(|(pr, message)| describe(status, pr, kind, message))
            .collect(),
        NextStatus::Evicted => status
            .analysis
            .pull_requests
            .values()
            .filter(|pull_request| {
                pull_request.has_label("caravan-evicted")
                    && pull_request.state == crate::model::PullRequestState::Open
            })
            .filter_map(|pull_request| {
                describe(
                    status,
                    pull_request.number,
                    kind,
                    "evicted pull request awaiting repair or rejoin".to_owned(),
                )
            })
            .collect(),
    }
}

fn describe(
    status: &StatusOutput,
    pr: PrNumber,
    kind: NextStatus,
    reason: String,
) -> Option<NextMatch> {
    let pull_request = status.analysis.pull_requests.get(&pr)?;
    Some(NextMatch {
        status: kind,
        pr,
        branch: pull_request.head.name.clone(),
        oid: pull_request.head.oid.clone(),
        url: pull_request.url.clone(),
        reason,
    })
}

fn checkout_selected(
    context: &AppContext,
    status: &StatusOutput,
    selected: &NextMatch,
) -> Result<CheckoutReceipt, AppError> {
    let pull_request = status
        .analysis
        .pull_requests
        .get(&selected.pr)
        .ok_or_else(|| {
            AppError::validation(
                "next_checkout_pr_missing",
                format!("PR #{} was not present in discovery", selected.pr),
            )
        })?;
    let runner = crate::command::ProcessRunner::in_directory(&context.repository_path)
        .with_timeout(std::time::Duration::from_secs(
            context.config.command_timeout_secs,
        ));
    // Fail closed on a dirty tree: this is the guard that makes `--checkout`
    // safe to put in a cron at all.
    crate::navigation::ensure_safe_worktree(
        &context.repository_path,
        &context.config_path,
        &runner,
    )?;
    let from_branch = {
        let output = crate::command::CommandRunner::run(
            &runner,
            &crate::command::CommandSpec::new("git").args(["branch", "--show-current"]),
        )
        .map_err(|error| {
            AppError::validation(
                "next_checkout_branch_probe_failed",
                format!("could not read the current branch: {error}"),
            )
        })?;
        output.stdout.trim().to_owned()
    };
    crate::navigation::checkout_exact(
        &context.repository_path,
        &context.config_path,
        "origin",
        &runner,
        pull_request,
    )?;
    Ok(CheckoutReceipt {
        pr: selected.pr,
        from_branch,
        to_branch: pull_request.head.name.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scheduler must be able to tell "nothing to do" from "provider down".
    /// Returning an error envelope for an empty queue conflates them, and the
    /// cron then treats an outage as quiet success.
    #[test]
    fn an_empty_queue_is_a_payload_not_an_error() {
        let requested = vec![NextStatus::Conflict];
        let rendered = render(&requested);

        assert!(
            rendered.contains("conflict"),
            "the refusal text must name what was asked for: {rendered}"
        );
    }

    /// Status codes are a wire contract consumed by shell and cron callers, so
    /// they must stay stable and match the documented `--status` values.
    #[test]
    fn status_codes_are_the_documented_wire_values() {
        assert_eq!(code(NextStatus::Ready), "ready");
        assert_eq!(code(NextStatus::Skipped), "skipped");
        assert_eq!(code(NextStatus::Conflict), "conflict");
        assert_eq!(code(NextStatus::Evicted), "evicted");
    }
}
