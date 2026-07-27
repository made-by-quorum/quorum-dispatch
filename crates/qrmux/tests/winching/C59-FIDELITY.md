# c59 per-operation fidelity audit

This is the round-12 systematic audit of every edge-relevant `Op` modeled by
`oracle.rs`. I re-walked the complete c59 `execute`, CSI, and ESC dispatcher
tables—not only the previously listed operations—and cross-checked every cursor operation and
every guarded content operation against the real c59 performer, plus resize/grid and both public
emission call paths. “Matches” concerns the edge-relevant observable: cursor
coordinates, accepted/no-op status, pending/sequential authorization,
content-write/revision occurrence, row topology, BCE cell values, and emitted
row order. The oracle's qualified provenance token is intentionally stricter
than c59's raw Boolean `wrap_pending` after any provenance cut, as A2/A7/A16
require; this is the contract being tested, not a claim that c59 already stores
provenance.

## Cursor, movement, and mode operations

| Modeled operation | Real c59 effect and source | Oracle match | Asserting workload/test |
|---|---|---|---|
| CR | Home x and clear pending (`performer.rs:630-634`). | Yes: home and cancel construction/target authorization. | `construction_cr_rewrite`; `reviewer_no_edge_cr_lf_rows` |
| LF / VT / FF | Clear pending; advance or scroll, but do not pass the physical bottom (`performer.rs:635-643`). `Op::Lf` represents all three aliases. | Yes, including cursor-below-partial-region bottom clamp. | `lf_at_margin_region_cut_severs`; `litmus_width5_emit10` |
| IND | Same vertical motion/clear-pending shape as LF (`performer.rs:783-790`). | Yes. | `ind_at_margin_region_cut_severs_a9` |
| NEL | Home x, clear pending, then LF/scroll (`performer.rs:792-800`). | Yes. | `reviewer_nel_at_margin_region_cut_severs_a9` |
| RI | Move up or reverse-scroll without clearing raw pending (`performer.rs:802-809`). The explicit reposition ends B1 provenance authorization. | Yes: qualified state is demoted to `untracked_wrap_pending`; the next printable performs c59's physical wrap but cannot publish A1. | `ri_at_margin_region_cut_severs_a9`; `r12_ri_raw_pending_is_untracked` |
| CUP / HVP | Clear pending; origin-relative or absolute y; set x (`performer.rs:273-286`, dispatch `:691`). | Yes, including clamping. | `target_cup_write_severs` |
| CSI A (CUU) | Clear pending and clamp to applicable top (`performer.rs:205-214`). | Yes. | `csi_a_target_stream_reposition_severs` |
| CSI B (CUD) | Clear pending and clamp to applicable bottom (`performer.rs:216-225`). | Yes. | `csi_b_clamped_target_reposition_severs` |
| CSI C (CUF) | Clear pending; saturating right clamp (`performer.rs:227-235`). | Yes. | `csi_c_target_reposition_severs` |
| CSI D (CUB) | Clear pending; saturating left move (`performer.rs:237-241`). | Yes. | `csi_d_target_reposition_severs` |
| CSI E (CNL) | Clear pending, home x, clamp down (`performer.rs:243-253`). | Yes. | `csi_e_clamped_target_reposition_severs` |
| CSI F (CPL) | Clear pending, home x, clamp up (`performer.rs:255-265`). | Yes. | `csi_f_source_reposition_severs` |
| CSI G (CHA) | Clear pending and set clamped x (`performer.rs:267-271`). | Yes. | `csi_g_target_reposition_severs` |
| VPA / CSI d | Clear pending; origin-relative or absolute y (`performer.rs:288-299`). | Yes. | `vpa_target_reposition_severs` |
| DECSTBM accepted | After CSI zero-defaults, require `top <= bottom`; install region, home x and y (origin top or row 0), clear pending (`performer.rs:468-480`, dispatch `:675-681,736-738`). | Yes. This is an explicit continuity cut even for `1;rows`. | `accepted_decstbm_home_severs_target_stream`; real-screen assertion in `round_7_counterexamples_match_real_c59_observations` |
| DECSTBM invalid | Changes no region, cursor, or pending state (`performer.rs:468-480`). An out-of-range top is not clamped into validity. | Yes: true no-op, including zero defaults. | `invalid_decstbm_is_noop_and_preserves_stream`; `out_of_range_decstbm_top_is_noop` |
| Origin mode | Enable homes to region top and clears pending; disable only changes the mode (`performer.rs:526-532`). | Yes. | `origin_mode_home_target_reposition_severs` (enable); disable is exercised by reset/alt corpus paths but has no separate edge mutation. |
| DECSC / DECRC | Save copies position, style, charsets, autowrap, origin, and raw pending; restore loads them (`performer.rs:80-119,810-811`). Same-domain restore is an explicit B1 reposition and cannot re-arm stale provenance. | Yes for every oracle-modeled field. Charset selection is not an oracle `Op`. | `decsc_decrc_saved_pending_cannot_rearm`; `saved_cursor_restore_restores_autowrap_mode` |
| CSI s / u | Same shared save/restore implementation (`performer.rs:739-740`). | Yes. | `csi_s_u_saved_pending_cannot_rearm` |
| Mode 1048 save/restore | Same shared save/restore implementation (`performer.rs:548-553`). | Yes. | `mode_1048_saved_pending_cannot_rearm` |
| BS | Clear pending; decrement x only above zero (`performer.rs:644-650`). | Yes, including numerically clamped motion. | `backspace_target_overwrite_severs` |
| HT / HTS / TBC | HT clears pending and consults the mutable grid table (`performer.rs:651-657`; `grid.rs:641-649`). HTS sets the current-column stop (`performer.rs:777-782`); TBC 0 clears the current stop and TBC 3 clears all stops (`performer.rs:741-753`). HTS/TBC do not themselves clear pending. Resize and RIS restore default stops (`grid.rs:741-772`; `performer.rs:812-840`). | Yes: `tab_stops` is mutable terminal state; `HorizontalTabSet` and `TabClear(0/3)` update it, and HT uses it before its ordinary authorization cut. | `hts_custom_stop_suppresses_a1`; `tbc_clear_all_creates_a1`; `tbc_current_clears_only_custom_stop`; `horizontal_tab_target_overwrite_severs` |
| Alt entry (all modes) | Every accepted entry drains/saves main rows, homes the one active cursor, resets the region, and clears raw pending; only 1049h first saves the shared cursor (`performer.rs:129-151,556-568`). | Yes, independently: `SavedMainRows` contains rows/modes/region but deliberately has no cursor, pending, sequential, or continuity fields. The active cursor is homed directly; only 1049 creates a `SavedCursor`. | Nine `r9_alt_exit_and_zero_resize.w1` combinations; `round_9_alt_exit_matrix_and_zero_height_resize_match_real_c59` |
| Alt exit 1049l | Rows/modes/region restore, then `do_restore_cursor == true` invokes the shared cursor restore (`performer.rs:159-185,556-562`). Real c59 can reload a raw saved pending bit after alt-time resize/restore trim. | Exact intact 1049h/1049l restores position and may resume epoch-qualified authorization only with a non-exhausted matching epoch. Intentional Spec-W correction: one geometry helper clears both qualified and raw-only saved motion after width change or source-row trim before either matched or stale/cross-mode restoration; entry-operation provenance still applies after AltEpoch exhaustion. At unchanged geometry stale/cross-mode pending retains c59 physical motion only and remains structurally no-edge. | Existing exact/stale/exhausted controls; round-18 matched and stale/cross-mode geometry cases |
| Alt exit 47l/1047l | Rows/modes/region restore, but `do_restore_cursor == false` skips `restore_cursor`, so the active alt cursor/pending state persists (`performer.rs:159-185,563-568`). | Yes: neither `SavedMainRows` nor `SavedCursor` is consulted for cursor authorization. The active coordinates persist; qualified alt tokens die with discarded alt rows. | Six matrix workloads ending in 47l/1047l, including matched 47 and 1047 and the 1049h/47l and 1049h/1047l mixed cases |
| DECALN | Reset region, home, clear pending, write default-style `E` everywhere (`performer.rs:867-879`). | Yes, including default style and one revision advance per row. | `decaln_severs_touched_edges` |
| RIS | Home/reset pending, modes, region, saved domains, visible content, scrollback, tab stops, and `current_style = Style::default()` (`performer.rs:812-845`). | Yes, including default style on later cells and a new post-RIS A1 edge. | `tui_ris_during_alt`; `ris_resets_style_before_new_a1` / `round_11_ris_resets_style_before_new_a1_and_referee_accepts` |
| DECAWM | Mode change only; it does not clear pending (`performer.rs:534`). Off gates wrap consumption/creation but an off/on toggle alone preserves pending and existing edges. A width-1 margin rewrite while off also leaves c59 raw pending set. | Yes: mode-only toggles preserve qualification; a source rewrite while off demotes the surviving raw bit before later physical wrap. | `decawm_off_never_wraps`; `decawm_off_preserves_existing_edge`; `decawm_toggle_preserves_existing_pending`; `r12_decawm_off_rewrite_demotes_raw_pending` |

