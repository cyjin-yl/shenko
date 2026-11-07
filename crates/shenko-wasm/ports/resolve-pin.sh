#!/usr/bin/env bash
# Resolve and record one upstream pin for a tool port, from the project's own
# repository. Run this once on a machine with network access; every later build
# reads the recorded pin and refuses unrecorded source.
#
# Usage: resolve-pin.sh <upstream-git-url> <ref> <pin.json>
#
# What is recorded (nothing is guessed):
#   url            the exact clone URL asked for
#   ref            the requested tag/branch/ref name
#   commit         `git ls-remote` commit for that ref (dereferenced for tags)
#   tree_sha256    sha256 of `git archive --format=tar <commit>` (deterministic
#                  content digest of that tree, independent of transport)
#   resolved_at    UTC time of resolution
#
# The script refuses to overwrite a pin.json that records a different commit:
# changing a port's upstream revision is a deliberate new file, not an edit.
set -euo pipefail

URL="${1:?usage: resolve-pin.sh <upstream-git-url> <ref> <pin.json>}"
REF="${2:?pass the upstream tag/branch/ref to resolve}"
OUT="${3:?pass the pin.json path to create}"
command -v git >/dev/null || { echo "git is required" >&2; exit 2; }
command -v sha256sum >/dev/null || { echo "sha256sum is required" >&2; exit 2; }

# Dereferenced commit for a tag; `ls-remote <url> <ref>` prints both the tag
# object and its peeled commit for annotated tags.
ls_remote="$(git ls-remote -- "$URL" "$REF")"
[ -n "$ls_remote" ] || { echo "ref $REF not found at $URL" >&2; exit 2; }
commit="$(printf '%s\n' "$ls_remote" | awk '$2 ~ /\^\{\}$/ { print $1 }')"
if [ -z "$commit" ]; then
  commit="$(printf '%s\n' "$ls_remote" | awk 'NR == 1 { print $1 }')"
fi
[ -n "$commit" ] || { echo "cannot resolve a commit for $REF" >&2; exit 2; }

if [ -e "$OUT" ]; then
  recorded="$(sed -n 's/.*"commit": "\([0-9a-f]\{40\}\)".*/\1/p' "$OUT" | head -n 1)"
  if [ -n "$recorded" ] && [ "$recorded" != "$commit" ]; then
    echo "$OUT already pins $recorded, refusing to rewrite it as $commit" >&2
    exit 3
  fi
fi

work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT
git -C "$work" init --quiet
git -C "$work" fetch --quiet --depth 1 -- "$URL" "$commit"
git -C "$work" archive --format=tar "$commit" > "$work/source.tar"
tree_sha256="$(sha256sum "$work/source.tar" | awk '{ print $1 }')"
bytes="$(wc -c < "$work/source.tar" | tr -d ' ')"

umask 022
cat > "$OUT" <<JSON
{
  "schema": 1,
  "url": "$URL",
  "ref": "$REF",
  "commit": "$commit",
  "tree_sha256": "$tree_sha256",
  "tree_bytes": $bytes,
  "resolved_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}
JSON
cat "$OUT"
