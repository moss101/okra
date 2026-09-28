# N0007 — Zed removed from the machine; ACP clients stay external

- **Status:** implemented
- **Decided:** 2026-09-28 (user instruction: "this app launched as zed — we never wanted that")

## Decision

Zed must not be installed on, or launched from, this machine by okra or any
ZCode session. The 2026-09-27 G4 ACP drive installed Zed via
`brew install --cask zed` and left it running ~18 h after the test; the user
rejected that state. On 2026-09-28 it was removed:
`brew uninstall --cask zed` (app + `/opt/homebrew/bin/zed` shim) plus all
app residue (`~/Library/{Application Support,Logs,Caches}/Zed*`,
`~/.config/zed`, saved state, WebKit data). Retained: `~/.okra-zed-acp/`
(12 K wire tee from the G4 drive) as gate evidence.

## Scope of ACP after this note

- The `--acp` surface (apps/okra/src/acp.rs) remains a protocol seam for
  EXTERNAL editors; okra never installs, launches, or auto-updates a client.
- Gate evidence that needs a real client uses the captured wire logs in
  `~/.okra-zed-acp/` or a disposable VM, not this machine's GUI.
- Product UI targets are unchanged (N0005): ZCode's Electron UI via the
  host bridge, then okra's own gateway/TUI (M4+).

## Why

The G4 drive was a one-shot compatibility proof, but its leftovers made
Zed look like part of the product surface. The product is okra; editors are
peers a user may attach, never dependencies we provision.
