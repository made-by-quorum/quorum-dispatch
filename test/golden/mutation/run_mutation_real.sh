#!/usr/bin/env bash
# test/golden/mutation/run_mutation_real.sh — the teeth against the REAL corpus.
#
# Part-1 (run_mutation.sh) proves the comparators bite against SYNTHETIC captures.
# THIS proves they bite against the REAL recorded golden corpus (Part-2, Step 4):
#
#   A. the 6 Part-1 divergence classes injected into the REAL fixtures (not
#      synthetic): altered byte-exact content, dropped CR, reordered backlog line,
#      dropped backlog line, wrong exit code, injected altscreen + class-appropriate
#      picks per corpus. verify MUST catch every one; the clean fixture MUST pass.
#   B. >=1 INTENTIONALLY-DIVERGENT TS INVOCATION (red-team m1 spec): a recorded
#      scenario RE-RUN with a deliberate divergence whose effect lands in a
#      NEVER-normalized field (exit code, per ADR-0003), compared against the
#      recorded golden — verify MUST catch it. (Driven by the sibling
#      divergent_invocation.sh helper when a pinned qd is available; otherwise the
#      already-captured divergent trace under mutation/divergent/ is replayed.)
#   C. R2 STUB-SEAM NEGATIVE CONTROLS (rider R2, BINDING): stub misbehaviours
#      injected through the stub's STUB_* seams (withheld/delayed PID file, missing
#      JSONL reply, dead relay /health) — the recorder/comparator OUTPUT must CHANGE
#      and verify must CATCH each. A contract row insensitive to its stub input is
#      vacuous gold. (Driven in-jail when a pinned qd + stub are available; the
#      seam EFFECT on the observable surface is always asserted here from the
#      captured seam-on vs seam-off outputs under mutation/r2-seams/.)
#
# Zero false negatives is the gate. Raw evidence -> EVIDENCE-mutation-real.txt.
#
# Bash 3.2 floor. Run directly. Exits non-zero if any tooth fails to bite OR any
# clean baseline fails to pass.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"          # test/golden
VERIFY="$ROOT/verify.sh"
MUTATE="$HERE/mutate.sh"
FIX="$ROOT/fixtures"
. "$ROOT/lib/compare.sh"
. "$ROOT/lib/normalize.sh"

PASS=0; FAIL=0
ok()  { PASS=$((PASS + 1)); printf 'ok   %s\n' "$1"; }
bad() { FAIL=$((FAIL + 1)); printf 'FAIL %s\n' "$1"; }

TMP="$(mktemp -d "${TMPDIR:-/tmp}/sbmut-real.XXXXXX")"
trap 'rm -rf "$TMP" 2>/dev/null || true' EXIT INT TERM

# A "caught" mutation = verify/comparator returns NON-ZERO on diverged input.
# A "missed" (false negative) = it returns ZERO (PASS) on diverged input.
expect_caught() {
    local name="$1" rc="$2"
    if [ "$rc" -ne 0 ]; then ok "$name (caught, rc=$rc)"
    else bad "$name (MISSED — false negative; oracle did not bite on REAL fixture)"; fi
}
expect_pass() {
    local name="$1" rc="$2"
    if [ "$rc" -eq 0 ]; then ok "$name (clean REAL fixture passes)"
    else bad "$name (false positive — clean REAL fixture flagged, rc=$rc)"; fi
}

# ===========================================================================
printf '=== A. clean REAL-fixture baselines (must PASS) ===\n'

# byte-exact: build-claude-cmd is a stable flag-order contract.
CMD="$FIX/build-claude-cmd/normalized/cmd.txt"
"$VERIFY" --replay "$CMD" --class byte-exact --expected "$CMD" >/dev/null 2>&1
expect_pass "baseline/build-claude-cmd (byte-exact)" $?

# byte-exact: relay-health contract surface.
RELAY="$FIX/relay-health/normalized/contract.txt"
"$VERIFY" --replay "$RELAY" --class byte-exact --expected "$RELAY" >/dev/null 2>&1
expect_pass "baseline/relay-health-contract (byte-exact)" $?

# backlog-complete: history backlog (12 SBLINEs).
HIST="$FIX/history/normalized/history.trace"
HIST_N="$(grep -c '^SBLINE ' "$HIST")"
"$VERIFY" --replay "$HIST" --class backlog-complete --marker "SBLINE " --count "$HIST_N" >/dev/null 2>&1
expect_pass "baseline/history-backlog ($HIST_N lines)" $?

# backlog-complete: reattach backlog (20 SBLINEs).
REATT="$FIX/attach-detach-reattach/normalized/reattach.trace"
REATT_N="$(grep -c '^SBLINE ' "$REATT")"
"$VERIFY" --replay "$REATT" --class backlog-complete --marker "SBLINE " --count "$REATT_N" >/dev/null 2>&1
expect_pass "baseline/reattach-backlog ($REATT_N lines)" $?

# no-altscreen: reattach passthrough (real capture).
"$VERIFY" --replay "$REATT" --class no-altscreen >/dev/null 2>&1
expect_pass "baseline/reattach-no-altscreen" $?

# dropped-CR target: macOS resolution.txt.raw carries real PTY CR/LF.
RESRAW="$FIX/zmx-dir-resolution/raw/resolution.txt.raw"
"$VERIFY" --replay "$RESRAW" --class byte-exact --expected "$RESRAW" >/dev/null 2>&1
expect_pass "baseline/resolution-raw-CR (byte-exact)" $?

