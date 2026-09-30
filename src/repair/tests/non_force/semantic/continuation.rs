use super::*;

#[test]
fn non_force_semantic_grants_publish_one_parent_and_replay_exact_receipt() {
    let f = contained_source_fixture();
    let original = git(&f.clone, &["cat-file", "-p", &f.candidate.head.oid.0]);
    let repair = grant_semantic(&f, &prepare_semantic(&f).unwrap().repair);
    let result = publish(&f, &repair).unwrap();
    let receipt = result.publication.unwrap();
    let workspace = Path::new(&repair.workspace);
    assert_eq!(receipt.parents, [f.candidate.head.oid.clone()]);
    assert_eq!(
        git(
            workspace,
            &["show", "-s", "--format=%P", &receipt.new_head.0]
        ),
        f.candidate.head.oid.0
    );
    assert_eq!(
        git(workspace, &["cat-file", "-p", &f.candidate.head.oid.0]),
        original
    );
    assert_eq!(
        git(workspace, &["show", "HEAD:README.md"]),
        "# Base\n\nReviewed shell-safe message body."
    );
    assert!(!receipt.force && receipt.remote_verified && receipt.fresh_ci_required);
    assert!(result.sync.is_none() && result.workspace_preserved);
    assert_eq!(remote(&f), receipt.new_head.0);
    assert_eq!(receipt.validation.len(), 1);
    assert!(receipt.validation[0].passed);
    assert_eq!(
        policy::publication_command(&result.repair, &receipt.new_head).args,
        [
            "push",
            "--no-follow-tags",
            "--recurse-submodules=no",
            "--no-force",
            result.repair.provider_git_url.as_str(),
            &format!("{}:refs/heads/feature", receipt.new_head.0),
        ]
    );

    // The provider accepted the exact bytes but the caller lost the acknowledgement.
    let mut interrupted = result.repair;
    interrupted.state = RepairState::Committed;
    interrupted.phase = RepairPhase::Committed;
    let paths = repair_paths(&f.clone, f.candidate.number).unwrap();
    write_manifest(&paths.manifest, &interrupted).unwrap();
    let mut request = continue_input(&interrupted);
    request.validation_commands = vec!["exit 99".to_owned()];
    assert_eq!(
        continue_with_verifier(&context(&f.clone), &request, |_, _| panic!(
            "no new preflight"
        ))
        .unwrap_err()
        .code(),
        "repair_non_force_validation_changed"
    );
    request.validation_commands.clear();
    let replay = continue_with_verifier(&context(&f.clone), &request, |_, _| {
        panic!("no new publication")
    })
    .unwrap();
    assert_eq!(replay.publication.unwrap(), receipt);
    let terminal = continue_with_verifier(&context(&f.clone), &request, |_, _| {
        panic!("terminal receipt")
    })
    .unwrap();
    assert_eq!(terminal.publication.unwrap(), receipt);
    assert_eq!(remote(&f), receipt.new_head.0);
}

#[test]
fn non_force_semantic_agent_edits_produce_bounded_receipt() {
    let f = contained_source_fixture();
    let repair = prepare_semantic(&f).unwrap().repair;
    authorize_agent_edits(
        &context(&f.clone),
        &RepairAuthorizeAgentEditsInput {
            session: repair.session.clone(),
            actor: "source-owner".to_owned(),
            reason: "reviewed new source file".to_owned(),
            expires_secs: 3600,
        },
    )
    .unwrap();
    let workspace = Path::new(&repair.workspace);
    fs::write(
        workspace.join("new-source.txt"),
        "reviewed implementation\n",
    )
    .unwrap();
    git(workspace, &["add", "new-source.txt"]);
    let receipt = publish(&f, &repair).unwrap().publication.unwrap();
    let edits = receipt.agent_edit_receipt.unwrap();
    assert_eq!(edits.actor, "source-owner");
    assert_eq!(edits.paths, ["new-source.txt"]);
    assert!(edits.diff_bytes > 0 && edits.fresh_ci_required);
    assert_eq!(receipt.parents, [f.candidate.head.oid.clone()]);
    assert_eq!(remote(&f), receipt.new_head.0);
}

