#!/usr/bin/env bash
# Replace only the port's explicit upstream cleanup calls. Existing generated
# targets are moved into this build run's retirement archive, never deleted.
# An absent target is recorded as absent; failure to preserve an existing
# target is an error, never a pretended successful removal.
set -euo pipefail

run="$(realpath -e -- "${SHENKO_TOYBOX_RUN_DIR:?missing owned build run}")"
archive="$run/retired"
[ -d "$archive" ] || { echo "missing retirement directory: $archive" >&2; exit 1; }
force=false
recursive=false
while [ "$#" -gt 0 ]; do
  case "$1" in
    -f) force=true; shift ;;
    -r) recursive=true; shift ;;
    -rf|-fr) force=true; recursive=true; shift ;;
    --) shift; break ;;
    -*) echo "unsupported retirement flag: $1" >&2; exit 2 ;;
    *) break ;;
  esac
done

for target in "$@"; do
  path="$(realpath -m -s -- "$target")"
  case "$path" in
    "$run"/vendor/*|"$run"/applets/*|"$run"/scratch/*) ;;
    *) echo "refusing retirement outside owned generated tree: $target" >&2; exit 1 ;;
  esac
  if [ ! -e "$path" ] && [ ! -L "$path" ]; then
    "$force" || { echo "retirement target is absent: $path" >&2; exit 1; }
    printf 'absent\t%q\n' "$path" >> "$archive/INDEX.tsv"
    continue
  fi
  # Resolve ancestors, but never dereference the final symlink being moved.
  parent="$(realpath -e -- "$(dirname -- "$path")")"
  case "$parent/" in
    "$run"/vendor/*|"$run"/applets/*|"$run"/scratch/*) ;;
    *) echo "retirement ancestor leaves the owned build tree: $path" >&2; exit 1 ;;
  esac
  if [ -d "$path" ] && [ ! -L "$path" ] && ! "$recursive"; then
    echo "directory retirement requires -r: $path" >&2
    exit 1
  fi
  slot="$(mktemp -d "$archive/item.XXXXXXXXXX")"
  printf '%q\n' "$path" > "$slot/ORIGIN.txt"
  # slot is exclusively created and payload never existed; -T prevents any
  # accidental directory merge. A failed move leaves the source intact and
  # propagates failure to upstream's original fail-fast boundary.
  mv -T -- "$path" "$slot/payload"
  printf 'moved\t%q\t%q\n' "$path" "$slot/payload" >> "$archive/INDEX.tsv"
done
