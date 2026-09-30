use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub struct Fixture {
    pub root: tempfile::TempDir,
    pub repo: PathBuf,
    pub candidate: String,
    pub target: String,
    path: std::ffi::OsString,
    real_git: PathBuf,
}

impl Fixture {
    pub fn new(timeout: u64) -> Self {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let bin = root.path().join("bin");
        fs::create_dir_all(repo.join(".caravan")).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(root.path().join("home")).unwrap();
        let search = std::env::var_os("PATH").unwrap();
        let real_git = std::env::split_paths(&search)
            .map(|entry| entry.join("git"))
            .find(|path| path.is_file())
            .unwrap();
        let path = std::env::join_paths(
            std::iter::once(bin.clone()).chain(std::env::split_paths(&search)),
        )
        .unwrap();
        let mut fixture = Self {
            root,
            repo,
            path,
            real_git,
            candidate: String::new(),
            target: String::new(),
        };
        fixture.git(&["init", "--initial-branch=main"]);
        fixture.git(&["config", "user.name", "Repair fixture"]);
        fixture.git(&["config", "user.email", "fixture@example.invalid"]);
        fs::write(fixture.repo.join(".caravan/config.yaml"), format!("version: 1\nrepository: acme/widgets\nstack_type: caravan\nrebase_on_join: false\ncommand_timeout_secs: {timeout}\n")).unwrap();
        fixture.git(&["add", "."]);
        fixture.git(&["commit", "-m", "base policy"]);
        let base = fixture.git(&["rev-parse", "HEAD"]);
        let mut branches = Vec::new();
        for name in ["active", "candidate", "target"] {
            fixture.git(&["checkout", "-b", name, "main"]);
            fs::write(
                fixture.repo.join(format!("{name}.txt")),
                format!("{name}\n"),
            )
            .unwrap();
            fixture.git(&["add", "."]);
            fixture.git(&["commit", "-m", name]);
            branches.push(fixture.git(&["rev-parse", "HEAD"]));
        }
        fixture.candidate.clone_from(&branches[1]);
        fixture.target.clone_from(&branches[2]);
        fixture.git(&["checkout", "main"]);
        let remote = fixture.root.path().join("remote.git");
        fixture.git(&[
            "clone",
            "--bare",
            "--no-hardlinks",
            ".",
            remote.to_str().unwrap(),
        ]);
        fixture.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
        fs::write(fixture.repo.join("preserved-wip"), "caller work\n").unwrap();
        for (name, source) in [
            ("gh", include_str!("gh.py")),
            ("git", include_str!("git.py")),
        ] {
            fs::write(bin.join(name), source).unwrap();
            fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let config = caravan::config::CaravanConfig::default();
        let labels = caravan::initialization::required_labels(&config.agent_priority_labels, false, false)
            .into_iter()
            .map(|label| json!({"name":label.name, "color":label.color, "description":label.description}))
            .collect::<Vec<_>>();
        fixture.json(
            "provider.json",
            &json!({
                "base":base,
                "active":pull(1, "active", &branches[0], &base, true),
                "candidate":pull(8, "candidate", &branches[1], &base, false),
                "target":pull(7, "target", &branches[2], &base, false),
                "unrelated":pull(99, "unrelated", &base, &base, false),
                "labels":labels
            }),
        );
        fixture
    }

    pub fn json(&self, name: &str, data: &Value) {
        fs::write(
            self.root.path().join(name),
            serde_json::to_vec(data).unwrap(),
        )
        .unwrap();
    }

    pub fn git(&self, args: &[&str]) -> String {
        let output = Command::new(&self.real_git)
            .args(args)
            .current_dir(&self.repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    pub fn start(&self, scenario: &str) -> (Output, Value, Vec<Vec<String>>) {
        let output = Command::new(env!("CARGO_BIN_EXE_cara"))
            .args([
                "--json",
                "repair",
                "start",
                "--pr",
                "8",
                "--target-pr",
                "7",
                "--non-force",
                "--actor",
                "source-owner",
                "--reason",
                "exact parent repair",
            ])
            .current_dir(&self.repo)
            .env_clear()
            .env("PATH", &self.path)
            .env("HOME", self.root.path().join("home"))
            .env("XDG_CONFIG_HOME", self.root.path().join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("FIXTURE_ROOT", self.root.path())
            .env("FIXTURE_REAL_GIT", &self.real_git)
            .env("FIXTURE_SCENARIO", scenario)
            .output()
            .unwrap();
        let result = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "expected JSON: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        let calls = fs::read_to_string(self.root.path().join("calls"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        (output, result, calls)
    }

    pub fn manifest(&self) -> PathBuf {
        self.repo
            .join(".git/caravan/repair-workspaces/pr-8/session.json")
    }

    pub fn workspace_git(&self, workspace: &Path, args: &[&str]) -> String {
        let output = Command::new(&self.real_git)
            .args(args)
            .current_dir(workspace)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}

fn pull(number: u64, name: &str, head: &str, base: &str, active: bool) -> Value {
    json!({
        "number":number, "title":name, "body":"", "state":"OPEN", "isDraft":false,
        "headRefName":name, "headRefOid":head,
        "headRepository":{"name":"widgets", "nameWithOwner":"acme/widgets"},
        "headRepositoryOwner":{"login":"acme"}, "isCrossRepository":false,
        "baseRefName":"main", "baseRefOid":base,
        "labels":if active {json!([{"name":"caravan"}])} else {json!([])},
        "autoMergeRequest":null, "statusCheckRollup":[],
        "createdAt":"2026-09-01T00:00:00Z", "updatedAt":"2026-09-02T00:00:00Z",
        "mergedAt":null, "url":format!("https://github.com/acme/widgets/pull/{number}")
    })
}
