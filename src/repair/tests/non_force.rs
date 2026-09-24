use super::*;
use crate::repair::non_force as policy;

fn source_fixture() -> Fixture {
    let mut f = fixture();
    f.candidate.created_at = Some("2026-09-01T00:00:00Z".to_owned());
    f.candidate.updated_at = Some("2026-09-02T00:00:00Z".to_owned());
    f
}

fn input(f: &Fixture) -> RepairStartInput {
    RepairStartInput {
        pr: f.candidate.number.0,
        target_pr: None,
        non_force: true,
        actor: Some("source-owner".to_owned()),
        reason: Some("preserve authored merge history".to_owned()),
    }
}

fn intent(f: &Fixture) -> RepairStartIntent {
    RepairStartIntent {
        target_pr: None,
        non_force: NonForceRepair::new(&input(f), &f.repository, &f.candidate, None, "main")
            .unwrap(),
    }
}

fn prepare(f: &Fixture) -> RepairSession {
    start_exact_with_writer_guard(
        &context(&f.clone),
        &f.repository,
        &f.candidate,
        &f.target,
        intent(f),
        f.root.path().join("remote.git").to_str().unwrap(),
        None,
    )
    .unwrap()
    .repair
}

fn continue_input(repair: &RepairSession) -> RepairContinueInput {
    RepairContinueInput {
        session: repair.session.clone(),
        actor: Some("source-owner".to_owned()),
        no_sync: true,
        validation_commands: vec!["git diff --check".to_owned()],
    }
}

fn resolve(repair: &RepairSession) {
    let workspace = Path::new(&repair.workspace);
    fs::write(workspace.join("shared.txt"), "resolved\n").unwrap();
    git(workspace, &["add", "shared.txt"]);
}

fn remote(f: &Fixture) -> String {
    git(
        &f.clone,
        &[
            "ls-remote",
            f.root.path().join("remote.git").to_str().unwrap(),
            "refs/heads/feature",
        ],
    )
    .split_whitespace()
    .next()
    .unwrap()
    .to_owned()
}

#[test]
fn non_force_preserves_authored_merge_after_parent_absorption() {
    let mut f = source_fixture();
    let base = f.candidate.base.oid.0.clone();
    git(&f.clone, &["checkout", "-b", "side", &base]);
    fs::write(f.clone.join("side.txt"), "authored side\n").unwrap();
    git(&f.clone, &["add", "side.txt"]);
    git(&f.clone, &["commit", "-m", "authored side"]);
    git(&f.clone, &["checkout", "feature"]);
    git(
        &f.clone,
        &["merge", "--no-ff", "side", "-m", "authored merge"],
    );
    git(&f.clone, &["push", "origin", "feature"]);
    f.candidate.head.oid = CommitOid(git(&f.clone, &["rev-parse", "HEAD"]));
    let authored = git(&f.clone, &["cat-file", "-p", &f.candidate.head.oid.0]);
    assert_eq!(
        commit_parents(
            &ProcessRunner::in_directory(&f.clone),
            &f.candidate.head.oid
        )
        .unwrap()
        .len(),
        2
    );
    git(&f.clone, &["checkout", "main"]);
    git(
        &f.clone,
        &["merge", "--no-ff", "side", "-m", "absorb side parent"],
    );
    git(&f.clone, &["push", "origin", "main"]);
    f.target.oid = CommitOid(git(&f.clone, &["rev-parse", "HEAD"]));
    let caller_head = git(&f.clone, &["rev-parse", "HEAD"]);
    let caller_dirt = git(&f.clone, &["status", "--porcelain"]);
    let repair = prepare(&f);
    resolve(&repair);
    let result = continue_with_verifier(
        &context(&f.clone),
        &continue_input(&repair),
        |session, _| {
            session
                .non_force
                .as_ref()
                .unwrap()
                .source
                .verify(&f.candidate)
        },
    )
    .unwrap();
    let receipt = result.publication.unwrap();
    assert!(!receipt.force);
    assert!(receipt.fresh_ci_required && result.workspace_preserved);
    assert!(result.sync.is_none());
    assert_eq!(
        receipt.parents,
        [f.candidate.head.oid.clone(), f.target.oid.clone()]
    );
    assert_eq!(remote(&f), receipt.new_head.0);
    let workspace = Path::new(&repair.workspace);
    assert_eq!(
        git(workspace, &["cat-file", "-p", &f.candidate.head.oid.0]),
        authored
    );
    git(
        workspace,
        &[
            "merge-base",
            "--is-ancestor",
            &f.candidate.head.oid.0,
            &receipt.new_head.0,
        ],
    );
    assert_eq!(git(&f.clone, &["rev-parse", "HEAD"]), caller_head);
    assert_eq!(git(&f.clone, &["status", "--porcelain"]), caller_dirt);
    let command = policy::publication_command(&result.repair, &receipt.new_head);
    assert!(command.args.iter().any(|arg| arg == "--no-force"));
    assert!(command.args.iter().any(|arg| arg == "--no-follow-tags"));
    assert!(
        !command
            .args
            .iter()
            .any(|arg| arg.starts_with("--force") || arg.starts_with('+'))
    );
    assert!(
        command
            .args
            .iter()
            .any(|arg| arg == &format!("{}:refs/heads/feature", receipt.new_head.0))
    );
}

