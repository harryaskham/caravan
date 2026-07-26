#!/usr/bin/env bash
# Emit exact downstream Cara runtime pin rows for one published release.
#
# Downstream consumers (notably Cacophony's `.cacophony/cara-runtime-pins.tsv`)
# record a reviewed digest per released platform. Deriving those rows by hand is
# where rollout drift comes from: a mistyped digest, an unverified archive, or a
# silently missing platform all read as "pinned" afterwards.
#
# This helper is read-only and fail-closed. It verifies each published
# `.sha256` against the archive it actually downloaded, extracts the single
# packaged `cara` binary, and prints the exact tab-separated rows. A platform
# whose assets are absent is reported and fails the run unless the caller
# explicitly accepts a partial audit.
#
# Nothing here creates, moves, edits, or force-pushes a tag or release.

set -euo pipefail

TARGETS=(x86_64-linux aarch64-linux aarch64-darwin)

usage() {
  cat >&2 <<'USAGE'
usage: release-pin-rows.sh VERSION [options]

  VERSION            released version without the leading v, e.g. 0.0.9

options:
  --repo OWNER/REPO  release repository (default: $CARA_RELEASE_REPO or harryaskham/caravan)
  --context TEXT     trailing review-context column (default: reviewed release binary)
  --dist DIR         use already-downloaded assets in DIR instead of the network
  --allow-partial    report missing platforms as comments and still exit 0
USAGE
  exit 2
}

digest_of() {
  local path="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$path" | awk '{print $1}'
  else
    echo "neither sha256sum nor shasum is available" >&2
    exit 1
  fi
}

version=""
repo="${CARA_RELEASE_REPO:-harryaskham/caravan}"
context="reviewed release binary"
dist=""
allow_partial=0

while (($#)); do
  case "$1" in
    --repo)
      (($# >= 2)) || usage
      repo="$2"
      shift 2
      ;;
    --context)
      (($# >= 2)) || usage
      context="$2"
      shift 2
      ;;
    --dist)
      (($# >= 2)) || usage
      dist="$2"
      shift 2
      ;;
    --allow-partial)
      allow_partial=1
      shift
      ;;
    -h | --help) usage ;;
    -*)
      echo "unknown option: $1" >&2
      usage
      ;;
    *)
      [ -z "$version" ] || usage
      version="$1"
      shift
      ;;
  esac
done

[ -n "$version" ] || usage
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "version must be X.Y.Z, got: $version" >&2
  exit 2
}
[[ "$context" != *$'\t'* && "$context" != *$'\n'* ]] || {
  echo "review context must be a single tab-free line" >&2
  exit 2
}

workspace="$(mktemp -d "${TMPDIR:-/tmp}/cara-pin-rows.XXXXXX")"
trap 'rm -rf "$workspace"' EXIT

if [ -n "$dist" ]; then
  [ -d "$dist" ] || {
    echo "asset directory does not exist: $dist" >&2
    exit 2
  }
  dist="$(cd "$dist" && pwd -P)"
else
  command -v gh >/dev/null 2>&1 || {
    echo "gh CLI is required to download release assets; pass --dist instead" >&2
    exit 2
  }
  dist="$workspace/dist"
  mkdir -p "$dist"
  # `gh release download` fails when no pattern matches, so ask per target and
  # let the missing-asset branch below classify the gap.
  for target in "${TARGETS[@]}"; do
    gh release download "v$version" --repo "$repo" --dir "$dist" --clobber \
      --pattern "cara-$version-$target.tar.gz" \
      --pattern "cara-$version-$target.sha256" >/dev/null 2>&1 || true
  done
fi

rows=()
archive_digests=()
missing=()

for target in "${TARGETS[@]}"; do
  root="cara-$version-$target"
  archive="$dist/$root.tar.gz"
  checksum="$dist/$root.sha256"

  if [ ! -f "$archive" ] || [ ! -f "$checksum" ]; then
    missing+=("$target")
    continue
  fi

  published="$(awk '{print $1; exit}' "$checksum")"
  actual="$(digest_of "$archive")"
  [ "$published" = "$actual" ] || {
    echo "published checksum mismatch for $root: published=$published actual=$actual" >&2
    exit 1
  }

  members="$(tar -tzf "$archive")"
  [ "$members" = "$root/
$root/cara" ] || {
    echo "unexpected archive layout for $root" >&2
    exit 1
  }

  extract="$workspace/extract-$target"
  mkdir -p "$extract"
  tar -xzf "$archive" -C "$extract"
  binary="$extract/$root/cara"
  [ -f "$binary" ] || {
    echo "packaged binary is missing from $root" >&2
    exit 1
  }

  rows+=("$(printf 'cara %s\t%s\t%s\t%s\t%s' "$version" "$(digest_of "$binary")" "$target" "$actual" "$context")")
  archive_digests+=("$target=$actual")
done

printf '# v%s assets: https://github.com/%s/releases/tag/v%s\n' "$version" "$repo" "$version"
if ((${#archive_digests[@]})); then
  # Match the downstream comment convention: targets sorted, one summary line.
  printf '# archive sha256:'
  while IFS= read -r entry; do
    printf ' %s' "$entry"
  done < <(printf '%s\n' "${archive_digests[@]}" | sort)
  printf '\n'
fi
if ((${#rows[@]})); then
  printf '%s\n' "${rows[@]}"
fi
for target in "${missing[@]}"; do
  printf '# missing %s: publish cara-%s-%s.tar.gz before pinning this platform\n' \
    "$target" "$version" "$target"
done

if ((${#missing[@]})); then
  echo "release v$version has no published assets for: ${missing[*]}" >&2
  if ((allow_partial)); then
    echo "continuing with a partial pin audit as requested" >&2
  else
    echo "run 'just release-backfill-target v$version <target>' on a host advertising that platform" >&2
    exit 1
  fi
fi
