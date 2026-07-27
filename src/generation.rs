//! Cacophony PR-generation integrity for admission and queue ownership.
//!
//! Cara remains repository-generic: ordinary PRs without Cacophony markers are
//! unchanged.  When a PR declares Cacophony generation metadata (or uses the
//! immutable `-pr-g<oid>` branch shape), this module groups only the same agent,
//! overlapping bead stream, and stack slot. Provider commit comparison then
//! proves one unique contained successor or fails the whole sibling set closed.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppError;
use crate::model::{CacophonyGenerationProvenance, CommitOid, PrNumber, PullRequestGenerationFact};

const MAX_STREAM_COMPARISONS: usize = 64;

/// Provider relationship for `base...head` source commits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommitRelation {
    /// `head` contains `base` and has additional commits.
    Ahead,
    /// `head` is an ancestor of `base`.
    Behind,
    Identical,
    Diverged,
    Unknown {
        reason: String,
    },
}

/// Admission disposition for one Cacophony-shaped open PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GenerationDisposition {
    CurrentGeneration,
    SupersededGeneration,
    AmbiguousGeneration,
    InvalidGenerationMetadata,
}

/// Bounded evidence explaining one generation classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GenerationFinding {
    pub pr: PrNumber,
    pub disposition: GenerationDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_pr: Option<PrNumber>,
    #[serde(default)]
    pub related_prs: Vec<PrNumber>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default)]
    pub bead_ids: BTreeSet<String>,
    pub reason: String,
    pub safe_next_action: String,
}

/// Complete deterministic classification for one fresh open-PR snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GenerationIntegrityStatus {
    pub schema_version: u32,
    pub fingerprint: String,
    #[serde(default)]
    pub findings: Vec<GenerationFinding>,
    pub comparisons_used: usize,
    pub comparisons_truncated: bool,
}

impl Default for GenerationIntegrityStatus {
    fn default() -> Self {
        Self {
            schema_version: 1,
            fingerprint: crate::membership::fnv1a64(b"[]"),
            findings: Vec::new(),
            comparisons_used: 0,
            comparisons_truncated: false,
        }
    }
}

impl GenerationIntegrityStatus {
    #[must_use]
    pub fn finding(&self, pr: PrNumber) -> Option<&GenerationFinding> {
        self.findings.iter().find(|finding| finding.pr == pr)
    }
}

