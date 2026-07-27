# W1 oracle systematic completeness audit

This inventory is intentionally workload-level. “Driven” names the first-class
oracle `Op` (or the transport-framed ordered-cell candidate surface); “asserted” names the
frozen corpus case whose live/severed counts and logical lines are checked by
`assert_expected`; “mapped” points back to the corresponding `RULE_MAP.md` row.
The corpus has 150 workloads. The referee consumes only the candidate's actual
ordered transport chunks (`chunks[].{cells,end_of_line}`), derives client lines
from the same flags that make the client append CRLF, and deterministically
matches full positive-length frozen rows within each derived line. Character,
width, combining data, semantic style, and WideEarly-padding identity all
participate. WideEarly padding style matches the real BCE blank: current
background only, with every other field defaulted. Cumulative row lengths
strictly increase, so one derived cell stream has exactly one row composition;
there is no candidate-supplied line
grouping, join list, row-ID list, run structure, trimmed text, or ambiguous
blank-row parse. Encoding copies the accepted flags unchanged, concatenates
opaque chunks through each `end_of_line`, scans that logical tail once, and
renders continuously before CRLF. This is the terminal-observable surface with
no candidate-controlled framing below it.
The round-6 pair now makes the G15 styled-CJK/combining seam claim concrete:
one fixture asserts combining continuity at the pending margin and the other
asserts exact styled WideEarly cells plus padding omission and referee acceptance.
Round 17 adds the source-faithful direct-write exception: the BCE suffix base
may retain a pre-existing sparse combining entry, and the live edge/referee
must accept that exact frozen cell while omitting it from replay.
Round 18 adds the raw-only × geometry cross-product. One helper clears both
qualified and untracked pending at matched/stale alt restore and live resize;
the no-resize control proves raw-preserving non-geometry cuts were not widened.
Round 14 additionally pins G14/G15 content fidelity: combining on a default
space renders, both wide-orphan repair paths retain c59's separately stored
combining data, and a physical-row chunk split preserves interior typed spaces.
The exhaustive operation/source cross-check is in
[`C59-FIDELITY.md`](C59-FIDELITY.md); this file remains the A1-A16 workload map.

## A1–A16

