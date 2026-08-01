set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

# Run exactly the validation CI runs, in the pinned development shell.
#
# CI's gate is these five commands. Before this recipe existed there was no way
# to run them as a unit, so every contributor hand-rolled a subset before
# publishing -- and a subset is what a hand-rolled gate always is. One such gate
# omitted `--bin cara` and shipped v0.0.59 with a command tree clap could not
# construct: a global `--repo` collided with the repeatable `--repo` on `web`,
# so `cara web` did not exist and the tag published with no assets. Any of
# `cargo test --all`, `cargo run -- help` or `cargo run -- mcp tools` would have
# caught it; the gate ran none of them (bd-8e2c93).
validate:
  #!/usr/bin/env bash
  set -euo pipefail
  nix develop --command bash -c '
    set -euo pipefail
    cargo fmt --all --check
    cargo clippy --all-targets -- -D warnings
    cargo test --all
    cargo run -- help >/dev/null
    cargo run -- mcp tools >/dev/null
  '
  echo "validate: green on the same block CI runs"

# Inner-loop check. NOT sufficient before publication: use `just validate`.
validate-fast:
  #!/usr/bin/env bash
  set -euo pipefail
  nix develop --command bash -c '
    set -euo pipefail
    cargo fmt --all --check
    cargo clippy --all-targets -- -D warnings
    cargo test --lib
  '
  echo "validate-fast: green -- run 'just validate' before publishing"

# Shake out timing- and order-dependent test failures before publication.
#
# `just validate` runs the suite once, so a race that fails intermittently can
# pass locally by luck. v0.0.71 was tagged after a green `validate`, then its
# release workflow failed on the concurrent lease-exclusion test. Tags are
# immutable, so that version number was burned and superseded by v0.0.72
# (bd-3c53b5, bd-1ead85).
#
# This is a probabilistic shake-out, not a proof of absence. The racy lease fake
# that burned v0.0.71 failed roughly 6 of 12 runs, so a few rounds would have
# caught it; a rarer race can still slip through.
#   just stress                    # 10 rounds of the whole lib suite
#   just stress 40                 # more rounds
#   just stress 40 remote_lease    # concentrate on one area
stress rounds="10" filter="":
  #!/usr/bin/env bash
  set -euo pipefail
  rounds="{{rounds}}"
  [[ "$rounds" =~ ^[0-9]+$ ]] && (( rounds > 0 )) || {
    echo "error: rounds must be a positive integer (got: $rounds)" >&2
    exit 2
  }
  # Pass the filter through the environment rather than interpolating it into a
  # shell string, so a filter containing quotes cannot break or inject.
  export CARA_STRESS_ROUNDS="$rounds"
  export CARA_STRESS_FILTER="{{filter}}"
  nix develop --command bash -c '
    set -euo pipefail
    # A filter that matches nothing would otherwise run zero tests every round
    # and still report green, which is exactly the vacuous confidence this
    # recipe exists to prevent.
    selected="$(cargo test --lib $CARA_STRESS_FILTER -- --list 2>/dev/null | grep -c ": test$" || true)"
    if [[ "$selected" -eq 0 ]]; then
      echo "error: filter ${CARA_STRESS_FILTER:-<none>} matched no tests; nothing would be stressed" >&2
      exit 2
    fi
    echo "stressing $selected test(s)"
    for round in $(seq 1 "$CARA_STRESS_ROUNDS"); do
      echo "=== stress round $round/$CARA_STRESS_ROUNDS ==="
      cargo test --lib $CARA_STRESS_FILTER
    done
  '
  echo "stress: {{rounds}} green rounds -- probabilistic, not proof of absence"

run-web-dev *repos:
  #!/usr/bin/env bash
  set -euo pipefail
  [[ -d ~/caravan-test ]] || git worktree add -b caravan-test ~/caravan-test
  cd ~/caravan-test
  git fetch origin main
  git reset --hard origin/main
  nix develop --command cargo install --path .
  flags=()
  for repo in {{repos}}; do
    flags+=("--repo" "$repo")
  done
  ~/.cargo/bin/cara web ${flags[@]}

