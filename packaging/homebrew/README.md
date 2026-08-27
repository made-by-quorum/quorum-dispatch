# Homebrew packaging

**The formula is not in this repo.** It lives in the tap, which is the only copy
Homebrew ever installs from:

> [`made-by-quorum/quorum-dispatch`](https://github.com/made-by-quorum/homebrew-quorum-dispatch) → `Formula/quorum-dispatch.rb`

There used to be a second copy here, and the two drifted: a fix that landed in
this directory kept printing retired caveats on real machines for months,
because the tap's copy — the one that actually installs — never got it. One
copy, in the tap. Do not add another here.

```sh
brew tap made-by-quorum/quorum-dispatch
brew trust made-by-quorum/quorum-dispatch/quorum-dispatch   # Homebrew 6 gates third-party taps
brew install quorum-dispatch
```

## Moving the pin

`bump-pin.sh` does it. It resolves the mirror's `main`, digests that commit
tarball, rewrites the formula in a clone of the tap, and commits — it does
**not** push unless you say so:

```sh
bash packaging/homebrew/bump-pin.sh            # stage the bump, print the diff
bash packaging/homebrew/bump-pin.sh --push     # ... and push it to the tap
bash packaging/homebrew/bump-pin.sh --version 0.2.0   # also move the stated version
bash packaging/homebrew/bump-pin.sh --ref <sha>       # pin something other than main
```

Two things it gets right that are easy to get wrong by hand:

- **`revision`.** `version` is stated by hand — it is what `qd --version`
  reports, not the crate version, which is `0.0.0` — so a source bump does not
  move it, and Homebrew offers no upgrade to anyone who already installed
  unless `revision` moves instead. The script increments it by default, and
  drops it when `--version` moves the version (the correct pairing).
- **The commit it pins exists on `made-by-quorum/quorum-dispatch`.** That is the
  repo the tarball url resolves against, and it is not always where the change
  was authored — so land the export first. The script also checks the tarball
  carries the export layout (root `Cargo.toml`, both crates) before pinning it,
  rather than letting an unexported tree fail on every user's machine.

Knobs: `QD_PUBLIC_REPO`, `QD_TAP_REMOTE`, and `QD_TAP_DIR` to use a tap clone
you already have instead of a throwaway one.

## Smoke-testing a formula change

`smoke.sh` installs the tap's formula against a tarball of your local
`HEAD` — so it proves the real formula builds this source, without waiting for an
export and a pin bump. It installs, runs `brew test`, then uninstalls and
untaps.

```sh
bash packaging/homebrew/smoke.sh                    # formula from the tap
QD_FORMULA=/path/to/quorum-dispatch.rb \
  bash packaging/homebrew/smoke.sh                  # or a local edit of it
```

It refuses to run while a real `quorum-dispatch` is installed — it would
uninstall it on the way out. That proves the PACKAGE installs. It never opens a session — `scripts/fresh-install-smoke.py`
is the other half, driving the real verbs against an already-installed `qd`.