| Rule | Oracle drives it? | Corpus workload(s) that assert it | RULE_MAP mapping |
|---|---|---|---|
| A1 | Yes: `Op::Print` and `RepeatLast` reach the sole `perform_deferred_wrap`; normal and `WideEarly` tokens are distinguished, including exact real BCE padding cells and the source-faithful sparse-combining suffix left by the direct c59 write; a c59 x-home with no y advance publishes no self-edge. HT consults mutable state set/cleared by `HorizontalTabSet`/`TabClear`, so later A1 timing uses the real destination. Sticky EdgeId exhaustion performs motion but publishes no edge. | prior A1 workloads plus `wide_early_direct_bce_write_preserves_sparse_combining` | A1 |
| A2 | Yes: qualified pending state includes source revision, cursor continuity, and alt-entry epoch; separate untracked raw state preserves c59 motion but cannot enter A1. RI, accepted addressed mutations, DECAWM-off rewrite, moved-source CSI S/T, and unchanged-geometry restores demote systematically. `invalidate_pending_for_geometry` clears either representation before matched/stale alt restoration or live resize can rearm an invalid source. Combining input updates REP before routing. | Existing construction/exhaustion cases; all `r12_*raw_pending*` cases; four Phase-A ordinary/exhausted cases; all six `r18_*` cases | A2 |
| A3 | Yes: `Op::Lf`/full-grid `ScrollUp` pushes stable rows into history and validates adjacency. | `litmus_width5_emit10`; `g8_scrollback_cap_evicts_oldest_preserves_later_edges` (later co-moving pairs) | A3 |
| A4 | Yes: `Op::Resize` distinguishes raw Screen height from sanitized Grid height. Its CHOP predicate ignores style but treats combining as content, intentionally exceeding c59's separate-storage char-only test. After CHOP classification, migration remains c59-faithful and cursor-bounded, with bottom-pop fallback. | prior resize cases; `r12_styled_blank_bottom_is_chopped`; all three `r15_*` shrink cases | A4 |
| A5 | Yes: `ScrollUp`/`ScrollDown` over DECSTBM retain only wholly co-moving pairs and do not blanket-cancel unaffected pending state. | `partial_region_co_move`; `csi_scroll_down_co_move_survives`; cut controls; `csi_scroll_up_preserves_unaffected_pending`; `csi_scroll_down_preserves_unaffected_pending` | A5 |
| A6 | Yes: 47, 1047, and 1049 are distinct Ops. `SavedMainRows` contains no cursor/token state; only 1049 stamps the independent shared cursor. The exit mode is checked first, so 47l/1047l retain active coordinates and never resume saved-main authorization, while exact 1049h/1049l may. Geometry classification reaches both matched and stale/cross-mode 1049 restore before raw reconstruction. Intact persistent edges otherwise retain at unchanged width. | Existing G8 cases; all nine round-9 combinations; `r18_matched_1049_width_change_clears_saved_raw_only`; `r18_stale_cross_mode_width_change_clears_saved_pending_before_raw_demotion` | A6 |
| A7 | Yes: accepted mutation ops use `before_content_write`/`sever_touching`; raw-retaining ED/EL/ECH/DCH/ICH and DECAWM-off source rewrites demote before writing; every no-op guard runs first. DCH/ICH post-shift orphan repair installs the current-background BCE blank and preserves combining, distinct from general default-blank repair. | prior mutation cases; `r12_ech_raw_pending_is_untracked`; `r12_decawm_off_rewrite_demotes_raw_pending`; both `r16_*bce_blank` cases | A7 |
| A8 | Yes: `Resize` compares each now-visible row width after restoration; only width-changing rows sever, while same-width resize preserves qualified construction when its source survives. Raw-only pending is separately cleared on every resize like c59. All alt-exit Ops sever saved endpoints before width adjustment and geometry-clear qualified/raw state. Both paths repair wide orphans before truncation while preserving combining stored at retained columns. | prior A8 cases; qualified same-size/vertical-grow preservation; all round-18 geometry cases and no-resize control | A8 |
| A9 | Yes: CSI S/T, IND/RI/LF/NEL, guarded IL/DL, direct `InsertRow`/`RemoveRow`, and sanitized grid bottom-pop all run endpoint destruction/topology validation. Direct helpers reconcile rather than blanket-cancel, retaining non-crossing edge/sequential state. | Existing cut/no-op cases; both row-4 direct-helper preservation cases; `zero_height_resize_skips_main_migration_then_bottom_pops` | A9 |
| A10 | Yes: alt exit enumerates trimmed IDs, severs before truncation, reconciles `Row.outgoing` on sources held in the local taken saved grid, and clears qualified/raw pending anchored to a trimmed row. Live active-alt bottom truncation uses the same pending helper before pop. | Existing G8 no-dangling and survival controls; `r18_alt_height_shrink_removes_raw_only_source` | A10 |
| A11 | Yes: `Ed3`, `ClearScrollback`, `Ris`, and actual cap enforcement all sever before destruction. | `tui_ed3_during_alt`; `bulk_clear_scrollback_severs_a11`; `tui_ris_during_alt`; `g8_scrollback_cap_evicts_oldest_preserves_later_edges` | A11 |
| A12 | Yes: both alt-exit variants destroy alt rows; `Ris` destroys active and saved-main domains. | `g8_1_live_alt_edge_dies_at_exit` creates a live alt edge then asserts it severed at exit; `tui_ris_during_alt` asserts saved-main destruction | A12 |
| A13 | Yes: the transport referee derives lines from exact chunks, consumes full rows uniquely, and checks every crossing. Round-12 hostile exact-cell merges prove untracked raw motion and combining REP cannot authorize framing; the round-14 renderer test repeats a hostile hard-break merge after the content-fidelity changes. | Existing framing/referee tests; `round_12_false_join_repros_fail_closed_and_fidelity_gaps_match_c59`; `round_14_render_witness_preserves_combining_repair_and_opaque_chunks` | A13 |
| A14 | Yes: blank/severed/no-wrap/untracked sources have no outgoing edge; DECAWM-off gates creation and demotes on a pending-source rewrite; the referee hard-breaks all such rows. | prior A14 cases; `r12_decawm_off_rewrite_demotes_raw_pending`; round-12 hostile merges | A14 |
| A15 | Yes: unified history+visible/saved ordering validates the boundary at unchanged width. | `vertical_resize_boundary_migration_survives`; `g8_4_boundary_unchanged_survives`; width/clear controls `g8_4_boundary_target_width_severs` and `g8_4_boundary_source_clear_severs` | A15 |
| A16 | Yes: fixed-width state survives authorized forward operations and unchanged-width resize only while still qualified and its source/cursor row survives without reposition. Every resize clears already-unqualified raw state; alt width/trim geometry clears qualified and raw saved state in both restore branches. Raw-retaining non-geometry cuts still demote to structurally no-edge motion. | Existing construction and qualified resize controls; seven round-12 workloads; all six `r18_*` cases, including the no-resize raw-motion control | A16 |