## Content writes, guards, and topology operations

| Modeled operation | Real c59 guard/write and source | Oracle match | Asserting workload/test |
|---|---|---|---|
| Ordinary print / deferred wrap | Zero-width routes to combining; pending+DECAWM wraps; in-bounds cell write sets pending at margin (`performer.rs:577-625`). `set_cell` repairs wide pairs (`grid.rs:776-799`). | Yes for cell/write, wide-pair repair, pending creation, and sole edge publication. | `litmus_width5_emit10`; `construction_edge_is_live_after_every_forward_target_character` |
| REP / CSI b | Every VTE print first stores the pre-map input, including width-0 combining input (`performer.rs:577-584`); CSI omitted or zero count defaults to one and REP re-invokes that character through `print` (`:698-702`). | Yes: both `Print` and `CombiningMark` reach `print_char`, which stores the input before width routing; REP then repeats the true last character. | Four prior REP cases; `r12_combining_rep_uses_true_last_char` |
| WideEarly print | Width-2 at last column: DECAWM off or width `<2` is no-op; otherwise directly assign a BCE blank base, wrap, then write glyph (`performer.rs:591-614`; BCE `:33-40`). The direct assignment bypasses `Grid::set_cell`, so a pre-existing separately stored `Row.combining` entry at that column survives (`cell.rs:43-50`; `grid.rs:782-797`). | Yes. The embedded oracle cell performs base-only BCE replacement, preserves the combining payload, and the exact-cell reader accepts it while deriving padding meaning only from the live edge. | prior WideEarly cases; `wide_early_direct_bce_write_preserves_sparse_combining` |
| Combining mark | The pre-map input becomes the REP character before width routing; no target / out-of-bounds is no-op; continuation targets base; only lengths `< MAX_COMBINING` write (`performer.rs:10-13,483-507,577-584`). | Yes: REP state updates even on a cap-rejected mark; the cap is checked before the content facade, so mark 17 causes no write, revision, sever, or raw consumption. Qualified and untracked raw pending select the same physical margin target. | prior combining cases; `r12_combining_rep_uses_true_last_char` |
| Style change | SGR changes current style, not row content (`performer.rs:704`). | Yes: no revision/sever. | `styled_wide_early_uses_bce_padding`; `severed_wide_early_padding_is_real_styled_content` |
| ED 0/1/2 | Valid variants erase their exact visible ranges with BCE blanks without clearing raw pending; invalid modes no-op (`performer.rs:301-329`; grid ranges `grid.rs:801-835`). | Yes: accepted writes demote qualified pending to untracked before mutation; invalid modes preserve it unchanged. | prior ED cases; raw behavior shares the ECH/RI round-12 gate |
| ED 3 | Clear scrollback only (`performer.rs:322-327`). | Yes. | `tui_ed3_during_alt` |
| EL 0/1/2 | Valid variants erase exact row ranges with BCE blanks without clearing raw pending; invalid modes no-op (`performer.rs:332-350`; `grid.rs:801-821`). | Yes: accepted writes demote to untracked; invalid modes change nothing. | prior EL cases |
| ECH | CSI zero defaults to one; end clamps to width; valid cursor writes BCE blanks without clearing raw pending (`performer.rs:353-361`, dispatch `:675-681,695`). | Yes: accepted mutation retains only untracked raw motion. | prior ECH cases; `r12_ech_raw_pending_is_untracked` |
| DCH | CSI zero defaults to one; count clamps to remaining width; post-shift orphan repair uses `blank_cell()` (current-background BCE); raw pending is retained (`performer.rs:364-389`). | Yes: accepted mutation retains only untracked raw motion, and post-shift orphan repair installs a BCE blank while preserving separately stored combining. | prior DCH cases; `r16_dch_post_shift_orphan_uses_blue_bce_blank` |
| ICH | CSI zero defaults to one; count clamps to remaining width; post-shift boundary repair uses `blank_cell()` (current-background BCE); raw pending is retained (`performer.rs:391-414`). | Yes: accepted mutation retains only untracked raw motion, and boundary orphan repair installs a BCE blank while preserving separately stored combining. | prior ICH cases; `r16_ich_boundary_orphan_uses_blue_bce_blank` |
| CSI S / T | Dispatcher defaults zero to one; helper caps at physical row count; region scroll uses BCE blank and does not clear pending (`performer.rs:416-428,732-733`; `grid.rs:650-712`). | Yes: an unaffected pending source remains qualified; if topology moves/destroys the source, the surviving raw bit is demoted and later wraps physically without A1. | prior survival/cut cases; `r12_csi_s_moved_pending_is_untracked` |
| IL / DL | Only apply with cursor inside inclusive region; outside is total no-op. Count clamps to remaining region (`performer.rs:430-466`). | Yes: guard precedes authorization cancellation/topology mutation. | `il_inside_region_cuts_edge`; `il_outside_region_is_noop`; `dl_inside_region_cuts_edge`; `dl_outside_region_is_noop` |
| Direct insert/remove row | Private grid helpers remove/insert at a visible index (`grid.rs:622-639`); not directly reachable through public bytes. | Oracle models the contract events separately; public corroboration uses IL/DL. It preserves a pre-existing edge and sequential authorization when the helper cut is wholly below/above the adjacent endpoints, and severs only endpoint/crossing/broken-adjacency cases. | crossing controls plus `direct_insert_row_below_edge_preserves` and `direct_remove_row_below_edge_preserves` |
| Resize | `Screen::resize` evaluates growth/shrink against raw height, and c59 shrink-chops bottom rows whose cell chars are space or width-0 regardless of style (`screen/mod.rs:321-350`; `grid.rs:520-538`). Combining is stored separately on `Row` (`cell.rs:43-50`), so that predicate does not inspect it. For a positive shrink, migration is limited to `needed.min(cursor_y)` (`screen/mod.rs:337-350`); `Grid::resize` then unconditionally clears its raw Boolean, bottom-pops remaining excess rows, and adjusts every survivor's width (`grid.rs:741-769`). | Raw-zero ordering, per-row width adjustment, cursor-bounded migration, bottom-pop fallback, and style-agnostic blankness match. There are two intentional Spec-W improvements: CHOP additionally requires empty combining data, and a still-qualified A2/A16 token survives unchanged-width resize when its source/cursor row survives. Already-unqualified raw state always clears on resize like c59; width change still severs qualified state under A8. | Prior resize and qualified-preservation cases; round-18 grow/same-size/active-alt-trim raw-only cases |
| Scrollback cap | Full-grid main scroll moves the row into history and evicts only beyond the configured cap (`grid.rs:650-680`). | Yes, severing only doomed endpoint edges. | `g8_scrollback_cap_evicts_oldest_preserves_later_edges` |
| Bulk clear scrollback | Drain all history (`grid.rs:715-720`). | Yes as a direct lifecycle event. | `bulk_clear_scrollback_severs_a11` |

