//! Effective landing-branch policy: legacy protection plus active repository /
//! organization rulesets. A refused, malformed or bounded read is never empty
//! policy. This reader performs no provider mutations.

use super::{
    BranchProtectionJson, BranchSettingsJson, CommandRunner, CommandSpec, Deserialize,
    DiscoveryError, GitHubMutationAdapter, MutationError, RepositoryId, branch_protection_command,
    branch_settings_command, encode_path_segment,
};
use crate::required_runs::{RequiredCheck, RequiredContextsRead};

#[derive(Deserialize)]
struct BranchRule {
    #[serde(rename = "type")]
    kind: String,
    parameters: Option<serde_json::Value>,
}

pub(super) fn branch_rules_command(repository: &RepositoryId, branch: &str) -> CommandSpec {
    CommandSpec::new("gh").args([
        "api".to_owned(),
        "--paginate".to_owned(),
        "--slurp".to_owned(),
        format!(
            "repos/{}/rules/branches/{}?per_page=100",
            repository.slug(),
            encode_path_segment(branch)
        ),
    ])
}

impl<R: CommandRunner> GitHubMutationAdapter<R> {
    pub(super) fn effective_required_checks(
        &self,
        repository: &RepositoryId,
        branch: &str,
    ) -> RequiredContextsRead {
        let Ok(settings) =
            self.json::<BranchSettingsJson>(branch_settings_command(repository, branch))
        else {
            return RequiredContextsRead::partial(branch);
        };
        // This endpoint returns the active rules applying to this exact branch,
        // including organization rulesets. A repository-ruleset listing alone
        // does not establish effective policy.
        let Ok(pages) = self.json::<Vec<Vec<BranchRule>>>(branch_rules_command(repository, branch))
        else {
            return RequiredContextsRead::partial(branch);
        };
        let complete_pages = !pages.is_empty();
        let rules = pages.into_iter().flatten().collect::<Vec<_>>();
        let mut result = RequiredContextsRead::unprotected(branch);
        result.complete = complete_pages;
        result.protected = settings.protected || !rules.is_empty();
        if settings.protected {
            match self.json::<BranchProtectionJson>(branch_protection_command(repository, branch)) {
                Ok(policy) => {
                    if let Some(required) = policy.required_status_checks {
                        result.contexts = required.contexts;
                        for check in required.checks {
                            let app_id = match check.app_id {
                                None | Some(-1) => None,
                                Some(id) if id > 0 => u64::try_from(id).ok(),
                                Some(_) => {
                                    result.complete = false;
                                    None
                                }
                            };
                            result.checks.push(RequiredCheck {
                                context: check.context,
                                app_id,
                            });
                        }
                    }
                }
                // A ruleset-protected branch need not have legacy protection.
                // Only the provider's explicit absence response may be ignored;
                // permission/transport/decoding failures stay unknown.
                Err(MutationError::Provider(DiscoveryError::CommandFailed { stderr, .. }))
                    if stderr.contains("Branch not protected") && stderr.contains("404") => {}
                Err(_) => result.complete = false,
            }
        }
        // Normalize the legacy duplicate names before unioning independent
        // ruleset requirements. An unbound ruleset requirement and an App-bound
        // legacy requirement must both remain represented.
        result = result.normalized();
        for rule in rules {
            // Required-workflow rules cannot be reduced to a guessed context.
            if rule.kind == "workflows" {
                result.complete = false;
            }
            if rule.kind != "required_status_checks" {
                continue;
            }
            let Some(checks) = rule
                .parameters
                .as_ref()
                .and_then(|parameters| parameters.get("required_status_checks"))
                .and_then(serde_json::Value::as_array)
            else {
                result.complete = false;
                continue;
            };
            for check in checks {
                let Some(context) = check.get("context").and_then(serde_json::Value::as_str) else {
                    result.complete = false;
                    continue;
                };
                let app_id = match check.get("integration_id") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(value) => {
                        let app_id = value.as_u64().filter(|id| *id > 0);
                        if app_id.is_none() {
                            result.complete = false;
                        }
                        app_id
                    }
                };
                result.checks.push(RequiredCheck {
                    context: context.to_owned(),
                    app_id,
                });
            }
        }
        result.normalized()
    }
}

#[cfg(test)]
mod tests;
