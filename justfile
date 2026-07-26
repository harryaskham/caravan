set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

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
    gh auth status >/dev/null 2>&1 || { echo "error: run 'gh auth login' first" >&2; exit 2; }

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

    # Upload without clobbering an already-published identical asset set.
    for asset in "$root.tar.gz" "$root.sha256"; do
      echo "uploading $asset to $REPO release $TAG"
      gh release upload "$TAG" "dist/$asset" --repo "$REPO" --clobber
    done

    echo "done: $root assets published to $REPO release $TAG"

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
