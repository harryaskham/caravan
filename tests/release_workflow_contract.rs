//! Source-light policy checks for immutable release and target-only backfill.

const RELEASE_WORKFLOW: &str = include_str!("../.github/workflows/release.yml");

fn between<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let after = source
        .split_once(start)
        .unwrap_or_else(|| panic!("missing release workflow marker `{start}`"))
        .1;
    after
        .split_once(end)
        .unwrap_or_else(|| panic!("missing release workflow marker `{end}` after `{start}`"))
        .0
}

/// bd-9715a4: persistent runners must never share operator-global Git config,
/// and an existing immutable tag must be able to select only Darwin without
/// scheduling or overwriting the already-published Linux asset.
#[test]
fn release_workflow_isolates_git_and_selects_exact_backfill_target() {
    assert!(!RELEASE_WORKFLOW.contains("git config --global --unset-all"));
    assert_eq!(
        RELEASE_WORKFLOW
            .matches("git_config=\"$RUNNER_TEMP/")
            .count(),
        3,
        "resolve, contract, and build jobs each require a runner-temp Git config"
    );
    assert_eq!(
        RELEASE_WORKFLOW
            .matches("Configure isolated anonymous Git")
            .count(),
        3
    );
    assert_eq!(
        RELEASE_WORKFLOW
            .matches(": > \"$GIT_CONFIG_GLOBAL\"")
            .count(),
        3,
        "each job must initialize its isolated config fail-closed before checkout"
    );
    assert_eq!(
        RELEASE_WORKFLOW
            .matches("echo \"GIT_CONFIG_NOSYSTEM=1\"")
            .count(),
        3
    );
    assert_eq!(RELEASE_WORKFLOW.matches(">> \"$GITHUB_ENV\"").count(), 3);
    let resolve = between(RELEASE_WORKFLOW, "\n  resolve:\n", "\n  test:\n");
    let contract = between(RELEASE_WORKFLOW, "\n  test:\n", "\n  build:\n");
    let build = RELEASE_WORKFLOW
        .split_once("\n  build:\n")
        .expect("release workflow has build job")
        .1;
    for (name, job) in [("resolve", resolve), ("test", contract), ("build", build)] {
        let isolation = job
            .find("Configure isolated anonymous Git")
            .unwrap_or_else(|| panic!("{name} job configures isolated Git"));
        let checkout = job
            .find("actions/checkout@v4")
            .unwrap_or_else(|| panic!("{name} job checks out source"));
        assert!(
            isolation < checkout,
            "{name} must isolate Git before checkout"
        );
        for required in ["set -euo pipefail", "umask 077", ">> \"$GITHUB_ENV\""] {
            assert!(job.contains(required), "{name} must retain `{required}`");
        }
    }

    for required in [
        "target:\n        description: \"Exact target to backfill, or all targets\"",
        "target=\"${{ inputs.target || 'all' }}\"",
        "case \"$target\" in",
        "unsupported release target",
        "echo \"target=$target\"",
        "echo \"matrix=$matrix\"",
        "} >> \"$GITHUB_OUTPUT\"",
        "matrix: ${{ fromJSON(needs.resolve.outputs.matrix) }}",
        "ref: ${{ needs.resolve.outputs.tag }}",
    ] {
        assert!(
            RELEASE_WORKFLOW.contains(required),
            "release workflow must retain `{required}`"
        );
    }

    let all = between(RELEASE_WORKFLOW, "            all)\n", "              ;;\n");
    assert!(all.contains("\"target\":\"x86_64-linux\""));
    assert!(all.contains("\"target\":\"aarch64-darwin\""));

    let linux = between(
        RELEASE_WORKFLOW,
        "            x86_64-linux)\n",
        "              ;;\n",
    );
    assert!(linux.contains("\"target\":\"x86_64-linux\""));
    assert!(!linux.contains("\"target\":\"aarch64-darwin\""));

    let darwin = between(
        RELEASE_WORKFLOW,
        "            aarch64-darwin)\n",
        "              ;;\n",
    );
    assert!(darwin.contains("\"target\":\"aarch64-darwin\""));
    assert!(
        !darwin.contains("\"target\":\"x86_64-linux\""),
        "Darwin-only backfill must not schedule Linux"
    );
}
