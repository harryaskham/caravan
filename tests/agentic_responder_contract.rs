use serde_json::Value;

const INSTRUCTIONS: &str = include_str!("../agentic/cara-responder.md");
const HELP: &str = include_str!("../agentic/cara-responder-help.md");
const POLICY: &str = include_str!("../agentic/cara-responder-policy.json");
const RUNTIME_SCHEMA: &str = include_str!("../agentic/cara-runtime-pin.schema.json");
const REPORT_SCHEMA: &str = include_str!("../agentic/cara-run-report.schema.json");

fn json(source: &str) -> Value {
    serde_json::from_str(source).expect("agentic responder contract is valid JSON")
}

fn strings(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .expect("contract field is an array")
        .iter()
        .map(|entry| entry.as_str().expect("contract item is a string"))
        .collect()
}

#[test]
fn responder_defaults_report_only_and_fences_the_single_writer() {
    let policy = json(POLICY);
    assert_eq!(policy["schema_version"], 1);
    assert_eq!(policy["mode_default"], "report_only");
    assert_eq!(policy["single_writer"]["agentic_mutation_enabled"], false);
    assert_eq!(
        policy["single_writer"]["current_mutating_workflow"],
        ".github/workflows/cara-sync.yml"
    );
    assert_eq!(
        policy["single_writer"]["cutover_requires_current_writer_disabled"],
        true
    );
    assert_eq!(policy["bounds"]["max_mutations_report_only"], 0);
    assert_eq!(policy["bounds"]["max_repairs"], 1);
    assert_eq!(policy["bounds"]["sleep_for_ci"], false);
    assert!(policy["triggers"]["schedule_minutes"].as_u64().unwrap() <= 20);
    assert!(strings(&policy["trusted_instructions"]).contains(&"agentic/cara-responder-help.md"));

    let report_tools = strings(&policy["typed_tools"]["report_only"]);
    assert_eq!(report_tools, ["cara_status", "cara_sync_dry_run"]);
    assert!(!report_tools.contains(&"cara_sync"));
    assert_eq!(
        strings(&policy["typed_tools"]["cutover_only"]),
        ["cara_sync"]
    );
}

#[test]
fn responder_bounds_outputs_credentials_and_untrusted_data() {
    let policy = json(POLICY);
    assert_eq!(policy["runtime"]["mutable_latest_allowed"], false);
    assert_eq!(policy["runtime"]["checksum_required"], true);
    assert_eq!(policy["runtime"]["credentials_visible_to_model"], false);
    assert_eq!(policy["safe_output_limits"]["repair_pull_requests"], 1);
    assert_eq!(policy["safe_output_limits"]["triggering_branch_updates"], 1);
    assert_eq!(policy["safe_output_limits"]["dispatches"], 1);

    for denied in [
        "arbitrary shell with GitHub App credentials",
        "arbitrary GitHub write API calls",
        "direct tag or release mutation",
        "unchanged workflow reruns",
        "Caco board credentials in model context",
        "executing candidate code in the planning model",
    ] {
        assert!(strings(&policy["denied"]).contains(&denied));
    }
    for required in [
        "report-only",
        "single-writer",
        "untrusted",
        "Never reveal or request",
        "sleeps waiting for CI",
        "Never rerun an unchanged workflow",
        "at most one repair PR",
        "Deduplicate by fingerprint",
    ] {
        assert!(
            INSTRUCTIONS.contains(required),
            "instructions must retain safety rule `{required}`"
        );
    }
    for required in [
        "cara status --json",
        "cara plan sync --json",
        "cara sync --all --dry-run --json",
        "status_partial",
        "stale_precondition",
        "indeterminate",
        "published_unjoined",
        "source-red",
        "cancelled or superseded",
        "transport or runner failure",
        "prompt-injection attempt",
    ] {
        assert!(
            HELP.contains(required),
            "typed help must retain `{required}`"
        );
    }
}

#[test]
fn runtime_and_report_schemas_require_immutable_receipts() {
    let runtime = json(RUNTIME_SCHEMA);
    assert_eq!(runtime["additionalProperties"], false);
    let runtime_required = strings(&runtime["required"]);
    for field in [
        "version",
        "asset",
        "url",
        "sha256",
        "source_commit",
        "provenance",
    ] {
        assert!(runtime_required.contains(&field));
    }
    assert_eq!(runtime["properties"]["sha256"]["pattern"], "^[a-f0-9]{64}$");
    assert_eq!(
        runtime["properties"]["source_commit"]["pattern"],
        "^[a-f0-9]{40}$"
    );

    let report = json(REPORT_SCHEMA);
    assert_eq!(report["additionalProperties"], false);
    let report_required = strings(&report["required"]);
    for field in [
        "runtime_pin",
        "input_heads",
        "cara_receipts",
        "repairs",
        "feedback_receipts",
        "mutation_count",
        "idempotency_fingerprint",
        "next_cursor",
    ] {
        assert!(report_required.contains(&field));
    }
    assert_eq!(report["properties"]["repairs"]["maxItems"], 1);
    assert_eq!(report["properties"]["feedback_receipts"]["maxItems"], 1);
    assert_eq!(report["properties"]["mutation_count"]["minimum"], 0);
}