#[test]
fn non_force_semantic_recovers_commit_boundary_only_with_authorized_tree() {
    for case in ["unrecorded", "recorded", "altered_tree"] {
        let f = contained_source_fixture();
        let mut repair = grant_semantic(&f, &prepare_semantic(&f).unwrap().repair);
        let runner = ProcessRunner::in_directory(&repair.workspace);
        let paths = repair_paths(&f.clone, f.candidate.number).unwrap();
        if case != "unrecorded" {
            verify_resolution(&runner, &repair, Some("source-owner")).unwrap();
            semantic_only::record_prepared_tree(&runner, &mut repair, &paths.manifest).unwrap();
        }
        if case == "altered_tree" {
            fs::write(
                Path::new(&repair.workspace).join("stable.txt"),
                "unreviewed\n",
            )
            .unwrap();
            git(Path::new(&repair.workspace), &["add", "stable.txt"]);
        }
        commit_locally(&repair);
        let prepared = git(Path::new(&repair.workspace), &["rev-parse", "HEAD"]);
        let result = publish(&f, &repair);
        if case == "recorded" {
            assert_eq!(result.unwrap().publication.unwrap().new_head.0, prepared);
            assert_eq!(remote(&f), prepared);
        } else {
            assert_eq!(
                result.unwrap_err().code(),
                "repair_semantic_tree_unverified"
            );
            assert_eq!(remote(&f), f.candidate.head.oid.0);
        }
    }
}

#[test]
fn non_force_semantic_committed_bytes_cannot_be_edited_as_resolving_work() {
    let f = contained_source_fixture();
    let mut repair = grant_semantic(&f, &prepare_semantic(&f).unwrap().repair);
    let runner = ProcessRunner::in_directory(&repair.workspace);
    let paths = repair_paths(&f.clone, f.candidate.number).unwrap();
    verify_resolution(&runner, &repair, Some("source-owner")).unwrap();
    semantic_only::record_prepared_tree(&runner, &mut repair, &paths.manifest).unwrap();
    commit_locally(&repair);
    let before = fs::read(&paths.manifest).unwrap();
    let error = authorize_agent_edits(
        &context(&f.clone),
        &RepairAuthorizeAgentEditsInput {
            session: repair.session.clone(),
            actor: "source-owner".to_owned(),
            reason: "must not edit committed checkpoint".to_owned(),
            expires_secs: 3600,
        },
    )
    .unwrap_err();
    assert_eq!(error.code(), "repair_semantic_workspace_changed");
    assert_eq!(fs::read(&paths.manifest).unwrap(), before);
    assert_eq!(remote(&f), f.candidate.head.oid.0);
    // A pre-lock reader cannot overwrite a different durable session snapshot.
    let mut stale = repair;
    stale.updated_unix_ms = 0;
    assert_eq!(
        semantic_only::verify_editable(&runner, &stale, &paths.manifest)
            .unwrap_err()
            .code(),
        "repair_non_force_session_changed"
    );
}

#[test]
fn non_force_semantic_unchanged_remote_cannot_clear_uncertain_intent_or_abort() {
    let f = contained_source_fixture();
    let mut repair = grant_semantic(&f, &prepare_semantic(&f).unwrap().repair);
    let runner = ProcessRunner::in_directory(&repair.workspace);
    let paths = repair_paths(&f.clone, f.candidate.number).unwrap();
    verify_resolution(&runner, &repair, Some("source-owner")).unwrap();
    semantic_only::record_prepared_tree(&runner, &mut repair, &paths.manifest).unwrap();
    commit_locally(&repair);
    repair.state = RepairState::Committed;
    repair.phase = RepairPhase::Committed;
    repair.published_head = Some(rev_parse(&runner, "HEAD").unwrap());
    repair.non_force.as_mut().unwrap().publication_attempted = true;
    write_manifest(&paths.manifest, &repair).unwrap();
    let before = fs::read(&paths.manifest).unwrap();
    let mut request = continue_input(&repair);
    request.validation_commands.clear();
    assert_eq!(
        continue_with_verifier(&context(&f.clone), &request, |_, _| panic!(
            "uncertain intent"
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
    assert_eq!(fs::read(paths.manifest).unwrap(), before);
    assert_eq!(remote(&f), f.candidate.head.oid.0);
}