# Local release backfill, mirroring Cacophony's `just push-release` fallback.
#
# The tag-triggered `release` workflow remains authoritative. These recipes
# exist for when GitHub runners are down, busy, or exhausted: they build the
# exact tagged source in a detached worktree, package the same
# updatable-cli-compatible assets as CI, and upload them to the release for
# that tag. Nothing here creates, moves, or force-pushes a tag.
#
# Assets match `scripts/package-release.sh`:
#   cara-<version>-<target>.tar.gz
#   cara-<version>-<target>.sha256

# Build and upload the native-host target for an existing tag.
#   just release-backfill v0.0.9
release-backfill tag:
    #!/usr/bin/env bash
    set -euo pipefail
    case "$(uname -s)/$(uname -m)" in
      Linux/x86_64|Linux/amd64) target="x86_64-linux" ;;
      Linux/aarch64|Linux/arm64) target="aarch64-linux" ;;
      Darwin/arm64|Darwin/aarch64) target="aarch64-darwin" ;;
      *) echo "error: unsupported host $(uname -s)/$(uname -m)" >&2; exit 2 ;;
    esac
    just release-backfill-target "{{tag}}" "$target"

# Build and upload one explicit target for an existing tag. Linux targets build
# through Nix (aarch64-linux needs a host advertising that Nix platform); the
# Darwin target uses the pinned dev shell, exactly like release.yml.
#   just release-backfill-target v0.0.9 aarch64-darwin
release-backfill-target tag target:
    #!/usr/bin/env bash
    set -euo pipefail

    TAG="{{tag}}"
    TARGET="{{target}}"
    REPO="${CARA_RELEASE_REPO:-harryaskham/caravan}"

    [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
      echo "error: tag must be vX.Y.Z (got: $TAG)" >&2
      exit 2
    }
    case "$TARGET" in
      x86_64-linux|aarch64-linux|aarch64-darwin) ;;
      *) echo "error: unsupported target: $TARGET" >&2; exit 2 ;;
    esac
    command -v gh >/dev/null 2>&1 || { echo "error: gh CLI is required" >&2; exit 2; }
    # Check the exact release host: an unrelated enterprise host failure must not
    # look like a missing github.com login.
    GH_HOST_NAME="${CARA_RELEASE_GH_HOST:-github.com}"
    gh auth status --hostname "$GH_HOST_NAME" >/dev/null 2>&1 || {
      echo "error: not logged into $GH_HOST_NAME; run 'gh auth login --hostname $GH_HOST_NAME' first" >&2
      exit 2
    }

    git fetch origin --tags --quiet
    git rev-parse --verify "refs/tags/$TAG^{commit}" >/dev/null

    VERSION="${TAG#v}"
    # The tagged Cargo.toml version is load-bearing for updatable-cli assets.
    CARGO_VERSION="$(git show "$TAG:Cargo.toml" | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)"
    [[ "$VERSION" == "$CARGO_VERSION" ]] || {
      echo "error: tag $TAG does not match tagged Cargo.toml version $CARGO_VERSION" >&2
      exit 2
    }

    WORKTREE="$(mktemp -d "${TMPDIR:-/tmp}/cara-release-backfill.XXXXXX")"
    cleanup() { git worktree remove --force "$WORKTREE" >/dev/null 2>&1 || true; rm -rf "$WORKTREE"; }
    trap cleanup EXIT
    rm -rf "$WORKTREE"
    git worktree add --detach "$WORKTREE" "$TAG" >/dev/null

    echo "=== cara release backfill ==="
    echo "  repo:    $REPO"
    echo "  tag:     $TAG"
    echo "  version: $VERSION"
    echo "  target:  $TARGET"
    echo "  source:  $WORKTREE (detached at $TAG)"
    echo "==="

    cd "$WORKTREE"
    if [[ "$TARGET" == "aarch64-darwin" ]]; then
      nix develop '.#devShells.aarch64-darwin.default' --builders '' \
        --command ./scripts/build-darwin-release.sh
      binary="target/release/cara"
    else
      nix build ".#packages.$TARGET.caravan" --out-link "result-$TARGET"
      binary="result-$TARGET/bin/cara"
    fi

    nix develop --command ./scripts/package-release.sh "$VERSION" "$TARGET" "$binary" dist

    root="cara-$VERSION-$TARGET"
    if [[ "$TARGET" != "aarch64-linux" || "$(uname -m)" == "aarch64" || "$(uname -m)" == "arm64" ]]; then
      rm -rf smoke && mkdir smoke
      nix develop --command tar -xzf "dist/$root.tar.gz" -C smoke
      "smoke/$root/cara" --version
    fi

    # A published asset is immutable input to downstream digest pins, so never
    # silently swap one. Re-uploading the identical bytes is a no-op; different
    # bytes under the same name fail closed for a human to resolve.
    published="$(mktemp -d "${TMPDIR:-/tmp}/cara-release-published.XXXXXX")"
    if gh release download "$TAG" --repo "$REPO" --dir "$published" \
         --pattern "$root.sha256" >/dev/null 2>&1; then
      remote_digest="$(awk '{print $1; exit}' "$published/$root.sha256")"
      local_digest="$(awk '{print $1; exit}' "dist/$root.sha256")"
      if [[ "$remote_digest" == "$local_digest" ]]; then
        echo "already published with identical digest $local_digest; nothing to do"
        rm -rf "$published"
        exit 0
      fi
      echo "error: $TAG already publishes $root.tar.gz with a different digest" >&2
      echo "  published: $remote_digest" >&2
      echo "  local:     $local_digest" >&2
      echo "  refusing to replace an asset that downstream pins may already reference" >&2
      rm -rf "$published"
      exit 1
    fi
    rm -rf "$published"

    for asset in "$root.tar.gz" "$root.sha256"; do
      echo "uploading $asset to $REPO release $TAG"
      gh release upload "$TAG" "dist/$asset" --repo "$REPO"
    done

    echo "done: $root assets published to $REPO release $TAG"

