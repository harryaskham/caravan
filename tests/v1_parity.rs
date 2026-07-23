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
        ("next_candidate", &["next-candidate"]),
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
    let calls = [
        ("help", serde_json::json!({})),
        ("init", serde_json::json!({})),
        ("log", serde_json::json!({})),
        ("status", serde_json::json!({})),
        ("next_candidate", serde_json::json!({})),
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
        "next_candidate",
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
