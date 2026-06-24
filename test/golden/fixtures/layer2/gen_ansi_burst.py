#!/usr/bin/env python3
"""gen_ansi_burst.py — deterministic 64KB ANSI burst x100 generator (layer-2).

Emits 100 iterations of a ~64KB chunk of mixed ANSI: SGR color runs, cursor
moves with coordinates, and printable text lines. DETERMINISTIC (seeded) so the
fixture is stable across runs. Crucially it emits ZERO alt-screen sequences —
the no-altscreen invariant must hold under load (R1 from the spike). Each text
line carries a monotonic "BURST N" marker so backlog-completeness is checkable.

Usage: gen_ansi_burst.py <outfile> [iterations] [chunk_bytes]
"""
import sys

ESC = "\x1b"


def chunk(idx, target_bytes):
    """Build one ~target_bytes chunk of mixed, alt-screen-free ANSI."""
    out = []
    n = 0
    line = 0
    # A small palette of SGR + cursor moves (coordinates are load-bearing).
    sgrs = ["1", "0", "31", "32", "33;1", "0;36", "7", "0"]
    while n < target_bytes:
        sgr = sgrs[(idx + line) % len(sgrs)]
        row = (line % 24) + 1
        col = (line % 80) + 1
        # SGR + cursor-move + a marked text line + reset. No ?1049h/?47h/2J/3J.
        piece = "%s[%sm%s[%d;%dHBURST %d.%d payload-%s%s[0m\r\n" % (
            ESC, sgr, ESC, row, col, idx, line, "x" * 8, ESC,
        )
        out.append(piece)
        n += len(piece)
        line += 1
    return "".join(out)


def main():
    if len(sys.argv) < 2:
        sys.stderr.write(__doc__)
        sys.exit(2)
    out = sys.argv[1]
    iterations = int(sys.argv[2]) if len(sys.argv) > 2 else 100
    chunk_bytes = int(sys.argv[3]) if len(sys.argv) > 3 else 65536
    with open(out, "wb") as f:
        for i in range(iterations):
            f.write(chunk(i, chunk_bytes).encode("latin-1"))
    sys.stderr.write("gen_ansi_burst: wrote %d iterations of >=%dB to %s\n"
                     % (iterations, chunk_bytes, out))


if __name__ == "__main__":
    main()
