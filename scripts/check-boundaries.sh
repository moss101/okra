#!/usr/bin/env bash
# Crate boundary checks for okra (MASTER-PLAN §3 #61: architecture policy, enforced).
#
# Allowed dependency directions (→ = may depend on). Transitive through the
# allowed set is fine; anything not listed is forbidden. The bin crate
# (apps/okra) may depend on everything.
#
#   protocol   → (leaf)
#   policy     → (leaf)
#   tools      → (leaf)
#   providers  → (leaf)
#   memory     → (leaf)
#   workflow   → (leaf)
#   computer   → (leaf)
#   tui        → (leaf)
#   kernel     → protocol
#   compaction → providers, protocol
#   gateway    → protocol
#   session    → kernel, protocol
#   host       → policy, protocol, kernel
#   agent-core → tools, policy, providers, kernel, protocol, compaction
set -euo pipefail
cd "$(dirname "$0")/.."

viol=0
check() { # crate allowed...
  local crate="$1"; shift
  local deps
  deps=$(sed -n 's/^okra-\([a-z-]*\) *= *.*/\1/p' "crates/$crate/Cargo.toml" | grep -v "^$crate$" | sort -u || true)
  for dep in $deps; do
    local ok=0
    for allowed in "$@"; do
      [ "$dep" = "$allowed" ] && ok=1
    done
    if [ "$ok" = 0 ]; then
      echo "BOUNDARY VIOLATION: okra-$crate depends on okra-$dep (not allowed)"
      viol=1
    fi
  done
}

check protocol
check policy
check tools
check providers
check memory
check workflow
check computer
check tui
check kernel protocol
check compaction providers protocol
check gateway protocol
check session kernel protocol
check host policy protocol kernel
check agent-core tools policy providers kernel protocol compaction

if [ "$viol" = 0 ]; then echo "boundaries: ok"; else exit 1; fi
