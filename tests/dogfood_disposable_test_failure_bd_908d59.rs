#[test]
fn disposable_current_cara_canary_fails_by_design_bd_908d59() {
    assert_eq!(
        "expected-green",
        "intentional-red",
        "bd-908d59 is a disposable test-only Cara canary and must never merge"
    );
}
