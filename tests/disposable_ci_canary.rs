//! Disposable operator-authorized canary: intentionally fails ordinary test CI.
//! Must be closed unmerged after autonomous Cara terminal-red observation.
#[test]
fn disposable_ci_failure_canary() {
    let observed = std::hint::black_box(false);
    assert!(observed, "intentional disposable Cara CI-failure canary; do not repair or merge");
}