## Systematic preservation-half matrix (round 17)

Every row below names an executable corpus workload whose final live-edge count proves the
preservation half. Liveness is monotone outside A1, so composite preservation workloads cannot hide
an intermediate false break by recreating the edge. The paired column names the crossing,
endpoint-touching, geometry-changing, or otherwise inapplicable control. The audit found the four
reported gaps and no additional oracle over-sever after adding the named-operation controls.

| Rule | Preservation workload(s): edge survives | Severance / hard-break control | Oracle result |
|---|---|---|---|
| A1 | `litmus_width5_emit10`; `wide_early_direct_bce_write_preserves_sparse_combining`; `rep_one_fires_deferred_wrap` | source-death/no-self-edge and EdgeId-exhaustion controls | Sole setter creates only the qualified live edge; source-faithful WideEarly suffix is accepted. |
| A2 | `construction_forward_stream_preserves_a16`; `same_size_resize_preserves_pending_construction`; `vertical_grow_preserves_pending_construction`; `g8_4_boundary_unchanged_sequential_continues` | cursor/rewrite cuts, four Phase-A controls, and round-18 raw-only geometry cases | Forward fixed-width authorization remains live, including qualified unchanged-width resize; geometry-cleared raw never requalifies. |
| A3 | `litmus_width5_emit10`; later pairs in `g8_scrollback_cap_evicts_oldest_preserves_later_edges` | partial/cap endpoint cuts | Whole-row full-grid/scrollback co-movement retains identity and adjacency. |
| A4 | `vertical_resize_boundary_migration_survives`; `vertical_grow_preserves_pending_construction`; `r15_combining_bottom_row_migrates_not_chopped` | zero-height and bottom-pop controls | Unchanged-width migration retains qualifying rows/edge/construction. |
| A5 | `partial_region_co_move`; `csi_scroll_down_co_move_survives`; `persistent_edge_survives_unrelated_scroll_region` | `partial_region_cut`; `csi_scroll_down_cut_severs` | Co-moving or wholly unrelated region topology is retained. |
| A6 | `g8_1_unchanged_survives`; `g8_4_boundary_unchanged_sequential_continues`; `main_edge_survives_47_and_1047_alt_domain_destruction` | mixed/stale exit and geometry controls | Same saved rows/IDs retain; only numeric non-exhausted epoch can resume authority. |
| A7 | `nonendpoint_cell_operations_preserve_edge`; `visible_edge_survives_nonendpoint_ed_and_history_clear`; `history_edge_survives_visible_ed_and_decaln` | source/target mutation fixtures | Writes/erases preserve edges wholly outside their affected cell domain. |
| A8 | `same_size_resize_preserves_pending_construction`; `vertical_grow_preserves_pending_construction`; G8 unchanged-width controls | width severance plus round-18 raw-only same-size/grow/trim clears | Unchanged width does not cut qualified construction, but never preserves stale raw-only motion across resize. |
| A9 | `direct_insert_row_below_edge_preserves`; `direct_remove_row_below_edge_preserves`; `persistent_edge_survives_unrelated_scroll_region`; A5 co-move cases | direct crossing/endpoint, region cut, and zero-height pop cases | Direct helpers and scroll topology retain only non-crossing adjacent endpoints. |
| A10 | `g8_3_vertical_padding_survives`; `g8_3_pair_wholly_above_trim_survives` | `g8_3_trimmed_target_severs_no_dangling` | Pairs wholly outside the restore-time trim survive. |
| A11 | `visible_edge_survives_nonendpoint_ed_and_history_clear`; later pairs in the cap workload | ED3/cap/RIS endpoint destruction controls | Destruction severs only touching edges; RIS is universal domain destruction and has no applicable survivor. |
| A12 | `g8_1_unchanged_survives`; `main_edge_survives_47_and_1047_alt_domain_destruction` | `g8_1_live_alt_edge_dies_at_exit`; `tui_ris_during_alt` | Discarding alt rows leaves an untouched saved-main edge live; edges in discarded domains die. |
| A13 | accepted emissions in `wide_early_direct_bce_write_preserves_sparse_combining` and every live-edge case | hostile false-merge/marker/style/cell candidates | Exact source-faithful live joins pass; unauthorized joins still fail closed. |
| A14 | `decawm_off_preserves_existing_edge`; `decawm_toggle_preserves_existing_pending` | `decawm_off_never_wraps`; severed/untracked hostile merges | Existing provenance survives mode-only gating; absent provenance stays hard. |
| A15 | `vertical_resize_boundary_migration_survives`; both unchanged G8 boundary workloads | boundary width/source-destruction controls | Unified history→visible/saved adjacency retains at unchanged width. |
| A16 | `construction_forward_stream_preserves_a16` (asserted after each char); qualified same-size/vertical-grow resize; combining/REP/CSI-S/T/HTS/TBC/DECAWM controls; `r18_unchanged_geometry_preserves_c59_raw_motion_control` | explicit reposition, width change, and five round-18 geometry clears | Every permitted fixed-width construction step preserves; non-geometry raw motion remains c59-faithful; geometry cuts both representations. |

