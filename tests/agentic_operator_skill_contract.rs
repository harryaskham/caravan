const SKILL: &str = include_str!("../.agents/skills/cara-operator/SKILL.md");
const README: &str = include_str!("../README.md");
const SPEC: &str = include_str!("../SPEC.md");
const CANARY: &str = include_str!("../.agents/skills/cara-operator/references/safe-path-canary.md");

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
    assert!(spec.contains("legacy evidence/fixtures, not the execution path"));
    assert!(skill.contains("references/safe-path-canary.md"));
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