## Provisional scope disclosure

**PROVISIONAL — PENDING coordinator-backstop scope confirmation:** vertical-shrink below-cursor
content loss (cursor-bounded migration + bottom-pop) is c59-faithful, fail-closed w.r.t.
false-join, and outside the logical-line-replay/width-drift scope.

This disclosure does not roll back round 15: combining-bearing rows remain non-CHOP under the
oracle's combining-aware classification. It applies only after that classification, when too few
rows lie above the cursor for c59's migration bound and the remaining bottom excess is popped.

## Emission surfaces and WideEarly qualification

| Surface | Real c59 composition | Oracle/referee match | Assertion |
|---|---|---|---|
| Attach replay, main | History plus the attach seam/current view; B1 gates the resulting transport framing. | Existing attach domain remains the default. | transport-framing tests and G8 boundary workloads |
| Attach replay, alt active | Main history is suppressed (`server/session_bridge.rs:87-101`); active alt is replayed. | `EmissionSurface::AttachReplay` emits active alt rows only. | `round_7_c59_fidelity_counterexamples_and_emission_surfaces_pass` asserts attach result `DEF`. |
| One-shot GetHistory, main | Scrollback followed by visible rows, trimming only trailing blank visible rows (`screen/mod.rs:200-237`; `session.rs:216-232`). | `EmissionSurface::GetHistory` emits main order. | ordinary corpus comparisons use `get_content_history`. |
| One-shot GetHistory, alt active | Main scrollback followed by active visible alt rows; only trailing visible blanks are trimmed, never history (`screen/mod.rs:200-237`). No adjacency edge crosses that domain boundary. | Exact ordered domain; correct hard break accepted, a history→alt merge rejected, and blank history retained when alt is blank. | `get_history_during_alt_has_hard_main_alt_boundary`; dedicated referee assertions in the round-7 test |
| WideEarly padding | c59 stores an ordinary BCE blank (`performer.rs:33-40,591-603`); omission is a B1 live-edge reader fact. | Candidate marker is generated only for the suffix of a validated live WideEarly edge to the next emitted row. After severance the styled blank is unmarked, emitted, and refereed. | `styled_wide_early_uses_real_bce_padding_and_referee_accepts`; `severed_wide_early_padding_is_real_styled_content` |