# Emit exact downstream Cara runtime pin rows for a published release.
#
# Downstream consumers (Cacophony's `.cacophony/cara-runtime-pins.tsv`) record a
# reviewed digest per released platform. This verifies each published checksum
# against the archive it downloaded and fails closed when a platform is missing,
# so a partially published release can never be pinned as if it were complete.
#   just release-pin-rows v0.0.9
#   just release-pin-rows v0.0.9 "bd-e741b9 reviewed release binary"
release-pin-rows tag context="reviewed release binary":
    #!/usr/bin/env bash
    set -euo pipefail
    TAG="{{ tag }}"
    [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
      echo "error: tag must be vX.Y.Z (got: $TAG)" >&2
      exit 2
    }
    ./scripts/release-pin-rows.sh "${TAG#v}" --context "{{ context }}"

# Build and upload every target reachable from this host. Remote targets are
# skipped with an explicit message rather than silently omitted.
release-backfill-all tag:
    #!/usr/bin/env bash
    set -euo pipefail
    host="$(uname -s)/$(uname -m)"
    case "$host" in
      Linux/x86_64|Linux/amd64) targets=(x86_64-linux aarch64-linux) ;;
      Linux/aarch64|Linux/arm64) targets=(aarch64-linux) ;;
      Darwin/arm64|Darwin/aarch64) targets=(aarch64-darwin) ;;
      *) echo "error: unsupported host $host" >&2; exit 2 ;;
    esac
    echo "host $host can build: ${targets[*]}"
    for target in "${targets[@]}"; do
      just release-backfill-target "{{tag}}" "$target" || {
        echo "warning: $target backfill failed on this host; run it on a host advertising that platform" >&2
      }
    done

