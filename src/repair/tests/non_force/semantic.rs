use super::*;

mod continuation;
mod fences;
mod rejections;

fn grant_semantic(f: &Fixture, repair: &RepairSession) -> RepairSession {
    let source = semantic_source(f);
    grant_paths(
        &context(&f.clone),
        &RepairGrantInput {
            session: repair.session.clone(),
            paths: vec!["README.md".to_owned(), "SPEC.md".to_owned()],
            source_revision: source.0,
            actor: "source-owner".to_owned(),
            reason: "reviewed semantic correction".to_owned(),
            expires_secs: 3600,
        },
    )
    .unwrap();
    read_manifest(&repair_paths(&f.clone, f.candidate.number).unwrap().manifest).unwrap()
}

fn publish(f: &Fixture, repair: &RepairSession) -> Result<RepairContinueOutput, AppError> {
    continue_with_verifier(&context(&f.clone), &continue_input(repair), |session, _| {
        session
            .non_force
            .as_ref()
            .unwrap()
            .source
            .verify(&f.candidate)
    })
}

fn commit_locally(repair: &RepairSession) {
    git(
        Path::new(&repair.workspace),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "prepared semantic correction",
        ],
    );
}

fn contained_source_fixture() -> Fixture {
    let mut f = source_fixture();
    let merge = std::process::Command::new("git")
        .current_dir(&f.clone)
        .args(["merge", "--no-ff", "--no-commit", &f.target.oid.0])
        .output()
        .unwrap();
    assert_eq!(
        merge.status.code(),
        Some(1),
        "fixture conflict is intentional"
    );
    fs::write(f.clone.join("shared.txt"), "authored source plus target\n").unwrap();
    git(&f.clone, &["add", "shared.txt"]);
    git(
        &f.clone,
        &["commit", "-m", "source already incorporates current main"],
    );
    git(&f.clone, &["push", "origin", "feature"]);
    f.candidate.head.oid = CommitOid(git(&f.clone, &["rev-parse", "HEAD"]));
    f.candidate.base.oid = f.target.oid.clone();
    f
}

fn semantic_input(f: &Fixture) -> RepairStartInput {
    // Exercise the wire intent without deriving expected behavior from the
    // implementation's purpose selector. Pre-feature readers ignore this field
    // and reach the existing already-contains-target refusal.
    let mut wire = serde_json::to_value(input(f)).unwrap();
    wire["semantic_only"] = json!(true);
    serde_json::from_value(wire).unwrap()
}

fn prepare_semantic(f: &Fixture) -> Result<RepairStartOutput, AppError> {
    start_exact_with_writer_guard(
        &context(&f.clone),
        &f.repository,
        &f.candidate,
        &f.target,
        RepairStartIntent {
            target_pr: None,
            non_force: NonForceRepair::new(
                &semantic_input(f),
                &f.repository,
                &f.candidate,
                None,
                "main",
            )?,
        },
        f.root.path().join("remote.git").to_str().unwrap(),
        None,
    )
}

#[test]
fn non_force_semantic_prepares_contained_target_without_a_fake_merge() {
    let f = contained_source_fixture();
    let caller_head = git(&f.clone, &["rev-parse", "HEAD"]);
    let caller_dirt = git(&f.clone, &["status", "--porcelain"]);
    let original = git(&f.clone, &["cat-file", "-p", &f.candidate.head.oid.0]);
    let result = prepare_semantic(&f);
    assert!(
        result.is_ok(),
        "explicit semantic correction needs a grantable session: {result:?}"
    );
    let repair = result.unwrap().repair;
    assert_eq!(repair.state, RepairState::Resolving);
    assert!(repair.conflicting_paths.is_empty());
    assert!(
        try_rev_parse(
            &ProcessRunner::in_directory(&repair.workspace),
            "MERGE_HEAD"
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(
        git(Path::new(&repair.workspace), &["rev-parse", "HEAD"]),
        caller_head
    );
    assert_eq!(
        serde_json::to_value(&repair).unwrap()["non_force"]["semantic_only"],
        json!(true)
    );
    assert_eq!(
        remote(&f),
        f.candidate.head.oid.0,
        "preparation cannot publish"
    );
    assert_eq!(git(&f.clone, &["rev-parse", "HEAD"]), caller_head);
    assert_eq!(git(&f.clone, &["status", "--porcelain"]), caller_dirt);
    assert_eq!(
        git(&f.clone, &["cat-file", "-p", &f.candidate.head.oid.0]),
        original
    );
}
