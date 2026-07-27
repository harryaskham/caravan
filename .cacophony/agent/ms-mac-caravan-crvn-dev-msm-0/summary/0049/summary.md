# Session summary — NixOS-safe artifacts, ancestry proof, and proportional apply budget

## Beads

- `bd-0629ce` P0 — portable Linux release artifact plus runtime provenance.
- `bd-7546ea` P0 — unreachable/contradicted compare must not dead-end a stream.
- `bd-9ead1b` P0 — physical apply reserve blocked every large caravan (`bd-5528e6` is my duplicate, kept for closure).
- `bd-48610a` P1 — release-host scoped `gh auth` check.

## Landed vs ready

All four are implemented, rebased onto `v0.0.11` main, and green at **501/501** library tests with strict all-target Clippy. None landed: reintegration refused all night at fail-closed daemon gates (owner/generation, incident-hold 503, authoritative assignment reconciliation). No publication was ever attempted, and no gate was forced.

Commits on `agent/ms-mac/caravan/ms-mac-caravan-crvn-dev-msm-0`:
`b646bfe`, `103203e`, `5e532eb`, `701fb38`, `e032e6e`, `15a7838`, `d7dd7b9`.

## What changed

- **Static Linux artifacts.** `caravan-static` via `pkgsCross` musl with `crt-static`; `release.yml` builds Linux assets from it and refuses to publish a dynamically linked binary; packaged smoke now runs `status` on the NixOS runners.
- **Runtime provenance.** Every `status` receipt carries version, resolved executable, sha256, and `nix_store`, so a receipt proves which build answered.
- **Ancestry proof.** An exact local `merge-base --is-ancestor` proof is now authoritative over the provider, so a direct parent/child pair can never be reported as diverged. Unproved is distinct from diverged and names the exact unreachable pairs.
- **Proportional apply reserve.** `sync.reserve_secs_per_command` (default 15, capped by `command_timeout_secs`) replaces reserving the full timeout per slot. Existing cliff fixtures were pinned to the old worst case rather than loosened, and a new six-member fixture reproduces caravan 2210.

## Releases

`v0.0.9` and `v0.0.10` were cut and fully published, all six assets each. Both darwin legs never got a runner and were backfilled locally from the exact tag with CI-identical packaging and smoke. `v0.0.11` was released by another worker while I was gate-blocked.

## Corrections I made to my own analysis

- The 0.0.9→0.0.10 explicit-intent difference was **not** a regression: it implemented reviewed `choice-019f9d34`. I reverted a fix I had already written rather than silently undo an operator decision, and filed `decision-019fa028` for the empty-fleet gap.
- I argued byte-identical artifacts could not be a build-specific crash. Wrong conclusion: the same bytes are not the same program once nix-ld substitutes a different glibc.
- My first `bd-7546ea` fix only rescued `Unknown`/error compares and would not have saved the live 2228/2235 pair.

## Open, filed, unclaimed

`bd-e9fcd7` (unrepairable front), `bd-cd3be9` (merged containment), `bd-93e366` (arming provenance), `bd-22cc8c` (missing required runs), `bd-523dbf` (owner supersession), `bd-d7aae7` (next admission command).

## Next

1. Land the seven commits once the assignment gate recovers; close `bd-5528e6` as duplicate.
2. Prove the static binary on ms-dev-2, where the dynamic one segfaults.
3. Verify ancestry against live 2228/2235 objects and re-check historical ambiguous sets.
4. Make promotion atomic: base advance plus arm in one reserved plan, with the third `autoMergeRequest=null` meaning (promoted, awaiting handoff). Recorded on `bd-9ead1b`; the `bd comment` call was swallowed by the daemon.
