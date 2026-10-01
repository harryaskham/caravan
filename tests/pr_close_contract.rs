use caravan::pr_close::PrCloseInput;
use serde_json::{Value, json};

#[test]
fn close_tools_publish_the_same_typed_bounded_contract() {
    let metadata = serde_json::to_value(caravan::build_router().tool_metadata()).unwrap();
    let apply = metadata
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "pr_close_apply")
        .unwrap();
    let encoded = apply.to_string();
    for field in [
        "operation_key",
        "custody_reference",
        "represented_at",
        "confirmed",
        "atomic_provider_transaction",
        "intent_fence_fingerprint",
        "indeterminate",
        "reconciled_closed",
    ] {
        assert!(encoded.contains(field), "missing {field}");
    }
    assert!(
        metadata
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "pr_close_status")
    );
    let mut input = request_json();
    input["force"] = json!(true);
    assert!(serde_json::from_value::<PrCloseInput>(input).is_err());
}

#[test]
fn close_contract_inventory_is_backed_by_tests_and_states_fence_limits() {
    let inventory: Value = serde_json::from_str(include_str!(
        "../docs/exact-nonmember-close-acceptance.json"
    ))
    .unwrap();
    let sources = [
        include_str!("../src/pr_close/tests.rs"),
        include_str!("../src/pr_close/tests/storage.rs"),
        include_str!("../src/github/pr_close/tests.rs"),
        include_str!("pr_close_contract.rs"),
    ]
    .join("\n");
    let mut ids = std::collections::BTreeSet::new();
    for requirement in inventory["requirements"].as_array().unwrap() {
        assert!(ids.insert(requirement["id"].as_str().unwrap()));
        for test in requirement["tests"].as_array().unwrap() {
            assert!(
                sources.contains(&format!("fn {}(", test.as_str().unwrap())),
                "missing {test}"
            );
        }
    }
    let contract = include_str!("../docs/exact-nonmember-close.md");
    for boundary in [
        "not credentials or grants",
        "not a cross-host exactly-once",
        "does not freeze arbitrary external",
        "same key never sends another",
        "not a fresh provider observation",
    ] {
        assert!(
            contract
                .replace("**", "")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains(boundary),
            "missing {boundary}"
        );
    }
}

fn request_json() -> Value {
    json!({
        "repository":"acme/widgets", "pr":8, "head_ref":"feature", "head":"a".repeat(40),
        "base_ref":"main", "base":"b".repeat(40), "main_ref":"main", "main":"b".repeat(40),
        "represented_at":"c".repeat(40), "representation":"same_tree", "actor":"owner",
        "custody_reference":"owner:8:g1", "reason":"represented on main", "operation_key":"close-8-g1", "confirmed":true
    })
}

#[cfg(unix)]
mod process {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::{Command, Output};

    struct Fixture {
        root: tempfile::TempDir,
        repo: PathBuf,
        path: std::ffi::OsString,
    }