#[test]
fn non_force_clean_merge_needs_no_edits_and_never_enters_sync() {
    let mut f = source_fixture();
    git(&f.clone, &["checkout", "main"]);
    fs::write(f.clone.join("shared.txt"), "base\n").unwrap();
    git(&f.clone, &["add", "shared.txt"]);
    git(&f.clone, &["commit", "-m", "non-conflicting target"]);
    git(&f.clone, &["push", "origin", "main"]);
    f.target.oid = CommitOid(git(&f.clone, &["rev-parse", "HEAD"]));
    let repair = prepare(&f);
    assert!(repair.conflicting_paths.is_empty());
    let result =
        continue_with_verifier(&context(&f.clone), &continue_input(&repair), |_, _| Ok(()))
            .unwrap();
    assert!(result.sync.is_none());
    assert!(!result.publication.unwrap().force);
    assert!(result.next.contains("existing authorized actor"));
    assert!(
        status(
            &context(&f.clone),
            &RepairStatusInput {
                session: repair.session
            }
        )
        .unwrap()
        .non_force
        .is_some()
    );
}

#[test]
fn non_force_actor_and_no_sync_are_required_even_on_replay() {
    let f = source_fixture();
    let repair = prepare(&f);
    resolve(&repair);
    let mut requested = continue_input(&repair);
    for (actor, no_sync) in [
        (Some("wrong-owner".to_owned()), true),
        (None, true),
        (Some("source-owner".to_owned()), false),
    ] {
        requested.actor = actor;
        requested.no_sync = no_sync;
        assert_eq!(
            continue_with_verifier(&context(&f.clone), &requested, |_, _| panic!(
                "before reads/writes"
            ))
            .unwrap_err()
            .code(),
            "repair_non_force_custody_required"
        );
    }
    assert_eq!(remote(&f), f.candidate.head.oid.0);
    continue_with_verifier(&context(&f.clone), &continue_input(&repair), |_, _| Ok(())).unwrap();
    assert_eq!(
        continue_with_verifier(&context(&f.clone), &requested, |_, _| panic!(
            "published replay never syncs"
        ))
        .unwrap_err()
        .code(),
        "repair_non_force_custody_required"
    );
}

#[test]
fn non_force_rejects_state_identity_base_and_control_generation_changes() {
    let f = source_fixture();
    let captured = intent(&f).non_force.unwrap().source;
    for case in 0..11 {
        let mut changed = f.candidate.clone();
        match case {
            0 => changed.state = PullRequestState::Closed,
            1 => changed.draft = true,
            2 => changed.updated_at = Some("reopened generation".to_owned()),
            3 => changed.created_at = Some("replacement generation".to_owned()),
            4 => changed.head.oid = CommitOid("b".repeat(40)),
            5 => changed.head.name = "different-owner-ref".to_owned(),
            6 => changed.base.oid = CommitOid("c".repeat(40)),
            7 => changed.base.name = "changed-parent".to_owned(),
            8 => {
                changed.labels.insert("caravan-paused".to_owned());
            }
            9 => changed.cross_repository = true,
            _ => changed.updated_at = None,
        }
        assert!(captured.verify(&changed).is_err(), "case {case}");
    }
    captured.verify(&f.candidate).unwrap();
}

