use super::*;
use crate::command::{CommandOutput, CommandRunError};
use crate::github::{CheckJson, OPEN_PR_PAGE_QUERY, pull_request_command};
use crate::model::CommitOid;
use std::cell::RefCell;
use std::collections::VecDeque;

struct Runner(RefCell<VecDeque<(CommandSpec, CommandOutput)>>);
impl CommandRunner for Runner {
    fn run(&self, command: &CommandSpec) -> Result<CommandOutput, CommandRunError> {
        let (expected, output) = self
            .0
            .borrow_mut()
            .pop_front()
            .expect("unexpected provider read");
        assert_eq!(&expected, command);
        Ok(output)
    }
}
fn repository() -> RepositoryId {
    RepositoryId {
        owner: "test".into(),
        name: "repo".into(),
    }
}
fn read(rules: CommandOutput, legacy: Option<CommandOutput>) -> RequiredContextsRead {
    let repo = repository();
    let mut calls = vec![
        (
            branch_settings_command(&repo, "landing/target"),
            CommandOutput::success(if legacy.is_some() {
                r#"{"protected":true}"#
            } else {
                r#"{"protected":false}"#
            }),
        ),
        (branch_rules_command(&repo, "landing/target"), rules),
    ];
    if let Some(legacy) = legacy {
        calls.push((branch_protection_command(&repo, "landing/target"), legacy));
    }
    let provider = GitHubMutationAdapter::new(Runner(RefCell::new(calls.into())));
    let result = provider
        .branch_required_contexts(&repo, "landing/target")
        .unwrap();
    assert!(provider.runner.0.borrow().is_empty());
    result
}

#[test]
fn effective_policy_provider_unions_paginated_rules_and_app_bound_legacy() {
    let result = read(
        CommandOutput::success(
            r#"[
        [{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"security","integration_id":77}]}}],
        [{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"gate","integration_id":44}]}}]
    ]"#,
        ),
        Some(CommandOutput::success(
            r#"{"required_status_checks":{"strict":false,"contexts":["gate","legacy"],"checks":[{"context":"gate","app_id":15368}]}}"#,
        )),
    );
    assert!(result.complete);
    assert_eq!(result.branch, "landing/target");
    assert_eq!(
        result.checks,
        vec![
            RequiredCheck {
                context: "gate".into(),
                app_id: Some(44)
            },
            RequiredCheck {
                context: "gate".into(),
                app_id: Some(15368)
            },
            RequiredCheck {
                context: "legacy".into(),
                app_id: None
            },
            RequiredCheck {
                context: "security".into(),
                app_id: Some(77)
            },
        ]
    );
}

#[test]
fn effective_policy_provider_rules_apply_without_legacy_protection() {
    let rules = r#"[[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"third-party","integration_id":77}]}}]]"#;
    for legacy in [
        None,
        Some(CommandOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: "gh: Branch not protected (HTTP 404)".into(),
        }),
    ] {
        let result = read(CommandOutput::success(rules), legacy);
        assert!(result.complete && result.protected);
        assert_eq!(result.checks[0].app_id, Some(77));
    }
}

#[test]
fn effective_policy_provider_unknown_refused_malformed_and_workflows_fail_closed() {
    for rules in [
        CommandOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: "403 forbidden".into(),
        },
        CommandOutput::success("not-json"),
        CommandOutput::success(r#"[[{"type":"required_status_checks"}]]"#),
        CommandOutput::success(
            r#"[[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"gate","integration_id":0}]}}]]"#,
        ),
        CommandOutput::success(r#"[[{"type":"workflows"}]]"#),
    ] {
        assert!(!read(rules, None).complete);
    }
    let denied = read(
        CommandOutput::success("[[]]"),
        Some(CommandOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: "gh: Not Found (HTTP 404)".into(),
        }),
    );
    assert!(
        !denied.complete,
        "generic hidden resource is not proof of no legacy policy"
    );
}

#[test]
fn effective_policy_provider_legacy_any_app_and_empty_policy_remain_supported() {
    let empty = read(CommandOutput::success("[[]]"), None);
    assert!(empty.complete && !empty.protected && empty.checks.is_empty());
    let any = read(
        CommandOutput::success("[[]]"),
        Some(CommandOutput::success(
            r#"{"required_status_checks":{"strict":true,"contexts":["gate"],"checks":[{"context":"gate","app_id":-1}]}}"#,
        )),
    );
    assert!(any.complete);
    assert_eq!(
        any.checks,
        vec![RequiredCheck {
            context: "gate".into(),
            app_id: None
        }]
    );
}

#[test]
fn effective_policy_exact_and_paged_queries_retain_provider_identity() {
    let command = pull_request_command(&repository(), "17");
    let query = command
        .args
        .iter()
        .find_map(|arg| arg.strip_prefix("query="))
        .unwrap();
    assert!(query.contains("pullRequest(number:$number)"));
    assert!(!query.contains("$cursor") && !query.contains("totalCount"));
    assert_eq!(query.matches('{').count(), query.matches('}').count());
    for projection in [query, OPEN_PR_PAGE_QUERY] {
        assert!(projection.contains("checkSuite{databaseId app{databaseId} commit{oid}"));
    }
    let jq = command.args.last().unwrap();
    assert!(jq.contains("appId:") && jq.contains("checkSuiteId:") && jq.contains("headOid:"));
    assert!(jq.contains("error(\"incomplete PR checks/labels\")"));
    let raw: CheckJson = serde_json::from_value(serde_json::json!({
        "__typename":"CheckRun", "name":"gate", "status":"COMPLETED", "conclusion":"SUCCESS",
        "appId":77, "checkSuiteId":900, "headOid":"other-head"
    }))
    .unwrap();
    let check = raw.into_snapshot();
    assert_eq!(check.app_id, Some(77));
    assert_eq!(check.check_suite_id, Some(900));
    assert_eq!(check.head_oid, Some(CommitOid("other-head".into())));
}
