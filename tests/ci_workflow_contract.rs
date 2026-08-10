//! Source-light policy checks for the required GitHub Actions gate.

const CI_WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");

/// bd-480dbc: run 31349811279 proved 16-way cold Cargo can OOM, while run
/// 31352989063 proved two-way monolithic Clippy exceeds the controller's
/// 60-minute ceiling. Bound Cargo at four workers, split every required command
/// into sub-60-minute jobs, and preserve a fail-closed `build-test` aggregate.
#[test]
fn ci_gate_has_portable_resource_and_time_bounds_without_weaker_commands() {
    let timeout_minutes = CI_WORKFLOW
        .lines()
        .filter_map(|line| line.trim().strip_prefix("timeout-minutes:"))
        .map(str::trim)
        .map(|value| value.parse::<u64>().expect("timeout-minutes is numeric"))
        .collect::<Vec<_>>();
    assert_eq!(
        timeout_minutes,
        vec![10, 55, 55, 55, 5],
        "every split job must stay below the external 60-minute ceiling"
    );
    for required in ["CARGO_BUILD_JOBS: \"4\"", "CARGO_INCREMENTAL: \"0\""] {
        assert!(
            CI_WORKFLOW.contains(required),
            "generic runners must retain resource control `{required}`"
        );
    }

    for required in [
        "cargo fmt --all --check",
        "cargo clippy --all-targets -- -D warnings",
        "cargo test --all",
        "cargo run -- help",
        "cargo run -- mcp tools >/dev/null",
    ] {
        assert_eq!(
            CI_WORKFLOW.matches(required).count(),
            1,
            "split repair must preserve required gate `{required}` exactly once"
        );
    }
    for required in [
        "if: ${{ always() }}",
        "needs: [format, clippy, tests, agent-surface]",
        "FORMAT_RESULT: ${{ needs.format.result }}",
        "CLIPPY_RESULT: ${{ needs.clippy.result }}",
        "TESTS_RESULT: ${{ needs.tests.result }}",
        "AGENT_SURFACE_RESULT: ${{ needs.agent-surface.result }}",
        "test \"$FORMAT_RESULT\" = success",
        "test \"$CLIPPY_RESULT\" = success",
        "test \"$TESTS_RESULT\" = success",
        "test \"$AGENT_SURFACE_RESULT\" = success",
    ] {
        assert!(
            CI_WORKFLOW.contains(required),
            "build-test must aggregate fail-closed contract `{required}`"
        );
    }
}