## Honest reachability/model limits

- `Grid::clear_scrollback` has no independent public `Screen` byte/API entry
  point; the direct oracle event is asserted, while c59 corroboration uses its
  public ED3 caller. ED3/RIS followed by print/resize/alt sequences cannot
  retain a destroyed endpoint: both callers sever first, and the universal
  reader rejects a stale source mark by target identity/adjacency.
- Direct visible-row insert/remove helpers are private. Their oracle events are
  asserted independently; public c59 corroboration uses IL/DL. Follow-on
  scroll, resize, and alt sequences cannot turn a missed private join into a
  false join because every read revalidates both IDs, width, and adjacency.
- The existing history→saved-main boundary-target-trim path remains unreachable,
  including after adversarial cursor moves, region scrolls, nested/ignored alt
  entries, zero/positive resizes, ED3, and RIS: the boundary target is always
  saved row zero, exit trimming is bottom-first, and sanitized dimensions retain
  at least one row. Width change severs it under A8; ED3/RIS destroy a domain.
- A new cap eviction while alt is active remains unreachable even when alt
  scrolling, resizing, ED3/RIS, and mixed exit modes are interleaved. Alt entry
  sets the active scrollback limit to zero; alt output cannot append main
  history, alt resize skips main migration, and the cap is fixed at construction.