#[test]
fn non_force_changed_provider_generation_stops_before_publication_intent() {
    let f = source_fixture();
    let repair = prepare(&f);
    resolve(&repair);
    let mut reopened = f.candidate.clone();
    reopened.updated_at = Some("2026-09-03T00:00:00Z".to_owned());
    let error = continue_with_verifier(
        &context(&f.clone),
        &continue_input(&repair),
        |session, _| session.non_force.as_ref().unwrap().source.verify(&reopened),
    )
    .unwrap_err();
    assert_eq!(error.code(), "repair_non_force_generation_changed");
    assert_eq!(remote(&f), f.candidate.head.oid.0);
    let saved =
        read_manifest(&repair_paths(&f.clone, f.candidate.number).unwrap().manifest).unwrap();
    assert!(!saved.non_force.unwrap().publication_attempted);
}

#[test]
fn non_force_rejects_stale_live_target_and_preserves_workspace() {
    let f = source_fixture();
    let repair = prepare(&f);
    resolve(&repair);
    git(&f.clone, &["checkout", "main"]);
    fs::write(f.clone.join("new-main.txt"), "later default\n").unwrap();
    git(&f.clone, &["add", "new-main.txt"]);
    git(&f.clone, &["commit", "-m", "move target"]);
    git(&f.clone, &["push", "origin", "main"]);
    assert_eq!(
        continue_with_verifier(&context(&f.clone), &continue_input(&repair), |_, _| panic!(
            "target moved"
        ))
        .unwrap_err()
        .code(),
        "repair_stale_head"
    );
    assert_eq!(remote(&f), f.candidate.head.oid.0);
    assert!(Path::new(&repair.workspace).exists());
}

#[test]
fn non_force_lost_acknowledgement_recovers_exact_successor_without_repeating_effects() {
    let f = source_fixture();
    let repair = prepare(&f);
    resolve(&repair);
    let result =
        continue_with_verifier(&context(&f.clone), &continue_input(&repair), |_, _| Ok(()))
            .unwrap();
    let expected = result.publication.unwrap();
    let mut interrupted = result.repair;
    interrupted.state = RepairState::Committed;
    interrupted.phase = RepairPhase::Committed;
    let paths = repair_paths(&f.clone, f.candidate.number).unwrap();
    write_manifest(&paths.manifest, &interrupted).unwrap();
    let mut requested = continue_input(&interrupted);
    requested.validation_commands = vec!["exit 99".to_owned()];
    assert_eq!(
        continue_with_verifier(&context(&f.clone), &requested, |_, _| panic!(
            "changed readback validation"
        ))
        .unwrap_err()
        .code(),
        "repair_non_force_validation_changed"
    );
    requested.validation_commands.clear();
    // Default movement after the original push cannot authorize another push,
    // but must not prevent readback of that exact historical source success.
    git(&f.clone, &["checkout", "main"]);
    fs::write(f.clone.join("after-publication"), "new default\n").unwrap();
    git(&f.clone, &["add", "after-publication"]);
    git(
        &f.clone,
        &["commit", "-m", "advance after source publication"],
    );
    git(&f.clone, &["push", "origin", "main"]);
    let replay = continue_with_verifier(&context(&f.clone), &requested, |_, _| {
        panic!("no new provider mutation")
    })
    .unwrap();
    assert_eq!(replay.publication.unwrap(), expected);
    assert_eq!(remote(&f), expected.new_head.0);
    let again = continue_with_verifier(&context(&f.clone), &requested, |_, _| {
        panic!("terminal replay")
    })
    .unwrap();
    assert_eq!(again.publication.unwrap(), expected);
}

