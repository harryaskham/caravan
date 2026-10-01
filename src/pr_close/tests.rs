mod storage;

use std::cell::{Cell, RefCell};

use mcp_cli::StructuredError;

use super::transaction::{CloseProvider, ReceiptStore, execute};
use super::*;
use crate::config::WriterMode;
use crate::model::CommitOid;

#[derive(Default)]
struct MemoryStore(RefCell<Option<PrCloseRecord>>);
impl ReceiptStore for MemoryStore {
    fn load(&self) -> Result<Option<PrCloseRecord>, AppError> {
        Ok(self.0.borrow().clone())
    }
    fn save(&self, record: &PrCloseRecord) -> Result<(), AppError> {
        self.0.replace(Some(record.clone()));
        Ok(())
    }
}

#[derive(Clone, Copy, Default)]
enum Scenario {
    #[default]
    Normal,
    HeadDrift,
    MainDrift,
    AdmissionRace,
    FenceLoss,
    FenceLossAfterIntent,
    AppliedResponseLost,
    AppliedResponseAndReadLost,
    PostWriteDrift,
    NativeUnavailable,
    ReadUnavailable,
}

struct Provider {
    facts: RefCell<CloseFacts>,
    reads: Cell<usize>,
    writes: Cell<usize>,
    requests: Cell<usize>,
    scenario: Cell<Scenario>,
}
impl Provider {
    fn new() -> Self {
        Self {
            facts: RefCell::new(facts()),
            reads: Cell::new(0),
            writes: Cell::new(0),
            requests: Cell::new(0),
            scenario: Cell::new(Scenario::Normal),
        }
    }
}
impl CloseProvider for Provider {
    fn observe(&self, _: &PrCloseInput) -> Result<CloseFacts, AppError> {
        let n = self.reads.get() + 1;
        self.reads.set(n);
        let mut facts = self.facts.borrow().clone();
        match self.scenario.get() {
            Scenario::HeadDrift if n >= 2 => facts.pull.head.oid = oid('f'),
            Scenario::MainDrift if n >= 2 => facts.default_branch.oid = oid('f'),
            Scenario::AdmissionRace if n >= 2 => {
                facts.pull.labels.insert("caravan".to_owned());
            }
            Scenario::PostWriteDrift if n >= 3 => {
                facts.pull.labels.insert("caravan-parked".to_owned());
            }
            Scenario::AppliedResponseAndReadLost if n >= 3 => return Err(failure("readback_lost")),
            Scenario::NativeUnavailable => return Err(failure("native_inventory_unavailable")),
            Scenario::ReadUnavailable => {
                return Err(AppError::execution(
                    "fixture_repository_identity_unavailable",
                    "fixture unavailable",
                    None,
                ));
            }
            _ => {}
        }
        Ok(facts)
    }
    fn revalidate_fence(&self) -> Result<String, AppError> {
        if matches!(self.scenario.get(), Scenario::FenceLoss) {
            return Err(failure("pr_close_fence_lost"));
        }
        Ok("fixture-fence-generation".to_owned())
    }
    fn close_once(&self, _: &CloseFacts) -> Result<(), AppError> {
        self.requests.set(self.requests.get() + 1);
        if matches!(self.scenario.get(), Scenario::FenceLossAfterIntent) {
            return Err(failure("fence_lost"));
        }
        self.writes.set(self.writes.get() + 1);
        self.facts.borrow_mut().pull.state = PullRequestState::Closed;
        if matches!(
            self.scenario.get(),
            Scenario::AppliedResponseLost | Scenario::AppliedResponseAndReadLost
        ) {
            return Err(failure("response_lost"));
        }
        Ok(())
    }
}

fn failure(code: &str) -> AppError {
    AppError::validation(code, "fixture failure")
}
fn oid(c: char) -> CommitOid {
    CommitOid(c.to_string().repeat(40))
}
fn request() -> PrCloseInput {
    PrCloseInput {
        repository: "acme/widgets".to_owned(),
        pr: 8,
        head_ref: "feature".to_owned(),
        head: oid('a').0,
        base_ref: "main".to_owned(),
        base: oid('b').0,
        main_ref: "main".to_owned(),
        main: oid('b').0,
        represented_at: oid('c').0,
        representation: MainRepresentation::SameTree,
        actor: "source-owner".to_owned(),
        custody_reference: "owner:8:g1".to_owned(),
        reason: "source represented on main".to_owned(),
        operation_key: "close-8-g1".to_owned(),
        confirmed: true,
    }
}
fn facts() -> CloseFacts {
    let repository = RepositoryId {
        owner: "acme".to_owned(),
        name: "widgets".to_owned(),
    };
    let branch = |name: &str, revision| BranchSnapshot {
        repository: repository.clone(),
        name: name.to_owned(),
        oid: oid(revision),
    };
    CloseFacts {
        repository: repository.clone(),
        pull: ClosePull {
            number: PrNumber(8),
            head: branch("feature", 'a'),
            base: branch("main", 'b'),
            state: PullRequestState::Open,
            merged_at: None,
            draft: false,
            cross_repository: false,
            auto_merge: false,
            labels: BTreeSet::new(),
        },
        default_branch: branch("main", 'b'),
        source: GitCommitIdentity {
            oid: oid('a'),
            tree_oid: oid('e'),
            parents: vec![],
        },
        represented_at: GitCommitIdentity {
            oid: oid('c'),
            tree_oid: oid('e'),
            parents: vec![],
        },
        represented_on_main: true,
        source_in_representation: true,
        native_stacks: vec![],
        permission: "WRITE".to_owned(),
    }
}
fn run(provider: &Provider, store: &MemoryStore) -> Result<PrCloseOutput, AppError> {
    execute(
        provider,
        store,
        &request(),
        "policy-1",
        WriterMode::LocalOnly,
    )
}