# exit-code: the recorded exit-1 path (info missing session).
"$VERIFY" --replay "$CMD" --class exit-code --exit-actual 1 --exit-expected 1 >/dev/null 2>&1
expect_pass "baseline/exit-code-1" $?

# Clean baselines for the row's-own-class replays (the golden passes its own comparator).
"$VERIFY" --replay "$FIX/new-session-trace/normalized/boot.trace" --class boot-readiness-event >/dev/null 2>&1
expect_pass "baseline/boot-readiness-event" $?
"$VERIFY" --replay "$FIX/send-pty-paste-burst/normalized/trace" --class submit-discipline >/dev/null 2>&1
expect_pass "baseline/submit-discipline" $?

# ===========================================================================
printf '\n=== A. 6 Part-1 divergence classes vs REAL fixtures (must be CAUGHT) ===\n'

# 1. ALTERED BYTE-EXACT CONTENT -> build-claude-cmd flag-order drift.
MUT="$TMP/mut_cmd.txt"
# Drop the load-bearing server:relay flag (a real, plausible buildClaudeCmd drift).
grep -v '^server:relay$' "$CMD" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$CMD" >/dev/null 2>&1
expect_caught "real/altered-byte-exact (build-claude-cmd drops server:relay)" $?

# 2. DROPPED CR -> resolution.txt.raw CR stripped (CR is never normalized).
MUT="$TMP/mut_cr.raw"
"$MUTATE" dropped-cr "$RESRAW" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$RESRAW" >/dev/null 2>&1
expect_caught "real/dropped-cr (resolution raw CR->LF)" $?

# 3. REORDERED BACKLOG LINE -> history backlog reversed (ordering check).
MUT="$TMP/mut_reorder.trace"
"$MUTATE" reordered-replay "$HIST" "SBLINE " > "$MUT"
"$VERIFY" --replay "$MUT" --class backlog-complete --marker "SBLINE " --count "$HIST_N" >/dev/null 2>&1
expect_caught "real/reordered-backlog (history reversed)" $?

# 4. DROPPED BACKLOG LINE -> reattach loses SBLINE 13 (dtach failure mode).
MUT="$TMP/mut_drop.trace"
grep -v '^SBLINE 13$' "$REATT" > "$MUT"
"$VERIFY" --replay "$MUT" --class backlog-complete --marker "SBLINE " --count "$REATT_N" >/dev/null 2>&1
expect_caught "real/dropped-backlog-line (reattach loses SBLINE 13)" $?

# 5. WRONG EXIT CODE -> the recorded exit-1 path reported as exit 0 (load-bearing).
"$VERIFY" --replay "$CMD" --class exit-code --exit-actual 0 --exit-expected 1 >/dev/null 2>&1
expect_caught "real/wrong-exit-code (info-missing reported 0 not 1)" $?

# 6. INJECTED ALT-SCREEN -> reattach passthrough gains an alt-screen enter.
MUT="$TMP/mut_alt.trace"
"$MUTATE" inject-altscreen "$REATT" > "$MUT"
"$VERIFY" --replay "$MUT" --class no-altscreen >/dev/null 2>&1
expect_caught "real/inject-altscreen (reattach passthrough)" $?

# class-appropriate extra pick: relay-health contract surface altered.
MUT="$TMP/mut_relay.txt"
sed 's/has_message_id=1/has_message_id=0/' "$RELAY" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$RELAY" >/dev/null 2>&1
expect_caught "real/relay-contract-altered (POST /message has_message_id 1->0)" $?

# class-appropriate extra pick: resolution-outcome divergence (XDG tier resolved
# to the WRONG dir — a real Bug-D regression would land the socket elsewhere).
RESLIN="$FIX/zmx-dir-resolution/normalized/resolution-linux.txt"
MUT="$TMP/mut_res.txt"
sed 's#resolved=<XDG_RUNTIME>/zmx#resolved=<TMPDIR>/wrong/zmx#' "$RESLIN" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$RESLIN" >/dev/null 2>&1
expect_caught "real/resolution-outcome-wrong-dir (XDG tier socket misplaced)" $?

# ===========================================================================
printf '\n=== B. intentionally-divergent TS invocation (never-normalized field) ===\n'
# A recorded scenario RE-RUN with a deliberate divergence whose effect lands in a
# NEVER-normalized field (exit code, ADR-0003), compared against the recorded
# golden. The recorded exit-codes golden asserts info-missing-session -> exit 1.
# The DIVERGENT invocation makes the SAME surface report a DIFFERENT real exit code
# (got_exit=0), and the comparison against the recorded golden MUST catch it.
#
# $DIV_TRACE is the LIVE divergent trace produced in-VM by divergent_invocation_drive.sh
# (committed under mutation/divergent/). This suite REPLAYS it vs the recorded golden;
# it does NOT synthesize the divergence (that would be a tautology). A missing capture
# FAILS LOUDLY.
EXITGOLD="$FIX/exit-codes/normalized/exit-codes.trace"
# The divergent trace is the LIVE capture (a real divergent re-run, committed). We
# DO NOT synthesize it from the golden — that would be a tautology. If the live
# capture is missing, FAIL LOUDLY (re-run divergent_invocation_drive.sh in-VM).
DIV_TRACE="${DIV_TRACE:-$HERE/divergent/divergent_exit.trace}"
if [ ! -f "$DIV_TRACE" ]; then
    bad "divergent-invocation/LIVE-capture-missing ($DIV_TRACE — re-run divergent_invocation_drive.sh; NOT synthesizing)"
fi
# Sanity: the divergent trace must actually differ from the golden in the exit field.
if grep -q 'cmd=info-missing-session expect_exit=1 got_exit=0' "$DIV_TRACE"; then
    ok "divergent-invocation/trace-differs-in-exit-field (got_exit 1->0)"
