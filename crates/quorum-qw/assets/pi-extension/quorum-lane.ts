/**
 * `quorum-lane` — the control channel behind the `pi/extension` lane.
 *
 * # What this is
 *
 * A pi extension that lets `qw` drive a REAL pi TUI — the same TUI a human is
 * looking at, in the same mux pane, on the same session — over a unix socket.
 * It is the pi analogue of `codex/app-server`: two clients, one session. The
 * agent delivers a turn through this socket; the human types into the composer;
 * both land in one conversation because there is only one.
 *
 * The alternative it replaces is `pi/mux-pane`, which delivers by TYPING
 * KEYSTROKES into the pane's PTY and confirms acceptance by watching the
 * transcript appear on disk. That works, but it is inference: it cannot see
 * whether pi is busy, cannot address a turn, and cannot tell a landed message
 * from a coincidence. This extension is inside pi's process, so it does not
 * infer any of it — it asks.
 *
 * # INERT BY DEFAULT — read this before changing the gate
 *
 * This file installs to `~/.pi/agent/extensions/`, which pi auto-discovers for
 * EVERY pi session on the box, not only the ones `qw` launches. So the gate
 * below is load-bearing: with no socket path configured this extension
 * registers one unused flag and returns. It opens nothing, listens on nothing,
 * and costs a normal `pi` invocation nothing.
 *
 * The socket path arrives one of two ways, checked in this order:
 *   1. `--quorum-sock <path>` — an explicit CLI flag, which is what `qw` passes.
 *   2. `$QUORUM_PI_SOCK` — the env fallback, for a TUI a human launched by hand
 *      and wants `qw` to be able to reach.
 *
 * # The wire
 *
 * Newline-delimited JSON, one request per line, one response per line — the
 * same framing `qw serve` itself speaks, so the shape is already familiar and
 * already tooled. A request is `{"id":<n>,"m":"<verb>",...}` and its response is
 * `{"id":<n>,"ok":{...}}` or `{"id":<n>,"err":{"code":"...","detail":"..."}}`.
 *
 * `id` is echoed verbatim and never interpreted. A caller that sends no `id`
 * gets no response — fire and forget.
 *
 * ## Verbs
 *
 * | verb        | does                                        | answers |
 * |-------------|---------------------------------------------|---------|
 * | `hello`     | handshake + identity                        | `{v, session, cwd, mode, pi}` |
 * | `health`    | is pi working right now?                    | `{status, turns, pending}` |
 * | `deliver`   | `pi.sendUserMessage()` — a real user turn   | `{accepted, queued_as}` |
 * | `session`   | identity without the handshake              | `{id, name, file, cwd}` |
 * | `interrupt` | `ctx.abort()` — stop the current run        | `{aborted}` |
 * | `subscribe` | opt in to unsolicited status frames         | `{subscribed:true}` |
 *
 * ## Why `subscribe` is opt-in and not the default
 *
 * A subscribed connection also receives UNSOLICITED frames shaped
 * `{"ev":"idle"|"busy", ...}` with no `id`. That is what turns `await_idle`
 * from a poll into a wait. It is opt-in because a request/response client that
 * reads exactly one line per request — which is precisely what `qw`'s own wire
 * client does — would otherwise decode a status event as its answer and report
 * a transport failure for a call that succeeded. Making the caller ASK for the
 * stream keeps that hazard unreachable for callers that do not want it.
 *
 * # Failure posture
 *
 * Every handler is wrapped. A malformed line, an unknown verb, or a throwing
 * handler produces an error FRAME, never an exception that reaches pi. The
 * socket is a debugging affordance for a session the user is actively using;
 * it must not be able to take that session down.
 */

import { VERSION } from "@earendil-works/pi-coding-agent";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import net from "node:net";
import fs from "node:fs";
import path from "node:path";

/** Env fallback for the socket path. See the gate note in the module docs. */
const SOCK_ENV = "QUORUM_PI_SOCK";

/** Wire protocol version, reported by `hello`. Bump on a breaking frame change. */
const WIRE_V = 1;

/** A parsed request line. `id` absent means fire-and-forget. */
type Req = { id?: number; m?: string; [k: string]: unknown };

