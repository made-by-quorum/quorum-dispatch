# Homebrew formula for the Rust qd engine.
#
# A7 packaging deliverable (plan §A7). Reality, stated plainly:
#   - The repo (private-org/qd-rust) is PRIVATE: a public `url` cannot resolve
#     without auth. Until the D-phase cutover publishes a release asset, install
#     from a LOCAL source tarball:
#       cd <repo> && git archive --prefix=qd-rust/ -o /tmp/qd-rust-local.tar.gz HEAD
#       QD_FORMULA_LOCAL=/tmp/qd-rust-local.tar.gz \
#         brew install --build-from-source --formula packaging/homebrew/quorum-dispatch.rb
#     (the formula prefers $QD_FORMULA_LOCAL when set; the canonical url is the
#     placeholder the D-phase release will make real).
#   - The package stages NO external multiplexer. It once shipped a pinned,
#     sha256-verified tarball of one plus caveats telling operators to build it;
#     that mux is retired and qd no longer drives it (FTUE R1), so the resource,
#     its pin, and the caveats are GONE. Do not reintroduce a third-party mux
#     here — this formula installs the two binaries this repo builds and nothing
#     else.
#   - Pure cargo build, no bun and no node — that is what this line has always
#     been about (it contrasts with the archived TypeScript stack's runtime
#     dependencies; it was never a ruling on how many executables the package
#     installs, though it was once read as one — see ADR-0020). The formula
#     builds `qd` AND its internal sibling `qw` in one cargo invocation.
#
# NOTE: deployed-artifact identity (⟨PETE:#2⟩, gate-2 correction) — package
# `quorum-dispatch`, formula `quorum-dispatch` (Pete-ruled for coherence with the
# crate), binary/command `qd` (unchanged). The internal lib crate stays
# `dispatch` (preserved subsystem); the `-p quorum-dispatch` package selector
# resolves because the package is named `quorum-dispatch`. Homebrew derives the
# class name from the file name `quorum-dispatch.rb` → `QuorumDispatch`.
class QuorumDispatch < Formula
  desc "Session engine for orchestrating Claude Code sessions (Rust port)"
  homepage "https://github.com/private-org/qd-rust"
  url ENV.fetch("QD_FORMULA_LOCAL", "https://github.com/private-org/qd-rust/archive/refs/tags/phase-a7.tar.gz")
  version "0.0.0-a7"
  sha256 ENV["QD_FORMULA_LOCAL"] ? :no_check : "PLACEHOLDER_UNTIL_RELEASE_ASSET_PUBLISHED"
  license :cannot_represent

  depends_on "rust" => :build

  def install
    # TWO binaries, ONE package (ADR-0020, dispatch/doc/adr/0020-*.md): `qd` is
    # the entry point; `qw` is the lane worker `qd` spawns over stdio. `qw` is a
    # [[bin]] of the in-repo `quorum-qw` crate, built from THIS source tree in
    # the same cargo invocation, so the pair is same-commit by construction —
    # not a fourth package and not a second pin. It must land in the same
    # directory as `qd`: `qd` resolves it as a sibling of its own executable and
    # never searches PATH (a `qw` on PATH could be another install's — the
    # version skew the wire handshake exists to catch). Homebrew's `bin` is that
    # one directory for both.
    system "cargo", "build", "--release",
           "-p", "quorum-dispatch", "--bin", "qd",
           "-p", "quorum-qw", "--bin", "qw"
    bin.install "target/release/qd"
    bin.install "target/release/qw"
  end

  # ONE caveat line, and one only (FTUE punch R24).
  #
  # The block that used to live here told a fresh installer to go build a
  # third-party multiplexer from a staged tarball — instructions for a dependency
  # that has since been retired — and R1 deleted it. What R1 did not notice is
  # that it deleted the ONLY thing a `brew install` said: the install ended in
  # silence, having put two binaries on a machine without naming either of them.
  #
  # This is the smallest repair that is also true, and it names the VERB rather
  # than just the binary. R24 first wrote "Run `qd`" and leaned on bare `qd`
  # redirecting into setup on a machine that looked fresh; that redirect is gone
  # (bare `qd` prints the help now, on every machine — see `verbs::dispatch`),
  # and even while it existed it did nothing for the far more common reader:
  # someone who already had qd from source and was moving to brew. They ran `qd`,
  # got a session list, and finished nothing. `qd setup` is the thing that
  # actually finishes an install, so the line says `qd setup`.
  #
  # Naming it here does not make the install DO it: C19 ruled install-time stays
  # inert — Homebrew's `post_install` is sandboxed away from `~/.zshrc`,
  # `~/.claude.json` and `~/.quorum`, is never a TTY, and re-fires on every
  # upgrade and every `brew bundle` — so a printed line a human chooses to run is
  # the whole handoff. `qd setup` is itself report-only until `--fix`, which is
  # what makes it safe to put in front of someone this way.
  #
  # NOTE FOR WHOEVER LANDS R17: the formula Homebrew actually installs from is
  # the TAP's copy (private-org/homebrew-tap, Formula/quorum-dispatch.rb), not
  # this file — which is why R1's mux-caveats deletion landed here and kept
  # printing `QD_MUX=zmx` on real machines for months. Both copies carry this
  # caveats block now. Keep the two in step, or R17 should collapse them into
  # one file.
  def caveats
    "Run `qd setup` to finish setup."
  end

  test do
    # `qd --version` prints the TS-parity version string (corpus rows 03/04).
    assert_match(/^\d+\.\d+\.\d+$/, shell_output("#{bin}/qd --version").strip)
    system bin/"qd", "--help"

    # ADR-0020: `qw` must be BESIDE `qd`, because that is the only place `qd`
    # looks for it. Assert the file, not a PATH lookup — a PATH lookup would
    # check the wrong property. `build-profile` is a machine verb (no user
    # surface) and the cheapest proof the installed file actually runs.
    assert_predicate bin/"qw", :exist?
    assert_equal "release", shell_output("#{bin}/qw build-profile").strip
  end
end