else
    bad "divergent-invocation/trace-differs-in-exit-field (divergence not present — vacuous)"
fi
# The comparison the gate requires: divergent trace vs the recorded golden.
"$VERIFY" --replay "$DIV_TRACE" --class byte-exact --expected "$EXITGOLD" >/dev/null 2>&1
expect_caught "divergent-invocation/caught-vs-recorded-golden (exit-code field)" $?

# ===========================================================================
printf '\n=== C. R2 stub-seam negative controls (seam-on output CHANGES + CAUGHT) ===\n'
# Each seam: prove (1) the seam-on observable OUTPUT differs from the recorded
# (seam-off) golden surface (non-vacuous), and (2) verify CATCHES the divergence.
# The seam-on captures are the REAL recorder output driven live in-VM by
# r2_seam_drive.sh (committed under mutation/r2-seams/); this suite REPLAYS them.
# R2_SEAM_DIR overrides the capture dir (default: the committed live captures).
R2DIR="${R2_SEAM_DIR:-$HERE/r2-seams}"

# --- SEAM 1: withheld/delayed PID file -> boot-readiness EVENT changes. -------
# The recorded new-session-trace golden asserts pidfile_appeared=1, status=idle.
# The withhold-PID seam makes the PID file NEVER appear -> the EVENT surface flips
# to pidfile_appeared=0 (and never goes idle). verify MUST catch it.
BOOTGOLD="$FIX/new-session-trace/normalized/boot.trace"
SEAM_BOOT="$R2DIR/withhold_pid_boot.trace"
if [ ! -f "$SEAM_BOOT" ]; then
    bad "r2/withhold-pid/LIVE-capture-missing ($SEAM_BOOT — re-run r2_seam_drive.sh; NOT synthesizing)"
fi
if ! cmp -s "$SEAM_BOOT" "$BOOTGOLD"; then
    ok "r2/withhold-pid/output-changed (boot EVENT surface differs from golden)"
else
    bad "r2/withhold-pid/output-changed (NO CHANGE — row insensitive to stub PID file = vacuous gold)"
fi
"$VERIFY" --replay "$SEAM_BOOT" --class byte-exact --expected "$BOOTGOLD" >/dev/null 2>&1
expect_caught "r2/withhold-pid/caught-vs-golden (byte-exact)" $?
# Exercise the ROW'S ACTUAL recorded class (boot-readiness-event), not just
# byte-exact (red-team #1): the same EVENT-field assert the scenario uses.
"$VERIFY" --replay "$SEAM_BOOT" --class boot-readiness-event >/dev/null 2>&1
expect_caught "r2/withhold-pid/caught-vs-row-class (boot-readiness-event)" $?

# --- SEAM 2: missing JSONL reply -> send:pty --wait surface changes. ----------
# The recorded send-pty-paste-burst golden records the queued user+assistant JSONL
# pairs draining. The withhold-JSONL seam drops the assistant reply -> the --wait
# anchor finds the user record but the reply pair is MISSING. verify MUST catch the
# changed paste-burst trace.
BURSTGOLD="$FIX/send-pty-paste-burst/normalized/trace"
SEAM_BURST="$R2DIR/withhold_jsonl_burst.trace"
if [ ! -f "$SEAM_BURST" ]; then
    bad "r2/withhold-jsonl/LIVE-capture-missing ($SEAM_BURST — re-run r2_seam_drive.sh; NOT synthesizing)"
fi
if ! cmp -s "$SEAM_BURST" "$BURSTGOLD"; then
    ok "r2/withhold-jsonl/output-changed (paste-burst --wait surface differs from golden)"
else
    bad "r2/withhold-jsonl/output-changed (NO CHANGE — row insensitive to the JSONL reply = vacuous gold; FLAGGED)"
fi
"$VERIFY" --replay "$SEAM_BURST" --class byte-exact --expected "$BURSTGOLD" >/dev/null 2>&1
expect_caught "r2/withhold-jsonl/caught-vs-golden (byte-exact)" $?
# Exercise the ROW'S ACTUAL recorded class (semantic-submit-discipline), not just
# byte-exact (red-team #1): the same queue/--wait field assert the scenario uses.
"$VERIFY" --replay "$SEAM_BURST" --class submit-discipline >/dev/null 2>&1
expect_caught "r2/withhold-jsonl/caught-vs-row-class (submit-discipline)" $?

# --- SEAM 3: dead relay /health -> relay-health contract surface changes. -----
# The recorded relay-health golden records GET /health -> status=ok. The
# dead-health seam answers 503 status=dead -> the contract surface flips. verify
# MUST catch it.
SEAM_RELAY="$R2DIR/dead_health_contract.txt"
if [ ! -f "$SEAM_RELAY" ]; then
    bad "r2/dead-health/LIVE-capture-missing ($SEAM_RELAY — re-run r2_seam_drive.sh; NOT synthesizing)"
fi
if ! cmp -s "$SEAM_RELAY" "$RELAY"; then
    ok "r2/dead-health/output-changed (relay /health contract surface differs from golden)"
else
    bad "r2/dead-health/output-changed (NO CHANGE — row insensitive to /health = vacuous gold; FLAGGED)"
fi
"$VERIFY" --replay "$SEAM_RELAY" --class byte-exact --expected "$RELAY" >/dev/null 2>&1
expect_caught "r2/dead-health/caught-vs-golden" $?

