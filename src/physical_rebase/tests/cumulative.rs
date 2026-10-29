use super::*;

fn budget() -> RebaseExecutionBudget {
    RebaseExecutionBudget::new(TEST_REBASE_BUDGET).with_cumulative_tree_replay(true)
}

fn commit_files(f: &Fixture, files: &[(&str, &str)], message: &str) -> CommitOid {
    for (name, contents) in files {
        std::fs::write(f.clone.join(name), contents).unwrap();
        git(&f.clone, &["add", name]);
    }
    git(&f.clone, &["commit", "-m", message]);
    CommitOid(git(&f.clone, &["rev-parse", "HEAD"]))
}

struct StackFixture {
    git: Fixture,
    parent: PullRequestSnapshot,
    child: PullRequestSnapshot,
    main: BranchSnapshot,
}

fn merge_synced_stack() -> StackFixture {
    let f = fixture();
    git(&f.clone, &["checkout", "main"]);
    let base = commit_files(
        &f,
        &[
            ("CHANGELOG.md", "release 0\n"),
            ("Cargo.toml", "version=0\n"),
            ("Cargo.lock", "version=0\n"),
            ("flake.lock", "flake 0\n"),
        ],
        "old release",
    );
    git(&f.clone, &["checkout", "-b", "predecessor"]);
    let parent_head = commit_files(&f, &[("flake.lock", "flake 1\n")], "update flake");
    git(&f.clone, &["push", "origin", "predecessor"]);
    git(&f.clone, &["checkout", "-b", "member", &base.0]);
    commit_files(&f, &[("source", "member change\n")], "own source change");
    git(&f.clone, &["checkout", "main"]);
    commit_files(
        &f,
        &[
            ("CHANGELOG.md", "release 1\n"),
            ("Cargo.toml", "version=1\n"),
            ("Cargo.lock", "version=1\n"),
        ],
        "intermediate release",
    );
    let main = commit_files(
        &f,
        &[
            ("CHANGELOG.md", "release 2\n"),
            ("Cargo.toml", "version=2\n"),
            ("Cargo.lock", "version=2\n"),
        ],
        "current release",
    );
    git(&f.clone, &["push", "origin", "main"]);
    git(&f.clone, &["checkout", "member"]);
    git(&f.clone, &["merge", "--no-ff", "main", "-m", "sync main"]);
    // Authored semantic resolution, not reconstructible just by recreating the
    // merge. Keep it together with a binary blob and executable mode.
    std::fs::write(f.clone.join("source"), "authored merge resolution\n").unwrap();
    std::fs::write(f.clone.join("binary"), [0, 255, 1, 0, 2]).unwrap();
    git(&f.clone, &["add", "source", "binary"]);
    git(&f.clone, &["update-index", "--chmod=+x", "source"]);
    git(&f.clone, &["commit", "--amend", "--no-edit"]);
    git(&f.clone, &["reset", "--hard", "HEAD"]);
    let child_head = CommitOid(git(&f.clone, &["rev-parse", "HEAD"]));
    git(&f.clone, &["push", "origin", "member"]);
    let parent = open_candidate(
        1,
        "flake",
        branch(&f.repository, "predecessor", &parent_head),
        branch(&f.repository, "main", &base),
    );
    // Provider base is the old predecessor even though the source owner merged
    // current main, not that predecessor, into the member.
    let child = open_candidate(
        2,
        "source",
        branch(&f.repository, "member", &child_head),
        parent.head.clone(),
    );
    let main = branch(&f.repository, "main", &main);
    StackFixture {
        git: f,
        parent,
        child,
        main,
    }
}

fn prepare_parent(f: &StackFixture) -> PreparedRebase {
    prepare_candidate(
        &f.git.clone,
        &f.git.repository,
        &f.parent,
        range_base_for_remote_target(&f.parent, &f.main),
        PlannedBase::Remote(f.main.clone()),
        &f.main,
        budget(),
    )
    .unwrap()
}

fn prepare_child(
    f: &StackFixture,
    parent: &PreparedRebase,
    budget: RebaseExecutionBudget,
) -> Result<PreparedRebase, AppError> {
    prepare_candidate(
        &f.git.clone,
        &f.git.repository,
        &f.child,
        range_base_for_rewritten_parent(&f.child, &f.parent.head),
        PlannedBase::Simulated(branch(
            &f.git.repository,
            "predecessor",
            &parent.plan.new_head_oid,
        )),
        &f.main,
        budget,
    )
}