#[test]
fn non_force_uncertain_attempt_cannot_retry_or_delete_evidence() {
    let f = source_fixture();
    let mut repair = prepare(&f);
    resolve(&repair);
    let workspace = Path::new(&repair.workspace);
    git(
        workspace,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "prepared",
        ],
    );
    repair.state = RepairState::Committed;
    repair.phase = RepairPhase::Committed;
    repair.published_head = Some(CommitOid(git(workspace, &["rev-parse", "HEAD"])));
    repair.non_force.as_mut().unwrap().publication_attempted = true;
    let paths = repair_paths(&f.clone, f.candidate.number).unwrap();
    write_manifest(&paths.manifest, &repair).unwrap();
    let mut readback = continue_input(&repair);
    readback.validation_commands.clear();
    assert_eq!(
        continue_with_verifier(&context(&f.clone), &readback, |_, _| panic!(
            "cannot repeat uncertain push"
        ))
        .unwrap_err()
        .code(),
        "repair_non_force_publication_unresolved"
    );
    assert_eq!(
        abort(
            &context(&f.clone),
            &RepairAbortInput {
                session: repair.session.clone(),
                confirm: true
            }
        )
        .unwrap_err()
        .code(),
        "repair_non_force_publication_unresolved"
    );
    assert_eq!(remote(&f), f.candidate.head.oid.0);
    assert!(paths.workspace.exists() && paths.manifest.exists());
}

#[test]
fn non_force_cannot_relabel_legacy_or_other_actor_evidence() {
    let f = source_fixture();
    let context = context(&f.clone);
    let provider = f.root.path().join("remote.git");
    let legacy = start_exact(
        &context,
        &f.repository,
        &f.candidate,
        &f.target,
        None,
        provider.to_str().unwrap(),
    )
    .unwrap();
    let error = start_exact_with_writer_guard(
        &context,
        &f.repository,
        &f.candidate,
        &f.target,
        intent(&f),
        provider.to_str().unwrap(),
        None,
    )
    .unwrap_err();
    assert_eq!(error.code(), "repair_publication_policy_changed");
    assert!(legacy.repair.non_force.is_none());
    let decoded: RepairSession =
        serde_json::from_value(serde_json::to_value(&legacy.repair).unwrap()).unwrap();
    assert!(decoded.non_force.is_none());
    assert!(
        policy::publication_command(&decoded, &f.target.oid)
            .args
            .iter()
            .any(|arg| arg.starts_with("--force-with-lease="))
    );
}

#[test]
fn non_force_wire_manifest_refuses_old_binary_and_lost_policy() {
    let f = source_fixture();
    let repair = prepare(&f);
    let bytes = fs::read(repair_paths(&f.clone, f.candidate.number).unwrap().manifest).unwrap();
    assert_eq!(repair.version, NON_FORCE_REPAIR_VERSION);
    // The legacy reader directly deserialized RepairSession; its unknown state
    // refusal also fences cleanup paths that did not check version.
    assert!(serde_json::from_slice::<RepairSession>(&bytes).is_err());
    assert_eq!(decode_manifest(&bytes).unwrap(), repair);
    let original: Value = serde_json::from_slice(&bytes).unwrap();
    for field in ["non_force", "non_force_state", "version"] {
        let mut lost = original.clone();
        lost.as_object_mut().unwrap().remove(field);
        assert!(decode_manifest(&serde_json::to_vec(&lost).unwrap()).is_err());
    }
    let mut missing_attempt = original.clone();
    missing_attempt["non_force"]
        .as_object_mut()
        .unwrap()
        .remove("publication_attempted");
    assert!(decode_manifest(&serde_json::to_vec(&missing_attempt).unwrap()).is_err());
    let mut mismatched_source = original;
    mismatched_source["non_force"]["source"]["head"]["name"] = json!("foreign-ref");
    assert!(decode_manifest(&serde_json::to_vec(&mismatched_source).unwrap()).is_err());
    assert!(
        decode_manifest(&serde_json::to_vec(&repair).unwrap()).is_err(),
        "API JSON must not replace the wire manifest"
    );
}

#[test]
fn non_force_rejects_unsafe_scope_and_missing_custody_inputs() {
    let f = source_fixture();
    for case in 0..6 {
        let mut requested = input(&f);
        let mut source = f.candidate.clone();
        match case {
            0 => requested.actor = None,
            1 => requested.reason = Some(" ".to_owned()),
            2 => source.head.name = "main".to_owned(),
            3 => source.head.repository.owner = "foreign".to_owned(),
            4 => requested.target_pr = Some(8),
            _ => requested.pr = 99,
        }
        assert!(NonForceRepair::new(&requested, &f.repository, &source, None, "main").is_err());
    }
}