### Named-operation preservation coverage

| Named operation(s) | Preservation workload | Why the edge survives |
|---|---|---|
| Literal print, REP, forward combining | `construction_forward_stream_preserves_a16`; `rep_one_fires_deferred_wrap`; `combining_at_pending_margin_refreshes_token` | Authorized forward stream. |
| SGR/style, HTS, TBC, DECAWM toggle | `nonendpoint_cell_operations_preserve_edge`; existing HTS/TBC and DECAWM preservation fixtures | Mode/table-only mutation does not touch provenance. |
| Nonendpoint print, EL, ECH, DCH, ICH | `nonendpoint_cell_operations_preserve_edge` | All writes are on row 4, outside row0→row1. |
| ED0, ED3, bulk clear_scrollback | `visible_edge_survives_nonendpoint_ed_and_history_clear` | ED0 is below the pair; history clears have no visible endpoint. |
| ED1, ED2, DECALN | `history_edge_survives_visible_ed_and_decaln` | The live pair is wholly in history while visible cells are changed. |
| CR, DECSC/DECRC, CSI s/u, 1048 save/restore, BS, HT, CSI A/B/C/D/E/F/G, VPA, origin home, CUP/HVP, accepted DECSTBM | `persistent_edge_survives_cursor_and_save_restore_ops` | These commands cut transient sequential authorization where specified but do not themselves mutate or destroy persistent endpoints. |
| LF/VT/FF, IND, RI, NEL away from a margin cut | `persistent_edge_survives_nonmargin_lf_ind_ri_nel` | Cursor motion does not cross or destroy the remote pair. |
| CSI S/T | `persistent_edge_survives_unrelated_scroll_region`; co-move fixtures | Region is wholly below the pair, or both endpoints co-move. |
| IL/DL | `il_outside_region_is_noop`; `dl_outside_region_is_noop` | Inapplicable helper is a total no-op; applicable crossing controls sever. |
| Direct insert/remove row | `direct_insert_row_below_edge_preserves`; `direct_remove_row_below_edge_preserves` | Row-4 balancing change is wholly below row0→row1. |
| Resize | `same_size_resize_preserves_pending_construction`; `vertical_grow_preserves_pending_construction`; `vertical_resize_boundary_migration_survives` | Width unchanged and source/endpoints survive. |
| 1049, 47, and 1047 alt entry/exit | G8 unchanged controls; `main_edge_survives_47_and_1047_alt_domain_destruction` | Saved-main rows retain identity; discarded alt domain is disjoint. |
| Scrollback cap | `g8_scrollback_cap_evicts_oldest_preserves_later_edges` | Only the oldest touching edge is doomed; later adjacent pairs survive. |
| GetHistory / attach referee | all accepted live-edge emissions; `get_history_during_alt_has_hard_main_alt_boundary` | Reader preserves authorized joins and hard boundaries exactly. |
| RIS | No preservation condition exists: RIS destroys active, saved-main, and history domains by contract. | `ris_resets_style_before_new_a1` proves old edges sever and only a later fresh A1 can create new provenance. |

