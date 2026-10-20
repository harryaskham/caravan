//! bd-78d4aa: the versioned membership/join receipt wire contract shared with
//! Cacophony.
//!
//! On 2026-10-20 a cosmetic spelling difference (`force_intent: "none"` versus
//! a consumer allowlist of `"absent"`) refused seven completions whose PRs the
//! provider already showed enrolled. These tests make the producer side of that
//! contract explicit and stable:
//!
//! - checked-in fixtures under `tests/fixtures/receipts/join-v1/` are real
//!   producer output (generated from Cara 0.0.143's `execute` and
//!   `build_join_receipt` against the hermetic provider) and must still
//!   round-trip byte-for-byte through the current types;
//! - the current producer must emit the same wire shape (field names, enum
//!   spellings and nesting) as those fixtures, apart from per-run operation
//!   identities and the hashes derived from them;
//! - `docs/receipts/join-receipt.v1.schema.json` is the generated JSON Schema
//!   of the join receipt and must match the code.
//!
//! An intentional wire change regenerates both with
//! `CARA_UPDATE_RECEIPT_CONTRACT=1 cargo test --lib receipt_contract`, which
//! makes the change visible in review and in the release notes Caco adopts.

use super::*;

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/receipts/join-v1"
);
const SCHEMA_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/docs/receipts/join-receipt.v1.schema.json"
);
const FIXTURES: [&str; 3] = ["new", "join", "join-preserved"];

fn update_requested() -> bool {
    std::env::var_os("CARA_UPDATE_RECEIPT_CONTRACT").is_some_and(|value| value == "1")
}

fn read_fixture(name: &str) -> serde_json::Value {
    let path = format!("{FIXTURE_DIR}/{name}.json");
    serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {path}: {error}")),
    )
    .unwrap_or_else(|error| panic!("parse {path}: {error}"))
}

/// Recompute a join receipt hash exactly as the producer does: the typed
/// receipt serialized in declaration order with `receipt_hash` omitted.
fn producer_hash(receipt: &JoinReceipt) -> String {
    let mut unsigned = receipt.clone();
    unsigned.receipt_hash.clear();
    fnv1a64(&serde_json::to_vec(&unsigned).expect("receipt serializes"))
}

/// Replace values that legitimately differ on every run: operation identities
/// and the hashes derived from them. Everything else is wire contract.
fn normalize_volatile(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if matches!(key.as_str(), "operation_id" | "receipt_hash") {
                    *child = serde_json::Value::String(format!("<{key}>"));
                } else {
                    normalize_volatile(child);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(normalize_volatile),
        _ => {}
    }
}

/// Run the real producer for one fixture scenario against the hermetic
/// provider. Mirrors the generator that produced the checked-in fixtures.
fn produce(name: &str) -> MembershipOutput {
    let (operation, forced) = match name {
        "new" => (MembershipOperation::New, false),
        "join" => (MembershipOperation::Join, false),
        "join-preserved" => (MembershipOperation::Join, true),
        other => panic!("unknown receipt fixture {other}"),
    };
    let is_join = operation == MembershipOperation::Join;
    let mut candidate = pull_request(2, "candidate", "main", &[]);
    candidate.head.oid = CommitOid("1".repeat(40));
    candidate.base.oid = CommitOid("2".repeat(40));
    if forced {
        candidate.labels.insert(FORCE_LABEL.to_owned());
    }
    let mut tail = pull_request(1, "tail", "main", &[ACTIVE_LABEL]);
    tail.head.oid = CommitOid("3".repeat(40));
    tail.base.oid = candidate.base.oid.clone();
    let others = if is_join { vec![tail.clone()] } else { vec![] };
    let mut before = status(candidate.clone(), others.clone());
    before.analysis.fleet.default_branch.oid = candidate.base.oid.clone();
    let provider =
        FakeProvider::with_pull_requests(others.into_iter().chain([candidate.clone()]).collect());
    provider.branch_heads.borrow_mut().extend([
        ("main".to_owned(), candidate.base.oid.clone()),
        ("tail".to_owned(), tail.head.oid.clone()),
    ]);
    let mut output = execute(
        before.clone(),
        &clean,
        &provider,
        MembershipRequest {
            expected_admission: None,
            operation,
            create_pr: false,
            tail_pr: is_join.then_some(1),
            head_pr: None,
            reason: None,
            priority_label: None,
            agent_priority_labels: Vec::new(),
        },
    )
    .expect("hermetic membership succeeds");
    let predecessor = if is_join {
        JoinPredecessorReceipt {
            pr: tail.number,
            branch: tail.head.name.clone(),
            head_oid: tail.head.oid.clone(),
        }
    } else {
        JoinPredecessorReceipt {
            pr: PrNumber(0),
            branch: "main".to_owned(),
            head_oid: candidate.base.oid.clone(),
        }
    };
    let mut context = AppContext::default();
    context.config.rebase_on_join = false;
    let receipt = build_join_receipt(
        &context,
        &repository(),
        &before,
        JoinReceiptEvidence {
            expected_admission: None,
            immutable_ancestry_verified: false,
            predecessor: Some(predecessor),
            candidate_source_head_oid: Some(candidate.head.oid),
            source: None,
            default_branch_oid: candidate.base.oid,
            rebase_receipt: None,
        },
        &output,
    )
    .expect("Cara builds and hashes its own receipt");
    output.join_receipt = Some(receipt);
    output
}

