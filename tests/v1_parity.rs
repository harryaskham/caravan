use std::process::{Command, Output};

fn cara_in(directory: &std::path::Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cara"))
        .current_dir(directory)
        .env_remove("GH_REPO")
        .args(arguments)
        .output()
        .expect("run cara test binary")
}

#[test]
#[allow(clippy::too_many_lines)]
fn every_bounded_v1_domain_command_has_a_real_json_operation() {
    let temp = tempfile::tempdir().expect("temp directory");
    let commands: &[(&str, &[&str])] = &[
        ("init", &["init"]),
        ("log", &["log"]),
        ("status", &["status"]),
        ("queue", &["queue"]),
        ("check", &["check"]),
        ("new", &["new"]),
        ("renew", &["renew"]),
        ("join", &["join", "--tail-pr", "1"]),
        ("rejoin", &["rejoin", "--head-pr", "1"]),
        (
            "priority_set",
            &[
                "priority",
                "set",
                "--pr",
                "1",
                "--label",
                "caravan-priority:high",
                "--actor",
                "parity",
                "--reason",
                "parity priority",
            ],
        ),
        (
            "priority_clear",
            &[
                "priority",
                "clear",
                "--pr",
                "1",
                "--actor",
                "parity",
                "--reason",
                "parity FIFO",
            ],
        ),
        ("show", &["show"]),
        (
            "force_arm",
            &[
                "force",
                "--pr",
                "1",
                "--actor",
                "parity",
                "--reason",
                "parity force",
            ],
        ),
        (
            "force_revoke",
            &[
                "force",
                "revoke",
                "--pr",
                "1",
                "--actor",
                "parity",
                "--reason",
                "parity revoke",
            ],
        ),
        (
            "force_intent_preview",
            &[
                "force-intent",
                "preview",
                "--pr",
                "1",
                "--head",
                "0000000000000000000000000000000000000000",
                "--membership-generation",
                "fnv1a64:1111111111111111",
                "--failure-fingerprint",
                "fnv1a64:2222222222222222",
                "--reason",
                "parity reviewed force",
                "--expires-at-ms",
                "9999999999999",
                "--auto-merge",
                "squash",
            ],
        ),
        (
            "force_intent_apply",
            &[
                "force-intent",
                "apply",
                "--pr",
                "1",
                "--head",
                "0000000000000000000000000000000000000000",
                "--membership-generation",
                "fnv1a64:1111111111111111",
                "--failure-fingerprint",
                "fnv1a64:2222222222222222",
                "--reason",
                "parity reviewed force",
                "--expires-at-ms",
                "9999999999999",
                "--auto-merge",
                "squash",
            ],
        ),
        (
            "force_intent_revoke",
            &[
                "force-intent",
                "revoke",
                "--pr",
                "1",
                "--head",
                "0000000000000000000000000000000000000000",
                "--membership-generation",
                "fnv1a64:1111111111111111",
                "--failure-fingerprint",
                "fnv1a64:2222222222222222",
                "--reason",
                "parity reviewed force",
                "--expires-at-ms",
                "9999999999999",
                "--auto-merge",
                "squash",
            ],
        ),
        (
            "pause_recovery_prepare",
            &[
                "pause-recovery",
                "prepare",
                "--operation-id",
                "owned-pr-retarget-head",
                "--external-reference",
                "INC-1",
                "--idempotency-key",
                "idem-1",
                "--actor",
                "oncall",
                "--owner-project",
                "cacophony",
                "--owner-agent",
                "agent-1",
                "--ownership-generation",
                "generation-1",
                "--repository-slug",
                "o/r",
                "--caravan-id",
                "1",
                "--members",
                "1,2",
                "--pause-id",
                "pause-1",
                "--pause-generation",
                "pause-generation-1",
                "--target-pr",
                "1",
                "--expected-base-ref",
                "main",
                "--expected-base-oid",
                "0000000000000000000000000000000000000000",
                "--expected-head-oid",
                "1111111111111111111111111111111111111111",
                "--desired-base-ref",
                "main",
                "--desired-base-oid",
                "2222222222222222222222222222222222222222",
                "--desired-head-oid",
                "3333333333333333333333333333333333333333",
                "--desired-head-tree",
                "4444444444444444444444444444444444444444",
                "--reason",
                "parity recovery",
            ],
        ),
        (
            "pause_recovery_checkpoint_base",
            &[
                "pause-recovery",
                "checkpoint-base",
                "--operation-id",
                "owned-pr-retarget-head",
                "--external-reference",
                "INC-1",
                "--idempotency-key",
                "idem-1",
                "--actor",
                "oncall",
                "--owner-project",
                "cacophony",
                "--owner-agent",
                "agent-1",
                "--ownership-generation",
                "generation-1",
                "--repository-slug",
                "o/r",
                "--caravan-id",
                "1",
                "--members",
                "1,2",
                "--pause-id",
                "pause-1",
                "--pause-generation",
                "pause-generation-1",
                "--target-pr",
                "1",
                "--expected-base-ref",
                "main",
                "--expected-base-oid",
                "0000000000000000000000000000000000000000",
                "--expected-head-oid",
                "1111111111111111111111111111111111111111",
                "--desired-base-ref",
                "main",
                "--desired-base-oid",
                "2222222222222222222222222222222222222222",
                "--desired-head-oid",
                "3333333333333333333333333333333333333333",
                "--desired-head-tree",
                "4444444444444444444444444444444444444444",
                "--reason",
                "parity recovery",
            ],
        ),
        (
            "pause_recovery_checkpoint_head",
            &[
                "pause-recovery",
                "checkpoint-head",
                "--operation-id",
                "owned-pr-retarget-head",
                "--external-reference",
                "INC-1",
                "--idempotency-key",
                "idem-1",
                "--actor",
                "oncall",
                "--owner-project",
                "cacophony",
                "--owner-agent",
                "agent-1",
                "--ownership-generation",
                "generation-1",
                "--repository-slug",
                "o/r",
                "--caravan-id",
                "1",
                "--members",
                "1,2",
                "--pause-id",
                "pause-1",
                "--pause-generation",
                "pause-generation-1",
                "--target-pr",
                "1",
                "--expected-base-ref",
                "main",
                "--expected-base-oid",
                "0000000000000000000000000000000000000000",
                "--expected-head-oid",
                "1111111111111111111111111111111111111111",
                "--desired-base-ref",
                "main",
                "--desired-base-oid",
                "2222222222222222222222222222222222222222",
                "--desired-head-oid",
                "3333333333333333333333333333333333333333",
                "--desired-head-tree",
                "4444444444444444444444444444444444444444",
                "--reason",
                "parity recovery",
            ],
        ),
        (
            "pause_recovery_finalize",
            &[
                "pause-recovery",
                "finalize",
                "--operation-id",
                "owned-pr-retarget-head",
                "--external-reference",
                "INC-1",
                "--idempotency-key",
                "idem-1",
                "--actor",
                "oncall",
                "--owner-project",
                "cacophony",
                "--owner-agent",
                "agent-1",
                "--ownership-generation",
                "generation-1",
                "--repository-slug",
                "o/r",
                "--caravan-id",
                "1",
                "--members",
                "1,2",
                "--pause-id",
                "pause-1",
                "--pause-generation",
                "pause-generation-1",
                "--target-pr",
                "1",
                "--expected-base-ref",
                "main",
                "--expected-base-oid",
                "0000000000000000000000000000000000000000",
                "--expected-head-oid",
                "1111111111111111111111111111111111111111",
                "--desired-base-ref",
                "main",
                "--desired-base-oid",
                "2222222222222222222222222222222222222222",
                "--desired-head-oid",
                "3333333333333333333333333333333333333333",
                "--desired-head-tree",
                "4444444444444444444444444444444444444444",
                "--virtual-merge-parents",
                "2222222222222222222222222222222222222222,3333333333333333333333333333333333333333",
                "--virtual-merge-tree",
                "5555555555555555555555555555555555555555",
                "--check-attribution",
                "{\"head_oid\":\"3333333333333333333333333333333333333333\",\"check_run_count\":1,\"status_context_count\":0}",
                "--reason",
                "parity recovery",
            ],
        ),
        (
            "pause_recovery_rollback",
            &[
                "pause-recovery",
                "rollback",
                "--operation-id",
                "owned-pr-retarget-head",
                "--external-reference",
                "INC-1",
                "--idempotency-key",
                "idem-1",
                "--actor",
                "oncall",
                "--owner-project",
                "cacophony",
                "--owner-agent",
                "agent-1",
                "--ownership-generation",
                "generation-1",
                "--repository-slug",
                "o/r",
                "--caravan-id",
                "1",
                "--members",
                "1,2",
                "--pause-id",
                "pause-1",
                "--pause-generation",
                "pause-generation-1",
                "--target-pr",
                "1",
                "--expected-base-ref",
                "main",
                "--expected-base-oid",
                "0000000000000000000000000000000000000000",
                "--expected-head-oid",
                "1111111111111111111111111111111111111111",
                "--desired-base-ref",
                "main",
                "--desired-base-oid",
                "2222222222222222222222222222222222222222",
                "--desired-head-oid",
                "3333333333333333333333333333333333333333",
                "--desired-head-tree",
                "4444444444444444444444444444444444444444",
                "--reason",
                "parity recovery",
            ],
        ),
        ("next", &["next"]),
        ("prev", &["prev"]),
        ("plan_sync", &["plan", "sync", "--all", "--rerun-failed"]),
        ("sync", &["sync", "--all", "--rerun-failed"]),
        ("repair_start", &["repair", "start", "--pr", "1"]),
        (
            "repair_status",
            &["repair", "status", "--session", "pr-1-parity"],
        ),
        (
            "repair_authorize_agent_edits",
            &[
                "repair",
                "authorize-agent-edits",
                "--session",
                "pr-1-parity",
                "--actor",
                "parity",
                "--reason",
                "parity agent repair",
            ],
        ),
        (
            "repair_grant",
            &[
                "repair",
                "grant",
                "--session",
                "pr-1-parity",
                "--path",
                "README.md",
                "--source-revision",
                "0000000000000000000000000000000000000000",
                "--actor",
                "parity",
                "--reason",
                "parity audit",
            ],
        ),
        (
            "repair_revoke_grant",
            &[
                "repair",
                "revoke-grant",
                "--session",
                "pr-1-parity",
                "--path",
                "README.md",
                "--actor",
                "parity",
                "--reason",
                "parity revocation",
            ],
        ),
        (
            "repair_continue",
            &["repair", "continue", "--session", "pr-1-parity"],
        ),
        (
            "repair_abort",
            &["repair", "abort", "--session", "pr-1-parity", "--confirm"],
        ),
        ("evict", &["evict", "--pr", "1", "--reason", "parity audit"]),
        ("split", &["split", "--pr", "1"]),
        ("loop_once", &["loop", "--once"]),
        ("van_list", &["van", "list"]),
        ("van_next", &["van", "next"]),
        ("van_prev", &["van", "prev"]),
        ("lock_status", &["lock", "status"]),
        (
            "lock_recover",
            &[
                "lock",
                "recover",
                "--token",
                "parity-audit",
                "--stale-after-secs",
                "1",
                "--confirm",
            ],
        ),
    ];

    for (name, arguments) in commands {
        let mut invocation = vec!["--json"];
        invocation.extend_from_slice(arguments);
        let output = cara_in(temp.path(), &invocation);
        let envelope: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("{name} did not return a JSON envelope: {error}"));
        assert!(
            matches!(envelope["status"].as_str(), Some("success" | "error")),
            "{name} returned an invalid envelope: {envelope}"
        );
        assert_ne!(
            envelope["error"]["code"], "not_implemented",
            "{name} still routes to a scaffold"
        );
        assert_eq!(
            envelope["meta"]["schema_version"], 1,
            "{name} omitted the stable envelope version"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn every_bounded_mcp_domain_tool_routes_to_the_real_operation() {
    let temp = tempfile::tempdir().expect("temp directory");
    let context = caravan::AppContext {
        repository_path: temp.path().to_path_buf(),
        config_path: temp.path().join("config.yaml"),
        config_existed: false,
        config: caravan::config::CaravanConfig::default(),
    };
    let router = caravan::build_router();
    let recovery_input = serde_json::json!({
        "schema_version": 1,
        "operation_id": "owned-pr-retarget-head",
        "external_reference": "INC-1",
        "idempotency_key": "idem-1",
        "actor": "oncall",
        "owner_project": "cacophony",
        "owner_agent": "agent-1",
        "ownership_generation": "generation-1",
        "repository": "o/r",
        "caravan_id": 1,
        "members": [1, 2],
        "pause_id": "pause-1",
        "pause_generation": "pause-generation-1",
        "target_pr": 1,
        "expected_base_ref": "main",
        "expected_base_oid": "0000000000000000000000000000000000000000",
        "expected_head_oid": "1111111111111111111111111111111111111111",
        "desired_base_ref": "main",
        "desired_base_oid": "2222222222222222222222222222222222222222",
        "desired_head_oid": "3333333333333333333333333333333333333333",
        "desired_head_tree": "4444444444444444444444444444444444444444",
        "reason": "parity recovery"
    });
    let mut recovery_finalize = recovery_input.clone();
    recovery_finalize["virtual_merge_parents"] = serde_json::json!([
        "2222222222222222222222222222222222222222",
        "3333333333333333333333333333333333333333"
    ]);
    recovery_finalize["virtual_merge_tree"] =
        serde_json::json!("5555555555555555555555555555555555555555");
    recovery_finalize["check_attribution"] = serde_json::json!({
        "head_oid": "3333333333333333333333333333333333333333",
        "check_run_count": 1,
        "status_context_count": 0
    });
    let calls = [
        ("help", serde_json::json!({})),
        ("init", serde_json::json!({})),
        ("log", serde_json::json!({})),
        ("status", serde_json::json!({})),
        ("queue", serde_json::json!({})),
        ("check", serde_json::json!({})),
        ("new", serde_json::json!({})),
        ("renew", serde_json::json!({})),
        ("join", serde_json::json!({ "tail_pr": 1 })),
        ("rejoin", serde_json::json!({ "head_pr": 1 })),
        (
            "priority_set",
            serde_json::json!({
                "pr": 1,
                "label": "caravan-priority:high",
                "actor": "parity",
                "reason": "parity priority"
            }),
        ),
        (
            "priority_clear",
            serde_json::json!({
                "pr": 1,
                "actor": "parity",
                "reason": "parity FIFO"
            }),
        ),
        ("show", serde_json::json!({})),
        (
            "force_arm",
            serde_json::json!({"pr": 1, "actor": "parity", "reason": "parity force"}),
        ),
        (
            "force_revoke",
            serde_json::json!({"pr": 1, "actor": "parity", "reason": "parity revoke"}),
        ),
        (
            "force_intent_preview",
            serde_json::json!({"pr": 1, "head": "0000000000000000000000000000000000000000", "membership_generation": "fnv1a64:1111111111111111", "failure_fingerprint": "fnv1a64:2222222222222222", "reason": "parity reviewed force", "expires_at_ms": 9_999_999_999_999_u64, "auto_merge": "squash"}),
        ),
        (
            "force_intent_apply",
            serde_json::json!({"pr": 1, "head": "0000000000000000000000000000000000000000", "membership_generation": "fnv1a64:1111111111111111", "failure_fingerprint": "fnv1a64:2222222222222222", "reason": "parity reviewed force", "expires_at_ms": 9_999_999_999_999_u64, "auto_merge": "squash"}),
        ),
        (
            "force_intent_revoke",
            serde_json::json!({"pr": 1, "head": "0000000000000000000000000000000000000000", "membership_generation": "fnv1a64:1111111111111111", "failure_fingerprint": "fnv1a64:2222222222222222", "reason": "parity reviewed force", "expires_at_ms": 9_999_999_999_999_u64, "auto_merge": "squash"}),
        ),
        ("pause_recovery_prepare", recovery_input.clone()),
        ("pause_recovery_checkpoint_base", recovery_input.clone()),
        ("pause_recovery_checkpoint_head", recovery_input.clone()),
        ("pause_recovery_finalize", recovery_finalize),
        ("pause_recovery_rollback", recovery_input),
        ("next", serde_json::json!({})),
        ("prev", serde_json::json!({})),
        (
            "plan_sync",
            serde_json::json!({ "all": true, "rerun_failed": true }),
        ),
        (
            "sync",
            serde_json::json!({ "all": true, "rerun_failed": true }),
        ),
        ("repair_start", serde_json::json!({ "pr": 1 })),
        (
            "repair_status",
            serde_json::json!({ "session": "pr-1-parity" }),
        ),
        (
            "repair_authorize_agent_edits",
            serde_json::json!({
                "session": "pr-1-parity",
                "actor": "parity",
                "reason": "parity agent repair",
                "expires_secs": 3600
            }),
        ),
        (
            "repair_grant",
            serde_json::json!({
                "session": "pr-1-parity",
                "paths": ["README.md"],
                "source_revision": "0000000000000000000000000000000000000000",
                "actor": "parity",
                "reason": "parity audit",
                "expires_secs": 3600
            }),
        ),
        (
            "repair_revoke_grant",
            serde_json::json!({
                "session": "pr-1-parity",
                "paths": ["README.md"],
                "actor": "parity",
                "reason": "parity revocation"
            }),
        ),
        (
            "repair_continue",
            serde_json::json!({ "session": "pr-1-parity" }),
        ),
        (
            "repair_abort",
            serde_json::json!({ "session": "pr-1-parity", "confirm": true }),
        ),
        (
            "evict",
            serde_json::json!({ "pr": 1, "reason": "parity audit" }),
        ),
        ("split", serde_json::json!({ "pr": 1 })),
        ("van_list", serde_json::json!({})),
        ("van_next", serde_json::json!({})),
        ("van_prev", serde_json::json!({})),
        ("lock_status", serde_json::json!({})),
        (
            "lock_recover",
            serde_json::json!({
                "token": "parity-audit",
                "stale_after_secs": 1,
                "confirm": true
            }),
        ),
    ];

    for (name, input) in calls {
        let envelope = router.call_tool(&context, name, input);
        let value = serde_json::to_value(envelope).expect("MCP envelope serializes");
        assert!(
            matches!(value["status"].as_str(), Some("success" | "error")),
            "{name} returned an invalid MCP envelope: {value}"
        );
        assert_ne!(
            value["error"]["code"], "not_implemented",
            "{name} still routes to a scaffold"
        );
        assert_eq!(value["meta"]["schema_version"], 1);
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn pause_recovery_metadata_pins_cacophony_v1_names_and_flat_data_contract() {
    let temp = tempfile::tempdir().expect("temp directory");
    let output = cara_in(temp.path(), &["mcp", "tools"]);
    assert!(output.status.success());
    let tools: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).expect("MCP metadata JSON");
    let expected_tools = [
        "pause_recovery_prepare",
        "pause_recovery_checkpoint_base",
        "pause_recovery_checkpoint_head",
        "pause_recovery_finalize",
        "pause_recovery_rollback",
    ];
    let input_fields = [
        "schema_version",
        "operation_id",
        "external_reference",
        "idempotency_key",
        "actor",
        "owner_project",
        "owner_agent",
        "ownership_generation",
        "repository",
        "caravan_id",
        "members",
        "pause_id",
        "pause_generation",
        "target_pr",
        "expected_base_ref",
        "expected_base_oid",
        "expected_head_oid",
        "desired_base_ref",
        "desired_base_oid",
        "desired_head_oid",
        "desired_head_tree",
        "virtual_merge_parents",
        "virtual_merge_tree",
        "check_attribution",
        "reason",
    ];
    let output_fields = [
        "schema_version",
        "phase",
        "status",
        "provider_mutated",
        "operation_changed",
        "receipt_id",
        "next_action",
        "operation_id",
        "external_reference",
        "idempotency_key",
        "actor",
        "owner_project",
        "owner_agent",
        "ownership_generation",
        "repository",
        "caravan_id",
        "members",
        "pause_id",
        "pause_generation",
        "target_pr",
        "expected_base_ref",
        "expected_base_oid",
        "expected_head_oid",
        "desired_base_ref",
        "desired_base_oid",
        "desired_head_oid",
        "desired_head_tree",
        "fence_state",
        "rollback_state",
        "virtual_merge_parents",
        "virtual_merge_tree",
        "check_attribution",
    ];

    for name in expected_tools {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("missing exact MCP tool {name}"));
        let input = tool["inputSchema"]["properties"]
            .as_object()
            .expect("recovery input properties");
        for field in input_fields {
            assert!(input.contains_key(field), "{name} missing input {field}");
        }
        assert!(
            !input.contains_key("phase"),
            "tool name owns phase selection"
        );
        assert!(!input.contains_key("final_virtual_merge_tree"));
        let output = tool["outputSchema"]["$defs"]["PauseRecoveryOutput"]["properties"]
            .as_object()
            .expect("flat recovery data properties");
        for field in output_fields {
            assert!(output.contains_key(field), "{name} missing output {field}");
        }
        assert!(output.contains_key("fence_state"));
        assert!(
            !output.contains_key("fence"),
            "flat fence_state is canonical"
        );
        assert!(
            !output.contains_key("rollback"),
            "flat rollback_state is canonical"
        );
    }

    let attribution = tools
        .iter()
        .find(|tool| tool["name"] == "pause_recovery_finalize")
        .unwrap()["inputSchema"]["$defs"]["PauseRecoveryCheckAttribution"]
        .clone();
    assert_eq!(
        attribution["required"],
        serde_json::json!(["head_oid", "check_run_count", "status_context_count"])
    );
}

#[test]
fn mcp_registry_covers_all_bounded_v1_operations_with_schemas() {
    let temp = tempfile::tempdir().expect("temp directory");
    let output = cara_in(temp.path(), &["mcp", "tools"]);
    assert!(output.status.success());
    let tools: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).expect("MCP metadata JSON");
    let names: std::collections::BTreeSet<_> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();

    for expected in [
        "help",
        "init",
        "log",
        "status",
        "queue",
        "check",
        "new",
        "renew",
        "join",
        "rejoin",
        "priority_set",
        "priority_clear",
        "show",
        "force_arm",
        "force_revoke",
        "force_intent_preview",
        "force_intent_apply",
        "force_intent_revoke",
        "pause_recovery_prepare",
        "pause_recovery_checkpoint_base",
        "pause_recovery_checkpoint_head",
        "pause_recovery_finalize",
        "pause_recovery_rollback",
        "next",
        "prev",
        "plan_sync",
        "sync",
        "repair_start",
        "repair_authorize_agent_edits",
        "repair_grant",
        "repair_revoke_grant",
        "repair_status",
        "repair_continue",
        "repair_abort",
        "evict",
        "split",
        "van_list",
        "van_next",
        "van_prev",
        "lock_status",
        "lock_recover",
        "self_update_status",
        "self_update_check",
        "self_update_run",
        "feedback_report",
        "feedback_status",
    ] {
        assert!(names.contains(expected), "missing MCP tool {expected}");
    }
    assert!(
        !names.contains("loop"),
        "unbounded loop must stay out of MCP"
    );
    assert!(
        !names.contains("log_follow"),
        "unbounded log follow must stay out of MCP"
    );

    for tool in &tools {
        let name = tool["name"].as_str().expect("tool name");
        assert!(
            tool["description"]
                .as_str()
                .is_some_and(|value| !value.trim().is_empty()),
            "{name} has no description"
        );
        assert!(
            tool["inputSchema"].is_object(),
            "{name} has no input schema"
        );
        assert!(
            tool["outputSchema"].is_object(),
            "{name} has no output schema"
        );
        assert!(
            !tool["description"]
                .as_str()
                .unwrap_or_default()
                .contains("not implemented"),
            "{name} advertises a scaffold"
        );
    }
}