# ===========================================================================
printf '\n=== W3a. DELTA-STRENGTH strengthened-row mutants (every new assertion bites) ===\n'
# Each strengthened assertion added by W3a (W3.2 relay cross-checks, W3.3 boot
# strengthening, W3.8 backlog content-integrity) gets a mutant that FLIPS the
# recorded golden's corresponding field/line and is replayed through the ROW'S CLASS
# comparator; verify MUST catch every one, and the clean golden MUST pass. The relay
# derived booleans are computed PRE-normalization in the scenario (red-team B2) and
# land as 0/1 lines in the normalized golden, so a flipped value here is the same
# divergence a foreign-parent / stale-sidecar / wrong-sessionId engine would produce.

# --- W3.2 relay-health: clean baseline passes its own byte-exact class. ----------
RELAYW3="$FIX/relay-health/normalized/contract.txt"
"$VERIFY" --replay "$RELAYW3" --class byte-exact --expected "$RELAYW3" >/dev/null 2>&1
expect_pass "w3.2/relay-clean (byte-exact, derived cross-checks present)" $?

# W3.2 mutant 1: FOREIGN-PARENT sidecar -> relay child not under the claude pid.
MUT="$TMP/w32_foreign.txt"
sed 's/^relay_pid_is_child_of_claude=1$/relay_pid_is_child_of_claude=0/' "$RELAYW3" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$RELAYW3" >/dev/null 2>&1
expect_caught "w3.2/foreign-parent-sidecar (relay_pid_is_child_of_claude 1->0)" $?

# W3.2 mutant 2: STALE sidecar (dead pid) -> sidecar.pid != /health.pid (the live
# relay child's pid). Modeled as the same-pid join flipping false.
MUT="$TMP/w32_stale.txt"
sed 's/^relay_sidecar_health_pid_same=1$/relay_sidecar_health_pid_same=0/' "$RELAYW3" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$RELAYW3" >/dev/null 2>&1
expect_caught "w3.2/stale-sidecar-dead-pid (sidecar.pid != health.pid join 1->0)" $?

# W3.2 mutant 3: EMPTY message_id -> the deterministic token is missing.
MUT="$TMP/w32_emptymid.txt"
sed 's/^message_id=mid-4fab57b3a2a5$/message_id=/' "$RELAYW3" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$RELAYW3" >/dev/null 2>&1
expect_caught "w3.2/empty-message-id (mid token -> empty)" $?

# W3.2 mutant 4: CROSS-FIELD sessionId mismatch -> the identity chain breaks.
MUT="$TMP/w32_sidmismatch.txt"
sed 's/^sessionid_chain_eq=1$/sessionid_chain_eq=0/' "$RELAYW3" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$RELAYW3" >/dev/null 2>&1
expect_caught "w3.2/cross-field-sessionid-mismatch (chain 1->0)" $?

# --- W3.3 new-session-trace: clean baseline passes its boot-readiness-event class. -
BOOTW3="$FIX/new-session-trace/normalized/boot.trace"
"$VERIFY" --replay "$BOOTW3" --class boot-readiness-event >/dev/null 2>&1
expect_pass "w3.3/boot-clean (boot-readiness-event, strengthened fields present)" $?

# W3.3 mutant 1: DECOY-WINS -> engine matched the pre-seeded decoy, not our name.
MUT="$TMP/w33_decoy.trace"
sed 's/^EVENT decoy_rejected_matched_our_name=1$/EVENT decoy_rejected_matched_our_name=0/' "$BOOTW3" > "$MUT"
"$VERIFY" --replay "$MUT" --class boot-readiness-event >/dev/null 2>&1
expect_caught "w3.3/decoy-wins (decoy_rejected 1->0)" $?

# W3.3 mutant 2: NO-WENT-BUSY -> a poll-only drive never observed idle->busy.
MUT="$TMP/w33_nobusy.trace"
sed 's/^EVENT went_busy_observed=1$/EVENT went_busy_observed=0/' "$BOOTW3" > "$MUT"
"$VERIFY" --replay "$MUT" --class boot-readiness-event >/dev/null 2>&1
expect_caught "w3.3/no-went-busy (went_busy_observed 1->0)" $?

# W3.3 mutant 3: PRE-PID STDIN SPAM -> chars read before the PID write != 0.
MUT="$TMP/w33_spam.trace"
sed 's/^EVENT input_chars_before_pidfile=0$/EVENT input_chars_before_pidfile=5/' "$BOOTW3" > "$MUT"
"$VERIFY" --replay "$MUT" --class boot-readiness-event >/dev/null 2>&1
expect_caught "w3.3/pre-pid-stdin-spam (input_chars_before_pidfile 0->5)" $?

# --- W3.8 backlog content-integrity: clean baselines pass the multiset class. ------
# The multiset class runs on a WHOLE-LINE view (one "SBLINE i" per line). The clean
# golden's -o view IS exactly that (each line is the whole token), so it passes.
HISTW3="$FIX/history/normalized/history.trace"
HISTW3_N="$(grep -c '^SBLINE ' "$HISTW3")"
"$VERIFY" --replay "$HISTW3" --class backlog-multiset --marker "SBLINE " --count "$HISTW3_N" >/dev/null 2>&1
expect_pass "w3.8/history-multiset-clean ($HISTW3_N lines)" $?

REATTW3="$FIX/attach-detach-reattach/normalized/reattach.trace"
REATTW3_N="$(grep -c '^SBLINE ' "$REATTW3")"
"$VERIFY" --replay "$REATTW3" --class backlog-multiset --marker "SBLINE " --count "$REATTW3_N" >/dev/null 2>&1
expect_pass "w3.8/reattach-multiset-clean ($REATTW3_N lines)" $?

