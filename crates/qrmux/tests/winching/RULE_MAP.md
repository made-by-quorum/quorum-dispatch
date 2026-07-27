# Independent oracle rule-fidelity map

The referee is `oracle.rs`. Its **only authoritative candidate surface is W2's
actual ordered transport framing plus cells**, represented by
`CandidateTransportEmission.chunks[]`, where every chunk carries full untrimmed
cells and the exact `end_of_line` flag copied to `HistoryChunk`. The referee
derives client logical lines only by concatenating chunk cells until that flag
is true—the same point where the client writes CRLF. It then walks the oracle's
ordered frozen rows once, matching every full row cell-for-cell and checking
each physical-row crossing within the derived client line against the live A1
ledger. Every row has positive cell count, so cumulative lengths give one
composition; there is no backtracking, pre-grouped line list, join list, row-ID
list, run structure, or alternate blank-row parse. Encoding can only render the
accepted cells and copy the accepted flag, so this is the terminal-observable
surface and there is no candidate-controlled layer below it. Missing/extra/
mid-row/unterminated framing and unconsumable, duplicate, marker/style-altered,
or reordered cells are findings. The legacy c59
`get_content_history()` comparison in `winching_oracle.rs` is corroborating
"before" evidence only; it is not the A13/A14 gate.

`RULE A*` comments are stable search anchors; the symbols below are the exact
enforcement sites. Preservation never writes an edge: only the successful A1
completion does. Every destructive path calls `sever_edge`/`sever_touching`,
and `validate_all_edges` can only retain or sever. This makes
severance-dominates-preservation structural rather than call-order policy.