- `Op::Lf` represents c59's LF/VT/FF aliases. `Op::Cup` represents both CUP and
  HVP, which dispatch to the same method. Modes 47 and 1047 intentionally have
  separate Ops despite sharing helpers, because their complete 3x3 entry/exit
  mode matrix is evidence-bearing for this audit. Alias→save/restore,
  alias→region-scroll, alias→alt, and alias→later-print attacks all enter the
  identical c59 helper before any later operation, so no hidden alias state can
  authorize a different edge.
- SO/SI and the ESC G0/G1 charset selectors remain unmodeled: they configure
  mapping for a later print but do none of the four audited actions themselves.
  This residual was re-attacked with selector→print-at-margin, selector→REP,
  and selector→save/switch/restore→print sequences. Every ASCII/DEC-line-drawing
  mapping remains width one, so cursor motion, pending creation/consumption, row
  revisions, and A1 timing are identical. A mapped-glyph mismatch is caught by
  the exact-cell referee as corruption and can only reject transport; it cannot
  authorize a join.
- Raw-only physical wrap is now a state invariant, not a path-specific
  disclosure: `untracked_wrap_pending` can select the same combining target or
  perform the same later physical wrap as c59, but the sole A1 seam accepts only
  `PendingWrap`. Consuming untracked state clears sequential authorization and
  cannot publish an edge. It may therefore misplace/break content if a future
  unmodeled physical detail diverges, but it can never join rows. Executable
  attacks cover save/restore, RI, ECH, moved-source CSI S, DECAWM-off source
  rewrite, later print, and hostile exact-cell transport framing.
