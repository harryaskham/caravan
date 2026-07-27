//! Hermetic Git-fixture proofs for squash-equivalence reconciliation.
//!
//! Every fixture is a local temporary repository: no provider, no network, and
//! no live queue is involved. The positive cases assert that already-landed
//! stacked history is recognised with exact blob evidence; the negative cases
//! assert that ordinary three-way divergence, vacuous matches, partial
//! representation, and equal patches with different trees all fail closed and
//! drop nothing.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::Command;

use mcp_cli::StructuredError;
use tempfile::TempDir;

use super::*;
use crate::command::ProcessRunner;
use crate::model::RepositoryId;

struct TestRepo {
    directory: TempDir,
}

impl TestRepo {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("temp repository");
        git(
            directory.path(),
            ["init", "--quiet", "--initial-branch=main"],
        );
        git(directory.path(), ["config", "user.name", "Caravan Test"]);
        git(
            directory.path(),
            ["config", "user.email", "caravan@example.invalid"],
        );
        git(
            directory.path(),
            [
                "remote",
                "add",
                "fixture",
                directory.path().to_str().expect("utf-8 fixture path"),
            ],
        );
        Self { directory }
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn runner(&self) -> ProcessRunner {
        ProcessRunner::in_directory(self.path())
    }

    /// Write the exact file contents and commit them.
    ///
    /// Blobs are content addressed, so committing identical content on the
    /// default branch reproduces a squash landing exactly: the same blob under
    /// an unrelated commit identity.
    fn commit(&self, files: &[(&str, &str)], message: &str) -> CommitOid {
        for (path, contents) in files {
            let full_path = self.path().join(path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).expect("create parent");
            }
            fs::write(full_path, contents).expect("write fixture");
            git(self.path(), ["add", "--", path]);
        }
        git(self.path(), ["commit", "--quiet", "--message", message]);
        CommitOid(rev_parse(self.path(), "HEAD"))
    }

    fn switch_new(&self, branch: &str, start: &CommitOid) {
        git(
            self.path(),
            ["switch", "--quiet", "--create", branch, start.0.as_str()],
        );
    }

    fn switch(&self, branch: &str) {
        git(self.path(), ["switch", "--quiet", branch]);
    }

    fn branch(name: &str, oid: &CommitOid) -> BranchSnapshot {
        BranchSnapshot {
            repository: RepositoryId {
                owner: "harryaskham".to_owned(),
                name: "caravan".to_owned(),
            },
            name: name.to_owned(),
            oid: oid.clone(),
        }
    }
}

fn analyze_fixture(
    repository: &TestRepo,
    candidate: &CommitOid,
    target: &CommitOid,
) -> SquashEquivalenceReport {
    let report = analyze_with_runner(
        &repository.runner(),
        &TestRepo::branch("feature", candidate),
        &TestRepo::branch("main", target),
        candidate,
        target,
    )
    .expect("reconciliation analysis is evidence, not an execution error");
    assert_eq!(report.policy, SQUASH_EQUIVALENCE_POLICY);
    assert_eq!(report.schema_version, 1);
    report
}

fn tree_of(repository: &TestRepo, commit: &CommitOid) -> String {
    rev_parse(repository.path(), &format!("{}^{{tree}}", commit.0))
}

fn before(report: &SquashEquivalenceReport) -> &MergeEvidence {
    report.before.as_ref().expect("baseline merge evidence")
}

fn after(report: &SquashEquivalenceReport) -> &MergeEvidence {
    report.after.as_ref().expect("reconciled merge evidence")
}