    impl Fixture {
        fn new(mode: &str) -> Self {
            let root = tempfile::tempdir().unwrap();
            let repo = root.path().join("repo");
            let bin = root.path().join("bin");
            std::fs::create_dir_all(repo.join(".caravan")).unwrap();
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::create_dir_all(root.path().join("home")).unwrap();
            let output = Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&repo)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(output.status.success());
            std::fs::write(
                repo.join(".caravan/config.yaml"),
                format!("version: 1\nrepository: acme/widgets\nsync:\n  checkout_on_decision: false\nwriter:\n  mode: {mode}\n"),
            )
            .unwrap();
            std::fs::write(bin.join("gh"), include_str!("pr_close/gh.py")).unwrap();
            std::fs::set_permissions(bin.join("gh"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            let search = std::env::var_os("PATH").unwrap();
            let path =
                std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&search)))
                    .unwrap();
            Self { root, repo, path }
        }

        fn command(&self) -> Command {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_cara"));
            cmd.current_dir(&self.repo)
                .env_clear()
                .env("PATH", &self.path)
                .env("HOME", self.root.path().join("home"))
                .env("XDG_CONFIG_HOME", self.root.path().join("home"))
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("CLOSE_FIXTURE_ROOT", self.root.path());
            cmd
        }

        fn apply(&self, lost: bool, custody: &str) -> (Output, Value) {
            let input: PrCloseInput = serde_json::from_value(request_json()).unwrap();
            let output = self
                .command()
                .args([
                    "--json",
                    "pr-close",
                    "apply",
                    "--expected-repository",
                    &input.repository,
                    "--pr",
                    "8",
                    "--head-ref",
                    &input.head_ref,
                    "--head",
                    &input.head,
                    "--base-ref",
                    &input.base_ref,
                    "--base",
                    &input.base,
                    "--main-ref",
                    &input.main_ref,
                    "--main",
                    &input.main,
                    "--represented-at",
                    &input.represented_at,
                    "--representation",
                    "same-tree",
                    "--actor",
                    &input.actor,
                    "--custody-reference",
                    custody,
                    "--reason",
                    &input.reason,
                    "--operation-key",
                    &input.operation_key,
                    "--confirmed",
                ])
                .env("CLOSE_FIXTURE_LOST", if lost { "1" } else { "0" })
                .output()
                .unwrap();
            let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
                panic!(
                    "stdout={} stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )
            });
            (output, value)
        }

        fn calls(&self) -> Vec<Vec<String>> {
            std::fs::read_to_string(self.root.path().join("calls.jsonl"))
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    #[test]
    fn actual_cli_closes_once_and_local_status_matches_mcp() {
        let fixture = Fixture::new("local_only");
        let (output, first) = fixture.apply(false, "owner:8:g1");
        assert!(output.status.success(), "{first}");
        assert_eq!(first["data"]["outcome"], "closed");
        let (output, replay) = fixture.apply(false, "owner:8:g1");
        assert!(output.status.success(), "{replay}");
        assert_eq!(replay["data"]["outcome"], "reconciled_closed");
        assert_eq!(replay["data"]["provider_mutated"], false);
        assert_eq!(
            fixture
                .calls()
                .iter()
                .filter(|args| args.iter().any(|arg| arg == "PATCH"))
                .count(),
            1
        );
        let before = fixture.calls();
        let result = fixture
            .command()
            .args([
                "--json",
                "pr-close",
                "status",
                "--operation-key",
                "close-8-g1",
            ])
            .output()
            .unwrap();
        assert!(result.status.success());
        let cli: Value = serde_json::from_slice(&result.stdout).unwrap();
        let context =
            caravan::AppContext::load_for_receipt_read_from_directory(&fixture.repo, None).unwrap();
        let mcp = serde_json::to_value(caravan::build_router().call_tool(
            &context,
            "pr_close_status",
            json!({"operation_key":"close-8-g1"}),
        ))
        .unwrap();
        assert_eq!(cli["data"], mcp["data"]);
        assert_eq!(
            fixture.calls(),
            before,
            "local status must not call the provider"
        );
        let (output, changed) = fixture.apply(false, "another-generation");
        assert!(!output.status.success());
        assert_eq!(changed["error"]["code"], "pr_close_retry_identity_changed");
        assert_eq!(fixture.calls(), before);
    }

    #[test]
    fn process_restart_reconciles_a_lost_close_response_without_replay() {
        let fixture = Fixture::new("local_only");
        let (output, first) = fixture.apply(true, "owner:8:g1");
        assert!(!output.status.success(), "{first}");
        assert_eq!(first["error"]["code"], "pr_close_indeterminate");
        let (output, replay) = fixture.apply(false, "owner:8:g1");
        assert!(output.status.success(), "{replay}");
        assert_eq!(replay["data"]["outcome"], "reconciled_closed");
        assert_eq!(
            fixture
                .calls()
                .iter()
                .filter(|args| args.iter().any(|arg| arg == "PATCH"))
                .count(),
            1
        );
    }

    #[test]
    fn open_native_membership_refuses_but_closed_stack_history_is_not_membership() {
        for mode in ["open", "closed"] {
            let fixture = Fixture::new("local_only");
            std::fs::write(fixture.root.path().join("native-mode"), mode).unwrap();
            let (output, value) = fixture.apply(false, "owner:8:g1");
            if mode == "open" {
                assert!(!output.status.success(), "{value}");
                assert_eq!(
                    value["error"]["details"]["code"],
                    "pr_close_native_member_refused"
                );
                assert!(!fixture.root.path().join("closed").exists());
            } else {
                assert!(output.status.success(), "{value}");
                assert_eq!(value["data"]["outcome"], "closed");
            }
        }
    }

    #[test]
    fn writer_policy_refuses_before_provider_or_intent() {
        for (mode, code) in [
            ("read_only", "writer_read_only"),
            ("remote_fenced", "invalid_config"),
        ] {
            let fixture = Fixture::new(mode);
            let (output, value) = fixture.apply(false, "owner:8:g1");
            assert!(!output.status.success(), "{value}");
            assert_eq!(value["error"]["code"], code);
            if mode == "remote_fenced" {
                assert!(
                    value["error"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("CARA_REMOTE_LEASE_COMMAND")
                );
            }
            assert!(fixture.calls().is_empty());
            assert!(!fixture.root.path().join("closed").exists());
        }
    }
}