- SGR is represented by `SetStyle`. Keypad, mouse/focus,
  cursor-visibility/shape, title, and device-response dispatches were re-attacked
  before saves/restores, alt transitions, resizes, and later prints. None feeds
  cursor geometry, cell content, tab state, autowrap, row topology, or the A1
  seam; their omission therefore cannot authorize a join.

## Round-12 raw-pending operation walk

Every modeled operation was classified by c59 raw-pending behavior:

| c59 behavior | Operations | Oracle result |
|---|---|---|
| Explicitly clears raw pending | CR; LF/VT/FF; IND; NEL; BS; HT; CSI A/B/C/D/E/F/G; CUP/HVP; VPA; accepted DECSTBM; origin-mode enable; applicable IL/DL; alt entry; DECALN; RIS; c59 resize | `cancel_sequential` or the operation-specific reset clears qualified and untracked state, except for the contract's deliberate resize improvement: unchanged-width/no-reposition resize preserves qualified A2/A16 construction, while width change clears it under A8. No stale untracked motion is retained by resize. |
| Retains raw and does not cut qualified construction | Save-only DECSC/CSI-s/1048h; HTS/TBC; SGR; DECAWM mode-only toggle; ED3/bulk history clear when the visible pending source survives; invalid ED/EL/DECSTBM and outside-region IL/DL; ignored/non-edge dispatches | Qualified state is retained exactly; history destruction still severs any persistent endpoints. |
| Retains raw while an accepted action cuts qualification | RI; ED/EL 0–2; ECH; DCH; ICH; a width-1 source rewrite at the margin while DECAWM is off | `cut_sequential_preserving_raw_wrap` converts the bit to untracked before movement/write. The later print wraps physically and cannot publish A1. |
| Retains raw across topology | CSI S/T | Post-topology validation retains a genuinely unaffected token; a moved, destroyed, revision-mismatched, or width-mismatched source is demoted to untracked. |
| Loads/carries raw across a cursor/domain restore | DECRC/CSI-u/1048l; alt exits | Exact intact matched 1049 suspension may restore qualified state only with a non-exhausted matching epoch. At unchanged geometry, same-domain/stale/cross-mode state may be carried only as untracked. Alt-exit width change or source-row trim invokes the common geometry helper before matched or stale/cross-mode restoration, clearing qualified and raw-only state even after AltEpoch exhaustion. |
| Print/REP/combining consumes, refreshes, or creates state | Every VTE print stores the pre-map REP char first. Width-0 accepted combining refreshes a qualified source token and never consumes raw; cap/no-target is a content no-op. Width-positive print consumes qualified or untracked raw according to DECAWM, and only a newly qualified margin write can create later construction. | `print_char` is the common REP/print seam. `perform_untracked_wrap` has no edge setter; the exact-cell referee rejects every attempted merge across its boundary. |

