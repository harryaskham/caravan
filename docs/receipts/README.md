# Membership receipts and provider truth

Receipts are **advisory evidence**. They let an auditor see what Cara did and
replay nothing. They are never the authority on whether publication, join,
admission or merge happened. The provider is.

## Authority: `cara membership-proof`

```sh
cara --json membership-proof --pr 4575 --expected-head <40-hex OID> [--expected-base-ref tail]
```

The same verdict is the `membership_proof` MCP tool. It makes four light
read-only provider reads (repository, default branch, open members carrying
the member label, and the exact PR) and derives:

| `outcome` | Meaning | `succeeded` |
|---|---|---|
| `enrolled` | open, labelled, and a member of a derived active caravan | true when the identity checks below pass |
| `merged` | merged by the provider | true when the expected head matches |
| `unjoined` | open, neither labelled nor a graph member | false; admission may be retried for this exact generation |
| `closed_unmerged` | closed without merge | false |
| `inconsistent` | draft, or label and graph membership disagree | false; let sync converge |

Real safety stays inside the verdict, reported in `mismatches[]`:

- `head_oid`: the expected head differs. That is a different generation and is
  never another generation's success.
- `head_repository`: a fork head is never a member.
- `base_ref`: an open member's base differs from `--expected-base-ref`.
- `force_intent`: durable `caravan-force` intent is present. Ordinary
  completion must not count a forced PR as a plain enrollment.

A consumer that holds a join receipt whose fields it does not recognise, or a
receipt it cannot validate, should ask `membership-proof` and act on
`succeeded`. It must not dead-letter a PR the provider shows enrolled or merged.

## Versioned join receipt (`schema_version: 1`)

- JSON Schema: [`join-receipt.v1.schema.json`](join-receipt.v1.schema.json),
  generated from the `JoinReceipt` type.
- Real producer fixtures: [`tests/fixtures/receipts/join-v1/`](../../tests/fixtures/receipts/join-v1/)
  (`new.json`, `join.json`, `join-preserved.json`). They were produced by Cara
  0.0.143's own `execute` and `build_join_receipt` against the hermetic
  provider (originally exported by Cacophony bd-18e81f) and are byte-stable.
- `receipt_hash` is `fnv1a64:` over the compact JSON of the typed receipt in
  declaration order with `receipt_hash` omitted. Consumers must not normalise
  or rehash a received receipt.

`src/membership/tests/receipt_contract.rs` enforces in Caravan CI that the
fixtures still round-trip exactly through the current types, that the current
producer emits the same wire shape apart from per-run `operation_id` and
`receipt_hash`, and that the schema file matches the code. An intentional wire
change regenerates them with
`CARA_UPDATE_RECEIPT_CONTRACT=1 cargo test --lib receipt_contract` and must be
called out in the release notes Cacophony adopts.

### Compatibility rules

- Additive optional fields do not change `schema_version`. Readers must ignore
  unknown fields.
- Renaming or removing a field, or changing an enum spelling, is a new
  `schema_version` and a new fixture directory.
- `force_intent` is emitted as `none` (no force intent) or `preserved` (real
  durable PR-scoped force intent). Readers accept the historical `absent` as
  `none`. Any other value is invalid, never silently mapped. Ordinary
  completion refuses `preserved`.

## Failure outcomes (`cara failures`, `cara failure-classify`)

Every journalled sync, join, admission, eviction and CI failure is classified
into one stable class: `provider_error`, `budget_refusal`, `timeout`,
`conflict_or_test_failure`, `process_refusal` (a receipt, invariant or
bookkeeping step refused) or `unknown`. Each outcome names the exact
`refusing_step` when one exists (for example `force_intent`,
`deferred_label_absent`, `repository_probe`), whether a later retry is useful,
and the tracking `bug` for known failure modes. A process refusal with no known
bug sets `needs_bug: true`.

- `cara status --json` adds `failure_outcomes`: counts per class and per
  `surface/class/code[/step]`, plus the ten newest examples. `process_only` is
  set from the same status's provider facts: `true` when the PR is enrolled or
  merged although the refusal fired.
- `cara failures` returns the same summary from the local journal only.
- `cara failure-classify --error-file PATH [--surface S] [--pr N --expected-head OID]`
  classifies an external envelope, such as a Caco reintegration or dead-letter
  record. With `--pr`, provider truth from `membership-proof` sets
  `process_only` for process refusals.

Outcomes contain only allowlisted identifiers, PR numbers and timestamps; never
messages, argv, environment, provider bodies or tokens. Fixtures for every
class, including Caravan PR #268's 2026-10-20 `force_intent` dead letter, live in
`tests/fixtures/failures/`.