# W3.8 mutant 1: DUPLICATED sentinel line -> the middle index appears twice.
MID_H=$(( (HISTW3_N + 1) / 2 ))
MUT="$TMP/w38_dup.trace"
awk -v m="SBLINE $MID_H" '{print} $0==m{print}' "$HISTW3" > "$MUT"
"$VERIFY" --replay "$MUT" --class backlog-multiset --marker "SBLINE " --count "$HISTW3_N" >/dev/null 2>&1
expect_caught "w3.8/duplicated-sentinel-line (history SBLINE $MID_H twice)" $?

# W3.8 mutant 2: BANNER-PREFIXED line -> a present line gains a prefix (NOT the whole
# literal). The -o ordering view cannot see this; the whole-line multiset DOES.
MUT="$TMP/w38_banner.trace"
sed "s/^SBLINE $MID_H\$/BANNER:SBLINE $MID_H/" "$HISTW3" > "$MUT"
"$VERIFY" --replay "$MUT" --class backlog-multiset --marker "SBLINE " --count "$HISTW3_N" >/dev/null 2>&1
expect_caught "w3.8/banner-prefixed-line (history SBLINE $MID_H -> BANNER:SBLINE $MID_H)" $?

# ===========================================================================
printf '\n=== W3b. DELTA-STRENGTH new-row mutants (P4/P5/P9/P10 — every new assertion bites) ===\n'
# Each NEW W3b assertion gets a mutant that FLIPS the recorded golden's corresponding
# field and is replayed through byte-exact vs the clean golden (the row's SHAPE/MAP/
# PRESENCE/TERMIOS booleans normalize to themselves, so a flipped value is exactly the
# divergence a misbehaving engine would produce). Clean golden MUST pass; each flip
# MUST be caught. NOTE: the W3.4 rows assert via a file-grep scn_assert (no registered
# replay comparator class), so the biting replay is byte-exact-vs-golden — identical in
# spirit to the W3.2 relay derived-boolean mutants above.

# --- W3.4a neg-boot-timeout ---
NBT="$FIX/neg-boot-timeout/normalized/shape.txt"
"$VERIFY" --replay "$NBT" --class byte-exact --expected "$NBT" >/dev/null 2>&1
expect_pass "w3.4a/boot-timeout-clean (byte-exact)" $?
# Mutant: SWALLOW-FAILURE-AS-SUCCESS — boot reported rc 0 (failed_nonzero 1->0).
MUT="$TMP/w34a_swallow.txt"
sed 's/^SHAPE failed_nonzero=1$/SHAPE failed_nonzero=0/' "$NBT" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$NBT" >/dev/null 2>&1
expect_caught "w3.4a/swallow-failure-as-success (failed_nonzero 1->0)" $?
# Mutant: stderr failure-shape token stripped (readiness_timeout_token 1->0).
MUT="$TMP/w34a_notoken.txt"
sed 's/^SHAPE readiness_timeout_token=1$/SHAPE readiness_timeout_token=0/' "$NBT" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$NBT" >/dev/null 2>&1
expect_caught "w3.4a/stderr-token-stripped (readiness_timeout_token 1->0)" $?

# --- W3.4b neg-wait-no-reply ---
NWR="$FIX/neg-wait-no-reply/normalized/shape.txt"
"$VERIFY" --replay "$NWR" --class byte-exact --expected "$NWR" >/dev/null 2>&1
expect_pass "w3.4b/wait-no-reply-clean (byte-exact)" $?
# Mutant: a FABRICATED reply — the no-text sentinel is gone (engine invented a reply).
MUT="$TMP/w34b_fab.txt"
sed 's/^SHAPE no_text_response_sentinel=1$/SHAPE no_text_response_sentinel=0/' "$NWR" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$NWR" >/dev/null 2>&1
expect_caught "w3.4b/fabricated-reply (no_text_response_sentinel 1->0)" $?
# Mutant: an assistant record APPEARED despite withhold (reply not actually withheld).
MUT="$TMP/w34b_assist.txt"
sed 's/^SHAPE assistant_record_absent=1$/SHAPE assistant_record_absent=0/' "$NWR" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$NWR" >/dev/null 2>&1
expect_caught "w3.4b/assistant-record-appeared (assistant_record_absent 1->0)" $?

# --- W3.4c neg-relay-unhealthy ---
NRU="$FIX/neg-relay-unhealthy/normalized/shape.txt"
"$VERIFY" --replay "$NRU" --class byte-exact --expected "$NRU" >/dev/null 2>&1
expect_pass "w3.4c/relay-unhealthy-clean (byte-exact)" $?
# Mutant: ls-join RE-GATED on /health and DROPPED the live-sidecar relay on dead health.
MUT="$TMP/w34c_drop.txt"
sed 's/^SHAPE ls_join_sidecar_driven_robust=1$/SHAPE ls_join_sidecar_driven_robust=0/' "$NRU" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$NRU" >/dev/null 2>&1
expect_caught "w3.4c/ls-join-regated-on-health (sidecar_driven_robust 1->0)" $?