export default function (pi: ExtensionAPI) {
  // Registered unconditionally so `pi --help` documents it and so `getFlag`
  // below has something to read. Registering a flag is inert on its own.
  pi.registerFlag("quorum-sock", {
    description: "Serve the quorum lane control channel on this unix socket path",
    type: "string",
  });

  /**
   * The live context, captured at `session_start`.
   *
   * Socket requests arrive OUTSIDE any pi event handler, so they have no `ctx`
   * of their own — this is the only way to reach `isIdle`, `abort` and
   * `sessionManager` from a socket callback. It is re-captured on every
   * `session_start` because `/new` and `/resume` rebind extensions against a
   * fresh context, and a stale one would answer for the previous session.
   */
  let ctx: ExtensionContext | undefined;

  let server: net.Server | undefined;
  let sockPath: string | undefined;

  /** Connections that opted into status frames via `subscribe`. */
  const subscribers = new Set<net.Socket>();

  /**
   * Completed turns observed on this connection's session.
   *
   * Counted from `turn_end` rather than read off the transcript because the
   * transcript is not written until pi's first assistant reply — the lazy-write
   * window the `pi/mux-pane` lane has to work around. An in-process counter has
   * no such window, but it also does not survive a restart, so it is reported
   * as "turns this process has seen", never as the session's turn count.
   */
  let turnsSeen = 0;

  const send = (sock: net.Socket, frame: unknown): void => {
    try {
      sock.write(JSON.stringify(frame) + "\n");
    } catch {
      // A peer that vanished mid-write is not this extension's problem.
    }
  };

  const ok = (sock: net.Socket, id: number | undefined, body: unknown): void => {
    if (id === undefined) return; // fire-and-forget
    send(sock, { id, ok: body });
  };

  const err = (
    sock: net.Socket,
    id: number | undefined,
    code: string,
    detail: string,
  ): void => {
    if (id === undefined) return;
    send(sock, { id, err: { code, detail } });
  };

  /** Push a status frame to every subscribed connection. */
  const broadcast = (ev: string, body: Record<string, unknown> = {}): void => {
    for (const sock of subscribers) send(sock, { ev, ...body });
  };

  const status = (): "busy" | "idle" =>
    ctx && !ctx.isIdle() ? "busy" : "idle";

  // -- verbs ---------------------------------------------------------------

  const handle = async (sock: net.Socket, req: Req): Promise<void> => {
    const id = typeof req.id === "number" ? req.id : undefined;
    const sm = ctx?.sessionManager;

    switch (req.m) {
      case "hello":
        return ok(sock, id, {
          v: WIRE_V,
          session: sm?.getSessionId(),
          cwd: ctx?.cwd,
          mode: ctx?.mode,
          pi: VERSION,
        });

      case "session":
        return ok(sock, id, {
          id: sm?.getSessionId(),
          name: pi.getSessionName(),
          file: sm?.getSessionFile(),
          cwd: ctx?.cwd,
        });

      case "health":
        return ok(sock, id, {
          status: status(),
          turns: turnsSeen,
          pending: ctx?.hasPendingMessages() ?? false,
        });

      case "deliver": {
        const text = req.text;
        if (typeof text !== "string" || text.length === 0) {
          return err(sock, id, "bad-request", "deliver needs a non-empty string `text`");
        }
        // `deliverAs` is REQUIRED by pi while a turn is streaming and rejected
        // as meaningless when idle, so the mode is chosen from live status
        // rather than taken from the caller. A caller may still override it —
        // that is what steering INTO a running turn needs — but the default is
        // the one that always works.
        const busy = status() === "busy";
        const deliverAs =
          typeof req.deliver_as === "string"
            ? (req.deliver_as as "steer" | "followUp")
            : busy
              ? "steer"
              : undefined;
        try {
          if (deliverAs) pi.sendUserMessage(text, { deliverAs });
          else pi.sendUserMessage(text);
        } catch (e) {
          return err(sock, id, "refused", String(e));
        }
        return ok(sock, id, { accepted: true, queued_as: deliverAs ?? "immediate" });
      }

      case "interrupt": {
        const wasBusy = status() === "busy";
        try {
          ctx?.abort();
        } catch (e) {
          return err(sock, id, "refused", String(e));
        }
        return ok(sock, id, { aborted: wasBusy });
      }

      case "subscribe":
        subscribers.add(sock);
        return ok(sock, id, { subscribed: true, status: status() });

      default:
        return err(sock, id, "unknown-verb", `no such verb: ${String(req.m)}`);
    }
  };

  // -- the listener --------------------------------------------------------

  const listen = (target: string): void => {
    // A socket left behind by a crashed predecessor would make `listen` fail
    // with EADDRINUSE for a session nobody is serving. Removing it is only safe
    // because the path is qw-owned and per-session: nothing else can be on it.
    try {
      fs.rmSync(target, { force: true });
    } catch {
      /* a path we cannot clear will surface as a listen error below */
    }
    try {
      fs.mkdirSync(path.dirname(target), { recursive: true });
    } catch {
      /* ditto */
    }

    const srv = net.createServer((sock) => {
      sock.setEncoding("utf8");
      let buf = "";
      sock.on("data", (chunk: string) => {
        buf += chunk;
        // Newline-delimited: everything before the last \n is complete.
        let nl: number;
        while ((nl = buf.indexOf("\n")) !== -1) {
          const line = buf.slice(0, nl).trim();
          buf = buf.slice(nl + 1);
          if (!line) continue;
          let req: Req;
          try {
            req = JSON.parse(line) as Req;
          } catch {
            send(sock, { err: { code: "bad-json", detail: "unparseable line" } });
            continue;
          }
          void handle(sock, req).catch((e) => {
            err(sock, typeof req.id === "number" ? req.id : undefined, "internal", String(e));
          });
        }
      });
      const drop = () => subscribers.delete(sock);
      sock.on("close", drop);
      sock.on("error", drop);
    });

    srv.on("error", () => {
      // Refuse loudly in logs but never throw: a session whose control channel
      // failed to bind is still a perfectly good pi session for its human.
    });

    try {
      srv.listen(target);
      // Owner-only. The socket is a full remote-control surface for a live
      // agent session with the user's credentials; nothing else on the box has
      // any business connecting to it.
      try {
        fs.chmodSync(target, 0o600);
      } catch {
        /* best effort — a filesystem that cannot chmod still binds */
      }
      server = srv;
      sockPath = target;
    } catch {
      /* see the error handler above */
    }
  };

  const teardown = (): void => {
    for (const sock of subscribers) {
      try {
        sock.destroy();
      } catch {
        /* nothing to do */
      }
    }
    subscribers.clear();
    try {
      server?.close();
    } catch {
      /* nothing to do */
    }
    server = undefined;
    if (sockPath) {
      try {
        fs.rmSync(sockPath, { force: true });
      } catch {
        /* a leftover socket is cleared by the next `listen` on this path */
      }
      sockPath = undefined;
    }
  };

  // -- wiring --------------------------------------------------------------

  pi.on("session_start", async (_event, live) => {
    ctx = live;
    turnsSeen = 0;

    // THE GATE. See the module docs — with no socket configured this extension
    // does nothing at all, which is what makes it safe to install globally.
    const flag = pi.getFlag("quorum-sock");
    const target =
      (typeof flag === "string" && flag.length > 0 ? flag : undefined) ??
      process.env[SOCK_ENV];
    if (!target) return;

    if (!server) listen(target);
    broadcast("session", { id: live.sessionManager.getSessionId(), status: status() });
  });

  pi.on("agent_start", async () => {
    broadcast("busy", { turns: turnsSeen });
  });

  pi.on("turn_end", async () => {
    turnsSeen += 1;
  });

  // `agent_settled`, NOT `agent_end`: pi may auto-retry, auto-compact and
  // retry, or continue with queued follow-ups after `agent_end`, so `agent_end`
  // is "this run stopped", not "pi is done". A lane that reported idle on
  // `agent_end` would hand back control mid-turn.
  pi.on("agent_settled", async () => {
    broadcast("idle", { turns: turnsSeen });
  });

  pi.on("session_shutdown", async () => {
    teardown();
  });
}
