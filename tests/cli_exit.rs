use std::process::{Command, Output};

fn cara(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cara"))
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
fn json_success_still_exits_zero() {
    let output = cara(&["--json", "help"]);

    assert!(output.status.success());
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("JSON success envelope");
    assert_eq!(envelope["status"], "success");
}
