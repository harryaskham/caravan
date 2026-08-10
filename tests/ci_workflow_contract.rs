//! Source-light policy checks for the required GitHub Actions gate.

const CI_WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");

/// bd-480dbc: main run 31349811279 exposed 16 host CPUs to cold compilation
/// and SIGKILLed the Caravan crate with exit 137. Bound Cargo parallelism and
/// incremental-state memory across generic runners, retain a finite 90-minute
/// budget for the deliberately serialized gate, and preserve every command.
#[test]
fn ci_gate_has_portable_resource_bounds_without_weaker_commands() {
    let timeout_minutes = CI_WORKFLOW
        .lines()
        .filter_map(|line| line.trim().strip_prefix("timeout-minutes:"))
        .map(str::trim)
        .map(|value| value.parse::<u64>().expect("timeout-minutes is numeric"))
        .collect::<Vec<_>>();
    assert_eq!(
        timeout_minutes,
        vec![90],
        "CI must retain exactly one reviewed 90-minute runaway bound"
    );
    for required in ["CARGO_BUILD_JOBS: \"2\"", "CARGO_INCREMENTAL: \"0\""] {
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
        assert!(
            CI_WORKFLOW.contains(required),
            "timeout repair must not weaken required gate `{required}`"
        );
    }
}
