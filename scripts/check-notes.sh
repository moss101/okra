#!/usr/bin/env bash
# Decision-record lifecycle check (MASTER-PLAN §3 #62, deepseek pattern).
# Every .agents/notes/NNNN-*.md must carry a Status line whose value is one of
# proposed|implemented|rejected|archived, and non-proposed notes must cite
# evidence (Evidence:) or a reason (Because:/Superseded-by:).
set -euo pipefail
cd "$(dirname "$0")/.."

viol=0
for f in .agents/notes/[0-9]*.md; do
  [ -e "$f" ] || continue
  status=$(grep -m1 -E '^- \*\*Status:\*\*' "$f" || true)
  if [ -z "$status" ]; then
    echo "NOTE VIOLATION: $f missing '**Status:**' line"; viol=1; continue
  fi
  case "$status" in
    *proposed*) : ;;
    *implemented*|*rejected*|*archived*)
      if ! grep -qE '^(- \*\*(Evidence|Because|Superseded-by)\*\*|## Evidence|## Why)' "$f"; then
        echo "NOTE VIOLATION: $f is '$status' but has no Evidence/Why/Superseded-by section"; viol=1
      fi
      ;;
    *) echo "NOTE VIOLATION: $f has unknown status: $status"; viol=1 ;;
  esac
done

if [ "$viol" = 0 ]; then echo "notes: ok"; else exit 1; fi
