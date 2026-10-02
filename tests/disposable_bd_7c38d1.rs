// Disposable dogfood input for bd-7c38d1. This PR must never land on main.
// Only the test verdict is varied; workflow and production code stay unchanged.
#[test]
fn disposable_canary_intentionally_fails() {
    panic!("bd-7c38d1: deliberate test-only failure for autonomous Cara observation");
}
