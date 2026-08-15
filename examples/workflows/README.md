# Canonical Caravan workflow bundle

Copy these three workflows together:

- `caravan-gate.yml` — exact-head PR suite with the trusted admission gate and fail-closed required aggregate;
- `cara-admit.yml` — fast trusted admission-only writer, triggered by PR events and a bounded fallback schedule;
- `cara-sync.yml` — ordinary fleet convergence, repair routing, parking, promotion, and merge.

Before enabling the writers, replace `REPLACE_WITH_PINNED_VERSION` and
`REPLACE_WITH_64_HEX_ARCHIVE_SHA256` in both controller files with the same
published Cara release and x86_64 Linux archive digest. The release must contain
`cara admit`. Never use a branch, `latest`, PR content, or a remotely fetched
checksum as authority.

Keep the `cara-writer-${{ github.repository }}` concurrency key byte-identical
between admission and convergence. The admission workflow uses
`pull_request_target` only as a trusted default-branch wake: it checks out the
provider default branch and never interpolates or executes pull-request content.
The event PR is not the candidate; Cara hot-discovers global priority/FIFO order.

Adapt the heavy commands and required aggregate in `caravan-gate.yml` to the
repository, but preserve its code-generation-only trigger, admission-gate
outputs, heavy-job predicates, and deferred sentinel. Never add `labeled` or
`unlabeled` to the heavy workflow.
