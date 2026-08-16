#[test]
fn disposable_test_only_failure_canary() {
    let observed = std::hint::black_box(1_u8);
    assert_eq!(
        observed, 2,
        "intentional disposable Cara test-only failure canary"
    );
}