#[test]
fn checked_in_v1_receipts_round_trip_exactly_and_verify_their_hash() {
    for name in FIXTURES {
        let wire = read_fixture(name);
        let typed: MembershipOutput = serde_json::from_value(wire.clone())
            .unwrap_or_else(|error| panic!("{name}: current types reject v1 wire: {error}"));
        assert_eq!(
            serde_json::to_value(&typed).unwrap(),
            wire,
            "{name}: re-serialization changed the v1 wire receipt; bump the contract deliberately"
        );
        let receipt = typed.join_receipt.expect("fixture carries a join receipt");
        assert_eq!(receipt.schema_version, 1, "{name}");
        assert_eq!(
            producer_hash(&receipt),
            receipt.receipt_hash,
            "{name}: producer hash recipe changed"
        );
        assert!(receipt.membership_durable, "{name}");
    }
}

#[test]
fn current_producer_emits_the_v1_wire_shape() {
    for name in FIXTURES {
        let produced = produce(name);
        let receipt = produced.join_receipt.as_ref().unwrap();
        assert_eq!(producer_hash(receipt), receipt.receipt_hash, "{name}");
        let mut produced = serde_json::to_value(&produced).unwrap();
        if update_requested() {
            let path = format!("{FIXTURE_DIR}/{name}.json");
            std::fs::write(
                &path,
                serde_json::to_string_pretty(&produced).unwrap() + "\n",
            )
            .unwrap();
            continue;
        }
        let mut expected = read_fixture(name);
        normalize_volatile(&mut produced);
        normalize_volatile(&mut expected);
        assert_eq!(
            produced, expected,
            "{name}: producer wire shape drifted from tests/fixtures/receipts/join-v1; \
             regenerate with CARA_UPDATE_RECEIPT_CONTRACT=1 and tell Cacophony"
        );
    }
}

#[test]
fn force_intent_wire_spelling_is_stable_and_tolerant() {
    // Producer spelling: non-force admission is `none`, real force `preserved`.
    assert_eq!(
        serde_json::to_value(JoinForceIntent::Absent).unwrap(),
        "none"
    );
    assert_eq!(
        serde_json::to_value(JoinForceIntent::Preserved).unwrap(),
        "preserved"
    );
    // Reader tolerance: the historical `absent` spelling still means no force.
    for spelling in ["none", "absent"] {
        assert_eq!(
            serde_json::from_value::<JoinForceIntent>(json!(spelling)).unwrap(),
            JoinForceIntent::Absent,
            "{spelling}"
        );
    }
    // Anything else is not silently mapped to either meaning.
    for spelling in ["force", "present", "", "None"] {
        assert!(
            serde_json::from_value::<JoinForceIntent>(json!(spelling)).is_err(),
            "{spelling}"
        );
    }
    // Real fixtures carry the producer spellings for each case.
    assert_eq!(read_fixture("join")["join_receipt"]["force_intent"], "none");
    assert_eq!(read_fixture("new")["join_receipt"]["force_intent"], "none");
    assert_eq!(
        read_fixture("join-preserved")["join_receipt"]["force_intent"],
        "preserved"
    );
}

#[test]
fn join_receipt_json_schema_matches_the_published_contract() {
    let generated =
        serde_json::to_string_pretty(&schemars::schema_for!(JoinReceipt)).unwrap() + "\n";
    if update_requested() {
        std::fs::create_dir_all(std::path::Path::new(SCHEMA_PATH).parent().unwrap()).unwrap();
        std::fs::write(SCHEMA_PATH, &generated).unwrap();
        return;
    }
    let published = std::fs::read_to_string(SCHEMA_PATH).unwrap_or_default();
    assert!(
        published == generated,
        "docs/receipts/join-receipt.v1.schema.json is stale; regenerate with \
         CARA_UPDATE_RECEIPT_CONTRACT=1 cargo test --lib receipt_contract and review the diff"
    );
}