/// Landed member content plus a later same-file edit is the canonical hazard:
/// the target already holds the pre-squash content byte-for-byte, so replaying
/// it conflicts while the retained edit alone applies cleanly.
#[test]
fn exact_blob_equality_authorizes_dropping_the_landed_prefix() {
    let repository = TestRepo::new();
    let base = repository.commit(&[("app.rs", "alpha\n")], "base");
    repository.switch_new("feature", &base);
    let landed = repository.commit(&[("app.rs", "beta\n")], "member one content");
    let child = repository.commit(
        &[("app.rs", "gamma\n"), ("child.rs", "child\n")],
        "member two content",
    );
    repository.switch("main");
    let squash = repository.commit(&[("app.rs", "beta\n")], "squash of member one");

    let report = analyze_fixture(&repository, &child, &squash);

    assert_eq!(report.outcome, SquashEquivalenceOutcome::Reconcilable);
    assert!(report.reconciliation_required());
    assert_eq!(before(&report).outcome, CompatibilityOutcome::Conflict);
    assert_eq!(before(&report).conflicting_paths, ["app.rs"]);
    assert_eq!(after(&report).outcome, CompatibilityOutcome::Clean);
    assert_eq!(report.dropped_commits(), std::slice::from_ref(&landed));
    assert_eq!(report.retained_commits(), std::slice::from_ref(&child));
    assert_eq!(report.authorized_range_base(), Some(&landed));
    assert_eq!(report.affected_paths(), ["app.rs"]);
    // The reconciled cumulative tree is exactly the candidate's reviewed tree:
    // the target contributed nothing the candidate had not already superseded.
    assert_eq!(after(&report).result_tree.0, tree_of(&repository, &child));
    assert_eq!(
        report.boundary_tree.expect("boundary tree").0,
        tree_of(&repository, &landed)
    );
    assert_eq!(
        report.target_tree.expect("target tree").0,
        tree_of(&repository, &squash)
    );
}

/// The proof binds only the paths the prefix actually changed, so unrelated
/// later movement on the target neither creates nor destroys equivalence.
#[test]
fn disjoint_later_target_movement_preserves_the_proof() {
    let repository = TestRepo::new();
    let base = repository.commit(&[("app.rs", "alpha\n")], "base");
    repository.switch_new("feature", &base);
    let landed = repository.commit(&[("app.rs", "beta\n")], "member one content");
    let child = repository.commit(&[("app.rs", "gamma\n")], "member two content");
    repository.switch("main");
    repository.commit(&[("app.rs", "beta\n")], "squash of member one");
    let moved = repository.commit(&[("docs/readme.md", "docs\n")], "unrelated later landing");

    let report = analyze_fixture(&repository, &child, &moved);

    assert_eq!(report.outcome, SquashEquivalenceOutcome::Reconcilable);
    assert_eq!(report.dropped_commits(), [landed]);
    assert_eq!(report.affected_paths(), ["app.rs"]);
    assert_eq!(after(&report).outcome, CompatibilityOutcome::Clean);
    // The disjoint target movement survives reconciliation instead of being
    // reverted by the retained replay.
    let listing = git_stdout(
        repository.path(),
        [
            "ls-tree",
            "-r",
            "--name-only",
            after(&report).result_tree.0.as_str(),
        ],
    );
    assert!(listing.contains("docs/readme.md"), "{listing}");
}

/// Only part of the prefix landed, so the cumulative content is not
/// represented and nothing may be dropped.
#[test]
fn partially_represented_prefix_fails_closed() {
    let repository = TestRepo::new();
    let base = repository.commit(&[("app.rs", "alpha\n"), ("util.rs", "util\n")], "base");
    repository.switch_new("feature", &base);
    repository.commit(
        &[("app.rs", "beta\n"), ("util.rs", "util-changed\n")],
        "member one content",
    );
    let child = repository.commit(&[("app.rs", "gamma\n")], "member two content");
    repository.switch("main");
    let squash = repository.commit(&[("app.rs", "beta\n")], "partial squash of member one");

    let report = analyze_fixture(&repository, &child, &squash);

    assert_eq!(report.outcome, SquashEquivalenceOutcome::NoEquivalence);
    assert!(report.dropped_commits().is_empty());
    assert!(report.authorized_range_base().is_none());
    assert!(report.represented_paths.is_empty());
    assert!(
        report
            .commits
            .iter()
            .all(|commit| commit.action == ReconciliationAction::Retained),
        "{:?}",
        report.commits
    );
    assert_eq!(report.retained_commits().len(), 2);
    assert!(
        report.reason.contains("byte-identical"),
        "{}",
        report.reason
    );
    let _ = child;
}

