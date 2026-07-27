use std::process::{Command, Output};

fn cara_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cara"));
    command
        .env(
            "FEEDBACK_WEBHOOK_URL",
            "https://feedback.invalid/hooks/caravan",
        )
        .env("FEEDBACK_WEBHOOK_TOKEN_ENV", "CACOPHONY_FEEDBACK_TOKEN")
        .env_remove("CACOPHONY_FEEDBACK_TOKEN");
    command
}

fn cara(arguments: &[&str]) -> Output {
    cara_command()
        .args(arguments)
        .output()
        .expect("run cara test binary")
}

fn cara_in(directory: &std::path::Path, arguments: &[&str]) -> Output {
    cara_command()
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
fn manual_loop_refuses_json_and_noninteractive_use() {
    let output = cara(&["--json", "loop", "--once", "--manual"]);
    assert!(!output.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["error"]["code"], "manual_loop_json_unsupported");
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
    assert!(output.stderr.is_empty());
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("JSON feedback status envelope");
    for field in ["enabled", "strategy", "destination", "component", "project"] {
        assert!(
            envelope["data"].get(field).is_some(),
            "feedback status omitted {field}"
        );
    }
    assert_eq!(envelope["data"]["enabled"], false);
    assert_eq!(envelope["data"]["destination"], "disabled");
    assert_eq!(
        envelope["data"]["configuration_error"]["code"],
        "feedback_config_invalid"
    );
}

#[test]
fn human_feedback_misconfiguration_remains_visible_on_stderr() {
    let output = cara(&["feedback", "status"]);

    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.matches("feedback_config_invalid").count(), 1);
    assert!(stderr.contains("CACOPHONY_FEEDBACK_TOKEN"));
}

#[test]
fn json_feedback_report_returns_typed_configuration_error_without_stderr() {
    let output = cara(&[
        "--json",
        "feedback",
        "report",
        "--kind",
        "error",
        "--summary",
        "fixture",
    ]);

    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("JSON feedback error envelope");
    assert_eq!(envelope["error"]["code"], "feedback_config_invalid");
}

#[test]
fn json_success_still_exits_zero() {
    let output = cara(&["--json", "help"]);

    assert!(output.status.success());
    let envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("JSON success envelope");
    assert_eq!(envelope["status"], "success");
}

/// Copy a binary and make sure every writable handle is flushed and closed.
///
/// Linux refuses to exec a file that is still open for writing (`ETXTBSY`), and
/// a plain `fs::copy` can leave that window open just long enough for the
/// following exec to fail. Writing explicitly, syncing, and dropping the handle
/// closes it deterministically.
fn copy_executable(source: &std::path::Path, destination: &std::path::Path) {
    use std::io::Write;
    let bytes = std::fs::read(source).expect("read built cara binary");
    {
        let mut file = std::fs::File::create(destination).expect("create installed cara");
        file.write_all(&bytes).expect("write installed cara");
        file.sync_all().expect("flush installed cara");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o755))
            .expect("mark installed cara executable");
    }
}

/// Exec the copied binary, tolerating a brief `ETXTBSY` window.
///
/// Under a loaded Nix build the kernel can still consider the freshly written
/// file busy for a moment. That is an environment race, not a Cara defect, so
/// the fixture retries briefly rather than failing the whole release gate.
fn run_installed(
    installed: &std::path::Path,
    home: &std::path::Path,
    path: &std::path::Path,
) -> std::process::Output {
    let mut last = None;
    for attempt in 0..50 {
        match Command::new(installed)
            .env("HOME", home)
            .env("PATH", path)
            .args(["--json", "self-update", "status"])
            .output()
        {
            Ok(output) => return output,
            Err(error) if error.raw_os_error() == Some(26) => {
                std::thread::sleep(std::time::Duration::from_millis(20 * (attempt + 1)));
                last = Some(error);
            }
            Err(error) => panic!("run copied installed cara: {error:?}"),
        }
    }
    panic!(
        "copied cara stayed busy: {:?}",
        last.expect("at least one busy error")
    );
}

#[test]
fn self_update_status_targets_the_exact_path_visible_cargo_install() {
    let temporary = tempfile::tempdir().unwrap();
    let install_dir = temporary.path().join(".cargo/bin");
    std::fs::create_dir_all(&install_dir).unwrap();
    let installed = install_dir.join(if cfg!(windows) { "cara.exe" } else { "cara" });
    copy_executable(std::path::Path::new(env!("CARGO_BIN_EXE_cara")), &installed);
    let output = run_installed(&installed, temporary.path(), &install_dir);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        envelope["data"]["installed_path"],
        installed.canonicalize().unwrap().display().to_string()
    );
}

#[test]
fn self_update_refuses_the_cargo_target_development_binary() {
    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_cara"));
    let output = cara_command()
        .env("PATH", binary.parent().unwrap())
        .args(["--json", "self-update", "status"])
        .output()
        .expect("run development cara");
    assert!(!output.status.success());
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["error"]["code"], "self_update_development_binary");
}
