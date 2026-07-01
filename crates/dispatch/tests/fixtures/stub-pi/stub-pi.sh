#!/bin/sh
# stub-pi — a deterministic adversarial fake of `pi --mode rpc` for C-RED's S layer.
#
# It is reachable ONLY when a test sets QD_PI_BIN to THIS script (never on PATH,
# never shadowing the real pinned pi). Driven through the REAL
# dispatch::provider::pi::stdio::PiStdio::spawn, it elicits the private
# reader_loop / request() wire-framing (oversized / torn-tail / interleaved /
# U+2028 / id-less / wrong-id / EOF) WITHOUT a real pi and WITHOUT creds.
#
# The driver mints command ids c1, c2, … (stdio.rs mint_id); the FIRST command
# (the boot get_state) is always "c1", so a correlating boot response uses id c1.
# Behavior is selected by $STUB_PI_MODE. Args (e.g. --mode rpc) are ignored.

mode="${STUB_PI_MODE:-boot}"

# A correlating, valid boot response (lets PiStdio::get_state() land OK so we can
# then exercise the post-boot framing).
emit_boot() {
  printf '{"id":"c1","type":"response","command":"get_state","success":true,"data":{"sessionId":"stub-uuid","isStreaming":false}}\n'
}

# Stream a ~1 MiB run of 'A' (no giant argv — avoids ARG_MAX) as a JSON string value.
emit_oversized_line() {
  printf '{"id":"pad","type":"oversized_event","p":"'
  head -c 1048577 /dev/zero | tr '\0' 'A'
  printf '"}\n'
}

case "$mode" in
  boot)
    emit_boot ;;

  oversized)
    # A ~1MiB+ line BEFORE the real boot response: the reader must consume the
    # huge line and still correlate c1 (degrade-not-wedge).
    emit_oversized_line
    emit_boot ;;

  oversized-garbage)
    # A ~1MiB+ NON-JSON line (no valid object) then the boot response: the reader
    # must skip the garbage line (serde_json err → continue) and correlate c1.
    printf 'GARBAGE-'
    head -c 1048577 /dev/zero | tr '\0' 'X'
    printf '\n'
    emit_boot ;;

  interleaved)
    # garbage, a valid event, more garbage, then the boot response.
    printf 'not json at all\n'
    printf '{"type":"agent_start"}\n'
    printf '@@@ %%%% not even close\n'
    emit_boot ;;

  u2028)
    # A boot response whose data carries a literal U+2028 (E2 80 A8) inside a JSON
    # string — does the reader/serde survive the JS line-separator byte on the wire?
    printf '{"id":"c1","type":"response","command":"get_state","success":true,"data":{"sessionId":"stub\342\200\250uuid","isStreaming":false}}\n'
    ;;

  idless)
    # A response with NO id → never correlates → get_state must Timeout (short
    # driver timeout), never panic/hang past the deadline.
    printf '{"type":"response","command":"get_state","success":true,"data":{"sessionId":"x"}}\n' ;;

  wrongid)
    # A response for a DIFFERENT id → dropped (continue) → get_state must Timeout.
    printf '{"id":"zzz","type":"response","command":"get_state","success":true,"data":{"sessionId":"x"}}\n' ;;

  dupid)
    # TWO responses for c1 → first correlates (Ok), the second arrives with no
    # command in flight → next_event drops it (no wedge).
    emit_boot
    emit_boot ;;

  eof-immediate)
    # Exit at once → stdout EOF before any response → get_state must map Closed. (no boot)
    exit 0 ;;

  torn-tail)
    # Boot OK, then an un-newlined PARTIAL line, then STAY ALIVE (fall through to cat).
    # The reader blocks on the unterminated line (no newline + no EOF while we're
    # alive) so it is never a frame and never corrupts → boot=Ok, next_event=Ok(None).
    # Staying alive makes this DETERMINISTIC: an immediate exit here races get_state's
    # stdin write → a flaky EPIPE/broken-pipe (the real pi never exits right after boot
    # anyway). The EOF-mid-stream "pi gone" path is covered by the eof-immediate mode.
    emit_boot
    printf '{"type":"message","id":"trunc' ;;

  *)
    emit_boot ;;
esac

# Modes that must stay alive (so the driver doesn't see a premature EOF) block on
# stdin until the driver closes it; the EOF/torn modes already exited above.
cat >/dev/null
