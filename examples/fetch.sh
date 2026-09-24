#!/bin/sh
# Using the engine from a shell script.
#
# Two ways. The plain one is an exit code and a progress bar on the terminal:
#
#   dn get "$URL" -o out.iso --expect "$SHA"
#
# The other is --json, one object per line, for when the script needs the numbers. No jq
# required: the fields are flat, so a case statement is enough.
set -eu

URL=${1:?usage: fetch.sh <url> <dest> [sha256]}
DEST=${2:?usage: fetch.sh <url> <dest> [sha256]}
SHA=${3:-}

set -- get "$URL" -o "$DEST" --json
[ -n "$SHA" ] && set -- "$@" --expect "$SHA"

# exec preserves dn's exit status, including loader errors and interrupted downloads.
# JSON is emitted directly; consumers should use a real JSON parser for strings.
exec dn "$@"