/// The same patch applied to different context yields a different blob. Patch
/// text is never proof; only the resulting object identity is.
#[test]
fn identical_patch_with_different_resulting_tree_is_not_equivalence() {
    let repository = TestRepo::new();
    let base = repository.commit(&[("app.rs", "alpha\n")], "base");
    repository.switch_new("feature", &base);
    repository.commit(&[("app.rs", "beta\n")], "member one content");
    let child = repository.commit(&[("app.rs", "gamma\n")], "member two content");
    repository.switch("main");
    // The identical alpha -> beta hunk lands on top of extra context, so the
    // patch matches while the cumulative blob does not.
    let squash = repository.commit(&[("app.rs", "beta\nextra\n")], "squash with extra context");

    let report = analyze_fixture(&repository, &child, &squash);

    assert_eq!(report.outcome, SquashEquivalenceOutcome::NoEquivalence);
    assert!(report.dropped_commits().is_empty());
    assert!(report.authorized_range_base().is_none());
}

/// Two landed members are both represented, so the longest proven boundary is
/// selected and only genuinely new history is replayed.
#[test]
fn multiple_squashes_reconcile_to_the_longest_proven_boundary() {
    let repository = TestRepo::new();
    let base = repository.commit(&[("a.rs", "a0\n"), ("b.rs", "b0\n")], "base");
    repository.switch_new("feature", &base);
    let first = repository.commit(&[("a.rs", "a1\n")], "member one content");
    let second = repository.commit(&[("b.rs", "b1\n")], "member two content");
    let child = repository.commit(&[("a.rs", "a3\n"), ("c.rs", "c\n")], "member three content");
    repository.switch("main");
    repository.commit(&[("a.rs", "a1\n")], "squash of member one");
    let squashes = repository.commit(&[("b.rs", "b1\n")], "squash of member two");

    let report = analyze_fixture(&repository, &child, &squashes);

    assert_eq!(report.outcome, SquashEquivalenceOutcome::Reconcilable);
    assert_eq!(report.dropped_commits(), [first, second.clone()]);
    assert_eq!(report.retained_commits(), std::slice::from_ref(&child));
    assert_eq!(report.authorized_range_base(), Some(&second));
    assert_eq!(report.affected_paths(), ["a.rs", "b.rs"]);
    assert_eq!(after(&report).outcome, CompatibilityOutcome::Clean);
    assert_eq!(after(&report).result_tree.0, tree_of(&repository, &child));
}

/// PR2227 shape: equality existed at a stacked ancestor, but the target moved
/// again on the same file, so merge base, target tip, and candidate head are
/// three distinct blobs. This is ordinary divergence and never reconcilable.
#[test]
fn divergence_after_the_equality_point_is_never_reconciled() {
    let repository = TestRepo::new();
    let base = repository.commit(&[("app.rs", "alpha\n")], "base");
    repository.switch_new("feature", &base);
    let ancestor = repository.commit(&[("app.rs", "beta\n")], "member one content");
    let child = repository.commit(&[("app.rs", "gamma\n")], "member two content");
    repository.switch("main");
    repository.commit(&[("app.rs", "beta\n")], "squash of member one");
    let moved = repository.commit(&[("app.rs", "delta\n")], "later independent landing");

    let report = analyze_fixture(&repository, &child, &moved);

    // Three distinct blobs at the merge base, target tip, and candidate head.
    let blob = |commit: &CommitOid| {
        git_stdout(
            repository.path(),
            ["rev-parse", &format!("{}:app.rs", commit.0)],
        )
        .trim()
        .to_owned()
    };
    assert_ne!(blob(&base), blob(&moved));
    assert_ne!(blob(&moved), blob(&child));
    assert_ne!(blob(&base), blob(&child));
    // Equality did exist at the stacked ancestor; that is explicitly not
    // sufficient once the target moved past it.
    assert_ne!(blob(&ancestor), blob(&moved));

    assert_eq!(report.outcome, SquashEquivalenceOutcome::NoEquivalence);
    assert_eq!(before(&report).outcome, CompatibilityOutcome::Conflict);
    assert!(report.dropped_commits().is_empty());
    assert!(report.authorized_range_base().is_none());
    assert!(report.after.is_none());
}

