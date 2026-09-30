use super::*;

#[test]
fn non_force_semantic_rejects_divergence_and_missing_non_force() {
    let f = source_fixture();
    assert_eq!(
        prepare_semantic(&f).unwrap_err().code(),
        "repair_semantic_target_not_contained"
    );
    assert_eq!(remote(&f), f.candidate.head.oid.0);
    let mut request = semantic_input(&f);
    request.non_force = false;
    assert_eq!(
        NonForceRepair::new(&request, &f.repository, &f.candidate, None, "main")
            .unwrap_err()
            .code(),
        "repair_non_force_input_invalid"
    );
}

#[test]
fn non_force_semantic_does_not_convert_a_preserved_failed_merge_session() {
    let f = contained_source_fixture();
    assert_eq!(
        start_exact_with_writer_guard(
            &context(&f.clone),
            &f.repository,
            &f.candidate,
            &f.target,
            intent(&f),
            f.root.path().join("remote.git").to_str().unwrap(),
            None,
        )
        .unwrap_err()
        .code(),
        "repair_non_force_already_contains_target"
    );
    let paths = repair_paths(&f.clone, f.candidate.number).unwrap();
    let before = fs::read(&paths.manifest).unwrap();
    fs::write(paths.workspace.join("preserved-wip"), "do not overwrite\n").unwrap();
    let refusal = prepare_semantic(&f).unwrap_err();
    assert_eq!(refusal.code(), "repair_publication_policy_changed");
    let existing = read_manifest(&paths.manifest).unwrap();
    assert_eq!(
        refusal.details().unwrap()["existing"]["session"],
        existing.session
    );
    assert_eq!(fs::read(paths.manifest).unwrap(), before);
    assert_eq!(
        fs::read_to_string(paths.workspace.join("preserved-wip")).unwrap(),
        "do not overwrite\n"
    );
    assert_eq!(remote(&f), f.candidate.head.oid.0);
}

#[test]
fn non_force_semantic_wire_refuses_old_readers_and_downgrades() {
    let f = contained_source_fixture();
    let repair = prepare_semantic(&f).unwrap().repair;
    let bytes = fs::read(repair_paths(&f.clone, f.candidate.number).unwrap().manifest).unwrap();
    let original: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(original["version"], 3);
    assert_eq!(original["state"], "non_force_semantic_v1");
    assert_eq!(decode_manifest(&bytes).unwrap(), repair);
    // Literal v2 reader behavior: unwrap only its known discriminator, then
    // deserialize the unchanged lifecycle enum. Cleanup used this same reader.
    let mut old_reader = original.clone();
    if old_reader["state"] == "non_force_v1" {
        old_reader["state"] = old_reader["non_force_state"].clone();
    }
    assert!(serde_json::from_value::<RepairSession>(old_reader).is_err());
    assert!(serde_json::from_slice::<RepairSession>(&bytes).is_err());
    for field in ["version", "non_force", "non_force_state", "validation"] {
        let mut lost = original.clone();
        lost.as_object_mut().unwrap().remove(field);
        assert!(decode_manifest(&serde_json::to_vec(&lost).unwrap()).is_err());
    }
    for field in ["semantic_only", "publication_attempted"] {
        let mut lost = original.clone();
        lost["non_force"].as_object_mut().unwrap().remove(field);
        assert!(decode_manifest(&serde_json::to_vec(&lost).unwrap()).is_err());
    }
    let mut downgrade = original;
    downgrade["state"] = json!("non_force_v1");
    downgrade["version"] = json!(2);
    assert!(decode_manifest(&serde_json::to_vec(&downgrade).unwrap()).is_err());
}

#[test]
fn non_force_semantic_rejects_noop_ungranted_dirty_and_wrong_actor() {
    for (case, expected) in [
        ("noop", "repair_semantic_no_changes"),
        ("ungranted", "repair_agent_edit_authorization_required"),
        ("unstaged", "repair_unstaged_changes"),
        ("untracked", "repair_untracked_files"),
        ("extra_path", "repair_agent_edit_authorization_required"),
        ("grant_drift", "repair_grant_result_drift"),
        ("wrong_actor", "repair_non_force_custody_required"),
        ("sync", "repair_non_force_custody_required"),
    ] {
        let f = contained_source_fixture();
        let mut repair = prepare_semantic(&f).unwrap().repair;
        if !matches!(case, "noop" | "ungranted") {
            repair = grant_semantic(&f, &repair);
        }
        let workspace = Path::new(&repair.workspace);
        let mut request = continue_input(&repair);
        match case {
            "ungranted" | "extra_path" => {
                fs::write(workspace.join("stable.txt"), "unauthorized\n").unwrap();
                git(workspace, &["add", "stable.txt"]);
            }
            "unstaged" => fs::write(workspace.join("stable.txt"), "unstaged\n").unwrap(),
            "untracked" => fs::write(workspace.join("untracked.txt"), "unexpected\n").unwrap(),
            "grant_drift" => {
                fs::write(workspace.join("README.md"), "wrong reviewed bytes\n").unwrap();
                git(workspace, &["add", "README.md"]);
            }
            "wrong_actor" => request.actor = Some("other-owner".to_owned()),
            "sync" => request.no_sync = false,
            _ => {}
        }
        let error = continue_with_verifier(&context(&f.clone), &request, |_, _| {
            panic!("must not publish {case}")
        })
        .unwrap_err();
        assert_eq!(error.code(), expected, "{case}");
        assert_eq!(remote(&f), f.candidate.head.oid.0, "{case}");
        assert_eq!(
            git(workspace, &["rev-parse", "HEAD"]),
            f.candidate.head.oid.0,
            "{case}"
        );
    }
}

#[test]
fn non_force_semantic_validation_and_fresh_generation_remain_required() {
    for (case, expected) in [
        ("validation", "repair_validation_failed"),
        ("validation_dirt", "repair_non_force_workspace_changed"),
        ("generation", "repair_non_force_generation_changed"),
        ("target", "repair_stale_head"),
    ] {
        let f = contained_source_fixture();
        let repair = grant_semantic(&f, &prepare_semantic(&f).unwrap().repair);
        let mut request = continue_input(&repair);
        let mut candidate = f.candidate.clone();
        match case {
            "validation" => request.validation_commands = vec!["exit 7".to_owned()],
            "validation_dirt" => {
                request.validation_commands = vec!["printf changed > stable.txt".to_owned()]
            }
            "generation" => candidate.updated_at = Some("2026-09-03T00:00:00Z".to_owned()),
            "target" => {
                git(&f.clone, &["checkout", "main"]);
                fs::write(f.clone.join("new-target.txt"), "new target\n").unwrap();
                git(&f.clone, &["add", "new-target.txt"]);
                git(&f.clone, &["commit", "-m", "advance target"]);
                git(&f.clone, &["push", "origin", "main"]);
            }
            _ => unreachable!(),
        }
        let error = continue_with_verifier(&context(&f.clone), &request, |saved, _| {
            saved.non_force.as_ref().unwrap().source.verify(&candidate)
        })
        .unwrap_err();
        assert_eq!(error.code(), expected, "{case}");
        let saved =
            read_manifest(&repair_paths(&f.clone, f.candidate.number).unwrap().manifest).unwrap();
        assert!(!saved.non_force.unwrap().publication_attempted);
        assert_eq!(remote(&f), f.candidate.head.oid.0);
    }
}
