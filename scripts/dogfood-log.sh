#!/bin/sh
# dogfood-log.sh — append a dated entry to journal/<label>.md
# usage: dogfood-log.sh <label> [--stream <ndjson-file>] [notes ...]

set -eu

label=${1:?usage: dogfood-log.sh <label> [--stream <ndjson-file>] [notes ...]}
shift

stream=
if [ $# -gt 0 ] && [ "$1" = "--stream" ]; then
    if [ $# -lt 2 ]; then
        echo "usage: dogfood-log.sh <label> --stream <ndjson-file> [notes ...]" >&2
        exit 1
    fi
    stream=$2
    shift 2
fi

notes=
if [ $# -gt 0 ]; then
    notes=$*
fi

mkdir -p journal

stream_line=
if [ -n "$stream" ]; then
    mkdir -p streams
    if [ -f "$stream" ]; then
        cp "$stream" streams/$label.ndjson
        stream_n=$(wc -l < streams/$label.ndjson)
        stream_line="streams/$label.ndjson ($stream_n lines)"
    else
        stream_line="missing ($stream)"
    fi
fi

date=$(date -u +%Y-%m-%d)

if command -v okra >/dev/null 2>&1; then
    version=$(okra --version 2>/dev/null || echo okra)
else
    version=okra
fi

file=journal/$label.md

if [ ! -f "$file" ]; then
    printf '# %s\n' "$label" > "$file"
fi

{
    printf '\n## %s (UTC)\n' "$date"
    printf -- '- version: %s\n' "$version"
    if [ -n "$stream_line" ]; then
        printf -- '- stream: %s\n' "$stream_line"
    fi
    if [ -n "$notes" ]; then
        printf -- '- notes: %s\n' "$notes"
    fi
} >> "$file"

printf 'logged %s -> %s\n' "$date" "$file"