/// The prefix is genuinely represented, but the retained commits still diverge
/// from the target. Reconciliation must not claim the attach is now clean.
#[test]
fn residual_conflict_after_a_proven_boundary_fails_closed() {
    let repository = TestRepo::new();
    let base = repository.commit(&[("app.rs", "alpha\n"), ("shared.rs", "shared0\n")], "base");
    repository.switch_new("feature", &base);
    let landed = repository.commit(&[("app.rs", "beta\n")], "member one content");
    let child = repository.commit(&[("shared.rs", "shared-candidate\n")], "member two content");
    repository.switch("main");
    repository.commit(&[("app.rs", "beta\n")], "squash of member one");
    let diverged = repository.commit(&[("shared.rs", "shared-main\n")], "independent landing");

    let report = analyze_fixture(&repository, &child, &diverged);

    assert_eq!(report.outcome, SquashEquivalenceOutcome::ResidualConflict);
    assert_eq!(report.proven_boundary, Some(landed));
    assert_eq!(after(&report).outcome, CompatibilityOutcome::Conflict);
    assert_eq!(after(&report).conflicting_paths, ["shared.rs"]);
    assert!(report.dropped_commits().is_empty());
    assert!(report.authorized_range_base().is_none());
    assert!(
        report.reason.contains("three-way divergence"),
        "{}",
        report.reason
    );
    let _ = child;
}

/// A file none of the stacked commits touched is identical everywhere. That
/// vacuous equality is not evidence and cannot carry an unrepresented prefix.
#[test]
fn vacuous_equality_on_untouched_paths_proves_nothing() {
    let repository = TestRepo::new();
    let base = repository.commit(
        &[("app.rs", "alpha\n"), ("queue.rs", "queue-unchanged\n")],
        "base",
    );
    repository.switch_new("feature", &base);
    repository.commit(&[("app.rs", "beta\n")], "member one content");
    let child = repository.commit(&[("app.rs", "gamma\n")], "member two content");
    repository.switch("main");
    // The target never landed the member's content; only the untouched file is
    // identical on both sides.
    let unrelated = repository.commit(&[("app.rs", "main-only\n")], "unrelated landing");

    let report = analyze_fixture(&repository, &child, &unrelated);

    assert_eq!(report.outcome, SquashEquivalenceOutcome::NoEquivalence);
    assert!(report.represented_paths.is_empty());
    assert!(report.dropped_commits().is_empty());
    assert_eq!(
        git_stdout(
            repository.path(),
            ["rev-parse", &format!("{}:queue.rs", child.0)]
        )
        .trim(),
        git_stdout(
            repository.path(),
            ["rev-parse", &format!("{}:queue.rs", unrelated.0)]
        )
        .trim(),
        "the untouched path really is identical on both sides"
    );
}

/// A four-member stacked tail whose first member landed as a squash: exactly
/// that member's commits are reconciled and every later member is retained.
#[test]
fn stacked_tail_topology_drops_only_the_landed_member() {
    let repository = TestRepo::new();
    let base = repository.commit(
        &[
            ("cli.rs", "cli0\n"),
            ("queue.rs", "queue0\n"),
            ("web.rs", "web0\n"),
        ],
        "base",
    );
    repository.switch_new("feature", &base);
    let member_one_first = repository.commit(&[("cli.rs", "cli-landed-a\n")], "member one part a");
    let member_one_head = repository.commit(&[("cli.rs", "cli-landed\n")], "member one part b");
    let member_two = repository.commit(&[("queue.rs", "queue-two\n")], "member two");
    let member_three = repository.commit(&[("web.rs", "web-three\n")], "member three");
    let tail = repository.commit(&[("cli.rs", "cli-tail\n")], "member four tail");
    repository.switch("main");
    let squash = repository.commit(&[("cli.rs", "cli-landed\n")], "squash of member one");

    let report = analyze_fixture(&repository, &tail, &squash);

    assert_eq!(report.outcome, SquashEquivalenceOutcome::Reconcilable);
    assert_eq!(report.candidate_commit_count, 5);
    assert!(report.analyzed_prefix_complete);
    assert_eq!(
        report.dropped_commits(),
        [member_one_first, member_one_head.clone()]
    );
    assert_eq!(
        report.retained_commits(),
        [member_two, member_three, tail.clone()]
    );
    assert_eq!(report.authorized_range_base(), Some(&member_one_head));
    assert_eq!(report.affected_paths(), ["cli.rs"]);
    assert_eq!(after(&report).outcome, CompatibilityOutcome::Clean);
    assert_eq!(after(&report).result_tree.0, tree_of(&repository, &tail));
}

