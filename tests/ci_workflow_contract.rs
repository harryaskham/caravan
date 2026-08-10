//! Source-light policy checks for the required GitHub Actions gate.

const CI_WORKFLOW: &str = include_str!("../.github/workflows/ci.yml");

/// bd-877ded: main run 31345354513 spent 29m46s in the pinned gate and then
/// exhausted the old 30-minute job budget during finalization. Keep a finite
/// 45-minute budget (50% measured headroom) and preserve every required gate.
#[test]
fn ci_gate_has_bounded_measured_headroom_without_weaker_commands() {
    let timeout_minutes = CI_WORKFLOW
        .lines()
        .filter_map(|line| line.trim().strip_prefix("timeout-minutes:"))
        .map(str::trim)
        .map(|value| value.parse::<u64>().expect("timeout-minutes is numeric"))
        .collect::<Vec<_>>();
    assert_eq!(
        timeout_minutes,
        vec![45],
        "CI must retain exactly one reviewed 45-minute runaway bound"
    );

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
