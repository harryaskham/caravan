# GitHub native Stack direct-merge sandbox — 2026-07-31

## Scope

This is the disposable-provider acceptance run for `bd-67b62b`. It used only the documented REST surface with `X-GitHub-Api-Version: 2026-03-10`; no local `gh stack` tracking, linking, sync, or rebase command was used.

The private repository `harryaskham/caravan-stack-sandbox-20260731224558` was created solely for this run. All branches and pull requests were disposable. After capture it was archived and marked for deletion; the active automation token lacked GitHub's separate `delete_repo` scope, so destructive cleanup is explicitly pending an operator credential with that scope rather than silently claiming deletion.

The tested direct request was:

```json
{
  "sha": "<selected-top-head>",
  "merge_method": "squash",
  "merge_action": "direct_merge"
}
```

submitted to `PUT /repos/{owner}/{repo}/pulls/{selected}/merge-async`, then observed through `GET .../merge-async/{uuid}` and fresh PR, Stack, and default-ref reads.

## Provider results

### Selected-top SHA lease rejects movement

Stack `#3`, PRs `#1 -> #2`:

- selected top was moved from `366bd694982af493c0fce64fde0445356a66c04a` to `951520627b0783d1e9dcc1b4df722bb90f46c915`;
- submitting the old SHA returned HTTP 400, request ID `C217:28D938:1FDC2C:257008:6A6D17BA`;
- body: `failed / Pull request head branch was modified.`;
- both PRs remained open and unmerged.

This proves the documented top `sha` lease.

### Lower fast-forward before processing fails all-or-none

Stack `#6`, PRs `#4 -> #5`:

- lower head moved from `e32c1d4f58adf22f1adf2145e6ac07ec1a1f44ca` to `d53c111d60c25f9aaafbfad5b037ae601888c19d` while the selected top remained `f92112bfb1a94dcf4380c980d527acfa3ddfb2b8`;
- submission returned 202 and UUID `8c72821b-bdc0-41b3-9832-f210fd4644bd`;
- polling returned `failed / Stack needs to be rebased: PR #5's branch is not a linear descendant of PR #4's branch.`;
- both PRs remained open and unmerged; default stayed unchanged.

### Lower fast-forward after 202 fails all eight entries

Stack `#15`, PRs `#7 -> #14`:

- submission of top `3c55d66918ac12224f4bf670d306bdb9e0e6744a` returned 202 in 509 ms and UUID `151d8804-c0a4-44c4-be24-b2fad5ec933f`;
- immediately afterward, lower PR `#7` was pushed from `3168a0822a51bb9beeb2e504e2326c47a0998979` to `427fc62e8523b9285e22f1154dc38243a99dc26b`;
- polling returned the same non-linear-descendant failure;
- every PR `#7` through `#14` remained open and unmerged; default stayed unchanged.

This proves all-or-none behavior for the ordinary lower fast-forward race.

### Full direct squash succeeds atomically

Stack `#3`, retried with the current selected top:

- UUID `7a79bf00-9b19-4b36-89a0-2b2a2b3ab759` moved through repeated `pending` states to `merged`;
- provider result SHA was `6c0ef407963c7cc6e5d7c82828c9af79de9fc2d2`;
- both PRs became merged and Stack `#3` became closed;
- default exactly matched the reported result SHA.

### Partial prefix succeeds and rewrites the remaining generation

Stack `#19`, PRs `#16 -> #17 -> #18`, selected `#17`:

- UUID `69dfb69f-ac7a-4638-89b6-ce3c0cc0bd99` returned `merged` with result SHA `ec07345c0a30b39e2c1c6a9505f7032ad62d318d`;
- exactly PRs `#16` and `#17` became merged;
- PR `#18` remained open, was retargeted from `partial-2` to `main`, and its head was rewritten to `80015ff731c80a57d62761a4a8874493f3c4c543`;
- its new base SHA exactly matched the new default `ec07345c0a30b39e2c1c6a9505f7032ad62d318d`;
- the Stack remained open and retained the closed prefix plus open suffix.

The adapter therefore validates the live open suffix from the Stack base rather than incorrectly requiring it to target the last retained closed entry.

### Duplicate submission recovers the same UUID

Stack containing PRs `#20 -> #21`:

- first submission returned UUID `a2bdd48c-d2bb-4e2c-b81e-2423e4c361f7`;
- an immediate identical submission returned HTTP 409 with the same UUID, top SHA, `squash`, and `direct_merge` fields;
- polling that UUID completed at `c46c03b14104c4c3f63077487f38f3a7dbb34670`, with both PRs merged.

