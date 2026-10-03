use super::*;

#[test]
fn projection_preflight_preserves_existing_effects_and_refuses_another_ordinary_tail() {
    let root = pull_request(1, "root", "main", &[ACTIVE_LABEL]);
    let child = pull_request(2, "child", "root", &[ACTIVE_LABEL]);
    let tail = pull_request(3, "tail", "child", &[ACTIVE_LABEL]);
    let candidate = pull_request(4, "candidate", "tail", &[]);
    let mut current = status(candidate, vec![root, child, tail.clone()]);
    let target = JoinTarget {
        caravan: current
            .analysis
            .fleet
            .containing(PrNumber(1))
            .unwrap()
            .clone(),
        tail,
    };
    let mut context = AppContext::default();
    context.config.stack_type = crate::config::StackType::Github;
    context.config.rebase_on_join = false;
    let accepted_before = current.analysis.pull_requests.clone();
    let error = require_exact_native_join_projection(&context, &current, &target).unwrap_err();
    assert_eq!(error.code(), "github_stack_membership_repair_required");
    let details = error.details().unwrap();
    assert_eq!(details["membership_preflight_mutated"], false);
    assert!(
        details.get("mutated").is_none(),
        "a local preflight cannot claim prior whole-operation zero effects"
    );
    assert_eq!(current.analysis.pull_requests, accepted_before);
    // Existing membership is not replayed or treated as another extension.
    current.current_pr = Some(PrNumber(3));
    require_exact_native_join_projection(&context, &current, &target).unwrap();
    // This native fence does not rewrite the policy of a non-native caravan.
    context.config.stack_type = crate::config::StackType::Caravan;
    current.current_pr = Some(PrNumber(4));
    require_exact_native_join_projection(&context, &current, &target).unwrap();
    assert!(!context.config.physical_branch_rewrites_enabled());
}