This walk also covers raw state attached to an exhausted row/edge identity: it
performs physical motion through the same untracked path and cannot acquire a
numeric token later. No surviving raw-state path can bless a false join.

## Round-12 independence and disclosure re-check

I re-walked the complete `Perform::execute`, `csi_dispatch`, and `esc_dispatch`
matches in `performer.rs:628-884`, including every arm that (a) calls `print` or
writes content, (b) repositions the cursor, (c) inserts/removes/repeats cells or
rows, or (d) enters/exits alt. This found two previously omitted dispatcher
surfaces: REP (`CSI b`) and modes 47/1047. Both are now first-class operations
with workloads above. The separately rechecked resize/alt-restore paths exposed
the wide-orphan pre-truncation repair, also now modeled and asserted. No further
operation satisfying (a)–(d) was omitted. The aliases LF/VT/FF, CUP/HVP, and
47/1047 execute one behavior but retain separate operations and workloads; the
distinct 1049 cursor-save/restore behavior remains separate.

Finding 1 invalidated the former “match” claim because the oracle and B1 both
stored main cursor/pending state in their saved-main object. The oracle no
longer does that. Its `SavedMainRows` mirrors only real `SavedGrid`'s rows and
grid-level modes/region. Cursor coordinates, raw pending, style, and qualified
authorization live on the independently modeled active performer cursor or
shared `SavedCursor`; `exit_alt` checks the exit flag before touching either.
The nine mode combinations prove that only 1049h/1049l resumes the matching
snapshot, while every 47l/1047l retains the active cursor. The matched-47
`AZCDE` case also submits the erroneous `ABCDEZ` transport to the referee and
requires rejection.

The resize model was independently split at the same real call boundary:
`Screen::resize` uses raw `rows` for its guard, and the later grid phase alone
uses `rows.max(1)`. The zero-height workload proves the skipped migration and
bottom-pop, rather than merely checking an equivalent final height. These
representation differences are intentional safeguards against another
design/oracle mirror.

Round 14 closes a representation consequence exposed by that split. Real c59
stores combining marks separately from `Cell`; both boundary repair and grid
wide-pair repair replace only base cells, while row resize removes combining
only at truncated columns. The oracle's embedded representation now performs
base-only blanking and retains the combining string at every repaired retained
column. The live-resize and 1049 restore fixtures assert the exact
`space+U+0301` candidate cell accepted by the referee.

Round 18 re-attacked the raw-only × geometry cross-product with multi-operation sequences. Matched
1049 and stale DECSC/47→1049 width changes overwrite `ABZ`; vertical-grow and same-size live resize
overwrite `ABCDZ`; active-alt bottom trim clears raw motion from the removed source row. The paired
no-resize ECH control still performs c59 physical raw motion and publishes no edge.

Round 12 re-attacked every disclosure above with multi-operation sequences,
not isolated commands. The former raw-only premise failed on RI and addressed
mutations; the full walk then found the moved-source CSI S/T and DECAWM-off
rewrite variants. All now retain physical state only through the structurally
no-edge untracked path. The remaining boundary-target-trim, cap-during-alt,
charset, private-helper/public-caller, alias, and non-action residuals remain
false-join-closed for the reasons stated here. No infeasible required path or
remaining operation capable of blessing a false join was found.
