# Readiness required-report selection contract

Status: **consumer contract and executable regressions, not a live-gate fix**.
The engine does not grant cross-workflow replacement authority. Producer and
installed/runtime acceptance remain separate obligations.

## The observed boundary

Cacophony PR4094 at `6612123f5d307425296bce00478647574964eb19` proposes a cheap
`ready_for_review` observer alongside the ordinary source workflow. Its synthetic
fixture keeps two `Caravan admission gate` checks on the same repository, PR, head,
base, and App: an older failing `CI` job and a later successful
`Caravan readiness membership refresh` job. The fixture is **not** a provider
receipt or a proof that the failure meant only `run_unproven`.

The producer's reviewed shell validates its transient typed membership receipt
and its live reporting job/check identity, but retains no immutable prior-report
linkage for the consumer. Its success message, job name, label, and fixture
`decision` field cannot authorize overriding the earlier failed check. Historical
receipts cannot be reconstructed retroactively from those observations.

More fundamentally, GitHub's [protected-branches documentation][protected]
explicitly warns that using the same job name in multiple workflows can create
ambiguous status checks and block merging. A custom engine proof schema would
not by itself establish which duplicate required check GitHub accepts. Therefore
this change does **not** add such a schema or a newest-success-wins selector.

[protected]: https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-protected-branches/about-protected-branches

## Actual engine contract

1. Required policy matches the effective landing branch's context and App, on
   the exact head. All observations stay available for diagnostics.
2. `CheckIdentity` and workflow-generation identity retain workflow provenance.
   Different workflow identities do not supersede one another merely because
   context/App/head match, timestamps advance, or one result says `run_member`.
3. The existing **same-workflow** rules can retire a positively ordered older
   generation. Without ordering evidence, both rows remain current. A newer
   pending or failed check is not green; a workflow conclusion alone is not a
   passing required reporting check.
4. A required source or third-party failure remains a failure, even if membership
   passes. Wrong/missing App or head, unknown state, partial required policy, and
   incomplete lineage never prove all requirements satisfied.
5. Required-check assessment is not source-content or current-base equivalence
   proof. Synthetic-candidate, event/base/default, member, hold, and writer guards
   remain independently mandatory. No historical intake becomes retryable here.

The checked-in fixture and tests execute these rules through
`latest_checks_per_identity` and `required_runs::assess`. They make no GitHub
write, rerun, admission, check-overwrite, protection edit, or runtime change.

## Supported producer continuation: one protected reporter

The existing Cacophony producer owner accepted this direction after its active
host-work checkpoint; that is an acknowledged follow-up, not completed source.

- Keep **one dedicated cheap membership workflow** as the sole producer of the
  configured protected membership context. It handles the ordinary
  `opened`/`synchronize`/`reopened` events and `ready_for_review`.
- Keep source-validation workflow triggers, source generations, required source
  checks, and actual results intact. Reuse its internal read-only observation if
  needed, but do not emit a second job with the protected membership context.
  A readiness event must not fabricate skipped-success source checks or launch
  another heavy suite merely to refresh membership.
- Retain typed, exact repository/PR/head/ref/base/ref/default/policy observation,
  actual reporting run/job/check/App validation, fail-closed unknown decisions,
  and exact unjoined deferral. Membership is never inferred from calculation
  success or `run_ci=true` alone.
- Preserve the configured required context; no branch-protection migration or
  manual check rewriting is authorized by this contract. Confirm with the
  existing owner that no second workflow emits that context in the proposed
  source generation.
- Qualify the ordinary **new source generation** containing this producer change.
  Do not claim it retroactively fixes duplicate contexts on an unchanged old
  head. Historical failed checks and uncertain intake/operation records remain
  unchanged. Actual provider qualification must be read back through the existing
  actor before claiming admission or delivery.

If the provider still treats a current generation as ambiguous, or policy demands
other report identity, retain the exact refusal and route it to the existing
producer/engine owners. Do not clear skips, relabel, manually start CI, bypass
required checks, or invent another queue actor.

## Regression inventory and completion boundary

`src/required_runs/readiness_contract_tests.rs` covers:

- production-shaped old-CI-red/new-readiness-green retained as two current rows;
- names, descriptive provider text, and input ordering confer no replacement;
- missing/wrong App/head and partial required policy never prove green;
- same-workflow positive ordering, pending/failing successor, absent ordering;
- independent required source and third-party failure/unknown controls;
- changed source head and incomplete lineage refusal.

`tests/fixtures/readiness-required-selection.json` records the synthetic origin
explicitly. Passing these tests proves the consumer contract, **not** producer
implementation, installation, a current provider gate, Android acceptance, Play
delivery, or completion of Cacophony's combined `bd-c2a0a7` work. The expected
admission ABI (`bd-48ebab`), unproved-gate diagnostic classification (`bd-e73c05`),
CI-start selection (`bd-720f37`), and non-force repair (`bd-23bec3`) remain distinct.
