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

The formula pins a commit tarball of `made-by-quorum/quorum-dispatch` over
https, because that repo carries no tags yet. To ship a newer `main`:

```sh
SHA=$(git ls-remote https://github.com/made-by-quorum/quorum-dispatch main | cut -f1)
curl -sL -o /tmp/qd.tar.gz \
  "https://github.com/made-by-quorum/quorum-dispatch/archive/$SHA.tar.gz"
shasum -a 256 /tmp/qd.tar.gz
```

Put that `$SHA` in `url` and the digest in `sha256`. If `version` does not move
(it is stated by hand — it is what `qd --version` reports, not the crate
version, which is `0.0.0`), add or increment `revision` as well, or Homebrew
will not offer the upgrade to anyone who already installed.

Pin a SHA that exists **on `made-by-quorum/quorum-dispatch`** — that is the repo
the formula downloads from, and it is not always where the change was authored.

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