## Every named mutation, topology, destruction, and cursor surface

| Named contract surface | Oracle `Op` that drives it | Corpus workload that asserts it | RULE_MAP mapping |
|---|---|---|---|
| Source print/write | `Print` | `source_print_rewrite_severs` | A7 |
| REP deferred-wrap print | `RepeatLast(Some(1))` | `rep_one_fires_deferred_wrap` | A1/A2/A16 |
| REP post-reposition rewrite | `RepeatLast(Some(1))` after CUP | `rep_after_cup_source_rewrite_severs_a7` | A7 |
| REP zero/default/count parameters | `RepeatLast(Some(0)/None/Some(2))` | `rep_zero_and_default_each_repeat_once`; `rep_explicit_count_repeats_exactly` | A1/A16 guard |
| Source erase | `EraseLine` | `erase_line_source_severs_a7` | A7 |
| ECH | `EraseChars` | `ech_source_severs_a7` | A7 |
| DCH | `DeleteChars` | `reviewer_dch_source_severs_a7`; exact BCE repair `r16_dch_post_shift_orphan_uses_blue_bce_blank` | A7/G15 |
| ICH | `InsertChars` | `ich_source_severs_a7`; exact BCE repair `r16_ich_boundary_orphan_uses_blue_bce_blank` | A7/G15 |
| DECALN | `Decaln` | `decaln_severs_touched_edges` | A7 |
| Forward combining at pending margin | `CombiningMark` | `combining_at_pending_margin_refreshes_token` | A2/A16 |
| Addressed combining write | `Cup` + `CombiningMark` | `combining_source_severs_a7` | A7 |
| Target write after CUP/HVP reposition | `Cup` + `Print` | `target_cup_write_severs` | A7 |
| CSI A (CUU) | `CursorUp` + `Print` | `csi_a_target_stream_reposition_severs` | A2/A7/A16 |
| CSI B (CUD), including clamped motion | `CursorDown` + `Print` | `csi_b_clamped_target_reposition_severs` | A2/A7/A16 |
| CSI C (CUF) | `CursorForward` + `Print` | `csi_c_target_reposition_severs` | A2/A7/A16 |
| CSI D (CUB), reviewer counterexample | `CursorBack` + `Print` | `csi_d_target_reposition_severs` | A2/A7/A16 |
| CSI E (CNL), including clamped motion | `CursorNextLine` + `Print` | `csi_e_clamped_target_reposition_severs` | A2/A7/A16 |
| CSI F (CPL) | `CursorPrevLine` + `Print` | `csi_f_source_reposition_severs` | A2/A7/A16 |
| CSI G (CHA) | `CursorHorizontalAbsolute` + `Print` | `csi_g_target_reposition_severs` | A2/A7/A16 |
| VPA / CSI d | `CursorVerticalAbsolute` + `Print` | `vpa_target_reposition_severs` | A2/A7/A16 |
| Origin-mode enable/home | `OriginMode(true)` + `Print` | `origin_mode_home_target_reposition_severs` | A2/A7/A16 |
| Accepted DECSTBM home | `SetScrollRegion` + `Print` | `accepted_decstbm_home_severs_target_stream` | A2/A7/A16 |
| Invalid DECSTBM no-op | invalid `SetScrollRegion` + `Print` | `invalid_decstbm_is_noop_and_preserves_stream`; out-of-range `out_of_range_decstbm_top_is_noop` | A2/A7/A16 guard |
| CSI S | `ScrollUp` | `partial_region_cut`; survival `partial_region_co_move` | A5/A9 |
| CSI T | `ScrollDown` | `csi_scroll_down_cut_severs`; survival `csi_scroll_down_co_move_survives` | A5/A9 |
| IND | `Index` | `ind_at_margin_region_cut_severs_a9` | A9 |
| RI | `ReverseIndex` | `ri_at_margin_region_cut_severs_a9` | A9 |
| RI retained raw pending | `ReverseIndex` + later `Print` | `r12_ri_raw_pending_is_untracked` | A2/A9/A16 |
| Addressed mutation retained raw pending | `EraseChars` + later `Print` | `r12_ech_raw_pending_is_untracked` (shared transition also used by ED/EL/DCH/ICH) | A2/A7/A16 |
| CSI S moved pending source | `ScrollUp` + later `Print` | `r12_csi_s_moved_pending_is_untracked` | A2/A5/A9/A16 |
| DECAWM-off pending-source rewrite | `Decawm(false)` + margin `Print` + enable/print | `r12_decawm_off_rewrite_demotes_raw_pending` | A2/A7/A14/A16 |
| LF at DECSTBM margin | `Lf` | `lf_at_margin_region_cut_severs`; full-grid preservation `litmus_width5_emit10` | A3/A9 |
| NEL / ESC E at margin | `NextLine` | `reviewer_nel_at_margin_region_cut_severs_a9` | A9 |
| IL inside region (cut) | `InsertLines` | `il_inside_region_cuts_edge` | A9 |
| IL outside region (ignored) | `InsertLines` with cursor outside DECSTBM | `il_outside_region_is_noop` | A9 applicability guard |
| DL inside region (cut) | `DeleteLines` | `dl_inside_region_cuts_edge` | A9 |
| DL outside region (ignored) | `DeleteLines` with cursor outside DECSTBM | `dl_outside_region_is_noop` | A9 applicability guard |
| Direct row insert | `InsertRow` | crossing `direct_insert_row_crossing_severs`; preservation `direct_insert_row_below_edge_preserves` | A9 |
| Direct row remove | `RemoveRow` | endpoint `direct_remove_row_endpoint_severs`; preservation `direct_remove_row_below_edge_preserves` | A9 |
| Live horizontal resize | `Resize` | `live_horizontal_resize_severs` | A8 |
| Restored row width change at unchanged global columns | two `Resize` operations | `r12_restored_row_resizes_at_unchanged_global_cols` | A4/A8/A15 |
| Styled-blank bottom shrink chop | styled `EraseLine` + vertical `Resize` | `r12_styled_blank_bottom_is_chopped` | A4/A11/G8 |
| Combining-bearing bottom shrink migration | wrapped spaces + `CombiningMark` + vertical `Resize` | `r15_combining_bottom_row_migrates_not_chopped`, with plain/styled chop controls | A4/A9/A15/G10 |
| Raw zero-height resize | `Resize { rows: 0 }` | `zero_height_resize_skips_main_migration_then_bottom_pops` | A4/A9/G8 |
| Live resize wide-orphan repair | `Print` + `CombiningMark` + `Resize` | `r14_live_resize_wide_repair_preserves_combining` plus the prior round-8 control | A8/G10/G15 |
| Alt-exit horizontal adjustment | `EnterAlt1049` + `Resize` + `ExitAlt1049` | `g8_2_horizontal_resize_severs`; boundary target `g8_4_boundary_target_width_severs` | A8 |
| Alt-exit wide-orphan repair | `EnterAlt1049` + `Resize` + `ExitAlt1049` | `r14_alt_restore_wide_repair_preserves_combining` plus the prior round-8 control | A8/G8 cell 2/G10/G15 |
| Restore-time trim | `EnterAlt1049` + vertical `Resize` + `ExitAlt1049` | `g8_3_trimmed_target_severs_no_dangling` (`expect-outgoing 0`) plus direct `Row.outgoing` assertion test | A10 |
| Alt-exit geometry rejects saved pending | matched 1049 + horizontal `Resize` or restore-time trim + later `Print` | ordinary and `*_alt_epoch_exhausted` Phase-A width/trim workloads assert no stale motion/history; the width-case hostile merge is still rejected | A2/A8/A10/A13/A16 |
| Scrollback-cap eviction | actual `scrollback_limit=1` enforcement from `Print` wraps | `g8_scrollback_cap_evicts_oldest_preserves_later_edges` | A11 / G8 retained topology |
| ED3 | `Ed3` | `tui_ed3_during_alt` | A11 |
| RIS | `Ris` | `tui_ris_during_alt`; post-reset style/A1 `ris_resets_style_before_new_a1` | A11/A12/A13 |
| Bulk `clear_scrollback` | `ClearScrollback` | `bulk_clear_scrollback_severs_a11` | A11 |
| Alt 1049 save/restore | `EnterAlt1049` / `ExitAlt1049` | `g8_1_unchanged_survives`; all four G8 fixture files | A6/A8/A10/A12/A15 |
| Alt 47 cursor entry/exit | `EnterAlt47` / `ExitAlt47` | 47h row and 47l column of the round-9 3x3 matrix, including matched-47 `AZCDE` | A2/A6/A7/A16 |
| Alt 1047 cursor entry/exit | `EnterAlt1047` / `ExitAlt1047` | 1047h row and 1047l column of the round-9 3x3 matrix | A2/A6/A7/A16 |
| Alt 1049 shared-cursor entry/exit | `EnterAlt1049` / `ExitAlt1049` | 1049h/1049l restore control plus both mixed columns/rows in the round-9 matrix | A2/A6/A7/A16 |
| DECSC / DECRC (ESC 7/8) | `Decsc` / `Decrc` | `decsc_decrc_saved_pending_cannot_rearm` | A2/A7/A16 |
| CSI s / CSI u | `CsiSaveCursor` / `CsiRestoreCursor` | `csi_s_u_saved_pending_cannot_rearm` | A2/A7/A16 |
| Mode 1048 save/restore | `Mode1048Save` / `Mode1048Restore` | `mode_1048_saved_pending_cannot_rearm` | A2/A7/A16 |
| Backspace | `Backspace` | `backspace_target_overwrite_severs` | A7/A16 |
| Horizontal tab | `HorizontalTab` | `horizontal_tab_target_overwrite_severs` | A7/A16 |
| HTS / ESC H | `HorizontalTabSet` | `hts_custom_stop_suppresses_a1` | A1/A2/A16 |
| TBC / CSI g, clear current/all | `TabClear(0/3)` | `tbc_current_clears_only_custom_stop`; `tbc_clear_all_creates_a1` | A1/A2/A16 |
| Combining cap no-op | `CombiningMark` at length 16 | `combining_mark_17_is_noop_and_preserves_edge` | A2/A7/A16 guard |
| Combining input as REP character | sixteen `CombiningMark` inputs + `RepeatLast(None)` | `r12_combining_rep_uses_true_last_char` | A1/A2/A16 |
| WideEarly marker after severance | styled `Print` + source rewrite | `severed_wide_early_padding_is_real_styled_content` | A7/A13/A14 |
| GetHistory during alt | `EmissionSurface::GetHistory` | `get_history_during_alt_has_hard_main_alt_boundary` | A13/A14 |