# --- W3.4d neg-two-stage-tolerance ---
N2S="$FIX/neg-two-stage-tolerance/normalized/shape.txt"
"$VERIFY" --replay "$N2S" --class byte-exact --expected "$N2S" >/dev/null 2>&1
expect_pass "w3.4d/two-stage-tolerance-clean (byte-exact)" $?
# Mutant: PARTIAL-JSON-INTOLERANT reader (pre-A1-PR#20 whole-row-drop) — the session is
# dropped on the partial read and never recovers (session_visible_after 1->0).
MUT="$TMP/w34d_intolerant.txt"
sed 's/^SHAPE session_visible_after=1$/SHAPE session_visible_after=0/' "$N2S" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$N2S" >/dev/null 2>&1
expect_caught "w3.4d/partial-json-intolerant-reader (session_visible_after 1->0, models pre-PR#20 drop)" $?
# Mutant: ls CRASHED on a mid-write partial PID file (not partial-tolerant).
MUT="$TMP/w34d_crash.txt"
sed 's/^SHAPE ls_never_crashed_during_write=1$/SHAPE ls_never_crashed_during_write=0/' "$N2S" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$N2S" >/dev/null 2>&1
expect_caught "w3.4d/ls-crash-on-partial (ls_never_crashed_during_write 1->0)" $?

# --- W3.4e noise-stale ---
NS="$FIX/noise-stale/normalized/shape.txt"
"$VERIFY" --replay "$NS" --class byte-exact --expected "$NS" >/dev/null 2>&1
expect_pass "w3.4e/noise-stale-clean (byte-exact)" $?
# Mutant: duplicate same-name same-sessionId NOT deduped (double-counted).
MUT="$TMP/w34e_dup.txt"
sed 's/^SHAPE dup_same_sid_deduped_to_one=1$/SHAPE dup_same_sid_deduped_to_one=0/' "$NS" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$NS" >/dev/null 2>&1
expect_caught "w3.4e/dup-not-deduped (dup_same_sid_deduped_to_one 1->0)" $?
# Mutant: dead-pid registry entry DROPPED (unexpected liveness filter).
MUT="$TMP/w34e_dead.txt"
sed 's/^SHAPE dead_pid_entry_still_visible=1$/SHAPE dead_pid_entry_still_visible=0/' "$NS" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$NS" >/dev/null 2>&1
expect_caught "w3.4e/dead-pid-dropped (dead_pid_entry_still_visible 1->0)" $?

# --- W4.3 wrong-typed-timestamp (A1 F3 fallout; joins the W3.4e noise family) ---
WTT="$FIX/wrong-typed-timestamp/normalized/shape.txt"
"$VERIFY" --replay "$WTT" --class byte-exact --expected "$WTT" >/dev/null 2>&1
expect_pass "w4.3/wrong-typed-timestamp-clean (byte-exact)" $?
# Mutant: WHOLE-ROW DROP (pre-A1-PR#20 Rust behavior) — a wrong-typed timestamp drops
# the entire row, the session goes silently invisible to ls/resolve. The A1 PR#20
# per-field-permissive deserializer keeps the row (visible); this mutant flips that
# (wrong_typed_row_visible 1->0) and MUST be caught.
MUT="$TMP/w43_rowdrop.txt"
sed 's/^SHAPE wrong_typed_row_visible=1$/SHAPE wrong_typed_row_visible=0/' "$WTT" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$WTT" >/dev/null 2>&1
expect_caught "w4.3/whole-row-drop (wrong_typed_row_visible 1->0, models pre-PR#20 drop)" $?

# --- W3.5 attach-first-window ---
AFW="$FIX/attach-first-window/normalized/presence.txt"
"$VERIFY" --replay "$AFW" --class byte-exact --expected "$AFW" >/dev/null 2>&1
expect_pass "w3.5/attach-first-window-clean (byte-exact)" $?
# Mutant: ALT-SCREEN appeared in the attach window (altscreen_1049h_absent 1->0) — the
# recorded-row assertion flips (the attach took over the alt-screen).
MUT="$TMP/w35_alt.txt"
sed 's/^PRESENCE altscreen_1049h_absent=1$/PRESENCE altscreen_1049h_absent=0/' "$AFW" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$AFW" >/dev/null 2>&1
expect_caught "w3.5/altscreen-in-attach-window (altscreen_1049h_absent 1->0)" $?
# Mutant: backlog sentinel ABSENT (the backlog did not replay into the first window).
MUT="$TMP/w35_nosent.txt"
sed 's/^PRESENCE backlog_sentinel_present=1$/PRESENCE backlog_sentinel_present=0/' "$AFW" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$AFW" >/dev/null 2>&1
expect_caught "w3.5/backlog-sentinel-absent (backlog_sentinel_present 1->0)" $?
# MECHANISM proof (red-team spec: altscreen-INJECTED capture must flip red): feed a
# window with a REAL injected ?1049h through the no-altscreen comparator — the same
# detection the row's PRESENCE check uses — and confirm it bites. Synthesize a clean
# window then inject the alt-screen enter.
CLEANWIN="$TMP/w35_clean_window"
printf 'SBLINE 7\r\n> ready\r\n' > "$CLEANWIN"
"$VERIFY" --replay "$CLEANWIN" --class no-altscreen >/dev/null 2>&1
expect_pass "w3.5/clean-window-no-altscreen (mechanism baseline)" $?
INJWIN="$TMP/w35_inj_window"
"$MUTATE" inject-altscreen "$CLEANWIN" > "$INJWIN"
"$VERIFY" --replay "$INJWIN" --class no-altscreen >/dev/null 2>&1
expect_caught "w3.5/injected-altscreen-window-caught (no-altscreen mechanism)" $?

