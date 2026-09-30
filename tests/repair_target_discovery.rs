//! Public repair-start discovery with fake provider reads and real isolated Git.
//! No production receipt is stubbed and no network command can escape the fixture.
#![cfg(unix)]

#[path = "repair_target_discovery/fixture.rs"]
mod fixture;
use fixture::Fixture;
use std::path::Path;

#[test]
fn explicit_target_public_start_fetches_inactive_target_without_unrelated_rollups() {
    let fixture = Fixture::new(caravan::config::CaravanConfig::default().command_timeout_secs);
    let (output, result, calls) = fixture.start("missing-target");
    assert!(!output.status.success());
    assert!(
        calls
            .iter()
            .any(|args| args.iter().any(|arg| arg == "number=7")),
        "explicit inactive target must be fetched: {calls:?}\n{result}"
    );
    assert!(
        calls
            .iter()
            .any(|args| args.iter().any(|arg| arg == "number=8")),
        "candidate must be fetched"
    );
    assert!(
        calls
            .iter()
            .any(|args| args.windows(2).any(|pair| pair == ["--label", "caravan"])),
        "active topology retained"
    );
    assert!(
        !calls
            .iter()
            .flatten()
            .any(|arg| arg.contains("pullRequests(states:OPEN")),
        "must not enumerate unrelated full rollups"
    );
    assert!(
        !fixture.manifest().exists(),
        "missing target cannot create a session"
    );
}

#[test]
fn explicit_target_public_start_materializes_exact_pair_and_preserves_caller() {
    for scenario in ["inactive-target", "active-target"] {
        let fixture = Fixture::new(caravan::config::CaravanConfig::default().command_timeout_secs);
        let before = fixture.git(&["status", "--porcelain"]);
        let (output, result, calls) = fixture.start(scenario);
        assert!(
            output.status.success(),
            "{scenario}: {result}\n{}\nrefusals={}",
            String::from_utf8_lossy(&output.stderr),
            std::fs::read_to_string(fixture.root.path().join("refusals")).unwrap_or_default()
        );
        let repair = &result["data"]["repair"];
        assert_eq!(repair["state"], "resolving");
        assert_eq!(repair["head"]["oid"], fixture.candidate);
        assert_eq!(repair["target"]["oid"], fixture.target);
        assert_eq!(repair["target_pr"], 7);
        assert_eq!(repair["non_force"]["actor"], "source-owner");
        assert_eq!(repair["non_force"]["target"]["pr"], 7);
        assert_eq!(repair["non_force"]["publication_attempted"], false);
        let workspace = Path::new(repair["workspace"].as_str().unwrap());
        assert_eq!(
            fixture.workspace_git(workspace, &["rev-parse", "MERGE_HEAD"]),
            fixture.target
        );
        assert_eq!(
            fixture.workspace_git(workspace, &["rev-parse", "HEAD"]),
            fixture.candidate
        );
        assert_eq!(fixture.git(&["status", "--porcelain"]), before);
        assert!(fixture.manifest().exists());
        assert!(
            !calls
                .iter()
                .flatten()
                .any(|arg| arg.contains("pullRequests(states:OPEN"))
        );
    }
}

#[test]
fn explicit_target_public_start_keeps_initialization_and_freshness_refusals() {
    for scenario in ["missing-labels", "target-drift"] {
        let fixture = Fixture::new(caravan::config::CaravanConfig::default().command_timeout_secs);
        let (output, result, _) = fixture.start(scenario);
        assert!(!output.status.success(), "{scenario}: {result}");
        let encoded = result.to_string();
        assert!(
            if scenario == "target-drift" {
                encoded.contains("changed identity during focused discovery")
            } else {
                encoded.contains("initialization")
            },
            "{scenario}: {result}"
        );
        assert!(!fixture.manifest().exists());
    }
}

#[test]
fn explicit_target_public_start_does_not_reset_budget_after_discovery() {
    let fixture = Fixture::new(4);
    let started = std::time::Instant::now();
    let (output, result, _) = fixture.start("budget");
    assert!(
        !output.status.success(),
        "budget must remain shared: {result}"
    );
    assert_eq!(
        result["error"]["code"], "github_discovery_timeout",
        "{result}"
    );
    assert_eq!(
        result["error"]["details"]["phase"], "compatibility_analysis",
        "{result}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_millis(5500),
        "repair cannot borrow a fresh candidate budget"
    );
    assert!(!fixture.manifest().exists());
}
