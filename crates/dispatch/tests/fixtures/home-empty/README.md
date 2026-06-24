# `home-empty` fixture

An empty `$HOME`-shaped tree: `.claude/{sessions,projects,relay}` all exist but
contain no registry entries, transcripts, or relay sidecars (`.gitkeep` keeps the
dirs under version control).

The A1 gate (spec §9, gate item 2) requires `ls --json` over this tree to be
exactly `[]` — matching the 0b dryrun `ls_info_json` capture, which is literally
`[]`. The integration test asserts `render::to_pretty(ls_json(...)) == "[]"`.

Regenerate-friendly; pass (b) may revise.