#[test]
fn merge_synced_member_uses_cumulative_tree_over_restacked_release_base() {
    let f = merge_synced_stack();
    let parent = prepare_parent(&f);
    // Reproduce the actual bypass: the ordinary sequencer proves a clean tree
    // then replays main's intermediate release against its newer version.
    let error = prepare_child(&f, &parent, RebaseExecutionBudget::new(TEST_REBASE_BUDGET))
        .err()
        .expect("legacy merge replay conflicts");
    assert_eq!(error.code(), "rebase_merge_replay_conflict");
    assert_eq!(
        error.details().unwrap()["conflicting_paths"],
        json!(["CHANGELOG.md", "Cargo.lock", "Cargo.toml"])
    );
    let child = prepare_child(&f, &parent, budget()).expect("net source change is clean");
    assert!(!child.plan.already_satisfied);
    let proof = child.plan.cumulative_tree.as_ref().unwrap();
    assert_eq!(proof.merge_base_oid, f.main.oid);
    assert_eq!(proof.result_tree_oid, child.plan.new_tree_oid);
    assert_eq!(child.plan.range_source.branch(), &f.child.base);
    assert_eq!(child.plan.new_base.branch().oid, parent.plan.new_head_oid);
    assert_eq!(
        git(
            &f.git.clone,
            &["diff", "--name-only", &f.main.oid.0, &f.child.head.oid.0]
        ),
        "binary\nsource"
    );
    assert_eq!(
        git(
            &f.git.clone,
            &[
                "diff",
                "--name-only",
                &parent.plan.new_head_oid.0,
                &child.plan.new_head_oid.0
            ]
        ),
        "binary\nsource"
    );
    for name in ["CHANGELOG.md", "Cargo.lock", "Cargo.toml"] {
        assert_eq!(
            git(
                &f.git.clone,
                &["rev-parse", &format!("{}:{name}", child.plan.new_head_oid)]
            ),
            git(
                &f.git.clone,
                &["rev-parse", &format!("{}:{name}", f.main.oid)]
            )
        );
    }
    assert_eq!(
        git(
            &f.git.clone,
            &["ls-tree", &child.plan.new_head_oid.0, "source"]
        ),
        git(&f.git.clone, &["ls-tree", &f.child.head.oid.0, "source"])
    );
    let receipt = apply_prepared_atomically(&[&parent, &child]).unwrap();
    assert!(receipt.atomic);
    assert_eq!(receipt.receipts[1].cumulative_tree.as_ref(), Some(proof));
    assert_eq!(receipt.receipts[1].old_head_oid, f.child.head.oid);
    assert_eq!(
        receipt.receipts[1].lease,
        format!("--force-with-lease=refs/heads/member:{}", f.child.head.oid)
    );
    for old in [&f.child.head.oid, &parent.plan.new_head_oid] {
        git(
            &f.git.clone,
            &[
                "merge-base",
                "--is-ancestor",
                &old.0,
                &child.plan.new_head_oid.0,
            ],
        );
    }
    assert_eq!(remote_branch(&f.git, "member"), child.plan.new_head_oid.0);
    let encoded = serde_json::to_value(&receipt).unwrap();
    let decoded: AtomicRebaseReceipt = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded, receipt);
}

#[test]
fn member_already_containing_new_predecessor_keeps_exact_head_and_ci_generation() {
    let f = merge_synced_stack();
    let parent = prepare_parent(&f);
    let child = prepare_child(&f, &parent, budget()).unwrap();
    apply_prepared_atomically(&[&parent, &child]).unwrap();
    let target = branch(&f.git.repository, "predecessor", &parent.plan.new_head_oid);
    let mut current = f.child.clone();
    current.head.oid = child.plan.new_head_oid.clone();
    // Keep the historical provider base: it must not defeat the ancestry no-op.
    let prepared = prepare_candidate(
        &f.git.clone,
        &f.git.repository,
        &current,
        range_base_for_remote_target(&current, &target),
        PlannedBase::Remote(target),
        &f.main,
        budget(),
    )
    .unwrap();
    assert!(prepared.plan.already_satisfied);
    assert_eq!(prepared.plan.new_head_oid, current.head.oid);
    assert_eq!(prepared.plan.new_tree_oid, child.plan.new_tree_oid);
    // A no-op must not even attempt a receive-pack update.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let hook = f.git.bare.join("hooks/pre-receive");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let receipt = apply_prepared_atomically(&[&prepared]).unwrap();
    assert!(receipt.receipts[0].already_satisfied);
    assert!(
        receipt.receipts[0]
            .rewrite_reason
            .comment(&receipt.receipts[0])
            .is_none()
    );
    assert_eq!(receipt.old_heads, receipt.new_heads);
}

