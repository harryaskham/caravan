const GATE: &str = include_str!("../examples/workflows/caravan-gate.yml");
const ADMIT: &str = include_str!("../examples/workflows/cara-admit.yml");
const SYNC: &str = include_str!("../examples/workflows/cara-sync.yml");

#[test]
fn canonical_workflow_bundle_separates_gate_admission_and_convergence() {
    assert!(GATE.contains("types: [opened, synchronize, reopened]"));
    assert!(GATE.contains("ci-admission-gate"));
    assert!(GATE.contains("deferred-unjoined"));
    assert!(!GATE.contains("labeled, unlabeled"));

    for required in [
        "pull_request_target:",
        "ready_for_review",
        "edited",
        "labeled",
        "unlabeled",
        "cara --json admit >cara-admit-receipt.json",
        "ref: ${{ github.event.repository.default_branch }}",
        "persist-credentials: false",
        "CARA_VERSION: \"REPLACE_WITH_PINNED_VERSION\"",
        "CARA_ARCHIVE_SHA256: \"REPLACE_WITH_64_HEX_ARCHIVE_SHA256\"",
        "[[ \"$CARA_ARCHIVE_SHA256\" =~ ^[0-9a-f]{64}$ ]]",
        "sha256sum --check",
        "actions: write",
        "checks: write",
        "pull-requests: write",
    ] {
        assert!(
            ADMIT.contains(required),
            "admission template lost `{required}`"
        );
    }
    assert!(!ADMIT.contains("github.event.pull_request.head"));
    assert!(!ADMIT.contains("cara --json sync"));
    assert!(!ADMIT.contains("cara new"));
    assert!(!ADMIT.contains("cara join"));
    assert!(!ADMIT.contains("gh pr"));

    assert!(SYNC.contains("cara --json sync --all >cara-sync-receipt.json"));
    assert!(!SYNC.contains("pull_request_target:"));
    assert!(!SYNC.contains("cara --json admit"));

    let admit_key = ADMIT
        .lines()
        .find(|line| line.trim_start().starts_with("group: cara-writer-"))
        .expect("admission writer key");
    let sync_key = SYNC
        .lines()
        .find(|line| line.trim_start().starts_with("group: cara-writer-"))
        .expect("sync writer key");
    assert_eq!(admit_key.trim(), sync_key.trim());
    assert!(ADMIT.contains("cancel-in-progress: false"));
    assert!(SYNC.contains("cancel-in-progress: false"));
}
