use caravan::repair::{RepairContinueInput, RepairStartInput};
use clap::{Args, Command, FromArgMatches};

#[test]
fn non_force_repair_interface_parses_actual_documented_flags() {
    let matches = RepairStartInput::augment_args(Command::new("start"))
        .try_get_matches_from([
            "start",
            "--pr",
            "7",
            "--target-pr",
            "6",
            "--non-force",
            "--actor",
            "source-owner",
            "--reason",
            "preserve authored history",
        ])
        .unwrap();
    let start = RepairStartInput::from_arg_matches(&matches).unwrap();
    assert!(start.non_force);
    assert_eq!(start.target_pr, Some(6));
    assert_eq!(start.actor.as_deref(), Some("source-owner"));
    let matches = RepairContinueInput::augment_args(Command::new("continue"))
        .try_get_matches_from([
            "continue",
            "--session",
            "pr-7-generation",
            "--actor",
            "source-owner",
            "--no-sync",
            "--validate",
            "git diff --check",
        ])
        .unwrap();
    let next = RepairContinueInput::from_arg_matches(&matches).unwrap();
    assert!(next.no_sync);
    assert_eq!(next.actor, start.actor);
    let legacy: RepairStartInput = serde_json::from_value(serde_json::json!({"pr":7})).unwrap();
    assert!(!legacy.non_force);
    assert!(legacy.actor.is_none());
    let source = include_str!("../src/lib.rs");
    assert!(source.contains("\"repair_start_non_force\""));
    let typed = serde_json::json!({"pr":7,"actor":"owner","reason":"preserve"});
    assert!(
        serde_json::from_value::<caravan::repair::NonForceRepairStartInput>(typed.clone()).is_ok()
    );
    let mut unsupported = typed;
    unsupported["non_force"] = serde_json::json!(false);
    assert!(
        serde_json::from_value::<caravan::repair::NonForceRepairStartInput>(unsupported).is_err()
    );
    assert!(
        caravan::EvictInput::augment_args(Command::new("evict"))
            .try_get_matches_from(["evict", "--pr", "7", "--reason", "inspect", "--dry-run"])
            .is_err()
    );
}

#[test]
fn non_force_operator_contract_states_real_force_and_uncertainty_boundaries() {
    let text = [
        include_str!("../.agents/skills/cara-operator/references/history-preserving-repair.md"),
        include_str!("../SPEC.md"),
    ]
    .join("\n");
    let normalized = text
        .replace("**", "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for required in [
        "--non-force --actor",
        "--no-sync",
        "git push --no-force",
        "there is no standalone sealed tail-eviction preview CLI",
        "auto_apply_from_status",
        "repair_start_non_force",
        "never fall back",
        "legacy",
        "force-with-lease",
        "repair_non_force_publication_unresolved",
        "not proof",
        "earlier uncertain",
        "actor string",
        "old source",
    ] {
        assert!(
            normalized.to_lowercase().contains(&required.to_lowercase()),
            "missing {required}"
        );
    }
    let skill = include_str!("../.agents/skills/cara-operator/SKILL.md");
    assert!(skill.contains("references/history-preserving-repair.md"));
    let canary = include_str!("../.agents/skills/cara-operator/references/safe-path-canary.md");
    assert!(canary.contains("There is no standalone sealed tail-eviction preview CLI"));
    let native = include_str!("../src/native_stack_rebase.rs");
    assert!(native.contains("pub(crate) fn auto_apply_from_status"));
    assert!(native.contains("apply_prepared_atomically"));
    let physical = include_str!("../src/physical_rebase.rs");
    assert!(physical.contains("--force-with-lease=refs/heads/"));
}