#[test]
fn cumulative_tree_conflict_reports_only_real_net_changed_path_without_publication() {
    let mut f = merge_synced_stack();
    git(&f.git.clone, &["checkout", "predecessor"]);
    f.parent.head.oid = commit_files(&f.git, &[("source", "incompatible parent\n")], "conflict");
    git(&f.git.clone, &["push", "origin", "predecessor"]);
    f.child.base = f.parent.head.clone();
    let parent = prepare_parent(&f);
    let error = prepare_child(&f, &parent, budget())
        .err()
        .expect("real conflict");
    assert_eq!(error.code(), "rebase_merge_tree_conflict");
    let details = error.details().unwrap();
    assert_eq!(details["conflicting_paths"], json!(["source"]));
    assert_eq!(
        details["failure_generation"]["original_head"]["oid"],
        f.child.head.oid.0
    );
    assert_eq!(
        details["failure_generation"]["provider_mutation_outcome"],
        "not_attempted"
    );
    assert_eq!(remote_branch(&f.git, "predecessor"), f.parent.head.oid.0);
    assert_eq!(remote_branch(&f.git, "member"), f.child.head.oid.0);
}

#[test]
fn cumulative_atomic_source_race_preserves_every_remote_head() {
    let f = merge_synced_stack();
    let parent = prepare_parent(&f);
    let child = prepare_child(&f, &parent, budget()).unwrap();
    let moved = commit_files(&f.git, &[("owner", "concurrent edit\n")], "owner edit");
    git(&f.git.clone, &["push", "origin", "member"]);
    assert!(apply_prepared_atomically(&[&parent, &child]).is_err());
    assert_eq!(remote_branch(&f.git, "member"), moved.0);
    assert_eq!(remote_branch(&f.git, "predecessor"), f.parent.head.oid.0);
}

#[test]
fn cumulative_target_race_refuses_before_atomic_publication() {
    let f = merge_synced_stack();
    let parent = prepare_parent(&f);
    let child = prepare_child(&f, &parent, budget()).unwrap();
    git(&f.git.clone, &["checkout", "main"]);
    let moved = commit_files(&f.git, &[("new-main", "advanced\n")], "advance main");
    git(&f.git.clone, &["push", "origin", "main"]);
    assert!(apply_prepared_atomically(&[&parent, &child]).is_err());
    assert_eq!(remote_branch(&f.git, "main"), moved.0);
    assert_eq!(remote_branch(&f.git, "member"), f.child.head.oid.0);
    assert_eq!(remote_branch(&f.git, "predecessor"), f.parent.head.oid.0);
}

#[test]
fn cumulative_ambiguous_three_way_base_refuses_without_publication() {
    let f = fixture();
    let tree = git(&f.clone, &["rev-parse", "HEAD^{tree}"]);
    let left = git(
        &f.clone,
        &[
            "commit-tree",
            &tree,
            "-p",
            &f.feature.0,
            "-p",
            &f.new_main.0,
            "-m",
            "left merge",
        ],
    );
    let right = git(
        &f.clone,
        &[
            "commit-tree",
            &tree,
            "-p",
            &f.new_main.0,
            "-p",
            &f.feature.0,
            "-m",
            "right merge",
        ],
    );
    git(
        &f.clone,
        &[
            "push",
            "origin",
            &format!("{left}:refs/heads/feature"),
            &format!("{right}:refs/heads/predecessor"),
        ],
    );
    let main = branch(&f.repository, "main", &f.new_main);
    let candidate = open_candidate(
        1,
        "ambiguous",
        branch(&f.repository, "feature", &CommitOid(left.clone())),
        branch(&f.repository, "main", &f.old_main),
    );
    let error = prepare_candidate(
        &f.clone,
        &f.repository,
        &candidate,
        PlannedRangeBase::HistoricalSourceBranch {
            branch: candidate.base.clone(),
            current: main.clone(),
        },
        PlannedBase::Remote(branch(
            &f.repository,
            "predecessor",
            &CommitOid(right.clone()),
        )),
        &main,
        budget(),
    )
    .err()
    .expect("ambiguous cumulative merge must refuse");
    assert_eq!(error.code(), "rebase_range_ambiguous");
    assert_eq!(remote_branch(&f, "feature"), left);
    assert_eq!(remote_branch(&f, "predecessor"), right);
}

#[test]
fn identical_head_and_predecessor_is_a_noop_even_with_empty_source_range() {
    let f = fixture();
    let candidate = open_candidate(
        1,
        "already present",
        branch(&f.repository, "feature", &f.feature),
        branch(&f.repository, "main", &f.old_main),
    );
    let target = branch(&f.repository, "retained", &f.feature);
    let prepared = prepare_candidate(
        &f.clone,
        &f.repository,
        &candidate,
        PlannedRangeBase::HistoricalSourceBranch {
            branch: candidate.base.clone(),
            current: branch(&f.repository, "main", &f.new_main),
        },
        PlannedBase::Simulated(target),
        &branch(&f.repository, "main", &f.new_main),
        budget(),
    )
    .unwrap();
    assert!(prepared.plan.already_satisfied);
    assert_eq!(prepared.plan.commit_count, 0);
    assert_eq!(prepared.plan.new_head_oid, f.feature);
}