## Four G8 alt-geometry cells, both halves

| G8 cell | Survival workload | Severance workload(s) | RULE_MAP mapping |
|---|---|---|---|
| 1. Unchanged-size alt save/restore | `g8_1_unchanged_survives` | `g8_1_severance_still_dominates`; live alt-domain destruction `g8_1_live_alt_edge_dies_at_exit` | G8 cell 1 / A6+A7+A12 |
| 2. Horizontal resize during alt | `g8_2_unchanged_width_survives` | `g8_2_horizontal_resize_severs` | G8 cell 2 / A8 |
| 3. Vertical trim/padding | `g8_3_vertical_padding_survives`; `g8_3_pair_wholly_above_trim_survives` | `g8_3_trimmed_target_severs_no_dangling` | G8 cell 3 / A6+A10 |
| 4. History→saved-main boundary | `g8_4_boundary_unchanged_survives`; no-LF forward continuation `g8_4_boundary_unchanged_sequential_continues` | source: `g8_4_boundary_source_clear_severs`; target: `g8_4_boundary_target_width_severs` | G8 cell 4 / A2+A6+A8+A11+A15 |
| G8 retained-topology scrollback-cap cell | later two edges survive in `g8_scrollback_cap_evicts_oldest_preserves_later_edges` | oldest evicted source edge severs in the same workload, with no derived dangling join | G8 retained topology / A3+A11 |

