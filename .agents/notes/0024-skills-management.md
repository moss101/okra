# N0024 — skills management: the Tools tab can install, disable, enable, delete

- **Status:** implemented
- **Decided:** 2026-09-29
- Blocks: MASTER-PLAN #48 (skills domain → management surface); completes
  N0015's read-only Tools tab for the skills half.
- Builds on: the SkillCatalog loader (`.okra/skills/*.md`), N0015's
  Tools tab projections.

## Decision

1. **Files are the database.** Install writes
   `.okra/skills/SKILL-<name>.md` (frontmatter: name/description/match +
   body). Disable renames to `<file>.disabled` — the loader reads only
   `.md`, so a disabled skill is out of the activation path with zero new
   state. Enable renames back. Delete removes either form.
2. **Sanitization is the security boundary.** Skill names are stripped to
   letters/digits/`-`/`_` before touching the filesystem — `../evil`
   installs as `SKILL-evil.md` inside the skills dir, never outside.
   Duplicates conflict with 409; unknown names 404.
3. **Listing is disabled-aware.** `GET /api/skills` merges enabled
   (`disabled: false`) and disabled (`disabled: true`, parsed from the
   renamed file's frontmatter) entries.
4. **Tools tab:** per-skill disable/enable toggle and × delete; a `+`
   button prompts for name/description/patterns/body and installs. The
   list reloads after every management action.

## Why

N0015 deliberately shipped skills as read-only ("management rides on the
same projections later"). With agents authoring and using skills daily,
install/disable/delete is the natural management surface — and files as
the database keeps it inspectable and git-friendly (the skills dir can be
committed with the workspace).

## Evidence

- `g4_skills_management_lifecycle`: install → file on disk; duplicate →
  409; listing shows enabled + patterns; disable → enabled file renamed
  away, listing `disabled: true`; enable → restored; delete → empty
  listing; unknown delete → 404; unsanitizable name → 400; `../evil`
  sanitized to `SKILL-evil.md` inside the skills dir (traversal-proof).
- Hands-on browser drive (2026-09-29): installed rust-review +
  docs-keeper through the same POST the + form issues; docs-keeper
  toggled disabled via the card's button (listing `disabled: true`,
  enable button shown). Screenshot in the session record.
  `scripts/ci.sh` all gates green.
