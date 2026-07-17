use std::process::{Command, Output};

fn cara(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cara"))
        .args(arguments)
        .output()
        .expect("run cara test binary")
}

fn cara_in(directory: &std::path::Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cara"))
        .current_dir(directory)
        .args(arguments)
        .output()
        .expect("run cara test binary")
}

#[test]
fn json_domain_error_keeps_envelope_and_exits_nonzero() {
    let output = cara(&["--json", "evict", "--reason", ""]);

    assert!(!output.status.success());
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("JSON error envelope");
    assert_eq!(envelope["status"], "error");
    assert_eq!(envelope["error"]["code"], "eviction_reason_required");
}

#[test]
fn human_domain_error_exits_nonzero() {
    let output = cara(&["evict", "--reason", ""]);

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("eviction_reason_required"),
        "stderr should retain the structured error code"
    );
}

#[test]
fn json_config_error_keeps_the_machine_envelope() {
    let temp = tempfile::tempdir().expect("temp directory");
    let config = temp.path().join("invalid.yaml");
    std::fs::write(&config, "unknown: true\n").expect("write invalid config");

    let output = cara_in(
        temp.path(),
        &[
            "--json",
            "--config",
            config.to_str().expect("UTF-8 path"),
            "status",
        ],
    );

    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("JSON config error envelope");
    assert_eq!(envelope["status"], "error");
    assert_eq!(envelope["error"]["category"], "config_error");
    assert_eq!(envelope["error"]["code"], "config_parse_failed");
}

#[test]
fn feedback_status_uses_the_typed_mcp_shape() {
    let output = cara(&["--json", "feedback", "status"]);

    assert!(output.status.success());
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("JSON feedback status envelope");
    for field in ["enabled", "strategy", "destination", "component", "project"] {
        assert!(
            envelope["data"].get(field).is_some(),
            "feedback status omitted {field}"
        );
    }
}

#[test]
fn json_success_still_exits_zero() {
    let output = cara(&["--json", "help"]);

    assert!(output.status.success());
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("JSON success envelope");
    assert_eq!(envelope["status"], "success");
}