## Disclosed reachability limits

- `Grid::clear_scrollback` has no independent public `Screen` byte/API entry
  point in c59efe03. The oracle drives `ClearScrollback` directly; the legacy
  comparison uses ED3, a real public caller of that helper. Interleaving its
  public ED3/RIS callers with later prints cannot retain a destroyed endpoint;
  the universal reader also rejects any stale mark.
- Direct `insert_visible_row` / `remove_visible_row` are private product helper
  surfaces. The oracle has distinct `InsertRow` / `RemoveRow` events; the legacy
  comparison uses their public IL/DL callers. The oracle assertions themselves
  do not collapse these events into IL/DL; follow-on scroll/resize/alt sequences
  cannot bypass the immediate identity/adjacency validation.
- A history→saved-main boundary **target trim** is not reachable through the
  current public resize path: restore trimming is bottom-first and the boundary
  target is the first saved-main row, while dimensions retain at least one row,
  even after cursor/region changes, ignored nested alt entries, or zero resize.
  A10 models target removal generically; reachable cell-4 severance riders are
  source clear (ED3) and target width adjustment.
- A new scrollback-cap eviction while alt is active is not reachable with the
  current fixed-at-construction cap because alt output and alt-time resize
  cannot append main history. Actual cap eviction/no-dangling/co-movement is asserted by
  `g8_scrollback_cap_evicts_oldest_preserves_later_edges`; the reachable
  during-alt history-source destruction rider is asserted with ED3.

