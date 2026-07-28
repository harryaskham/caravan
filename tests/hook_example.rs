//! The shipped Caco bead-dispatch hook is executable contract, not prose.
//!
//! A scheduler that dispatches an agent for a bounded provider race spawns work
//! that has already fixed itself; one that treats an unresolvable caravan as
//! retryable leaves it stuck forever while the cron cheerfully reruns. Both
//! directions are cheap to get wrong and invisible until production, so the
//! example hook is exercised here against a fake `caco`.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn hook_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/hooks/caco-bead-dispatch.sh")
}

fn event(wake_class: &str, fingerprint: &str) -> String {
    format!(
        r#"{{"kind":"sync_failed","reason":"caravan root #2223 was not merged by cara","metadata":{{"error_code":"root_merge_refused","decision_fingerprint":"{fingerprint}","scheduler_status":{{"wake_class":"{wake_class}","cause_code":"default_branch_diverged_from_retained_patch_set"}}}}}}"#
    )
}

/// A fake `caco` that records its invocations instead of dispatching anything.
fn fake_caco(root: &Path) -> PathBuf {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("fake bin");
    let script = bin.join("caco");
    // `bd list --count-only` is the hook's dedupe probe: it reports how many
    // beads already carry this decision's label. The fake answers from the
    // recorded calls, so a repeat delivery sees its own first dispatch.
    fs::write(
        &script,
        format!(
            r#"#!/usr/bin/env bash
log={log}
labels={labels}
if [ "$1" = "bd" ] && [ "$2" = "list" ]; then
  wanted=""
  while [ $# -gt 0 ]; do
    if [ "$1" = "--label" ]; then wanted="$2"; fi
    shift
  done
  count=0
  if [ -e "$labels" ] && grep -qxF "$wanted" "$labels"; then count=1; fi
  printf '{{"count": %s}}
' "$count"
  exit 0
fi
printf '%s %s
' "$1" "$2" >> "$log"
if [ "$1" = "bd" ] && [ "$2" = "create" ]; then
  while [ $# -gt 0 ]; do
    if [ "$1" = "--labels" ]; then
      printf '%s
' "$2" | tr ',' '
' >> "$labels"
    fi
    shift
  done
  echo '{{"id": "bd-a1b2c3"}}'
fi
"#,
            log = root.join("calls.log").display(),
            labels = root.join("labels.txt").display()
        ),
    )
    .expect("write fake caco");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod fake caco");
    bin
}

fn run(root: &Path, payload: &str) {
    let bin = fake_caco(root);
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // CACO_BIN must be pinned, not merely shadowed on PATH. Agent and daemon
    // environments export it as an absolute path, so a test that only prepends
    // a fake directory silently drives the *real* caco and files real beads.
    let mut child = Command::new("bash")
        .arg(hook_path())
        .env("PATH", path)
        .env("CACO_BIN", bin.join("caco"))
        .env("CARA_HOOK_PROJECT", "cacophony")
        .env("CARA_REPOSITORY", "owner/repo")
        .env("CARA_CARAVAN_ID", "2223")
        .env("CARA_PRS", "2223,2225")
        .env("CARA_EVENT", "sync_failed")
        .env("CARA_EVENT_ID", "event-1")
        .env("CARA_OPERATION_ID", "operation-1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("hook runs");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(payload.as_bytes())
        .expect("payload");
    let status = child.wait().expect("hook exits");
    assert!(
        status.success(),
        "a hook must never fail the tick that observed the problem"
    );
}

fn calls(root: &Path) -> Vec<String> {
    fs::read_to_string(root.join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("cara-hook-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("temp root");
    root
}

#[test]
fn a_bounded_race_or_healthy_tick_dispatches_nothing() {
    let root = temp_root("quiet");
    run(&root, &event("retry_tick", "fnv1a64:aaaa"));
    run(&root, &event("none", "fnv1a64:bbbb"));
    assert!(
        calls(&root).is_empty(),
        "a race resolves itself on the next cron tick; waking an agent for it is pure noise"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn an_unresolvable_caravan_files_one_bead_and_dispatches_one_agent() {
    let root = temp_root("dispatch");
    run(&root, &event("external_decision", "fnv1a64:cccc"));
    let observed = calls(&root);
    assert_eq!(
        observed,
        vec!["bd create".to_owned(), "agent new".to_owned()],
        "a caravan that cannot self-resolve gets exactly one repair agent"
    );

    // Same unresolved decision, next cron tick. Without fingerprint dedupe a
    // one-minute cron would spawn one agent per minute for one stuck caravan.
    run(&root, &event("external_decision", "fnv1a64:cccc"));
    assert_eq!(
        calls(&root),
        observed,
        "the same decision must not dispatch twice"
    );

    // A genuinely different decision still dispatches.
    run(&root, &event("external_decision", "fnv1a64:dddd"));
    assert_eq!(
        calls(&root).len(),
        4,
        "a distinct decision is not suppressed"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn operator_action_notifies_a_human_instead_of_spawning_an_agent() {
    let root = temp_root("operator");
    run(&root, &event("operator_action", "fnv1a64:eeee"));
    assert_eq!(
        calls(&root),
        vec!["bd create".to_owned(), "msg broadcast".to_owned()],
        "the work is still recorded, but a human is told: an agent cannot change repository settings or clean a dirty checkout"
    );
    let _ = fs::remove_dir_all(&root);
}
