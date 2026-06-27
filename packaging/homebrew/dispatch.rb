# Homebrew formula for the Rust dispatch engine (Stage 1: ships ON pinned zmx 0.6.0).
#
# A7 packaging deliverable (plan §A7: "brew formula + cargo build of single dispatch
# binary (+ pinned zmx)"). Stage-1 reality, stated plainly:
#   - The repo (private-org/qd-rust) is PRIVATE: a public `url` cannot resolve
#     without auth. Until the D-phase cutover publishes a release asset, install
#     from a LOCAL source tarball:
#       cd <repo> && git archive --prefix=qd-rust/ -o /tmp/qd-rust-local.tar.gz HEAD
#       SB_FORMULA_LOCAL=/tmp/qd-rust-local.tar.gz \
#         brew install --build-from-source --formula packaging/homebrew/dispatch.rb
#     (the formula prefers $SB_FORMULA_LOCAL when set; the canonical url is the
#     placeholder the D-phase release will make real).
#   - zmx 0.6.0 is installed from the IN-REPO pinned mirror (vendor/zmx), sha256
#     pin-verified — the same fail-closed pin scripts/fetch-zmx.sh enforces
#     (selftested in CI: test_fetch_zmx.sh). No network fetch of zmx, ever.
#   - Single binary: `cargo build --release -p dispatch --bin dispatch`; no bun, no node.
#     Success criterion #1's "no external zmx" half is Stage-2-conditional
#     (B-track); Stage 1 deliberately pins and ships zmx alongside.
class Dispatch < Formula
  desc "Session engine for orchestrating Claude Code sessions (Rust port)"
  homepage "https://github.com/private-org/qd-rust"
  url ENV.fetch("SB_FORMULA_LOCAL", "https://github.com/private-org/qd-rust/archive/refs/tags/phase-a7.tar.gz")
  version "0.0.0-a7"
  sha256 ENV["SB_FORMULA_LOCAL"] ? :no_check : "PLACEHOLDER_UNTIL_RELEASE_ASSET_PUBLISHED"
  license :cannot_represent

  depends_on "rust" => :build

  def install
    system "cargo", "build", "--release", "-p", "dispatch", "--bin", "dispatch"
    bin.install "target/release/dispatch"

    # Pinned zmx 0.6.0: the in-repo mirror is SOURCE (a zig project), verified
    # against the same fail-closed sha256 pin scripts/fetch-zmx.sh enforces.
    # The formula VERIFIES the pin and stages the verified tarball under share/
    # but does NOT build it (that would pull a zig toolchain into the formula;
    # operators install zmx via scripts/fetch-zmx.sh, which builds/installs from
    # this exact mirror). Honest Stage-1 boundary, stated in caveats.
    zmx_tar = buildpath/"vendor/zmx/zmx-0.6.0.tar.gz"
    expected = "4b5a155a0956abb812ab52fb5d65e63cc1d745eee566ea7fd8930393284d4673"
    actual = Digest::SHA256.file(zmx_tar).hexdigest
    odie "vendored zmx sha256 mismatch: #{actual}" unless actual == expected
    (pkgshare/"zmx").install zmx_tar
    (pkgshare/"zmx").install buildpath/"vendor/zmx/SHA256SUMS"
    (pkgshare/"zmx").install buildpath/"scripts/fetch-zmx.sh"
  end

  def caveats
    <<~EOS
      dispatch (Stage 1) drives sessions through zmx 0.6.0 (pinned). Install it from
      the verified in-repo mirror staged at:
        #{pkgshare}/zmx/  (sha256-pinned tarball + fetch-zmx.sh)
      The Stage-2 embedded mux (qrmux) removes this dependency.
    EOS
  end

  test do
    # `dispatch --version` prints the TS-parity version string (corpus rows 03/04).
    assert_match(/^\d+\.\d+\.\d+$/, shell_output("#{bin}/dispatch --version").strip)
    system bin/"dispatch", "--help"
  end
end
