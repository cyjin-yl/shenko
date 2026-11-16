#!/bin/sh
# Invoked through a per-test symlink; keep observations beside that symlink.
printf '%s\0' "$@" > "$(dirname "$0")/args"
printf 'partial output'
printf 'connection lost\n' >&2
exit 255
