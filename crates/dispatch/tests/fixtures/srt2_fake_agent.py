#!/usr/bin/env python3
"""SRT-2 fixture ACP agent — an ADVERSARIAL peer for the terminal-ordering test.

It is NOT opencode: it exists to FORCE the interleaving the migration-spec's SRT-2/MF2 warns
about — the prompt-result becoming available to the crate's reader at the same instant as (or
tighter than) the final `session/update` — so the test proves the driver's single-ordered-bus +
inline-notification design, never opencode's incidental notifications-before-response timing.

On `session/prompt` it streams the reply as many one-word `agent_message_chunk` notifications and
then, for the FINAL chunk, writes the last chunk line AND the `session/prompt` response line in ONE
write() (a single buffer `"<last chunk>\n<response>\n"`). A naive driver that surfaced the terminal
via a path able to overtake a queued or in-flight notification would drop the last chunk
(truncation); a correct driver drains the last chunk before the terminal.

Wire law honored (legal ACP): notifications precede the response ON THE WIRE — but with zero gap.
The test asserts the last chunk is surfaced strictly before the terminal and the full text (incl.
the sentinel last token) is intact.
"""
import json, sys

# The reply the fixture "streams": the LAST token is a distinctive sentinel whose loss would prove
# truncation. Split into per-word chunks so there are many notifications before the terminal.
REPLY_WORDS = ["ordering", "holds", "across", "the", "crate", "boundary", "LASTWORD_SENTINEL"]

def write_line(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

def write_raw(s):
    sys.stdout.write(s)
    sys.stdout.flush()

def chunk_frame(session, text):
    return {
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text},
            },
        },
    }

def main():
    session = "srt2-fixture-session"
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except Exception:
            continue
        method = msg.get("method")
        mid = msg.get("id")

        if method == "initialize":
            write_line({"jsonrpc": "2.0", "id": mid, "result": {
                "protocolVersion": 1,
                "agentCapabilities": {"loadSession": True},
                "agentInfo": {"name": "srt2-fixture", "version": "0"},
            }})
        elif method == "session/new":
            write_line({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": session}})
        elif method == "session/load":
            write_line({"jsonrpc": "2.0", "id": mid, "result": {}})
        elif method == "session/prompt":
            # Stream all but the last word as separate notifications (flushed individually).
            for w in REPLY_WORDS[:-1]:
                write_line(chunk_frame(session, w))
            # ADVERSARIAL: the FINAL chunk notification AND the prompt response in ONE write —
            # zero gap on the wire. Forces the tightest legal interleaving.
            last_chunk = json.dumps(chunk_frame(session, REPLY_WORDS[-1]))
            response = json.dumps({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
            write_raw(last_chunk + "\n" + response + "\n")
        elif method == "session/cancel":
            # notification (no id) — resolve the in-flight prompt as cancelled if one were pending.
            pass
        elif mid is not None and method is not None:
            # any other reverse-ish request we don't model: method-not-found so nothing wedges.
            write_line({"jsonrpc": "2.0", "id": mid,
                        "error": {"code": -32601, "message": f"method {method} not supported"}})

if __name__ == "__main__":
    main()
