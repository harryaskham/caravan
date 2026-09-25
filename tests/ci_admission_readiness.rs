//! Run the actual Cara command, real Git policy materialization and a bounded
//! read-only gh protocol fixture. No Cara receipt or decision is stubbed.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

struct Fixture {
    root: tempfile::TempDir,
    repo: PathBuf,
    path: std::ffi::OsString,
    base: String,
    head: String,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let bin = root.path().join("bin");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(root.path().join("home")).unwrap();
        let path = std::env::join_paths(
            std::iter::once(bin.clone())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let mut fixture = Self {
            root,
            repo,
            path,
            base: String::new(),
            head: String::new(),
        };
        fixture.git(&["init", "--initial-branch=main"]);
        fixture.git(&["config", "user.name", "Readiness fixture"]);
        fixture.git(&["config", "user.email", "fixture@example.invalid"]);
        fs::create_dir_all(fixture.repo.join(".caravan")).unwrap();
        fs::write(fixture.repo.join(".caravan/config.yaml"),
            "version: 1\nrepository: owner/example\ncommand_timeout_secs: 5\nci:\n  admission_gate:\n    mode: caravan_label\n    context: Caravan admission gate\n    member_label: caravan\n").unwrap();
        fixture.git(&["add", ".caravan/config.yaml"]);
        fixture.git(&["commit", "-m", "trusted policy"]);
        fixture.base = fixture.git(&["rev-parse", "HEAD"]);
        fixture.git(&["checkout", "-b", "topic"]);
        fs::write(fixture.repo.join("feature.txt"), "ready source\n").unwrap();
        fixture.git(&["add", "feature.txt"]);
        fixture.git(&["commit", "-m", "candidate source"]);
        fixture.head = fixture.git(&["rev-parse", "HEAD"]);
        let remote = fixture.root.path().join("remote.git");
        fixture.git(&[
            "clone",
            "--bare",
            "--no-hardlinks",
            ".",
            remote.to_str().unwrap(),
        ]);
        fixture.git(&[
            "--git-dir",
            remote.to_str().unwrap(),
            "symbolic-ref",
            "HEAD",
            "refs/heads/main",
        ]);
        fixture.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
        fs::write(bin.join("gh"), GH_FIXTURE).unwrap();
        fs::set_permissions(bin.join("gh"), fs::Permissions::from_mode(0o755)).unwrap();
        fixture.json(
            "repository.json",
            &json!({"nameWithOwner":"owner/example", "defaultBranchRef":{"name":"main"}}),
        );
        fixture.json("default.json", &json!({"object":{"sha":fixture.base}}));
        fixture
    }

    fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.repo)
            .env_clear()
            .env("PATH", &self.path)
            .env("HOME", self.root.path().join("home"))
            .env("XDG_CONFIG_HOME", self.root.path().join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("FIXTURE_ROOT", self.root.path())
            .env(
                "FEEDBACK_WEBHOOK_URL",
                "https://feedback.invalid/hooks/readiness",
            )
            .env("FEEDBACK_WEBHOOK_TOKEN_ENV", "FIXTURE_ABSENT_TOKEN");
        command
    }

    fn git(&self, args: &[&str]) -> String {
        let output = self.command(Path::new("git")).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn json(&self, name: &str, value: &Value) {
        fs::write(
            self.root.path().join(name),
            serde_json::to_vec(value).unwrap(),
        )
        .unwrap();
    }

    fn event(&self, action: &str) -> Value {
        json!({"action":action,"repository":{"full_name":"owner/example"},"pull_request":{
            "number":41,"draft":false,"head":{"sha":self.head},"base":{"sha":self.base}
        }})
    }

    fn provider(&self, member: bool) -> Value {
        json!({"number":41,"title":"ready source","state":"OPEN","isDraft":false,
            "headRefName":"topic","headRefOid":self.head,"headRepository":{"nameWithOwner":"owner/example"},
            "isCrossRepository":false,"baseRefName":"main","baseRefOid":self.base,
            "labels":if member { json!([{"name":"caravan"}]) } else { json!([]) },
            "autoMergeRequest":null,"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:01Z",
            "mergedAt":null,"url":"https://github.com/owner/example/pull/41"})
    }

    fn run(
        &self,
        binary: &Path,
        event: &Value,
        provider: &Value,
        selected: Option<u64>,
    ) -> (Value, String) {
        self.json("event.json", event);
        self.json("pull.json", provider);
        // Exact focused read supplies the candidate; the membership inventory
        // can legitimately omit its not-yet-converged list projection.
        self.json("members.json", &json!([]));
        fs::write(self.root.path().join("calls"), "").unwrap();
        fs::write(self.root.path().join("outputs"), "").unwrap();
        let before = self.git(&["status", "--porcelain"]);
        let mut command = self.command(binary);
        command
            .args(["--json", "ci-admission-gate", "--event"])
            .arg(self.root.path().join("event.json"))
            .arg("--github-output")
            .arg(self.root.path().join("outputs"));
        if let Some(selected) = selected {
            command.arg("--selected-pr").arg(selected.to_string());
        }
        let output = command.output().unwrap();
        let data = successful_data(&output);
        assert_eq!(self.git(&["status", "--porcelain"]), before);
        assert_eq!(self.git(&["rev-parse", "HEAD"]), self.head);
        assert_eq!(
            self.git(&["worktree", "list", "--porcelain"])
                .matches("worktree ")
                .count(),
            1
        );
        let unchanged: Value =
            serde_json::from_slice(&fs::read(self.root.path().join("event.json")).unwrap())
                .unwrap();
        assert_eq!(
            &unchanged, event,
            "the action/payload must never be rewritten"
        );
        let calls = fs::read_to_string(self.root.path().join("calls")).unwrap();
        assert!(
            calls.lines().all(|line| line.starts_with("repo view ")
                || line.starts_with("api repos/")
                || line.starts_with("pr list ")
                || line.starts_with("pr view ")),
            "unexpected provider command: {calls}"
        );
        let outputs = fs::read_to_string(self.root.path().join("outputs")).unwrap();
        assert!(outputs.contains(&format!(
            "decision={}\n",
            data["decision_code"].as_str().unwrap()
        )));
        (data, calls)
    }
}

fn successful_data(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "Cara command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value = serde_json::from_slice(&output.stdout).expect("real command JSON");
    assert_eq!(envelope["status"], "success", "{envelope}");
    envelope["data"].clone()
}

const GH_FIXTURE: &str = r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$FIXTURE_ROOT/calls"
if [ -f "$FIXTURE_ROOT/unavailable" ]; then
  printf '%s\n' 'fixture provider unavailable' >&2
  exit 1
fi
case "$1 $2" in
  'repo view') test "$3" = owner/example; exec cat "$FIXTURE_ROOT/repository.json" ;;
  'api repos/owner/example/git/ref/heads/main') exec cat "$FIXTURE_ROOT/default.json" ;;
  'pr list') test "$3 $4 $5 $6 $7 $8" = '--repo owner/example --state open --label caravan'; exec cat "$FIXTURE_ROOT/members.json" ;;
  'pr view') test "$3 $4 $5" = '41 --repo owner/example'; exec cat "$FIXTURE_ROOT/pull.json" ;;
  *) printf '%s\n' 'unexpected provider request: refused by fixture' >&2; exit 91 ;;
esac
"#;