#[test]
fn non_force_validation_mutation_and_writer_contention_do_not_publish() {
    let f = source_fixture();
    let repair = prepare(&f);
    resolve(&repair);
    let held = OperationLock::acquire(&f.clone, "other-writer").unwrap();
    assert!(
        continue_with_verifier(&context(&f.clone), &continue_input(&repair), |_, _| panic!(
            "writer must be acquired"
        ))
        .is_err()
    );
    drop(held);
    let mut requested = continue_input(&repair);
    requested.validation_commands = vec!["printf changed > stable.txt".to_owned()];
    assert_eq!(
        continue_with_verifier(&context(&f.clone), &requested, |_, _| panic!(
            "unvalidated bytes"
        ))
        .unwrap_err()
        .code(),
        "repair_non_force_workspace_changed"
    );
    assert_eq!(remote(&f), f.candidate.head.oid.0);
}

#[test]
fn non_force_source_race_after_preflight_is_not_overwritten() {
    let f = source_fixture();
    let repair = prepare(&f);
    resolve(&repair);
    let error = continue_with_verifier(&context(&f.clone), &continue_input(&repair), |_, _| {
        fs::write(f.clone.join("other-writer"), "concurrent source\n").unwrap();
        git(&f.clone, &["add", "other-writer"]);
        git(&f.clone, &["commit", "-m", "concurrent writer"]);
        git(&f.clone, &["push", "origin", "feature"]);
        Ok(())
    })
    .unwrap_err();
    assert_eq!(error.code(), "repair_non_fast_forward");
    assert_eq!(remote(&f), git(&f.clone, &["rev-parse", "HEAD"]));
    let persisted =
        read_manifest(&repair_paths(&f.clone, f.candidate.number).unwrap().manifest).unwrap();
    assert!(persisted.non_force.unwrap().publication_attempted);
}

#[test]
fn non_force_already_stacked_source_gets_precise_zero_write_refusal() {
    let mut f = source_fixture();
    let merge = ProcessRunner::in_directory(&f.clone)
        .run(&CommandSpec::new("git").args(["merge", "--no-ff", "--no-commit", "main"]))
        .unwrap();
    assert_eq!(merge.code, Some(1));
    fs::write(f.clone.join("shared.txt"), "resolved feature and target\n").unwrap();
    git(&f.clone, &["add", "shared.txt"]);
    git(&f.clone, &["commit", "-m", "already contains exact target"]);
    git(&f.clone, &["push", "origin", "feature"]);
    f.candidate.head.oid = CommitOid(git(&f.clone, &["rev-parse", "HEAD"]));
    let error = start_exact_with_writer_guard(
        &context(&f.clone),
        &f.repository,
        &f.candidate,
        &f.target,
        intent(&f),
        f.root.path().join("remote.git").to_str().unwrap(),
        None,
    )
    .unwrap_err();
    assert_eq!(error.code(), "repair_non_force_already_contains_target");
    assert_eq!(remote(&f), f.candidate.head.oid.0);
    let persisted =
        read_manifest(&repair_paths(&f.clone, f.candidate.number).unwrap().manifest).unwrap();
    assert_eq!(persisted.last_error.unwrap().code, error.code());
}

