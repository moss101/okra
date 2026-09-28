#!/bin/sh
# dogfood-log.sh — append a dated entry to journal/<label>.md
# usage: dogfood-log.sh <label> [notes ...]

set -eu

label=${1:?usage: dogfood-log.sh <label> [notes ...]}
shift
notes=
if [ $# -gt 0 ]; then
    notes=$*
fi

mkdir -p journal

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
    if [ -n "$notes" ]; then
        printf -- '- notes: %s\n' "$notes"
    fi
} >> "$file"

printf 'logged %s -> %s\n' "$date" "$file"
