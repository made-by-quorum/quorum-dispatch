# qd engine — composition protocol

Contracts an external tool (e.g. `qb spawn`) can rely on when shelling out to
the `qd` engine. Each contract is also enforced by tests/ADRs; this file is the
discoverable index.

## `qd new` exit contract

`qd new <name> [-p <prompt>] [--model <m>] ...` exits with a code an external
spawner can branch on WITHOUT parsing stdout/stderr. (ADR 0008; `qd new --help`
epilogue; golden scenario `new_went_busy_exit.sh`.)

| Exit | Meaning | When |
|------|---------|------|
| `0`  | Created + ready; with `-p`, the prompt was ACCEPTED (the session went busy). | Normal success. Without `-p`, success is always 0. |
| `10` | Created + ready, prompt delivered, PID file readable, but the session never went busy after bounded remediation (STALLED). The session EXISTS — attach and check the composer. | Only reachable with `-p`. The prompt may sit unsubmitted; the turn-start is unconfirmed. |
| `1`  | Any other failure: create/boot/I6/Bug-D errors, OR the PID file vanished after boot (an infra failure, NOT a stall — routing it to 10 would lie about an addressable session). | Pre-delivery failures, and the PID-file-vanished case. |

Notes for composers:

- `--model`'s delivery is fire-and-forget and does NOT affect the exit code.
- `10` is unreachable without `-p`.
- The codes `2` (config usage) and `3` (`ping` validation) belong to OTHER verbs;
  `qd new` only ever returns `0`, `1`, or `10`.
- On `10`, the session is real: `qd attach <name>` / `qd ls` will find it. On `1`
  due to a vanished PID file, the registry row is gone — do not assume the
  session is addressable.
