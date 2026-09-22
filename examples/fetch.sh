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

dn "$@" | while IFS= read -r line; do
    case "$line" in
        *'"event":"progress"'*)
            done_bytes=${line#*\"done\":}; done_bytes=${done_bytes%%,*}
            total=${line#*\"total\":}; total=${total%%,*}
            [ "$total" -gt 0 ] && printf '\r  %s of %s bytes ' "$done_bytes" "$total"
            ;;
        *'"event":"done"'*)
            sha=${line#*\"sha256\":\"}; sha=${sha%%\"*}
            printf '\n  ok %s\n  sha256 %s\n' "$DEST" "$sha"
            ;;
        *'"event":"error"'*)
            msg=${line#*\"message\":\"}; msg=${msg%%\"*}
            printf '\n  failed: %s\n' "$msg" >&2
            exit 1
            ;;
    esac
done
