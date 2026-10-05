#!/usr/bin/env bash
# Publish only the exact commit validated by the release workflow.
set -euo pipefail

: "${RELEASE_SHA:?set RELEASE_SHA to the validated commit}"
HEAD_SHA=$(git rev-parse HEAD)
if [[ "$HEAD_SHA" != "$RELEASE_SHA" ]]; then
  echo "Checkout does not match validated RELEASE_SHA" >&2
  exit 1
fi
VERSION=$(python3 .github/scripts/version.py current)
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
  echo "Invalid release version: $VERSION" >&2
  exit 1
fi
TAG="v$VERSION"

# Fetch authoritative tags; never move or overwrite an existing release tag.
git fetch origin --tags
TAG_EXISTS=false
if git show-ref --verify --quiet "refs/tags/$TAG"; then
  TAG_EXISTS=true
  TAG_SHA=$(git rev-parse "$TAG^{commit}")
  if ! git merge-base --is-ancestor "$TAG_SHA" "$RELEASE_SHA"; then
    echo "Existing $TAG is not an ancestor of the validated commit" >&2
    exit 1
  fi
fi

# A failed API request must not be mistaken for a missing release. Listing all
# pages also detects drafts, and allows recovery after tag push succeeded.
RELEASES=$(gh api --paginate 'repos/{owner}/{repo}/releases?per_page=100')
if jq -es --arg tag "$TAG" 'any(.[][]; .tag_name == $tag)' <<<"$RELEASES" >/dev/null; then
  if [[ "$TAG_EXISTS" != true ]]; then
    echo "Release $TAG exists without its tag; refusing to recreate it" >&2
    exit 1
  fi
  echo "Release $TAG already exists; nothing to do."
  exit 0
fi
if [[ "$TAG_EXISTS" == true && "$TAG_SHA" != "$RELEASE_SHA" ]]; then
  echo "Missing release for $TAG: rerun validation/release at $TAG_SHA, not $RELEASE_SHA" >&2
  exit 1
fi

# Exclude the current tag itself on a recovery run. The closest reachable
# version tag on the parent defines the range; include every commit, not only
# PRs or the most recent 50 results. Squash/merge subjects retain PR references.
PREV_TAG=$(git describe --tags --match 'v[0-9]*' --abbrev=0 "$RELEASE_SHA^" 2>/dev/null || true)
RANGE="$RELEASE_SHA"
if [[ -n "$PREV_TAG" ]]; then RANGE="$PREV_TAG..$RELEASE_SHA"; fi
NOTES_FILE=$(mktemp)
trap 'rm -f "$NOTES_FILE"' EXIT
{
  printf 'Commits since %s:\n\n' "${PREV_TAG:-start}"
  git log --reverse --format='- %s (%h)' "$RANGE" --
} >"$NOTES_FILE"

if [[ "$TAG_EXISTS" != true ]]; then
  git config user.name "pantheon-release-bot"
  git config user.email "noreply@pantheon.invalid"
  git tag -a "$TAG" "$RELEASE_SHA" -m "Pantheon $TAG"
  git push origin "refs/tags/$TAG"
fi
gh release create "$TAG" --verify-tag --title "Pantheon $TAG" --notes-file "$NOTES_FILE"
echo "Released $TAG"