## Round-12 row-by-row, dispatcher, independence, and disclosure re-audit

After the additions above, every row marked asserted in this file was re-read
against the named workload in the actual 150-workload corpus and against the
assertion path in `assert_expected` or the specifically named Rust test. Each
named workload genuinely drives the listed `Op` and asserts the stated
live/severed and logical-output half; A10 additionally asserts the source-side
`Row.outgoing` mark, A13 asserts transport-framing false-join and corruption
verdicts, and A16's forward-stream cases include both per-character inspection
and the pending-margin combining refresh, plus sticky checked exhaustion for
continuity, content revision, RowId, and EdgeId. The four
reachability disclosures remain disclosures rather than claimed assertions.
I also re-walked the full c59 `execute`, CSI, and ESC dispatcher tables for every
print/write, cursor reposition, cell/row insert-delete-repeat, and alt entry/exit
arm. REP and 47/1047 were the only further dispatcher omissions found; both are
now modeled and asserted. The separate resize review found and closed the
wide-orphan repair gap. Round 9 then split 47 and 1047 into explicit operations
and added the complete 3x3 alt matrix plus raw-zero resize. The oracle's saved
rows no longer carry cursor/pending fields: exit-mode behavior is derived from
the active cursor and independent shared cursor exactly at the performer seam.
Resize likewise retains raw height through the Screen guard before sanitizing
at the grid phase. No additional qualifying dispatcher operation was omitted;
the per-operation evidence, aliases, and honest non-action disclosures are
recorded in `C59-FIDELITY.md`. Round 11 re-attacked each residual with
multi-operation sequences. HTS/TBC failed that attack and are now modeled.
Round 12 replaced the false raw-only disclosure with a complete operation walk
and executable RI, ECH, CSI-S, and DECAWM-off interactions. Boundary-target
trim, cap eviction during alt, charset selection, private-helper seams, aliases,
and non-action dispatches remain false-join-closed under the explicit arguments
in `C59-FIDELITY.md`; no other bad disclosure was found.