/// The same stacked tail, but the target moved past the equality point. The
/// structural shape is identical and the decision must still fail closed.
#[test]
fn stacked_tail_topology_fails_closed_once_the_target_moves_past_equality() {
    let repository = TestRepo::new();
    let base = repository.commit(&[("cli.rs", "cli0\n"), ("queue.rs", "queue0\n")], "base");
    repository.switch_new("feature", &base);
    repository.commit(&[("cli.rs", "cli-landed\n")], "member one");
    repository.commit(&[("queue.rs", "queue-two\n")], "member two");
    let tail = repository.commit(
        &[("cli.rs", "cli-tail\n"), ("queue.rs", "queue-tail\n")],
        "member three tail",
    );
    repository.switch("main");
    repository.commit(&[("cli.rs", "cli-landed\n")], "squash of member one");
    let moved = repository.commit(
        &[("cli.rs", "cli-main-later\n"), ("queue.rs", "queue-main\n")],
        "later main landings",
    );

    let report = analyze_fixture(&repository, &tail, &moved);

    assert_eq!(report.outcome, SquashEquivalenceOutcome::NoEquivalence);
    assert_eq!(before(&report).outcome, CompatibilityOutcome::Conflict);
    assert!(report.dropped_commits().is_empty());
    assert!(report.authorized_range_base().is_none());
}

/// A clean attach needs no reconciliation, so no rewrite is ever justified by
/// the presence of already-represented history alone.
#[test]
fn clean_attachment_reports_that_no_reconciliation_is_required() {
    let repository = TestRepo::new();
    let base = repository.commit(&[("app.rs", "alpha\n")], "base");
    repository.switch_new("feature", &base);
    repository.commit(&[("app.rs", "beta\n")], "member one content");
    let child = repository.commit(&[("child.rs", "child\n")], "member two content");
    repository.switch("main");
    let squash = repository.commit(&[("app.rs", "beta\n")], "squash of member one");

    let report = analyze_fixture(&repository, &child, &squash);

    assert_eq!(before(&report).outcome, CompatibilityOutcome::Clean);
    assert!(!report.reconciliation_required());
}

/// Non-linear candidate history is never partially replayed or reordered.
#[test]
fn nonlinear_candidate_history_is_not_reconciled() {
    let repository = TestRepo::new();
    let base = repository.commit(&[("app.rs", "alpha\n")], "base");
    repository.switch_new("side", &base);
    repository.commit(&[("side.rs", "side\n")], "side work");
    repository.switch_new("feature", &base);
    repository.commit(&[("app.rs", "beta\n")], "member one content");
    git(
        repository.path(),
        [
            "merge",
            "--quiet",
            "--no-ff",
            "--no-edit",
            "-m",
            "merge side",
            "side",
        ],
    );
    let child = repository.commit(&[("app.rs", "gamma\n")], "member content");
    repository.switch("main");
    // The stacked prefix content really did land, so only the non-linear range
    // shape can be what fails the decision closed.
    let squash = repository.commit(&[("app.rs", "beta\n")], "squash of member one");

    let report = analyze_fixture(&repository, &child, &squash);

    assert_eq!(report.outcome, SquashEquivalenceOutcome::Indeterminate);
    assert!(!report.analyzed_prefix_complete);
    assert!(report.dropped_commits().is_empty());
    assert!(report.authorized_range_base().is_none());
    assert!(report.reason.contains("linear"), "{}", report.reason);
}

/// Unrelated histories have no merge base, so no cumulative proof exists.
#[test]
fn absent_merge_base_is_indeterminate() {
    let repository = TestRepo::new();
    let main = repository.commit(&[("app.rs", "alpha\n")], "base");
    git(
        repository.path(),
        ["switch", "--quiet", "--orphan", "unrelated"],
    );
    let orphan = repository.commit(&[("other.rs", "other\n")], "unrelated root");

    let report = analyze_fixture(&repository, &orphan, &main);

    assert_eq!(report.outcome, SquashEquivalenceOutcome::Indeterminate);
    assert!(report.merge_base.is_none());
    assert!(report.before.is_none());
    assert!(report.authorized_range_base().is_none());
    let error = unauthorized(&report);
    assert_eq!(error.code(), "squash_equivalence_unproven");
}