Cara may adopt a 409 UUID only when all three returned request fields exactly match the sealed intent. A lost response without any UUID remains indeterminate and is never blindly resubmitted.

## Unsafe lower-head rewind result

Stack containing PRs `#23 -> #24 -> #25` resolves the central acceptance question negatively:

- selected top was `3a19640f7626efe9c252a34a1fd026befa462539`;
- submission returned 202 and UUID `322d1e78-a2f0-43be-aa09-2fb56e803ce8`;
- after 202, lower PR `#23` was force-moved from `7c26d64d5ae7411dea84e47b0fdb66ab57a4bc99` backward to its base `c46c03b14104c4c3f63077487f38f3a7dbb34670`;
- unlike the lower fast-forward race, the upper entries remained descendants of that lower head;
- GitHub returned `merged` at `b0cb0fe15e80fa8ceddf70557eb6d13eb2f67182` and marked all three PRs merged, including PR `#23` at the changed lower head.

Therefore `merge-async` does **not** bind the complete selected Stack generation. It binds only the selected top SHA and may merge every selected PR after a lower entry changes, provided the resulting chain remains linear. Cara can detect this afterward and seal `indeterminate`, but postcondition detection cannot prevent the already-completed merge.

## Follow-up preventive ruleset proof — 2026-08-01

The installed `github/gh-stack` v0.0.8 CLI has no `merge` subcommand and its source contains no `merge-async` client. It creates, links, submits, rebases, and synchronizes branch/PR topology; GitHub's web Stack merge uses the same top-SHA-only provider endpoint tested above. There is no hidden CLI complete-group lease.

A follow-up run in the same disposable repository proved a client-side provider-enforced equivalent:

1. One active repository ruleset with no bypass actors targeted exact selected refs `refs/heads/lease-lock-1` and `refs/heads/lease-lock-2` and contained exactly `update` plus `deletion` restrictions.
2. Exact GET readback reported `current_user_can_bypass: never`.
3. A repository-owner SSH push to the selected lower ref was rejected with `GH013 / Cannot update this protected ref`.
4. Owner-authenticated REST force-update and delete attempts both returned HTTP 422 repository-rule violations; the selected head remained exact.
5. With both selected refs locked, direct async merge of PRs `#27 -> #28` succeeded under UUID `282bd37c-e4e1-435f-b6ec-5109ca9eaf14` at default SHA `07005d1f22831c0f6b6142c85df1be615c9914fe`.
6. Unselected PR `#29` remained writable to GitHub's Stack service and was correctly rebased/retargeted to the new default, proving a partial prefix does not deadlock provider continuation.
7. Exact ruleset deletion was followed by 404/absence proof; the previously rejected owner push then succeeded, proving bounded reversible cleanup.

This closes the ordinary-user and repository-owner lower-head race without pretending the top-only merge API changed. The safety boundary is an exact active no-bypass ruleset held from final full-generation read through terminal async proof. Ruleset creation/deletion requires GitHub Administration(write), so this is an explicit native-Stack permission upgrade and never part of default Caravan mode.

## Acceptance decision

- Atomic all-or-none provider behavior is proven for success, direct failure, selected-top movement, and lower movement that breaks linear ancestry.
- UUID persistence, pending polling, terminal failure, terminal success, partial-prefix behavior, and exact 409 recovery are proven.
- Complete-group optimistic concurrency is **not** provided by the merge API itself because a lower rewind preserving linearity can merge at a generation different from Cara's sealed intent.
- An active exact-ref no-bypass repository ruleset is a proven preventive equivalent: it blocks SSH and REST owner writes while permitting GitHub to merge the selected prefix and rewrite only the unselected suffix.

The `github_stack_backend_read_only` workflow fence remains closed until the ruleset lock is threaded through executable Stack orchestration and its Administration(write) permission is explicit. It may then open only for the exact ruleset-locked path; an unlocked top-SHA-only merge remains permanently invalid.

## Cleanup

The sandbox is private and archived with description `DISPOSABLE Cara Stack sandbox completed 2026-07-31; delete when delete_repo-scoped operator token is available`. The attempted REST deletion returned HTTP 403 because the active token has `repo` but not `delete_repo`; no broader token was requested or stored. Final deletion is tracked by `bd-7aa0aa` and remains the only cleanup action.
