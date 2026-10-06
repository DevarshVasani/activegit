# Changelog

All notable changes to ActiveGit are documented here. Release notes are
built from these entries by cargo-dist.

## v0.2.3 - 2026-10-06

### Commits panel

- **Branch-graph rendering**: the `[3]-Commits` panel now draws a
  `git log --graph` style lane view with per-branch colors, author
  initials, merge joins, and bare tag names.
- **Push-state dots**: each commit's dot is filled (`●`) when the commit
  exists on the remote and open (`○`) while it is still local-only
  (yet to push) — shared ancestors on untracked branches stay filled.
- **Commit overview**: focusing the Commits panel shows the selected
  commit's message, author, date, and per-file change stats in the
  `[5]-Commit` panel.

### Layout

- **Focused section auto-expands**: the panel you are in automatically
  grows (rail panels gain rows from roomier siblings, the diff preview
  gains columns), so the active section always has the most room.
  Divider drags still apply first — focus only redistributes the rest.

## v0.2.2 - 2026-10-04

### Diff workflow

- **Restore a single hunk**: `x` reverts just the hunk under the cursor
  while leaving every other hunk untouched — workdir hunks revert toward
  the index, staged hunks revert toward HEAD.
- **Hunk navigation in the preview**: `J`/`K` snap the cursor to the next /
  previous hunk top in the right-side diff preview, not just fullscreen.
- **Active-hunk highlight**: the hunk under the cursor reads as one block
  (bold, diff washes preserved) in both the preview and fullscreen views.

### Mouse support

- **Resizable sections**: drag the rail/preview divider to resize the left
  rail, or drag the dividers between status/files/branches/commits/stash to
  resize each panel.
- **Clickable buttons**: context-aware footer buttons (`Stage`, `Discard`,
  `Commit`, `Pull`, `Push`, `Stage hunk`, `Restore hunk`, …) plus a button
  bar inside the fullscreen diff.
- Click files, branches, stashes, diff rows, hunk headers, finder matches,
  and project tabs; the wheel scrolls the panel under the cursor.

### Safer sync

- **Push confirmation**: `P` with a tracked upstream now shows
  `main → origin/main` in a confirm box — `Enter` pushes, `Esc` cancels
  (clickable `[Push]` / `[Cancel]` included).

### LLM setup

- The setup form (`A`) asks the provider for its **live model list**
  instead of shipping hardcoded models, and refetches whenever the
  provider, API key, or base URL changes. A typed custom model id is kept
  when the provider still offers it.

### Docs

- New README comparison section (activegit vs lazygit vs px0) with measured
  startup, binary-size, and hot-path numbers, plus `docs/compare.html`.