| Rule | Exact oracle enforcement |
|---|---|
| A1 | `Oracle::perform_deferred_wrap` is the sole `EdgeRecord { Live }` insertion and sole `outgoing = Some` assignment. Literal print and REP both reach it with qualified `Normal`/`WideEarly` tokens. WideEarly performs a BCE base-only replacement that preserves c59's separate sparse combining entry, and the reader accepts that exact suffix under the live edge. HTS/TBC mutate the table consulted by a later HT, so the cursor from which subsequent prints reach the margin is source-faithful. It returns without an edge if wrap movement destroyed the source or EdgeId allocation is exhausted. |
| A2 | `PendingWrap` carries source revision, fixed width, continuity, kind, padding count, and an alt-entry epoch while suspended. `untracked_wrap_pending` separately carries c59 physical motion but is not accepted by A1. `cut_sequential_preserving_raw_wrap` handles RI, accepted addressed mutations, and DECAWM-off source rewrites; CSI S/T demote only a topology-invalid token. `invalidate_pending_for_geometry` is the single clear seam for both representations: live resize and both matched/stale alt-restore branches classify width/source-row invalidation before raw motion can rearm. Accepted forward combining refreshes a qualified token; cap rejection is a content no-op but every input still updates REP first. `FailClosedGeneration` is sticky for continuity, revision, RowId, EdgeId, and alt epochs. |
| A3 | `scroll_up` moves a whole top row into `history`; `validate_all_edges` retains only persistent adjacent same-width pairs. |
| A4 | `resize` keeps the caller's raw height through the `Screen::resize` guard. Its bottom-row CHOP test ignores style but requires every space/width-0 base to have no combining data. The following c59-faithful migration is cursor-bounded; if fewer than `needed` top rows can migrate, the sanitized grid phase bottom-pops the remainder even when content-bearing. Raw zero skips migration entirely before bottom-pop. |
| A5 | `scroll_up`/`scroll_down` plus `validate_all_edges`: co-moving region rows remain adjacent; inserted/removed/crossed pairs do not. |
| A6 | `enter_alt` moves stable rows into cursor-free `SavedMainRows` and stamps a checked entry epoch. 1049 alone saves the independent shared cursor. `exit_alt` restores rows first, checks the exit mode before cursor/token handling, and permits resumption only for an exact 1049 snapshot/epoch. Before either matched or stale/cross-mode 1049 cursor restore, the same geometry helper clears qualified/raw state invalidated by saved-main width change or anchored-row trim. 47l/1047l preserve the active cursor and never restore saved-main authorization. Persistent edges retain by doing nothing only when IDs restore intact at `SavedMainRows::cols`. |
| A7 | `before_content_write` severs a touched source and any target write lacking sequential authorization. Accepted ED/EL/ECH/DCH/ICH first demote c59's retained raw pending; invalid modes do nothing. DCH/ICH post-shift orphan repair uses the current-background BCE blank, while general grid wide-pair repair remains default-blank. A DECAWM-off width-1 margin rewrite likewise demotes before mutation. Combining input updates REP before its cap/target guard; accepted forward combining refreshes qualified construction. |
| A8 | After vertical restoration, `resize` compares every now-visible row's own width with current columns and severs only mismatched rows, even when global columns are unchanged. Its geometry-helper call preserves a still-qualified unchanged-width surviving token but clears raw-only motion on every resize, matching c59. `exit_alt` applies the same endpoint rule to saved rows. Both repair a boundary-crossing wide base/continuation before truncation. |
| A9 | Named IND, RI, and NEL dispatch through `index`, `reverse_index`, and `next_line`; RI preserves only untracked raw motion. CSI S/T retain unaffected pending but demote moved/destroyed-source state after topology. LF, IL/DL, direct row helpers, and zero-height bottom-pop enumerate removed endpoints and validate adjacency. Direct insert/remove reconcile pending and sequential authorization after mutation, preserving both only when the cut is non-crossing and the same endpoints remain adjacent. |
| A10 | `exit_alt` computes bottom-trimmed IDs and calls `sever_touching` before truncation. Because the taken `SavedMainRows` is temporarily local, `clear_nonlive_outgoing_marks` then directly clears any surviving saved source's `Row.outgoing` whose ledger edge severed. The geometry helper also clears qualified pending by doomed source ID and raw-only pending by the pre-trim saved cursor row before restore. |
| A11 | ED3 and the separately named `ClearScrollback` event call `clear_history`; `enforce_history_cap` and `ris` cover cap/RIS. Each severs every edge touching removed rows before destruction. |
| A12 | `exit_alt` dooms discarded alt rows; `ris` dooms saved-main rows that are never restored. |
| A13 | `referee_transport_emission_for` accepts only exact transport chunks, derives lines from flags, consumes full rows, and rejects every join without a live edge. Candidate WideEarly markers are derived only from a validated live edge; its BCE base may retain the exact sparse combining entry left by c59's direct assignment, while a severed blank is ordinary content. RIS restores `current_style` to default before later exact-cell construction. `EmissionSurface` separately models attach replay and one-shot GetHistory, including the hard main-history→active-alt boundary. |
| A14 | `Row::blank` defaults to `outgoing: None`; DECAWM off gates consumption/creation, while a source rewrite demotes pre-existing raw pending. The A13 gate treats absent/severed/never-wrapped/untracked state as a hard break while leaving an older valid edge live. |
| A15 | `scroll_up` and positive-height vertical `resize` preserve stable adjacency across `history + visible/SavedMainRows`; `domain_adjacent` validates the boundary at unchanged width. |
| A16 | Fixed-width construction survives accepted literal/REP forward writes, unaffected CSI S/T topology, HTS/TBC, mode-only DECAWM toggles, and unchanged-width resize when the qualified source/cursor row survives without reposition. Every print stores the true pre-map REP character. Raw-retaining non-geometry cuts demote to untracked, whose physical wrap has no edge setter; any live resize clears that raw-only bit. Width-changing resize severs construction under A8. Matched and stale/cross-mode alt-exit geometry rejection clears qualified/raw saved state before restoration, including exhausted identities; exact intact non-exhausted matched 1049 restore alone may resume suspended authorization. |