/// Parse bounded provider body markers into exact generation facts.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn parse_generation_fact(
    pr: PrNumber,
    provider_head: CommitOid,
    head_ref: &str,
    created_at: Option<String>,
    body: &str,
) -> PullRequestGenerationFact {
    let shaped_branch = head_ref
        .rsplit_once("-pr-g")
        .is_some_and(|(_, suffix)| is_oid(suffix));
    let has_marker = body.lines().any(|line| {
        line.trim_start().starts_with("Cacophony-Generation:")
            || line.trim_start().starts_with("Cacophony-Agent:")
            || line.trim_start().starts_with("Cacophony-Head:")
            || line.trim_start().starts_with("Cacophony-Stack-Base:")
            || line.trim_start().starts_with("Cacophony-Stack-State:")
    });
    if !shaped_branch && !has_marker {
        return PullRequestGenerationFact {
            pr,
            provider_head,
            created_at,
            provenance: None,
            metadata_error: None,
            supersedes: BTreeSet::new(),
        };
    }

    let field = |name: &str| metadata_value(body, name);
    let generation = field("Cacophony-Generation");
    let agent = field("Cacophony-Agent");
    let source_head = field("Cacophony-Head");
    let stack_base = field("Cacophony-Stack-Base");
    let stack_state = field("Cacophony-Stack-State");
    let bead_ids = parse_beads(body);
    let mut missing = Vec::new();
    for (name, value) in [
        ("Cacophony-Generation", generation.as_deref()),
        ("Cacophony-Agent", agent.as_deref()),
        ("Cacophony-Head", source_head.as_deref()),
        ("Cacophony-Stack-Base", stack_base.as_deref()),
        ("Cacophony-Stack-State", stack_state.as_deref()),
    ] {
        if value.is_none_or(str::is_empty) {
            missing.push(name);
        }
    }
    if bead_ids.is_empty() {
        missing.push("Beads");
    }
    if !missing.is_empty() {
        return invalid_fact(
            pr,
            provider_head,
            created_at,
            format!("missing exact generation metadata: {}", missing.join(", ")),
        );
    }

    let generation = generation.expect("checked");
    let agent = agent.expect("checked");
    let source_head = source_head.expect("checked");
    let stack_base = stack_base.expect("checked");
    let stack_state = stack_state.expect("checked");
    if generation.len() > 512
        || agent.len() > 256
        || stack_base.len() > 512
        || stack_state.len() > 64
        || !safe_identity(&generation)
        || !safe_identity(&agent)
        || !safe_identity(&stack_base)
        || !safe_identity(&stack_state)
    {
        return invalid_fact(
            pr,
            provider_head,
            created_at,
            "generation metadata contains an oversized or unsafe identity".to_owned(),
        );
    }
    if !is_oid(&source_head) {
        return invalid_fact(
            pr,
            provider_head,
            created_at,
            "Cacophony-Head is not one exact 40-character OID".to_owned(),
        );
    }
    if !generation.ends_with(&source_head) || generation != head_ref {
        return invalid_fact(
            pr,
            provider_head,
            created_at,
            "Cacophony-Generation must equal the provider head ref and end in Cacophony-Head"
                .to_owned(),
        );
    }
    if !matches!(stack_state.as_str(), "root" | "blocked_on_parent") {
        return invalid_fact(
            pr,
            provider_head,
            created_at,
            "Cacophony-Stack-State must be root or blocked_on_parent".to_owned(),
        );
    }
    PullRequestGenerationFact {
        pr,
        provider_head,
        created_at,
        provenance: Some(CacophonyGenerationProvenance {
            generation,
            agent,
            source_head: CommitOid(source_head),
            bead_ids,
            stack_base,
            stack_state,
        }),
        metadata_error: None,
        supersedes: BTreeSet::new(),
    }
}

fn invalid_fact(
    pr: PrNumber,
    provider_head: CommitOid,
    created_at: Option<String>,
    message: String,
) -> PullRequestGenerationFact {
    PullRequestGenerationFact {
        pr,
        provider_head,
        created_at,
        provenance: None,
        metadata_error: Some(message),
        supersedes: BTreeSet::new(),
    }
}

