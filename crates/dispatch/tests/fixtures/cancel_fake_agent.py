#!/usr/bin/env python3
"""Cancel fixture ACP agent — a peer that resolves an in-flight prompt with `cancelled`.

The migration-spec carries the literal-`cancelled` stop-reason FORWARD to the claude atomic:
opencode returns `end_turn` on `session/cancel` (its disclosed best-effort semantic), so the
driver-level path `Command::Cancel → session/cancel → stopReason:"cancelled" → StopReason::Cancelled
→ AcpEvent::Terminal → queue-slot release` was never exercised end-to-end. This fixture — NOT a
real bridge, NO model/network — forces exactly that: on `session/prompt` it streams one chunk and
then HOLDS the request (no response); on the `session/cancel` notification it resolves the held
prompt id with `{"stopReason":"cancelled"}`. That is the interrupt terminal a claude break may
actually emit, exercised safely without touching Pete's live claude session.
"""
import json, sys

def write_line(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
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
    session = "cancel-fixture-session"
    pending_prompt_id = None  # the id of the in-flight session/prompt we deliberately hold open
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
                "agentInfo": {"name": "cancel-fixture", "version": "0"},
            }})
        elif method == "session/new":
            write_line({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": session}})
        elif method == "session/prompt":
            # Stream a chunk of real content, then HOLD the response — the turn is now in flight
            # awaiting a cancel (a naive echo-immediately fixture could not exercise the interrupt).
            write_line(chunk_frame(session, "working"))
            pending_prompt_id = mid
        elif method == "session/cancel":
            # The interrupt (a notification, no id): resolve the held prompt as `cancelled` — the
            # literal stop-reason the claude bridge may emit, distinct from opencode's end_turn.
            if pending_prompt_id is not None:
                write_line({"jsonrpc": "2.0", "id": pending_prompt_id,
                            "result": {"stopReason": "cancelled"}})
                pending_prompt_id = None
        elif mid is not None and method is not None:
            write_line({"jsonrpc": "2.0", "id": mid,
                        "error": {"code": -32601, "message": f"method {method} not supported"}})

if __name__ == "__main__":
    main()
