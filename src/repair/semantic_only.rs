//! Explicit one-parent semantic correction; shares the non-force publication fence.

use super::{
    AppError, CommandRunner, CommandSpec, CommitOid, Path, RepairSession, non_force::refusal,
    read_manifest, require_success, rev_parse, run, try_rev_parse, unix_ms, write_manifest,
};

pub(super) fn enabled(repair: &RepairSession) -> bool {
    repair
        .non_force
        .as_ref()
        .is_some_and(|policy| policy.semantic_only)
}

pub(super) fn verify_prepared(
    runner: &impl CommandRunner,
    repair: &RepairSession,
) -> Result<(), AppError> {
    if rev_parse(runner, "HEAD")? != repair.head.oid
        || try_rev_parse(runner, "MERGE_HEAD")?.is_some()
    {
        return Err(refusal(
            "repair_semantic_workspace_changed",
            "semantic correction requires the exact original source with no merge in progress",
        ));
    }
    let ancestor = run(
        runner,
        CommandSpec::new("git").args([
            "merge-base",
            "--is-ancestor",
            &repair.target.oid.0,
            &repair.head.oid.0,
        ]),
    )?;
    if !ancestor.is_success() {
        return Err(refusal(
            "repair_semantic_target_not_contained",
            "semantic-only repair requires proven target containment; use ordinary merge repair for divergent ancestry",
        ));
    }
    Ok(())
}

/// Grant/edit operations must not reuse a stale pre-lock snapshot or modify a
/// commit left at the pre-checkpoint crash boundary.
pub(super) fn verify_editable(
    runner: &impl CommandRunner,
    repair: &RepairSession,
    manifest: &Path,
) -> Result<(), AppError> {
    if !enabled(repair) {
        return Ok(());
    }
    if read_manifest(manifest)? != *repair {
        return Err(refusal(
            "repair_non_force_session_changed",
            "semantic session changed while acquiring the writer; preserve and reread it",
        ));
    }
    verify_prepared(runner, repair)
}

pub(super) fn record_prepared_tree(
    runner: &impl CommandRunner,
    repair: &mut RepairSession,
    manifest: &Path,
) -> Result<(), AppError> {
    if !enabled(repair) {
        return Ok(());
    }
    let tree = require_success(
        runner,
        CommandSpec::new("git").args(["write-tree"]),
        "repair_semantic_tree_unavailable",
        "could not record the authorized semantic index tree",
    )?;
    let tree = rev_parse(runner, &format!("{}^{{tree}}", tree.stdout.trim()))?;
    if tree == rev_parse(runner, &format!("{}^{{tree}}", repair.head.oid.0))? {
        return Err(refusal(
            "repair_semantic_no_changes",
            "semantic-only repair cannot publish an empty correction",
        ));
    }
    repair
        .non_force
        .as_mut()
        .expect("semantic policy")
        .prepared_tree = Some(tree);
    repair.updated_unix_ms = unix_ms();
    write_manifest(manifest, repair)
}

pub(super) fn verify_committed(
    runner: &impl CommandRunner,
    repair: &RepairSession,
    head: &CommitOid,
) -> Result<(), AppError> {
    if !enabled(repair) {
        return Ok(());
    }
    let expected = repair
        .non_force
        .as_ref()
        .and_then(|policy| policy.prepared_tree.as_ref());
    let actual = rev_parse(runner, &format!("{}^{{tree}}", head.0))?;
    if expected != Some(&actual)
        || actual == rev_parse(runner, &format!("{}^{{tree}}", repair.head.oid.0))?
        || try_rev_parse(runner, "MERGE_HEAD")?.is_some()
    {
        return Err(refusal(
            "repair_semantic_tree_unverified",
            "semantic successor must match the nonempty authorized tree recorded before commit; preserve unverified local work",
        ));
    }
    Ok(())
}