# Cut an annotated release tag at an already-landed main commit.
#
# `scripts/release.sh` is the one-shot path: bump, commit, tag, push. Caravan's
# reviewed flow instead lands the `release: vX.Y.Z` version bump through the
# ordinary agent/reintegration lifecycle, so by the time the tag is cut the bump
# already exists on main and `release.sh` refuses ("already current"). This
# recipe closes that exact gap and nothing else.
#
# It is fail-closed: the commit must be on origin/main, its tree's Cargo.toml
# and Cargo.lock versions must equal the tag, and flake.nix must either declare
# that literal version or derive `caravanVersion` from Cargo.toml. The tag must
# not already exist locally or on origin, and the working tree must be clean.
# It never moves or force-pushes a tag and never edits a file.
#   just release-tag v0.0.11                # tag exact origin/main
#   just release-tag v0.0.11 <commit-sha>   # tag one exact landed commit
release-tag tag commit="origin/main":
    #!/usr/bin/env bash
    set -euo pipefail

    TAG="{{ tag }}"
    REF="{{ commit }}"

    [[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
      echo "error: tag must be vX.Y.Z (got: $TAG)" >&2
      exit 2
    }
    [ -z "$(git status --porcelain)" ] || {
      echo "error: working tree is dirty; clean it before tagging" >&2
      exit 1
    }

    git fetch origin main --tags --quiet
    COMMIT="$(git rev-parse --verify "$REF^{commit}")"

    git merge-base --is-ancestor "$COMMIT" origin/main || {
      echo "error: $COMMIT is not contained in origin/main; tags are cut from landed main only" >&2
      exit 1
    }

    VERSION="${TAG#v}"
    CARGO_VERSION="$(git show "$COMMIT:Cargo.toml" | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)"
    LOCK_VERSION="$(git show "$COMMIT:Cargo.lock" | awk '/^name = "caravan"$/ { getline; sub(/^version = "/, ""); sub(/"$/, ""); print; exit }')"
    FLAKE_SOURCE="$(git show "$COMMIT:flake.nix")"
    if printf '%s\n' "$FLAKE_SOURCE" | grep -Fq 'caravanVersion = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;'; then
      FLAKE_VERSION="$CARGO_VERSION"
    else
      FLAKE_VERSION="$(printf '%s\n' "$FLAKE_SOURCE" | sed -n 's/^ *version = "\(.*\)";$/\1/p' | head -1)"
    fi
    for pair in "Cargo.toml:$CARGO_VERSION" "Cargo.lock:$LOCK_VERSION" "flake.nix:$FLAKE_VERSION"; do
      file="${pair%%:*}"
      found="${pair#*:}"
      [ "$found" = "$VERSION" ] || {
        echo "error: $file at $COMMIT declares version '$found', not '$VERSION'" >&2
        exit 1
      }
    done

    if git rev-parse --quiet --verify "refs/tags/$TAG" >/dev/null; then
      echo "error: tag already exists locally: $TAG" >&2
      exit 1
    fi
    set +e
    git ls-remote --exit-code --tags origin "refs/tags/$TAG" >/dev/null 2>&1
    remote_tag_status=$?
    set -e
    case "$remote_tag_status" in
      0) echo "error: tag already exists on origin: $TAG" >&2; exit 1 ;;
      2) ;;
      *) echo "error: could not verify whether $TAG exists on origin" >&2; exit 1 ;;
    esac

    echo "==> tagging $TAG at $COMMIT (version $VERSION verified in Cargo.toml, Cargo.lock, flake.nix)"
    git tag -a "$TAG" -m "$TAG" "$COMMIT"
    git push origin "refs/tags/$TAG"
    echo "==> pushed $TAG; release.yml will publish cara assets for that tag"
    echo "    next: just release-pin-rows $TAG \"reviewed release binary\""