/// The receipt carries exactly the evidence an operator needs to audit the
/// decision without rerunning Git.
#[test]
fn receipt_records_commits_paths_actions_and_cumulative_trees() {
    let repository = TestRepo::new();
    let base = repository.commit(&[("app.rs", "alpha\n")], "base");
    repository.switch_new("feature", &base);
    let landed = repository.commit(
        &[("app.rs", "beta\n"), ("landed.rs", "landed\n")],
        "member one content",
    );
    let child = repository.commit(&[("app.rs", "gamma\n")], "member two content");
    repository.switch("main");
    let squash = repository.commit(
        &[("app.rs", "beta\n"), ("landed.rs", "landed\n")],
        "squash of member one",
    );

    let report = analyze_fixture(&repository, &child, &squash);

    assert_eq!(report.outcome, SquashEquivalenceOutcome::Reconcilable);
    let dropped = report
        .commits
        .iter()
        .find(|commit| commit.action == ReconciliationAction::Dropped)
        .expect("dropped commit row");
    assert_eq!(dropped.oid, landed);
    assert_eq!(dropped.paths, ["app.rs", "landed.rs"]);
    assert_eq!(dropped.tree_oid.0, tree_of(&repository, &landed));
    assert_eq!(
        report
            .represented_paths
            .iter()
            .map(|path| (path.path.as_str(), path.deleted))
            .collect::<Vec<_>>(),
        [("app.rs", false), ("landed.rs", false)]
    );
    for path in &report.represented_paths {
        assert_eq!(path.mode, "100644");
        assert_eq!(
            path.blob,
            git_stdout(
                repository.path(),
                ["rev-parse", &format!("{}:{}", squash.0, path.path)]
            )
            .trim()
        );
    }
    assert!(!report.represented_paths_truncated);

    let details = report.details();
    assert_eq!(details["outcome"], "reconcilable");
    assert_eq!(details["proven_boundary"], landed.0);
    assert_eq!(details["dropped_commits"][0], landed.0);
    assert_eq!(details["retained_commits"][0], child.0);
    assert_eq!(details["affected_paths"][0], "app.rs");
    assert_eq!(
        details["cumulative_tree_before"],
        before(&report).result_tree.0
    );
    assert_eq!(
        details["cumulative_tree_after"],
        after(&report).result_tree.0
    );
    assert_eq!(details["policy"], SQUASH_EQUIVALENCE_POLICY);
    assert_eq!(ReconciliationAction::Dropped.name(), "dropped");
    assert_eq!(
        SquashEquivalenceOutcome::Reconcilable.name(),
        "reconcilable"
    );
}

/// A prefix whose cumulative effect is a deletion is represented only when the
/// target performed exactly that deletion.
#[test]
fn deleted_paths_require_the_target_to_have_deleted_them_too() {
    let repository = TestRepo::new();
    let base = repository.commit(&[("gone.rs", "gone\n"), ("app.rs", "alpha\n")], "base");
    repository.switch_new("feature", &base);
    git(repository.path(), ["rm", "--quiet", "--", "gone.rs"]);
    git(
        repository.path(),
        ["commit", "--quiet", "--message", "member one removes file"],
    );
    let landed = CommitOid(rev_parse(repository.path(), "HEAD"));
    let child = repository.commit(&[("app.rs", "gamma\n")], "member two content");

    repository.switch("main");
    let kept = repository.commit(&[("unrelated.rs", "unrelated\n")], "main keeps the file");
    let unproven = analyze_fixture(&repository, &child, &kept);
    assert_eq!(unproven.outcome, SquashEquivalenceOutcome::NoEquivalence);

    git(repository.path(), ["rm", "--quiet", "--", "gone.rs"]);
    git(
        repository.path(),
        ["commit", "--quiet", "--message", "squash removes file"],
    );
    let removed = CommitOid(rev_parse(repository.path(), "HEAD"));
    let report = analyze_fixture(&repository, &child, &removed);

    assert_eq!(report.outcome, SquashEquivalenceOutcome::Reconcilable);
    assert_eq!(report.dropped_commits(), [landed]);
    assert_eq!(
        report
            .represented_paths
            .iter()
            .map(|path| (path.path.as_str(), path.deleted))
            .collect::<Vec<_>>(),
        [("gone.rs", true)]
    );
}