#[test]
fn readiness_actual_binary_proves_member_and_unjoined_without_rewriting_action() {
    let fixture = Fixture::new();
    let binary = Path::new(env!("CARGO_BIN_EXE_cara"));
    for member in [false, true] {
        let (data, calls) = fixture.run(
            binary,
            &fixture.event("ready_for_review"),
            &fixture.provider(member),
            None,
        );
        assert_eq!(
            calls.lines().count(),
            4,
            "must perform the real membership read path"
        );
        assert_eq!(data["event_action"], "ready_for_review");
        assert_eq!(data["head"], fixture.head);
        assert_eq!(data["base"], fixture.base);
        assert_eq!(data["default_head"], fixture.base);
        assert_eq!(data["selected_pr"], 41);
        assert_eq!(data["policy"]["member_label"], "caravan");
        assert_eq!(
            data["decision"],
            if member {
                "run_member"
            } else {
                "deferred_unjoined"
            }
        );
        assert_eq!(data["run_ci"], member);
        assert_eq!(data["deferred_unjoined"], !member);
        assert_eq!(data["workflow_exit_code"], if member { 0 } else { 78 });
        assert!(
            data["receipt_fingerprint"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
    }
}

#[test]
fn readiness_actual_binary_refuses_drift_draft_fork_and_unavailable_membership() {
    let fixture = Fixture::new();
    let binary = Path::new(env!("CARGO_BIN_EXE_cara"));
    for (field, value) in [
        ("headRefOid", json!("f".repeat(40))),
        ("baseRefOid", json!("f".repeat(40))),
        ("number", json!(99)),
        ("isDraft", json!(true)),
        ("isCrossRepository", json!(true)),
        ("state", json!("CLOSED")),
    ] {
        let mut provider = fixture.provider(true);
        provider[field] = value;
        let (data, calls) =
            fixture.run(binary, &fixture.event("ready_for_review"), &provider, None);
        assert_unproven(&data);
        assert!(calls.contains("pr view 41"), "{field}");
    }
    fs::write(
        fixture.root.path().join("unavailable"),
        "bounded provider failure",
    )
    .unwrap();
    let (data, calls) = fixture.run(
        binary,
        &fixture.event("ready_for_review"),
        &fixture.provider(false),
        None,
    );
    assert_unproven(&data);
    assert_eq!(calls.lines().count(), 1);
}

#[test]
fn readiness_actual_binary_rejects_wrong_wake_repository_and_unrelated_actions() {
    let fixture = Fixture::new();
    let binary = Path::new(env!("CARGO_BIN_EXE_cara"));
    let (data, calls) = fixture.run(
        binary,
        &fixture.event("ready_for_review"),
        &fixture.provider(false),
        Some(99),
    );
    assert_unproven(&data);
    assert!(calls.is_empty());
    let mut event = fixture.event("ready_for_review");
    event["repository"]["full_name"] = json!("foreign/repository");
    let (data, calls) = fixture.run(binary, &event, &fixture.provider(false), None);
    assert_unproven(&data);
    assert!(calls.is_empty());
    for action in [
        "edited",
        "labeled",
        "unlabeled",
        "converted_to_draft",
        "closed",
        "unknown",
    ] {
        let (data, calls) = fixture.run(
            binary,
            &fixture.event(action),
            &fixture.provider(false),
            None,
        );
        assert_unproven(&data);
        assert!(calls.is_empty(), "{action}");
        assert_eq!(data["event_action"], action);
    }
}

fn assert_unproven(data: &Value) {
    assert_eq!(data["decision"], "run_unproven", "{data}");
    assert_eq!(data["run_ci"], true);
    assert_eq!(data["deferred_unjoined"], false);
    assert_eq!(data["workflow_exit_code"], 0);
}

#[test]
fn readiness_optional_released_binary_negative_control() {
    // Explicit local compatibility evidence only. CI always runs the built
    // command above; it never downloads an external binary or infers a release
    // from the package version of a development build.
    let Some(binary) = std::env::var_os("CARA_ADMISSION_BASELINE_BINARY") else {
        return;
    };
    let fixture = Fixture::new();
    let (data, calls) = fixture.run(
        Path::new(&binary),
        &fixture.event("ready_for_review"),
        &fixture.provider(false),
        None,
    );
    assert_unproven(&data);
    assert_eq!(data["event_action"], "ready_for_review");
    assert!(
        data["reason"]
            .as_str()
            .unwrap()
            .contains("unsupported pull_request action")
    );
    assert!(
        calls.is_empty(),
        "baseline must reject before live membership lookup"
    );
}
