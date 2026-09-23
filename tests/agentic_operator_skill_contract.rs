const SKILL: &str = include_str!("../.agents/skills/cara-operator/SKILL.md");
const README: &str = include_str!("../README.md");
const SPEC: &str = include_str!("../SPEC.md");
const CANARY: &str = include_str!("../.agents/skills/cara-operator/references/safe-path-canary.md");
const MONITORING: &str =
    include_str!("../.agents/skills/cara-operator/references/queue-monitoring.md");

fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn skill_is_short_and_defers_to_live_config_aware_cara_authority() {
    assert!(SKILL.starts_with("---\nname: cara-operator\n"));
    assert!(
        SKILL.lines().count() <= 100,
        "skill became a second handbook"
    );
    let skill = normalized(SKILL);
    for required in [
        "cara help --json",
        "MCP `help`",
        "live help wins",
        "cara config check",
        "cara status --json",
        "cara plan sync --all",
        "Rediscover first",
    ] {
        assert!(
            skill.contains(required),
            "missing live authority `{required}`"
        );
    }
}

#[test]
fn skill_routes_typed_dispositions_without_becoming_a_writer() {
    let skill = normalized(SKILL);
    for required in [
        "`retry_tick`",
        "`external_decision`",
        "cara init",
        "`cara new`, `join`, `rejoin`, or `renew`",
        "top-eviction/reshape",
        "first-party Cara repair",
        "failed caller receipt can follow a successful provider mutation",
        "One blocked generation",
    ] {
        assert!(
            skill.contains(required),
            "missing routing rule `{required}`"
        );
    }
    for denied in [
        "raw `git`",
        "direct GitHub merge/label/base changes",
        "generic authenticated shell",
        "admin bypass",
        "check spoofing",
        "second merge actor",
        "manually add/remove Caravan control labels",
    ] {
        assert!(skill.contains(denied), "missing denial `{denied}`");
    }
}

#[test]
fn skill_handoff_is_exact_secret_free_and_documented_as_current_path() {
    let skill = normalized(SKILL);
    let readme = normalized(README);
    let spec = normalized(SPEC);
    for required in [
        "exact head/base/main/check generations",
        "operation/plan/dead-letter receipts",
        "mutations performed (or `none`)",
        "GitHub App keys",
        "untrusted data",
        "Do not claim success",
    ] {
        assert!(
            skill.contains(required),
            "missing handoff rule `{required}`"
        );
    }
    assert!(readme.contains("`.agents/skills/cara-operator/SKILL.md` is the supported"));
    assert!(spec.contains("`.agents/skills/cara-operator/SKILL.md` is the supported"));
    assert!(readme.contains("legacy fixtures/evidence, not the autonomous execution path"));
    assert!(readme.contains(".agents/skills/cara-operator/references/queue-monitoring.md"));
    assert!(spec.contains("legacy evidence/fixtures, not the execution path"));
    assert!(skill.contains("references/safe-path-canary.md"));
    assert!(skill.contains("references/queue-monitoring.md) before acting"));
    for evidence in [
        "github_stack_partial_prefix_requires_tail_eviction",
        "mutations: none",
        "019fff1c-f242-7ba0-8af3-f2ca1c38e114",
        "performed no provider mutation",
        "unsafe direct rescue",
    ] {
        assert!(
            CANARY.contains(evidence),
            "missing canary evidence `{evidence}`"
        );
    }
}

#[test]
fn skill_and_monitoring_require_ready_prefix_decisions_not_endless_retry_reports() {
    let guide = normalized(&format!("{SKILL}\n{MONITORING}"));
    for required in [
        "qualified prefix",
        "Do not make ready parents wait indefinitely",
        "Do not assume healthy parents authorize a raw merge",
        "sealed partial-prefix",
        "whole member chain before it ever considers landing the prefix",
        "stable decision fingerprint",
        "next scheduled pass",
        "do not rewrite the producer's typed disposition",
        "By the next verification point",
        "Search existing open and closed work",
    ] {
        assert!(
            guide.to_lowercase().contains(&required.to_lowercase()),
            "missing liveness rule `{required}`"
        );
    }
}

#[test]
fn monitoring_requires_acknowledged_exclusive_custody_and_preservation() {
    let guide = normalized(MONITORING);
    for required in [
        "monitoring does not enroll them into Cara",
        "existing scheduler, loop, source owner, and recovery task",
        "A request to keep monitoring is not approval",
        "A capacity acknowledgement is not assignment",
        "explicit sequencing with WIP preservation",
        "Do not flush/replay queued input",
        "held or unvalidated local work",
        "Honor writer-lock contention and ownership fences",
        "A timeout or failed caller receipt can follow a successful provider mutation",
        "Resume only the supported uncompleted continuation",
    ] {
        assert!(
            guide.contains(required),
            "missing custody rule `{required}`"
        );
    }
}

#[test]
fn monitoring_keeps_generation_ci_and_delivery_acceptance_separate() {
    let guide = normalized(MONITORING);
    for required in [
        "A PR base projection can remain stale while the actual branch tip moves",
        "unevaluated, never green",
        "Repeatedly rerunning the stale event",
        "Do not carry an old retry authorization onto a new source generation",
        "at most the approved operation",
        "read back terminal state",
        "Credit it as operator intervention, not queue automation success",
        "a manual parent merge does not itself approve a child takeover",
        "patch-id alone is not a complete integration proof",
        "source merge, current-base CI qualification, immutable release",
        "installed bytes, running service identity, live/device acceptance and fleet rollout",
        "Re-read it after compaction before resuming",
    ] {
        assert!(
            guide.to_lowercase().contains(&required.to_lowercase()),
            "missing evidence rule `{required}`"
        );
    }
}

#[test]
fn historical_canary_is_not_authority_for_a_second_scheduler() {
    let canary = normalized(CANARY);
    assert!(canary.contains("historical evidence, not a reusable operation receipt"));
    assert!(canary.contains("existing designated writer"));
    assert!(canary.contains("[the monitoring guide](queue-monitoring.md)"));
}