/// The public entry point resolves exact advertised revisions before analysis.
#[test]
fn public_analysis_resolves_exact_remote_revisions() {
    let repository = TestRepo::new();
    let base = repository.commit(&[("app.rs", "alpha\n")], "base");
    repository.switch_new("feature", &base);
    let landed = repository.commit(&[("app.rs", "beta\n")], "member one content");
    let child = repository.commit(&[("app.rs", "gamma\n")], "member two content");
    repository.switch("main");
    let squash = repository.commit(&[("app.rs", "beta\n")], "squash of member one");
    let before_head = rev_parse(repository.path(), "HEAD");

    let report = analyze(
        repository.path(),
        "fixture",
        &TestRepo::branch("feature", &child),
        &TestRepo::branch("main", &squash),
    )
    .expect("public analysis");

    assert_eq!(report.outcome, SquashEquivalenceOutcome::Reconcilable);
    assert_eq!(report.dropped_commits(), [landed]);
    assert_eq!(rev_parse(repository.path(), "HEAD"), before_head);
}

/// Analysis never leaves the repository, index, or worktree modified.
#[test]
fn analysis_is_non_mutating() {
    let repository = TestRepo::new();
    let base = repository.commit(&[("app.rs", "alpha\n")], "base");
    repository.switch_new("feature", &base);
    repository.commit(&[("app.rs", "beta\n")], "member one content");
    let child = repository.commit(&[("app.rs", "gamma\n")], "member two content");
    repository.switch("main");
    let squash = repository.commit(&[("app.rs", "beta\n")], "squash of member one");
    let refs_before = git_stdout(repository.path(), ["show-ref"]);
    let status_before = git_stdout(repository.path(), ["status", "--porcelain=v1"]);
    let head_before = rev_parse(repository.path(), "HEAD");

    let report = analyze_fixture(&repository, &child, &squash);
    assert_eq!(report.outcome, SquashEquivalenceOutcome::Reconcilable);

    assert_eq!(git_stdout(repository.path(), ["show-ref"]), refs_before);
    assert_eq!(
        git_stdout(repository.path(), ["status", "--porcelain=v1"]),
        status_before
    );
    assert_eq!(rev_parse(repository.path(), "HEAD"), head_before);
}

/// Analysis is bounded: a very deep stacked range reports that its proof
/// search was truncated rather than silently claiming no equivalence.
#[test]
fn boundary_evaluation_is_bounded_and_reports_the_bound() {
    let repository = TestRepo::new();
    let base = repository.commit(&[("app.rs", "alpha\n")], "base");
    repository.switch_new("feature", &base);
    let mut last = base.clone();
    for index in 0..(MAX_EVALUATED_BOUNDARIES + 2) {
        last = repository.commit(&[("app.rs", &format!("step-{index}\n"))], "stacked commit");
    }
    repository.switch("main");
    // Only the very first stacked commit landed, which sits below the bound in
    // ancestry but far outside the evaluated window from the top of the range.
    let squash = repository.commit(&[("app.rs", "unrelated\n")], "unrelated landing");

    let report = analyze_fixture(&repository, &last, &squash);

    assert_eq!(report.candidate_commit_count, MAX_EVALUATED_BOUNDARIES + 2);
    assert_eq!(report.evaluated_boundaries, MAX_EVALUATED_BOUNDARIES);
    assert!(report.evaluation_bounded);
    assert_eq!(report.outcome, SquashEquivalenceOutcome::NoEquivalence);
    assert!(report.dropped_commits().is_empty());
    assert!(
        report
            .reason
            .contains(&format!("first {MAX_EVALUATED_BOUNDARIES} of")),
        "{}",
        report.reason
    );
}

fn git<I, S>(repository: &Path, arguments: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .current_dir(repository)
        .args(arguments)
        .output()
        .expect("run git fixture command");
    assert!(
        output.status.success(),
        "git fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout<I, S>(repository: &Path, arguments: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .current_dir(repository)
        .args(arguments)
        .output()
        .expect("run git fixture command");
    if output.status.success() {
        String::from_utf8_lossy(&output.stdout).into_owned()
    } else if output.status.code() == Some(1) && output.stdout.is_empty() {
        String::new()
    } else {
        panic!(
            "git fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn rev_parse(repository: &Path, revision: &str) -> String {
    git_stdout(repository, ["rev-parse", "--verify", revision])
        .trim()
        .to_owned()
}