#[cfg(unix)]
#[test]
fn non_force_readback_loss_preserves_possible_effects_without_another_push() {
    use std::os::unix::fs::PermissionsExt;
    let f = source_fixture();
    let repair = prepare(&f);
    resolve(&repair);
    let bare = f.root.path().join("remote.git");
    let hook = bare.join("hooks/post-receive");
    // Simulate a server-side writer rolling back after accepting our update.
    // An unchanged old ref afterward must not be treated as proof of no write.
    fs::write(&hook, format!("#!/bin/sh\nwhile read old new reference; do\n  if [ \"$reference\" = refs/heads/feature ]; then\n    printf applied >> publication-count\n    git update-ref refs/heads/feature {} \"$new\"\n  fi\ndone\n", f.candidate.head.oid.0)).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    let error = continue_with_verifier(&context(&f.clone), &continue_input(&repair), |_, _| Ok(()))
        .unwrap_err();
    assert_eq!(error.code(), "repair_non_force_publication_unconfirmed");
    assert_eq!(error.details().unwrap()["provider_mutation_possible"], true);
    assert_eq!(remote(&f), f.candidate.head.oid.0);
    let count = fs::read_to_string(bare.join("publication-count")).unwrap();
    assert_eq!(count, "applied");
    let error = continue_with_verifier(&context(&f.clone), &continue_input(&repair), |_, _| {
        panic!("no replay")
    })
    .unwrap_err();
    assert_eq!(error.code(), "repair_non_force_publication_unresolved");
    assert_eq!(
        fs::read_to_string(bare.join("publication-count")).unwrap(),
        count
    );
}

fn provider_row(pull: &PullRequestSnapshot) -> Value {
    json!({
        "number":pull.number.0,"title":"source","state":"OPEN","isDraft":pull.draft,
        "headRefName":pull.head.name,"headRefOid":pull.head.oid.0,"headRepository":null,
        "headRepositoryOwner":null,"isCrossRepository":false,"baseRefName":pull.base.name,
        "baseRefOid":pull.base.oid.0,"labels":[],"autoMergeRequest":null,"statusCheckRollup":[],
        "createdAt":pull.created_at,"updatedAt":pull.updated_at,"mergedAt":null,"url":"https://example.invalid/pr"
    })
}

struct ProviderRead {
    rows: BTreeMap<u64, Value>,
    default_ref: String,
    calls: std::cell::RefCell<Vec<CommandSpec>>,
}
impl CommandRunner for ProviderRead {
    fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
        assert_eq!(command.program, "gh");
        self.calls.borrow_mut().push(command.clone());
        if command.args.first().is_some_and(|arg| arg == "repo") {
            return Ok(CommandOutput::success(&self.default_ref));
        }
        assert!(command.args.iter().any(|arg| arg == "graphql"));
        assert!(!command.args.iter().any(|arg| arg.contains("mutation ")));
        let number = command
            .args
            .iter()
            .find_map(|arg| arg.strip_prefix("number="))
            .unwrap()
            .parse::<u64>()
            .unwrap();
        Ok(CommandOutput::success(self.rows[&number].to_string()))
    }
}

#[test]
fn non_force_provider_rechecks_source_parent_and_default_without_writes() {
    let f = source_fixture();
    let mut parent = f.candidate.clone();
    parent.number = PrNumber(8);
    parent.head.name = "parent".to_owned();
    parent.head.oid = f.target.oid.clone();
    let mut start = input(&f);
    start.target_pr = Some(8);
    let mut repair = prepare(&f);
    repair.non_force =
        NonForceRepair::new(&start, &f.repository, &f.candidate, Some(&parent), "main").unwrap();
    let mut runner = ProviderRead {
        rows: BTreeMap::from([(7, provider_row(&f.candidate)), (8, provider_row(&parent))]),
        default_ref: "main".to_owned(),
        calls: std::cell::RefCell::new(Vec::new()),
    };
    policy::verify_provider(&repair, &runner).unwrap();
    assert_eq!(runner.calls.borrow().len(), 3);
    runner.rows.get_mut(&8).unwrap()["updatedAt"] = json!("2026-09-03T00:00:00Z");
    assert_eq!(
        policy::verify_provider(&repair, &runner)
            .unwrap_err()
            .code(),
        "repair_non_force_generation_changed"
    );
    runner.rows.insert(8, provider_row(&parent));
    runner.rows.get_mut(&7).unwrap()["state"] = json!("CLOSED");
    assert_eq!(
        policy::verify_provider(&repair, &runner)
            .unwrap_err()
            .code(),
        "repair_non_force_generation_unsupported"
    );
    runner.rows.insert(7, provider_row(&f.candidate));
    runner.default_ref = "feature".to_owned();
    assert_eq!(
        policy::verify_provider(&repair, &runner)
            .unwrap_err()
            .code(),
        "repair_non_force_default_changed"
    );
    assert_eq!(remote(&f), f.candidate.head.oid.0);
}