# --- W3.6 hostile-cwd ---
HC="$FIX/hostile-cwd/normalized/mapping.txt"
"$VERIFY" --replay "$HC" --class byte-exact --expected "$HC" >/dev/null 2>&1
expect_pass "w3.6/hostile-cwd-clean (byte-exact)" $?
# Mutant: WRONG project-path mapping — a special char was sanitized (specials not
# preserved verbatim), the divergence a `:`->`_` / `.`->`-` sanitizing impl produces.
MUT="$TMP/w36_sanitized.txt"
sed 's/^MAP specials_preserved_verbatim=1$/MAP specials_preserved_verbatim=0/' "$HC" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$HC" >/dev/null 2>&1
expect_caught "w3.6/wrong-project-path-mapping (specials_preserved_verbatim 1->0)" $?
# Mutant: JSONL landed at the WRONG (mangled) project dir.
MUT="$TMP/w36_wrongdir.txt"
sed 's/^MAP jsonl_landed_at_mapped_dir=1$/MAP jsonl_landed_at_mapped_dir=0/' "$HC" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$HC" >/dev/null 2>&1
expect_caught "w3.6/jsonl-at-wrong-dir (jsonl_landed_at_mapped_dir 1->0)" $?

# --- W3.7 termios-report (macOS) ---
TR="$FIX/termios-report/normalized/report.txt"
"$VERIFY" --replay "$TR" --class byte-exact --expected "$TR" >/dev/null 2>&1
expect_pass "w3.7/termios-report-clean (byte-exact)" $?
# Mutant: COOKED->RAW substitution (icanon=1->0) — the spec's cooked-mode mutant,
# inverted to match the recorded reality (recorded macOS PTY is cooked). Must flip red.
MUT="$TMP/w37_raw_icanon.txt"
sed 's/^TERMIOS icanon=1$/TERMIOS icanon=0/' "$TR" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$TR" >/dev/null 2>&1
expect_caught "w3.7/raw-mode-substituted-icanon (icanon 1->0)" $?
# Mutant: echo cleared (raw-mode substitution).
MUT="$TMP/w37_raw_echo.txt"
sed 's/^TERMIOS echo=1$/TERMIOS echo=0/' "$TR" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$TR" >/dev/null 2>&1
expect_caught "w3.7/raw-mode-substituted-echo (echo 1->0)" $?

# --- W4.2 termios-report (LINUX side, recorded in-VM) ---
# The Linux sibling of the W3.7 row. The semantic booleans match macOS (cooked mode:
# the engine does not set raw mode, the stub is a line reader) — only the forensic
# bitmasks differ (why platform-split). The byte-exact-vs-golden replay here runs on
# any host (it reads the recorded report-linux.txt fixture); the IN-VM end-to-end
# replay is captured in fixtures/.verify-replay-evidence-linux.txt.
TRL="$FIX/termios-report/normalized/report-linux.txt"
"$VERIFY" --replay "$TRL" --class byte-exact --expected "$TRL" >/dev/null 2>&1
expect_pass "w4.2/termios-report-linux-clean (byte-exact)" $?
# Mutant: COOKED->RAW substitution on the Linux row (icanon=1->0). Must flip red.
MUT="$TMP/w42_lin_raw_icanon.txt"
sed 's/^TERMIOS icanon=1$/TERMIOS icanon=0/' "$TRL" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$TRL" >/dev/null 2>&1
expect_caught "w4.2/termios-linux-raw-substituted-icanon (icanon 1->0)" $?
# Mutant: echo cleared on the Linux row (raw-mode substitution).
MUT="$TMP/w42_lin_raw_echo.txt"
sed 's/^TERMIOS echo=1$/TERMIOS echo=0/' "$TRL" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$TRL" >/dev/null 2>&1
expect_caught "w4.2/termios-linux-raw-substituted-echo (echo 1->0)" $?

# ===========================================================================
printf '\n=== W3.1 P3 chunked-path mutants (orc-4 ruling; riders R3 + C2) ===\n'
# The send-pty-chunked-idle (>=4KB IDLE) row + the strengthened send-pty-paste-burst
# (sub-1KB busy) row. Each new assertion gets a mutant that FLIPS the recorded
# golden's corresponding line/field and is replayed through the ROW'S CLASS (idle:
# byte-exact vs golden, the row's derived-text lines normalize to themselves; busy:
# submit-discipline, the same fields the scenario keys on). Plus TWO LIVE seam
# controls (R3 + C2) whose committed captures (mutation/r2-seams/, driven by
# w31_seam_drive.sh at pin) must DIFFER from gold AND be CAUGHT.

# --- clean baselines (the goldens pass their own class) ---
IDLEGOLD="$FIX/send-pty-chunked-idle/normalized/trace"
"$VERIFY" --replay "$IDLEGOLD" --class byte-exact --expected "$IDLEGOLD" >/dev/null 2>&1
expect_pass "w3.1/chunked-idle-clean (byte-exact, full >=4KB payload present)" $?
BURSTGOLD2="$FIX/send-pty-paste-burst/normalized/trace"
"$VERIFY" --replay "$BURSTGOLD2" --class submit-discipline >/dev/null 2>&1
expect_pass "w3.1/busy-strengthened-clean (submit-discipline, ordered texts+anchor+reply)" $?

# --- R3 (BINDING): STUB_NO_QUEUE control — the busy-window burst is read-and-
# DISCARDED by the stub, so the strengthened busy outcome CHANGES (queue did not
# drain; the burst's user_text[1]/anchor/reply text differ; wait_reply absent). The
# committed LIVE capture must DIFFER from gold AND be caught by the row's class.
NQ="$R2DIR/no_queue_burst.trace"
if [ ! -f "$NQ" ]; then
    bad "w3.1/R3-no-queue/LIVE-capture-missing ($NQ — re-run w31_seam_drive.sh; NOT synthesizing)"
fi
if ! cmp -s "$NQ" "$BURSTGOLD2"; then
    ok "w3.1/R3-no-queue/output-changed (busy-window burst discarded -> outcome differs from golden)"
