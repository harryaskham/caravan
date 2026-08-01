//! bd-a79679: the default `stack_type: caravan` path must remain exactly what
//! it was before native GitHub Stack support existed, and every native-mode
//! upgrade must stay explicit.
//!
//! These are contract assertions, not behaviour experiments: they fail if a
//! future rollout slice makes the default path probe, mutate, gate, or require
//! anything new.

use caravan::config::{
    CaravanConfig, DEFAULT_GITHUB_MAX_CARAVAN_LENGTH, MIN_GITHUB_STACK_READER_VERSION, StackType,
};

const DEFAULT_POLICY: &str = r"
version: 1
force_merge: false
rebase_on_join: false
command_timeout_secs: 30
";

const GITHUB_POLICY: &str = r"
version: 1
min_cara_version: '0.0.65'
force_merge: false
stack_type: github
rebase_on_join: false
command_timeout_secs: 30
sync:
  head_merge_actor: caravan
";

#[test]
fn absent_stack_type_keeps_every_default_unchanged() {
    let legacy: CaravanConfig = serde_yaml::from_str(DEFAULT_POLICY).expect("legacy policy parses");
    legacy.validate().expect("legacy policy stays valid");

    assert_eq!(legacy.stack_type, StackType::Caravan);
    assert_eq!(CaravanConfig::default().stack_type, StackType::Caravan);
    // No batch bound, so admission keeps the historical dynamic capacity model.
    assert_eq!(legacy.max_caravan_length, None);
    assert_eq!(legacy.effective_max_caravan_length(), None);
    assert_eq!(
        CaravanConfig::default().effective_max_caravan_length(),
        None
    );
    // The native reader pin is required only after explicit opt-in.
    assert_eq!(legacy.min_cara_version, "0.0.0");

    // Serializing the default policy must not introduce native-only fields.
    let rendered = serde_yaml::to_string(&legacy).expect("default policy serializes");
    assert!(!rendered.contains("max_caravan_length"), "{rendered}");
    assert!(rendered.contains("stack_type: caravan"), "{rendered}");
}

#[test]
fn native_mode_upgrades_are_all_explicit_and_bounded() {
    let github: CaravanConfig = serde_yaml::from_str(GITHUB_POLICY).expect("native policy parses");
    github.validate().expect("reviewed native policy is valid");

    assert_eq!(
        github.effective_max_caravan_length(),
        Some(DEFAULT_GITHUB_MAX_CARAVAN_LENGTH),
        "one native Stack is a bounded merge batch"
    );

    // An older reader can never silently treat a native Stack as an ordinary
    // label/base-chain caravan.
    let mut stale_reader = github.clone();
    stale_reader.min_cara_version = "0.0.1".to_owned();
    assert!(
        stale_reader
            .validate()
            .unwrap_err()
            .to_string()
            .contains(MIN_GITHUB_STACK_READER_VERSION)
    );

    // Native mode never introduces a second physical branch writer, and never
    // hands Stack entries to provider auto-merge.
    let mut physical = github.clone();
    physical.rebase_on_join = true;
    assert!(
        physical
            .validate()
            .unwrap_err()
            .to_string()
            .contains("rebase_on_join: false")
    );

    // A batch bound outside GitHub's Stack range can never be one Stack.
    for invalid in [0, 1, 101] {
        let mut rejected = github.clone();
        rejected.max_caravan_length = Some(invalid);
        assert!(
            rejected
                .validate()
                .unwrap_err()
                .to_string()
                .contains("max_caravan_length must be between 2 and 100"),
            "batch bound {invalid} must be refused"
        );
    }
}

#[test]
fn the_native_permission_upgrade_is_documented_as_conditional() {
    let policy: serde_json::Value =
        serde_json::from_str(include_str!("../docs/github-app-policy.json"))
            .expect("valid policy JSON");
    let administration = &policy["optional_permission_upgrades"]["administration"];
    assert_eq!(administration["level"], "write");
    assert!(
        administration["scope"]
            .as_str()
            .expect("scope text")
            .contains("never general repository settings")
    );
    assert!(
        !policy["required_repository_permissions"]
            .as_object()
            .expect("baseline permissions")
            .contains_key("administration"),
        "Administration must never be a baseline permission"
    );
}
