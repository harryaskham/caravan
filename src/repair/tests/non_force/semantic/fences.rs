use super::*;

#[test]
fn non_force_semantic_rejects_redundant_merge_parent_even_with_authorized_tree() {
    let f = contained_source_fixture();
    let mut repair = grant_semantic(&f, &prepare_semantic(&f).unwrap().repair);
    let workspace_path = repair.workspace.clone();
    let workspace = Path::new(&workspace_path);
    let runner = ProcessRunner::in_directory(workspace);
    let paths = repair_paths(&f.clone, f.candidate.number).unwrap();
    verify_resolution(&runner, &repair, Some("source-owner")).unwrap();
    semantic_only::record_prepared_tree(&runner, &mut repair, &paths.manifest).unwrap();
    let tree = repair
        .non_force
        .as_ref()
        .unwrap()
        .prepared_tree
        .as_ref()
        .unwrap();
    // Deliberately forge the forbidden two-parent shape in this isolated fixture.
    // Production must refuse it, never use a redundant target as a workaround.
    let wrong = git(
        workspace,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit-tree",
            &tree.0,
            "-p",
            &repair.head.oid.0,
            "-p",
            &repair.target.oid.0,
            "-m",
            "invalid redundant merge",
        ],
    );
    git(workspace, &["checkout", "--detach", &wrong]);
    assert_eq!(
        publish(&f, &repair).unwrap_err().code(),
        "repair_parent_mismatch"
    );
    assert_eq!(remote(&f), f.candidate.head.oid.0);
}

#[test]
fn non_force_semantic_remote_race_preserves_other_writer_and_uncertain_intent() {
    let f = contained_source_fixture();
    let repair = grant_semantic(&f, &prepare_semantic(&f).unwrap().repair);
    let error = continue_with_verifier(&context(&f.clone), &continue_input(&repair), |saved, _| {
        saved
            .non_force
            .as_ref()
            .unwrap()
            .source
            .verify(&f.candidate)?;
        // Between the last source read and push, another writer advances it.
        fs::write(f.clone.join("other-writer.txt"), "concurrent source\n").unwrap();
        git(&f.clone, &["add", "other-writer.txt"]);
        git(&f.clone, &["commit", "-m", "concurrent source"]);
        git(&f.clone, &["push", "origin", "feature"]);
        Ok(())
    })
    .unwrap_err();
    assert_eq!(error.code(), "repair_non_fast_forward");
    assert_eq!(remote(&f), git(&f.clone, &["rev-parse", "HEAD"]));
    let saved =
        read_manifest(&repair_paths(&f.clone, f.candidate.number).unwrap().manifest).unwrap();
    assert!(saved.non_force.unwrap().publication_attempted);
}