## Executable closure

- `transport_framing_is_the_authoritative_false_join_surface` folds in the
  round-5 width-5 `ABC; CR; LF; DEF` probe. Correct `true,true` framing passes;
  changing only the first flag to false yields terminal bytes `ABC  DEF\r\n`
  (the first row's two trailing cells are interior after one logical-tail scan)
  and one false join. Extra mid-row termination and reordered chunks are corruption.
- `continuity_and_content_generations_exhaust_fail_closed_without_resurrection`
  drives both checked generation mechanisms at `u64::MAX` and proves exhaustion
  is sticky and the saved token never matches again.
- `row_and_edge_id_allocators_are_checked_nonwrapping_and_sticky` issues
  `u64::MAX` exactly once for each identity class, then proves repeated
  allocation stays `None` and neither MAX nor zero can match/reappear.
- `ordered_full_cells_make_physical_row_composition_unique` folds in the round-4
  5x3 `A` probe: five cells mean `r0`, ten mean `r0+r1(blank)` and force its
  false join, while an extra/all-blank trailing logical line is corruption.
- `styled_cells_are_refereed_before_continuous_ansi_rendering` consumes red
  cells for width-5 `ABCDEF`, checks `r0→r1`, witnesses the continuous
  `SGR-red; ABCDEF; reset` rendering, rejects the same styled merge without an
  edge, and rejects a style-altered shadow stream.
- `hostile_padded_over_under_and_reordered_cell_streams_fail_closed` accepts
  the true stream and rejects under, over, reordered, false-merged, and
  WideEarly-marker-altered candidates.
- `wide_early_and_wrap_source_death_counterexamples_pass` checks
  `abcd界` as a live `WideEarly,padding_count=1` edge emitting `abcd界`, and
  checks 5x1 alt `ABCDEF` as no edge/no panic when the source dies.
- `combining_at_pending_margin_refreshes_qualified_token` folds in the round-6
  width-5 `ABCDE; U+0301; F` case: the mark stays on `E`, the refreshed token
  creates one live edge, and the addressed combining control still severs.
- `styled_wide_early_uses_real_bce_padding_and_referee_accepts` folds in the
  red `abcd界` case: `abcd` and `界` are red, the live-edge-marked BCE blank has
  real `blank_cell` style (background-only, therefore default here), the
  correct transport-framed cells pass, and rendering omits the padding.
- `round_7_c59_fidelity_counterexamples_and_emission_surfaces_pass` folds in
  accepted/invalid DECSTBM, the combining cap, edge-qualified WideEarly
  padding, GetHistory-during-alt, valid/invalid erase modes, bounded character
  edits, CSI parameter defaults, and the no-y-advance/no-self-edge case.
- `round_8_alt_epoch_rep_and_wide_orphan_match_real_c59` folds in the 47-entry/
  1049-exit stale-DECSC counterexample and matched-1049 control, live and
  alt-restore wide-orphan repair, and REP omitted/zero/count dispatcher behavior.
  The six round-8 fixtures additionally pin REP autowrap/A7 and exact logical cells.
- `round_9_alt_exit_matrix_and_zero_height_resize_match_real_c59` executes all
  nine 47/1047/1049 entry/exit combinations plus raw-height-zero resize. It
  proves that only 1049h/1049l restores the saved cursor/token, that every
  47l/1047l result retains the active cursor (`AZCDE`), rejects the erroneous
  matched-47 `ABCDEZ` transport, and severs the row popped after 0→1 sanitization.
- `round_11_mutable_tab_stops_drive_ht_destination_and_a1` folds in both
  required HTS/TBC counterexamples plus TBC-current: a custom stop suppresses
  A1, clearing all stops creates A1 at the margin, and mode 0 clears only the
  current custom stop. All destinations match real c59.
- `round_11_ris_resets_style_before_new_a1_and_referee_accepts` destroys the
  red pre-RIS domain, creates a new default-style `GHIJKL` A1 edge, and accepts
  the source-faithful default-style transport cells.
- `raw_only_wrap_fallback_stays_no_edge_then_later_fresh_a1_is_modeled`
  re-attacks the fallback across save, restore, physical wrap, target rewrite,
  and later printing: the stale raw motion creates no edge, while a later fresh
  qualified margin token creates exactly one.
- `round_12_false_join_repros_fail_closed_and_fidelity_gaps_match_c59` folds in
  RI, ECH, moved-source CSI S, and DECAWM-off raw-pending paths; the 16-mark REP
  case; unchanged-global-column restored-row resize; and styled-blank shrink
  chop. Exact-cell hostile merges for RI, ECH, DECAWM, and REP are rejected.
- `round_15_shrink_chop_is_style_agnostic_and_combining_aware` preserves a
  wrapped combining-bearing target through vertical-shrink migration, while
  plain-space and styled-blank bottom rows remain chop-eligible. Its accepted
  transport still crosses the surviving edge through the A13 referee.
- `round_16_dch_ich_post_shift_repair_uses_bce_blank` asserts exact source-faithful
  current-background BCE cells for DCH's shifted-left orphan and ICH's right-boundary
  orphan, and requires both candidate emissions to pass the exact-cell referee.
- `oracle_self_test_a1_a16_g8_and_phase_a` now pins Phase A's no-stale-motion
  layouts (`ABC | FGZ` and `ABCDZ` with no history) after alt-exit width/trim
  invalidation, while a hostile width-case merge still fails the A13 gate.
- `exhausted_alt_epoch_matched_1049_geometry_clears_raw_motion` drives the same
  Phase-A width/trim workloads with `next_alt_epoch` already sticky-exhausted.
  Both retain zero history and the required layouts; exhaustion never restores
  pending/sequential authority.
- `round_17_false_break_fidelity_and_non_crossing_preservation_pass` accepts
  the exact live WideEarly `[BCE space+U+0301]` suffix and `ABCD界`, preserves
  row0→row1 plus forward `G` through direct row-4 insert/remove, retains
  construction through same-size resize, and reasserts the crossing controls.
- `r17_preservation_fidelity.w1` also groups every remaining named-operation
  preservation half. Since no path other than A1 can recreate a live edge, the
  final live result of each composite proves every included nonendpoint,
  cursor-only, unrelated-topology, history/visible, and disjoint-domain event
  retained rather than over-severed.
- `r18_geometry_raw_invalidation.w1` crosses raw-only pending with matched
  1049 width change, stale DECSC/47→1049 width change, vertical-grow and
  same-size live resize, and active-alt source-row trim. All geometry cases
  overwrite at the clamped margin with zero edges; its no-resize control keeps
  c59 physical raw motion and remains a hard break.
- `r1_named_surfaces.w1` executes source erase, ECH, DCH, ICH, combining-mark,
  IND, RI, NEL-at-margin, and bulk-clear events. In particular,
  `reviewer_dch_source_severs_a7`,
  `reviewer_nel_at_margin_region_cut_severs_a9`, and
  `bulk_clear_scrollback_severs_a11` pin the requested outcomes.
- `r2_findings_and_completeness.w1` executes DECAWM preservation, IL/DL
  inside/outside DECSTBM, CSI T, direct insert/remove, all three saved-cursor
  syntaxes, BS, HT, CSI A/B/C/D/E/F/G, VPA/CSI d, origin-mode home, direct
  horizontal resize, and construction continuity.
- `restore_time_trim_clears_surviving_source_row_outgoing_mark` directly
  inspects the surviving source's `Row.outgoing` after the G8 cell-3 trim.
- `unchanged_alt_restore_preserves_history_boundary_sequential_authorization`
  asserts height-1 `ABCDEF; EnterAlt; ExitAlt; G` without an intervening LF.
- `g8_scrollback_cap_evicts_oldest_preserves_later_edges` uses cap 1 and
  proves one evicted edge severed while two co-moving later edges survived.

The row-by-row evidence inventory for every A1–A16 rule, every named contract
surface, all four G8 cells, and disclosed reachability limits is in
[`COMPLETENESS.md`](COMPLETENESS.md).
The systematic source-line audit for every modeled operation is in
[`C59-FIDELITY.md`](C59-FIDELITY.md).

The A1--A16 claim is exhaustive for the adjacency-edge lifecycle and all named
contract event surfaces, not for unrelated terminal rendering semantics. The
oracle's pre-render boundary carries each real cell's full semantic style
value, including BCE background-only WideEarly blanks, and its test-side
renderer witnesses the real renderer's transition/reset shape; the
referee does not parse or strip ANSI and does not depend on product style-table
IDs.

Round 14 binds that post-gate witness to the accepted logical cell stream:
combining data participates in the one logical-line tail scan, base blanking in
wide-pair repair preserves c59's separate combining entry, and G14 chunk
boundaries remain opaque to trimming. `end_of_line` derivation, exact-row
matching, and live-edge validation are unchanged; the hostile hard-break merge
in `round_14_render_witness_preserves_combining_repair_and_opaque_chunks` still
fails closed before rendering.

## G8-extended cells, both halves

| Cell | Survival half | Severance half |
|---|---|---|
| 1 unchanged alt | `g8_1_unchanged_survives`: `enter_alt`/`exit_alt` move the same internal endpoints and A6 retains. | `g8_1_severance_still_dominates`: post-restore source rewrite reaches A7; `g8_1_live_alt_edge_dies_at_exit` creates a live alt edge and asserts A12 severance on discard. |
| 2 horizontal resize | `g8_2_unchanged_width_survives`: unchanged-width control retains. | `g8_2_horizontal_resize_severs`: `resize` changes alt geometry and `exit_alt` applies A8 to every saved endpoint. |
| 3 vertical trim | `g8_3_vertical_padding_survives` and `g8_3_pair_wholly_above_trim_survives`: padding/intact-above-cut endpoints retain. | `g8_3_trimmed_target_severs_no_dangling`: A10 severs before target pop and fixture `expect-outgoing 0` inspects the source mark; the dedicated Rust test also names the original source ID and reads its mark. |
| 4 history/saved boundary | `g8_4_boundary_unchanged_survives` and no-LF `g8_4_boundary_unchanged_sequential_continues`: A3/A6 restore the live boundary edge and its qualified forward authorization. | `g8_4_boundary_source_clear_severs` reaches A11; `g8_4_boundary_target_width_severs` reaches A8. |

The retained-topology cap cell
`g8_scrollback_cap_evicts_oldest_preserves_later_edges` additionally asserts
both halves in one eviction-triggering workload: the oldest source dies with
no dangling join, while the two later adjacent edges remain live.

The boundary target is the first saved-main row, while c59efe03 restore trim is
bottom-first and dimensions sanitize to at least one row.  Therefore a
history-to-saved-main *boundary target trim* is not reachable through the
current public `Screen` resize path.  A10 still encodes target trim generically,
and the reachable boundary severance riders (source clear and target width
adjustment) are executable fixtures.  The report calls out this reachability
fact rather than manufacturing a terminal byte sequence.

`Grid::clear_scrollback` has no independent public `Screen` byte/API entry
point in c59efe03; ED3 and RIS are its public callers. The oracle's named
`ClearScrollback` operation directly models the bulk A11 lifecycle event. For
the legacy-screen corroboration only, that op is driven through ED3, which
executes the same product helper. This limitation does not weaken the logical
emission referee, but the before-comparison cannot distinguish a defect in an
hypothetical future additional caller until that caller has an externally
drivable test seam.
