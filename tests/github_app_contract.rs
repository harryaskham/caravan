use serde_json::{Value, json};

const POLICY_TEXT: &str = include_str!("../docs/github-app-policy.json");
const GUIDE: &str = include_str!("../docs/github-app.md");

#[test]
fn github_app_permission_baseline_is_exact_and_least_privilege() {
    let policy: Value = serde_json::from_str(POLICY_TEXT).expect("valid policy JSON");
    assert_eq!(policy["schema_version"], 1);
    assert_eq!(
        policy["required_repository_permissions"],
        json!({
            "metadata": "read",
            "contents": "write",
            "pull_requests": "write",
            "issues": "write",
            "checks": "read",
            "statuses": "read",
            "actions": "read"
        })
    );
    assert_eq!(
        policy["optional_permission_upgrades"],
        json!({
            "actions": {
                "level": "write",
                "only_when": "the configured deployment authorizes workflow reruns"
            },
            "workflows": {
                "level": "write",
                "only_when": "Caravan-managed source branches may modify .github/workflows files"
            }
        })
    );
    assert_eq!(
        policy["forbidden_permissions"],
        json!([
            "administration",
            "members",
            "organization_administration",
            "organization_secrets",
            "repository_secrets"
        ])
    );
    assert_eq!(
        policy["branch_policy"]["default_branch_bypass"],
        "forbidden"
    );
}

#[test]
fn github_app_contract_keeps_ambient_default_and_names_unfinished_proofs() {
    let policy: Value = serde_json::from_str(POLICY_TEXT).expect("valid policy JSON");
    assert_eq!(policy["auth_modes"]["default"], "ambient");
    assert_eq!(policy["auth_modes"]["app"], "app_installation");
    assert_eq!(
        policy["outstanding_proofs"],
        json!(["bd-71e444", "remote_compare_and_swap_writer_lease"])
    );
    for required in [
        "CARA_GITHUB_AUTH_MODE=app_installation",
        "X-Hub-Signature-256",
        "force-with-lease",
        "caravan@localhost.invalid",
        "exactly one",
    ] {
        assert!(GUIDE.contains(required), "guide omitted `{required}`");
    }
    assert!(!POLICY_TEXT.contains("token"));
    assert!(!POLICY_TEXT.contains("private_key"));
}