fn metadata_value(body: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}:");
    body.lines().find_map(|line| {
        let value = line.trim().strip_prefix(&prefix)?.trim();
        let value = value
            .strip_prefix('`')
            .and_then(|value| value.strip_suffix('`'))
            .unwrap_or(value)
            .trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

fn parse_beads(body: &str) -> BTreeSet<String> {
    let Some(value) = body.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Beads:")
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }) else {
        return BTreeSet::new();
    };
    value
        .split(|character: char| character == ',' || character.is_ascii_whitespace())
        .filter(|value| {
            value.strip_prefix("bd-").is_some_and(|suffix| {
                suffix.len() == 6 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        })
        .take(32)
        .map(str::to_ascii_lowercase)
        .collect()
}

fn is_oid(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn safe_identity(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-' | b':' | b'@')
        })
}

fn same_stream_slot(
    left: &CacophonyGenerationProvenance,
    right: &CacophonyGenerationProvenance,
) -> bool {
    if left.agent != right.agent || left.bead_ids.is_disjoint(&right.bead_ids) {
        return false;
    }
    // A stacked child and its declared parent are separate queue slots even
    // when one agent legitimately carries the same bead through both PRs.
    if left.generation == right.stack_base || right.generation == left.stack_base {
        return false;
    }
    match (left.stack_state.as_str(), right.stack_state.as_str()) {
        ("root", "root") | ("blocked_on_parent", "blocked_on_parent") => {
            left.stack_base == right.stack_base
        }
        _ => false,
    }
}

/// Return PRs which need bounded provider-visible link inspection because at
/// least one same-stream stack-slot sibling exists.
#[must_use]
pub fn duplicate_stream_prs(facts: &[PullRequestGenerationFact]) -> BTreeSet<PrNumber> {
    let valid = facts
        .iter()
        .filter_map(|fact| {
            fact.provenance
                .as_ref()
                .map(|provenance| (fact.pr, provenance))
        })
        .collect::<Vec<_>>();
    let mut duplicates = BTreeSet::new();
    for (index, (left_pr, left)) in valid.iter().enumerate() {
        for (right_pr, right) in valid.iter().skip(index + 1) {
            if same_stream_slot(left, right) {
                duplicates.insert(*left_pr);
                duplicates.insert(*right_pr);
            }
        }
    }
    duplicates
}

/// Attach reviewed provider-visible canonical-generation links. The legacy
/// dogfood controller receipt is accepted only with its deterministic Caravan
/// priority marker, exact controller actor, and explicit superseding PR list.
pub fn attach_reviewed_supersession_links(
    facts: &mut [PullRequestGenerationFact],
    canonical_pr: PrNumber,
    comment_bodies: &[String],
) {
    let marker = format!("<!-- caravan-control-label-audit:v2:priority_set:{canonical_pr}:");
    let priority_prefix = "<!-- caravan-control-label-audit:v2:priority_";
    let supersedes = comment_bodies
        .iter()
        .rev()
        .find(|body| body.contains(priority_prefix))
        .and_then(|body| {
            if !body.contains(&marker)
                || !body.contains("- **Actor/source:** caravan-dogfood-controller")
            {
                return None;
            }
            let reason = body
                .lines()
                .find_map(|line| line.trim().strip_prefix("- **Reason:**").map(str::trim))?;
            let suffix = reason.split_once("superseding PRs ")?.1;
            let values = suffix
                .split(|character: char| !character.is_ascii_digit())
                .filter_map(|value| value.parse::<u64>().ok())
                .filter(|number| *number != 0 && *number != canonical_pr.0)
                .map(PrNumber)
                .collect::<BTreeSet<_>>();
            (!values.is_empty()).then_some(values)
        });
    if let Some(supersedes) = supersedes
        && let Some(fact) = facts.iter_mut().find(|fact| fact.pr == canonical_pr)
    {
        fact.supersedes = supersedes;
    }
}

fn record_canonical_component(
    findings: &mut BTreeMap<PrNumber, GenerationFinding>,
    valid: &BTreeMap<PrNumber, (&PullRequestGenerationFact, &CacophonyGenerationProvenance)>,
    component: &[PrNumber],
    canonical: PrNumber,
    authority: &str,
) {
    for pr in component {
        let (fact, provenance) = valid[pr];
        if *pr == canonical {
            let mut finding = current_finding(fact, provenance, component.to_vec());
            finding.reason = format!(
                "unique canonical generation for this exact Cacophony agent/bead/stack slot ({authority})"
            );
            findings.insert(*pr, finding);
        } else {
            findings.insert(
                *pr,
                GenerationFinding {
                    pr: *pr,
                    disposition: GenerationDisposition::SupersededGeneration,
                    canonical_pr: Some(canonical),
                    related_prs: component.to_vec(),
                    agent: Some(provenance.agent.clone()),
                    bead_ids: provenance.bead_ids.clone(),
                    reason: format!(
                        "generation {} is superseded by canonical open PR #{canonical} ({authority})",
                        provenance.generation
                    ),
                    safe_next_action: format!(
                        "preserve PR #{canonical}; after owner review, close or reflect PR #{pr} without admitting it. Cara never auto-closes generations"
                    ),
                },
            );
        }
    }
}

/// Classify all exact open generation facts. Provider comparisons are bounded
/// and policy-free; unknown/error relations make the affected stream ambiguous.
#[allow(clippy::too_many_lines)]
pub fn analyze(
    facts: &[PullRequestGenerationFact],
    mut compare: impl FnMut(&CommitOid, &CommitOid) -> CommitRelation,
) -> GenerationIntegrityStatus {
    let mut findings = BTreeMap::new();
    let mut valid = BTreeMap::new();
    for fact in facts {
        if let Some(error) = &fact.metadata_error {
            findings.insert(
                fact.pr,
                GenerationFinding {
                    pr: fact.pr,
                    disposition: GenerationDisposition::InvalidGenerationMetadata,
                    canonical_pr: None,
                    related_prs: vec![fact.pr],
                    agent: None,
                    bead_ids: BTreeSet::new(),
                    reason: error.clone(),
                    safe_next_action: "repair exact Cacophony PR metadata or close the stale PR after owner review; Cara never auto-closes it".to_owned(),
                },
            );
        } else if let Some(provenance) = &fact.provenance {
            valid.insert(fact.pr, (fact, provenance));
        }
    }

    let mut unseen = valid.keys().copied().collect::<BTreeSet<_>>();
    let mut components = Vec::new();
    while let Some(start) = unseen.pop_first() {
        let mut component = Vec::new();
        let mut queue = VecDeque::from([start]);
        while let Some(current) = queue.pop_front() {
            component.push(current);
            let peers = unseen
                .iter()
                .copied()
                .filter(|peer| same_stream_slot(valid[&current].1, valid[peer].1))
                .collect::<Vec<_>>();
            for peer in peers {
                unseen.remove(&peer);
                queue.push_back(peer);
            }
        }
        component.sort_unstable();
        components.push(component);
    }

    let mut comparisons_used = 0;
    let mut comparisons_truncated = false;
    for component in components {
        if component.len() == 1 {
            let pr = component[0];
            let (fact, provenance) = valid[&pr];
            findings.insert(pr, current_finding(fact, provenance, vec![pr]));
            continue;
        }
        let linked_canonicals = component
            .iter()
            .copied()
            .filter(|candidate| {
                component.iter().copied().all(|other| {
                    other == *candidate || valid[candidate].0.supersedes.contains(&other)
                })
            })
            .collect::<Vec<_>>();
        if let [canonical] = linked_canonicals.as_slice() {
            record_canonical_component(
                &mut findings,
                &valid,
                &component,
                *canonical,
                "reviewed provider-visible canonical supersession link",
            );
            continue;
        }
        if linked_canonicals.len() > 1 {
            for pr in &component {
                let (_, provenance) = valid[pr];
                findings.insert(
                    *pr,
                    GenerationFinding {
                        pr: *pr,
                        disposition: GenerationDisposition::AmbiguousGeneration,
                        canonical_pr: None,
                        related_prs: component.clone(),
                        agent: Some(provenance.agent.clone()),
                        bead_ids: provenance.bead_ids.clone(),
                        reason: format!(
                            "conflicting reviewed canonical-generation links name PRs {linked_canonicals:?}"
                        ),
                        safe_next_action: "the owning controller must revoke the conflicting links and select one exact canonical generation".to_owned(),
                    },
                );
            }
            continue;
        }
        let mut relations = BTreeMap::new();
        for (index, left) in component.iter().enumerate() {
            for right in component.iter().skip(index + 1) {
                let relation = if comparisons_used >= MAX_STREAM_COMPARISONS {
                    comparisons_truncated = true;
                    CommitRelation::Unknown {
                        reason: format!(
                            "generation comparison bound {MAX_STREAM_COMPARISONS} exhausted"
                        ),
                    }
                } else {
                    comparisons_used += 1;
                    compare(&valid[left].1.source_head, &valid[right].1.source_head)
                };
                relations.insert((*left, *right), relation);
            }
        }

        let contains = |candidate: PrNumber, other: PrNumber| {
            if candidate == other {
                return true;
            }
            let (low, high, reversed) = if other < candidate {
                (other, candidate, false)
            } else {
                (candidate, other, true)
            };
            match relations.get(&(low, high)) {
                Some(CommitRelation::Identical) => true,
                Some(CommitRelation::Ahead) => !reversed,
                Some(CommitRelation::Behind) => reversed,
                Some(CommitRelation::Diverged | CommitRelation::Unknown { .. }) | None => false,
            }
        };
        let mut maxima = component
            .iter()
            .copied()
            .filter(|candidate| {
                component
                    .iter()
                    .copied()
                    .all(|other| contains(*candidate, other))
            })
            .collect::<Vec<_>>();
        if maxima.len() > 1 {
            // Multiple maxima can only be identical source generations. The
            // immutable provider creation key then chooses one exact newest PR.
            maxima.sort_by_key(|pr| {
                let fact = valid[pr].0;
                (fact.created_at.clone().unwrap_or_default(), pr.0)
            });
            maxima = vec![*maxima.last().expect("non-empty")];
        }
        if let [canonical] = maxima.as_slice() {
            record_canonical_component(
                &mut findings,
                &valid,
                &component,
                *canonical,
                "exact source containment",
            );
        } else {
            let mut relation_rows = Vec::new();
            let mut unproved_pairs = Vec::new();
            let mut diverged = false;
            for (index, left) in component.iter().enumerate() {
                for right in component.iter().skip(index + 1) {
                    let relation = relations.get(&(*left, *right));
                    match relation {
                        Some(CommitRelation::Diverged) => diverged = true,
                        Some(CommitRelation::Unknown { reason }) => {
                            unproved_pairs.push(format!("#{left}...#{right} ({reason})"));
                        }
                        _ => {}
                    }
                    relation_rows.push(format!("{left}...{right}={relation:?}"));
                }
            }
            let relation_evidence = relation_rows.join(", ");
            // bd-7546ea: an unreachable provider comparison is not divergence.
            // Report the exact unprovable pairs instead of declaring the whole
            // stream divergent, so one 404 cannot dead-end every sibling.
            let (reason, safe_next_action) = if diverged || unproved_pairs.is_empty() {
                (
                    format!("same-stream open generations are divergent or unproved: {relation_evidence}"),
                    "the owning agent/controller must choose one exact canonical generation; do not admit, close, or rewrite any sibling automatically".to_owned(),
                )
            } else {
                (
                    format!(
                        "same-stream ancestry is unproved because {} provider comparison(s) were unreachable: {}",
                        unproved_pairs.len(),
                        unproved_pairs.join(", ")
                    ),
                    "no divergence was observed; re-run discovery once the referenced commits are reachable, or declare the exact canonical generation. Cara excludes these rows without wedging selection".to_owned(),
                )
            };
            for pr in &component {
                let (_, provenance) = valid[pr];
                findings.insert(
                    *pr,
                    GenerationFinding {
                        pr: *pr,
                        disposition: GenerationDisposition::AmbiguousGeneration,
                        canonical_pr: None,
                        related_prs: component.clone(),
                        agent: Some(provenance.agent.clone()),
                        bead_ids: provenance.bead_ids.clone(),
                        reason: reason.clone(),
                        safe_next_action: safe_next_action.clone(),
                    },
                );
            }
        }
    }
    let findings = findings.into_values().collect::<Vec<_>>();
    let mut fingerprint_facts = facts.to_vec();
    fingerprint_facts.sort_by_key(|fact| fact.pr);
    let fingerprint = crate::membership::fnv1a64(
        &serde_json::to_vec(&json!({
            "schema_version": 1,
            "facts": fingerprint_facts,
            "findings": findings,
        }))
        .expect("generation integrity serializes"),
    );
    GenerationIntegrityStatus {
        schema_version: 1,
        fingerprint,
        findings,
        comparisons_used,
        comparisons_truncated,
    }
}

fn current_finding(
    fact: &PullRequestGenerationFact,
    provenance: &CacophonyGenerationProvenance,
    related_prs: Vec<PrNumber>,
) -> GenerationFinding {
    GenerationFinding {
        pr: fact.pr,
        disposition: GenerationDisposition::CurrentGeneration,
        canonical_pr: Some(fact.pr),
        related_prs,
        agent: Some(provenance.agent.clone()),
        bead_ids: provenance.bead_ids.clone(),
        reason: "unique newest contained generation for this exact Cacophony agent/bead/stack slot"
            .to_owned(),
        safe_next_action:
            "normal admission preflight may continue under immediate generation revalidation"
                .to_owned(),
    }
}

/// Fail closed for every non-current Cacophony generation disposition.
pub fn require_admissible(
    integrity: &GenerationIntegrityStatus,
    pr: PrNumber,
) -> Result<(), AppError> {
    let Some(finding) = integrity.finding(pr) else {
        return Ok(());
    };
    let (code, message) = match finding.disposition {
        GenerationDisposition::CurrentGeneration => return Ok(()),
        GenerationDisposition::SupersededGeneration => (
            "superseded_generation",
            "selected PR is an older contained Cacophony generation",
        ),
        GenerationDisposition::AmbiguousGeneration => (
            "ambiguous_generation",
            "selected PR has divergent or unproved same-stream Cacophony siblings",
        ),
        GenerationDisposition::InvalidGenerationMetadata => (
            "invalid_generation_metadata",
            "selected PR has incomplete or invalid Cacophony generation metadata",
        ),
    };
    Err(AppError::structured(
        mcp_cli::ErrorCategory::Validation,
        code,
        message,
        Some(json!({
            "pr": pr,
            "generation_integrity": integrity,
            "finding": finding,
            "mutated": false,
            "safe_next_action": finding.safe_next_action,
        })),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(character: char) -> CommitOid {
        CommitOid(character.to_string().repeat(40))
    }

    fn fact(
        pr: u64,
        agent: &str,
        beads: &[&str],
        source: char,
        created: &str,
    ) -> PullRequestGenerationFact {
        let source_head = oid(source);
        let generation = format!("agent/{agent}-pr-g{}", source_head.0);
        PullRequestGenerationFact {
            pr: PrNumber(pr),
            provider_head: oid(char::from_u32(u32::from(source) + 1).unwrap_or(source)),
            created_at: Some(created.to_owned()),
            provenance: Some(CacophonyGenerationProvenance {
                generation,
                agent: agent.to_owned(),
                source_head,
                bead_ids: beads.iter().map(|value| (*value).to_owned()).collect(),
                stack_base: "main".to_owned(),
                stack_state: "root".to_owned(),
            }),
            metadata_error: None,
            supersedes: BTreeSet::new(),
        }
    }

    #[test]
    fn parses_live_cacophony_metadata_and_rejects_partial_shape() {
        let head = "b".repeat(40);
        let generation = format!("agent/ms-dev/caco-pr-g{head}");
        let body = format!(
            "Agent: x\nBeads: bd-c7440c, bd-4734d1\n\nCacophony-Generation: `{generation}`\nCacophony-Agent: `agent-a`\nCacophony-Head: `{head}`\nCacophony-Stack-Base: `main`\nCacophony-Stack-State: `root`"
        );
        let parsed = parse_generation_fact(
            PrNumber(2123),
            oid('c'),
            &generation,
            Some("2026-07-23T18:34:41Z".to_owned()),
            &body,
        );
        let provenance = parsed.provenance.unwrap();
        assert_eq!(provenance.agent, "agent-a");
        assert_eq!(
            provenance.bead_ids,
            BTreeSet::from(["bd-4734d1".to_owned(), "bd-c7440c".to_owned()])
        );

        let ordinary = parse_generation_fact(
            PrNumber(99),
            oid('e'),
            "feature/ordinary",
            None,
            "ordinary pull request body without Cacophony ownership",
        );
        assert!(ordinary.provenance.is_none());
        assert!(ordinary.metadata_error.is_none());

        let partial = parse_generation_fact(
            PrNumber(2107),
            oid('d'),
            &format!("agent/old-pr-g{}", "a".repeat(40)),
            None,
            "Beads: bd-c7440c\nCacophony-Agent: `agent-a`",
        );
        assert!(partial.provenance.is_none());
        assert!(partial.metadata_error.unwrap().contains("missing"));
    }

    #[test]
    fn live_2107_2123_shape_marks_only_contained_older_generation_superseded() {
        let old = fact(
            2107,
            "android-msd-0",
            &["bd-c7440c"],
            'a',
            "2026-07-23T01:50:32Z",
        );
        let new = fact(
            2123,
            "android-msd-0",
            &["bd-c7440c", "bd-4734d1"],
            'b',
            "2026-07-23T18:34:41Z",
        );
        let status = analyze(&[old, new], |base, head| {
            if base == &oid('a') && head == &oid('b') {
                CommitRelation::Ahead
            } else {
                CommitRelation::Behind
            }
        });
        assert_eq!(
            status.finding(PrNumber(2107)).unwrap().disposition,
            GenerationDisposition::SupersededGeneration
        );
        assert_eq!(
            status.finding(PrNumber(2107)).unwrap().canonical_pr,
            Some(PrNumber(2123))
        );
        assert_eq!(
            status.finding(PrNumber(2123)).unwrap().disposition,
            GenerationDisposition::CurrentGeneration
        );
    }

    #[test]
    fn reviewed_dogfood_link_proves_2123_over_2107_without_source_object_access() {
        let old = fact(
            2107,
            "android-agent",
            &["bd-c7440c"],
            'a',
            "2026-07-23T01:50:32Z",
        );
        let mut canonical = fact(
            2123,
            "android-agent",
            &["bd-c7440c", "bd-4734d1"],
            'b',
            "2026-07-23T18:34:41Z",
        );
        let comment = "<!-- caravan-control-label-audit:v2:priority_set:2123:abc -->\n### Caravan queue transition: `priority_set`\n\n- **Actor/source:** caravan-dogfood-controller\n- **Reason:** Owner-confirmed canonical Android generation superseding PRs 2107 (explicit cara priority command)".to_owned();
        attach_reviewed_supersession_links(
            std::slice::from_mut(&mut canonical),
            PrNumber(2123),
            std::slice::from_ref(&comment),
        );
        assert_eq!(canonical.supersedes, BTreeSet::from([PrNumber(2107)]));
        let mut revoked = canonical.clone();
        revoked.supersedes.clear();
        attach_reviewed_supersession_links(
            std::slice::from_mut(&mut revoked),
            PrNumber(2123),
            &[
                comment.clone(),
                "<!-- caravan-control-label-audit:v2:priority_clear:2123:def -->\n### Caravan queue transition: `priority_clear`".to_owned(),
            ],
        );
        assert!(
            revoked.supersedes.is_empty(),
            "latest priority clear revokes the link"
        );
        let status = analyze(&[old, canonical], |_base, _head| {
            panic!("complete reviewed link avoids unavailable source comparison")
        });
        assert_eq!(
            status.finding(PrNumber(2107)).unwrap().disposition,
            GenerationDisposition::SupersededGeneration
        );
        assert_eq!(
            status.finding(PrNumber(2107)).unwrap().canonical_pr,
            Some(PrNumber(2123))
        );
    }

    #[test]
    fn same_bead_across_agents_and_stacked_parent_child_are_distinct_slots() {
        let left = fact(1, "agent-a", &["bd-aaaaaa"], 'a', "1");
        let right = fact(2, "agent-b", &["bd-aaaaaa"], 'b', "2");
        let separate_agents = analyze(&[left, right], |_base, _head| {
            panic!("unrelated agents are never compared")
        });
        assert!(
            separate_agents
                .findings
                .iter()
                .all(|finding| finding.disposition == GenerationDisposition::CurrentGeneration)
        );

        let parent = fact(3, "agent-a", &["bd-bbbbbb"], 'c', "3");
        let mut child = fact(4, "agent-a", &["bd-bbbbbb"], 'd', "4");
        child.provenance.as_mut().unwrap().stack_state = "blocked_on_parent".to_owned();
        child.provenance.as_mut().unwrap().stack_base =
            parent.provenance.as_ref().unwrap().generation.clone();
        let stacked = analyze(&[parent, child], |_base, _head| {
            panic!("declared stack slots are never compared as replacements")
        });
        assert!(
            stacked
                .findings
                .iter()
                .all(|finding| finding.disposition == GenerationDisposition::CurrentGeneration)
        );
    }

    /// bd-7546ea: an unreachable provider comparison is reported as exactly
    /// that, naming the unprovable pair, instead of asserting divergence.
    #[test]
    fn unreachable_comparison_is_reported_as_unproved_not_divergent() {
        let status = analyze(
            &[
                fact(1, "agent-a", &["bd-aaaaaa"], 'a', "1"),
                fact(2, "agent-a", &["bd-aaaaaa"], 'b', "2"),
            ],
            |_base, _head| CommitRelation::Unknown {
                reason: "HTTP 404: No commit found for SHA".to_owned(),
            },
        );

        assert!(status.findings.iter().all(|finding| {
            finding.disposition == GenerationDisposition::AmbiguousGeneration
                && finding.canonical_pr.is_none()
        }));
        let finding = &status.findings[0];
        assert!(
            finding.reason.contains("unproved because"),
            "reason should name unreachability: {}",
            finding.reason
        );
        assert!(finding.reason.contains("#1...#2"));
        assert!(finding.reason.contains("404"));
        assert!(
            !finding.reason.contains("divergent"),
            "an unreachable comparison must not be reported as divergence"
        );
        assert!(
            finding
                .safe_next_action
                .contains("no divergence was observed")
        );
        assert!(
            finding
                .safe_next_action
                .contains("without wedging selection")
        );
    }

    /// A genuinely diverged component keeps the strict fail-closed wording.
    #[test]
    fn divergence_keeps_the_strict_failure_reason() {
        let status = analyze(
            &[
                fact(1, "agent-a", &["bd-aaaaaa"], 'a', "1"),
                fact(2, "agent-a", &["bd-aaaaaa"], 'b', "2"),
            ],
            |_base, _head| CommitRelation::Diverged,
        );

        let finding = &status.findings[0];
        assert_eq!(
            finding.disposition,
            GenerationDisposition::AmbiguousGeneration
        );
        assert!(finding.reason.contains("divergent or unproved"));
        assert!(
            finding
                .safe_next_action
                .contains("choose one exact canonical")
        );
    }

    #[test]
    fn divergent_or_unknown_siblings_fail_closed_without_canonical_guess() {
        for relation in [
            CommitRelation::Diverged,
            CommitRelation::Unknown {
                reason: "provider unavailable".to_owned(),
            },
        ] {
            let status = analyze(
                &[
                    fact(1, "agent-a", &["bd-aaaaaa"], 'a', "1"),
                    fact(2, "agent-a", &["bd-aaaaaa"], 'b', "2"),
                ],
                |_base, _head| relation.clone(),
            );
            assert!(status.findings.iter().all(|finding| {
                finding.disposition == GenerationDisposition::AmbiguousGeneration
                    && finding.canonical_pr.is_none()
            }));
        }
    }

    #[test]
    fn identical_sources_choose_immutable_newest_pr_and_never_auto_close() {
        let mut old = fact(1, "agent-a", &["bd-aaaaaa"], 'a', "1");
        let newer = fact(2, "agent-a", &["bd-aaaaaa"], 'a', "2");
        old.provider_head = oid('c');
        let status = analyze(&[old, newer], |_base, _head| CommitRelation::Identical);
        let old = status.finding(PrNumber(1)).unwrap();
        assert_eq!(old.disposition, GenerationDisposition::SupersededGeneration);
        assert_eq!(old.canonical_pr, Some(PrNumber(2)));
        assert!(old.safe_next_action.contains("never auto-closes"));
    }
}