#[test]
fn exact_close_persists_intent_and_replays_without_another_write() {
    let provider = Provider::new();
    let store = MemoryStore::default();
    let result = run(&provider, &store).unwrap();
    assert_eq!(result.outcome, CloseOutcome::Closed);
    assert_eq!(result.provider_mutated, Some(true));
    assert!(!result.atomic_provider_transaction);
    assert!(result.close_intent_recorded);
    let first = store.load().unwrap().unwrap().first_result;
    let replay = run(&provider, &store).unwrap();
    assert_eq!(replay.outcome, CloseOutcome::ReconciledClosed);
    assert_eq!(replay.provider_mutated, Some(false));
    assert!(!replay.close_attempted_this_call);
    assert_eq!(store.load().unwrap().unwrap().first_result, first);
    assert_eq!(provider.writes.get(), 1);
    assert_eq!(provider.requests.get(), 1);
}

#[test]
fn active_parked_force_and_native_members_refuse_without_intent() {
    for label in ["caravan", "caravan-parked", "caravan-force"] {
        let provider = Provider::new();
        provider
            .facts
            .borrow_mut()
            .pull
            .labels
            .insert(label.to_owned());
        let store = MemoryStore::default();
        assert!(run(&provider, &store).is_err());
        assert!(store.load().unwrap().unwrap().intent.is_none());
        assert_eq!(provider.writes.get(), 0);
    }
    let provider = Provider::new();
    provider.facts.borrow_mut().native_stacks.push(42);
    assert!(run(&provider, &MemoryStore::default()).is_err());
    assert_eq!(provider.requests.get(), 0);
}

#[test]
fn drift_admission_race_fence_loss_and_unknown_inventory_send_no_close() {
    for scenario in [
        Scenario::HeadDrift,
        Scenario::MainDrift,
        Scenario::AdmissionRace,
        Scenario::FenceLoss,
        Scenario::NativeUnavailable,
    ] {
        let provider = Provider::new();
        provider.scenario.set(scenario);
        let store = MemoryStore::default();
        assert!(run(&provider, &store).is_err());
        assert_eq!(provider.requests.get(), 0);
        assert!(store.load().unwrap().unwrap().intent.is_none());
    }
}

#[test]
fn representation_identity_and_permission_are_not_caller_assertions() {
    let alterations: [fn(&mut CloseFacts); 10] = [
        |f| f.represented_on_main = false,
        |f| f.source_in_representation = false,
        |f| f.represented_at.tree_oid = oid('f'),
        |f| f.source.tree_oid = CommitOid(String::new()),
        |f| f.pull.cross_repository = true,
        |f| f.pull.draft = true,
        |f| f.pull.auto_merge = true,
        |f| f.pull.head.name = "other".to_owned(),
        |f| f.pull.base.oid = oid('d'),
        |f| f.permission = "READ".to_owned(),
    ];
    for alter in alterations {
        let provider = Provider::new();
        alter(&mut provider.facts.borrow_mut());
        assert!(run(&provider, &MemoryStore::default()).is_err());
        assert_eq!(provider.requests.get(), 0);
    }
}

#[test]
fn ancestor_representation_allows_different_trees_when_ancestry_is_proven() {
    let provider = Provider::new();
    provider.facts.borrow_mut().represented_at.tree_oid = oid('f');
    let mut input = request();
    input.representation = MainRepresentation::Ancestor;
    assert!(
        execute(
            &provider,
            &MemoryStore::default(),
            &input,
            "policy-1",
            WriterMode::LocalOnly
        )
        .is_ok()
    );
}