else
    bad "w3.1/R3-no-queue/output-changed (NO CHANGE — busy row insensitive to STUB_NO_QUEUE = vacuous; FLAGGED)"
fi
"$VERIFY" --replay "$NQ" --class submit-discipline >/dev/null 2>&1
expect_caught "w3.1/R3-no-queue/caught-vs-row-class (submit-discipline flips RED)" $?

# --- C2 (BINDING): seam-load-bearing control — the idle >=4KB row driven with
# STUB_RAW_STDIN UNSET (inline seam stripped) -> cooked-mode MAX_CANON=1024 drops the
# >4KB write -> rc1/no-payload (full_payload_present=0, user_record_count=0). Proves
# the inline seam is LIVE (detects silent seam-loss). PAIRED with the sub-1KB cooked
# control (the busy row, 422B < MAX_CANON, lands cooked) so MAX_CANON is bracketed
# from both sides.
CK="$R2DIR/cooked_idle.trace"
if [ ! -f "$CK" ]; then
    bad "w3.1/C2-cooked/LIVE-capture-missing ($CK — re-run w31_seam_drive.sh; NOT synthesizing)"
fi
if ! cmp -s "$CK" "$IDLEGOLD"; then
    ok "w3.1/C2-cooked/output-changed (seam-unset cooked-drop differs from golden = seam is live)"
else
    bad "w3.1/C2-cooked/output-changed (NO CHANGE — seam not load-bearing / silent seam-loss undetectable; FLAGGED)"
fi
"$VERIFY" --replay "$CK" --class byte-exact --expected "$IDLEGOLD" >/dev/null 2>&1
expect_caught "w3.1/C2-cooked/caught-vs-golden (cooked-drop signature flips RED)" $?
# The sub-1KB cooked-control PAIR: the busy row's 422B burst lands cooked (its golden
# is recorded with NO raw seam) — so MAX_CANON is bracketed: <1024 lands, >1024 drops.
"$VERIFY" --replay "$BURSTGOLD2" --class submit-discipline >/dev/null 2>&1
expect_pass "w3.1/C2-pair/sub-1KB-cooked-lands (422B busy burst, MAX_CANON bracketed low)" $?

# --- idle row field-flip mutants (byte-exact vs golden) ---
# TRUNCATED-BURST: the >=4KB payload is cut below 4KB -> the user-record text differs.
MUT="$TMP/w31_idle_trunc.txt"
sed 's/idle-payload-word-no-hex-run-xyzwxyzwxyzwxyzw-40.*$/idle-payload-word-no-hex-run-xyzwxyzwxyzwxyzw-40/' "$IDLEGOLD" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$IDLEGOLD" >/dev/null 2>&1
expect_caught "w3.1/idle-truncated-burst (payload cut below 4KB -> user-record text differs)" $?
# FULL-PAYLOAD-ABSENT: chunk loss -> the payload did not land whole.
MUT="$TMP/w31_idle_nopayload.txt"
sed 's/^full_payload_present=1$/full_payload_present=0/' "$IDLEGOLD" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$IDLEGOLD" >/dev/null 2>&1
expect_caught "w3.1/idle-full-payload-absent (full_payload_present 1->0, chunk loss)" $?
# WRONG-REPLY-TEXT: the --wait reply is not the deterministic STUB-REPLY of the burst.
MUT="$TMP/w31_idle_wrongreply.txt"
sed 's/^wait_reply_text=STUB-REPLY to: CHUNKED-IDLE-4KB/wait_reply_text=STUB-REPLY to: WRONG-CACHED-REPLY/' "$IDLEGOLD" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$IDLEGOLD" >/dev/null 2>&1
expect_caught "w3.1/idle-wrong-reply-text (cached/wrong --wait reply)" $?
# SPLIT-TURN: chunking split the message into 2 user records (user_record_count 1->2).
MUT="$TMP/w31_idle_split.txt"
sed 's/^user_record_count=1$/user_record_count=2/' "$IDLEGOLD" > "$MUT"
"$VERIFY" --replay "$MUT" --class byte-exact --expected "$IDLEGOLD" >/dev/null 2>&1
expect_caught "w3.1/idle-split-turn (user_record_count 1->2, chunking split the turn)" $?

# --- busy row field-flip mutants (submit-discipline class) ---
# SWAPPED-ORDER: turn1 and the burst user records are reordered (m4: ordered texts,
# not a boolean) -> user_text[0] is the burst, not turn1.
MUT="$TMP/w31_busy_swap.txt"
sed -e 's/^user_text\[0\]=first-turn-holds-busy$/user_text[0]=PASTE-BURST word0 SWAPPED/' \
    -e 's/^user_text\[1\]=PASTE-BURST /user_text[1]=first-turn-holds-busy SWAP /' "$BURSTGOLD2" > "$MUT"
"$VERIFY" --replay "$MUT" --class submit-discipline >/dev/null 2>&1
expect_caught "w3.1/busy-swapped-order (user_text order flipped -> submit-discipline RED)" $?
# WRONG-REPLY-TEXT on the busy row.
MUT="$TMP/w31_busy_wrongreply.txt"
sed 's/^wait_reply_text=STUB-REPLY to: PASTE-BURST/wait_reply_text=STUB-REPLY to: WRONG-BURST-REPLY/' "$BURSTGOLD2" > "$MUT"
"$VERIFY" --replay "$MUT" --class submit-discipline >/dev/null 2>&1
expect_caught "w3.1/busy-wrong-reply-text (--wait anchored on the wrong reply)" $?

# ===========================================================================
printf '\n--- run_mutation_real: %d passed, %d failed ---\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