#[test]
fn accepted_response_loss_reconciles_in_place_without_false_attribution() {
    let provider = Provider::new();
    provider.scenario.set(Scenario::AppliedResponseLost);
    let result = run(&provider, &MemoryStore::default()).unwrap();
    assert_eq!(result.outcome, CloseOutcome::ReconciledClosed);
    assert_eq!(result.provider_mutated, None);
    assert_eq!(provider.writes.get(), 1);
}

#[test]
fn accepted_response_and_readback_loss_survive_retry() {
    let provider = Provider::new();
    provider.scenario.set(Scenario::AppliedResponseAndReadLost);
    let store = MemoryStore::default();
    assert_eq!(
        run(&provider, &store).unwrap_err().code(),
        "pr_close_indeterminate"
    );
    assert!(store.load().unwrap().unwrap().intent.is_some());
    provider.scenario.set(Scenario::Normal);
    assert_eq!(
        run(&provider, &store).unwrap().outcome,
        CloseOutcome::ReconciledClosed
    );
    assert_eq!(provider.requests.get(), 1);
    assert_eq!(provider.writes.get(), 1);
}

#[test]
fn intent_without_observed_effect_never_reissues_close() {
    let provider = Provider::new();
    provider.scenario.set(Scenario::FenceLossAfterIntent);
    let store = MemoryStore::default();
    assert!(run(&provider, &store).is_err());
    provider.scenario.set(Scenario::Normal);
    assert!(run(&provider, &store).is_err());
    assert_eq!(provider.requests.get(), 1);
    assert_eq!(provider.writes.get(), 0);
    assert_eq!(
        store
            .load()
            .unwrap()
            .unwrap()
            .latest_result
            .unwrap()
            .outcome,
        CloseOutcome::Indeterminate
    );
}

#[test]
fn changed_retry_identity_or_policy_is_refused_before_provider_access() {
    let provider = Provider::new();
    let store = MemoryStore::default();
    run(&provider, &store).unwrap();
    let reads = provider.reads.get();
    let mut other = request();
    other.custody_reference = "another-owner-generation".to_owned();
    assert_eq!(
        execute(&provider, &store, &other, "policy-1", WriterMode::LocalOnly)
            .unwrap_err()
            .code(),
        "pr_close_retry_identity_changed"
    );
    assert_eq!(
        execute(
            &provider,
            &store,
            &request(),
            "policy-2",
            WriterMode::LocalOnly
        )
        .unwrap_err()
        .code(),
        "pr_close_retry_identity_changed"
    );
    assert_eq!(provider.reads.get(), reads);
    assert_eq!(provider.requests.get(), 1);
}

#[test]
fn external_completion_does_not_create_a_write_intent() {
    for (state, outcome) in [
        (PullRequestState::Closed, CloseOutcome::ExternallyClosed),
        (PullRequestState::Merged, CloseOutcome::ExternallyMerged),
    ] {
        let provider = Provider::new();
        provider.facts.borrow_mut().pull.state = state;
        let store = MemoryStore::default();
        assert_eq!(run(&provider, &store).unwrap().outcome, outcome);
        assert!(store.load().unwrap().unwrap().intent.is_none());
        assert_eq!(provider.requests.get(), 0);
    }
}

#[test]
fn post_write_drift_remains_indeterminate_and_never_reopens() {
    let provider = Provider::new();
    provider.scenario.set(Scenario::PostWriteDrift);
    let store = MemoryStore::default();
    assert!(run(&provider, &store).is_err());
    assert!(run(&provider, &store).is_err());
    let last = store.load().unwrap().unwrap().latest_result.unwrap();
    assert_eq!(last.outcome, CloseOutcome::Indeterminate);
    assert_eq!(last.observed.unwrap().pull.state, PullRequestState::Closed);
    assert_eq!(provider.writes.get(), 1);
}

#[test]
fn unavailable_preflight_can_resume_same_request_without_prior_close() {
    let provider = Provider::new();
    provider.scenario.set(Scenario::ReadUnavailable);
    let store = MemoryStore::default();
    assert_eq!(
        run(&provider, &store).unwrap_err().code(),
        "pr_close_unavailable"
    );
    assert_eq!(provider.requests.get(), 0);
    assert!(store.load().unwrap().unwrap().intent.is_none());
    provider.scenario.set(Scenario::Normal);
    assert_eq!(
        run(&provider, &store).unwrap().outcome,
        CloseOutcome::Closed
    );
    assert_eq!(provider.requests.get(), 1);
}

#[test]
fn input_requires_confirmation_and_safe_bound_identities() {
    let mut input = request();
    assert!(validate_input(&input).is_ok());
    input.confirmed = false;
    assert!(validate_input(&input).is_err());
    input.confirmed = true;
    input.operation_key = "../escape".to_owned();
    assert!(validate_input(&input).is_err());
    input = request();
    input.head.clear();
    assert!(validate_input(&input).is_err());
}
