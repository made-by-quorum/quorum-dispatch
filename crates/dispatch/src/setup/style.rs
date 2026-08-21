//! Terminal styling for the `qd setup` report — one [`Style`] value, threaded
//! through the renderers, that is either real SGR or nothing at all.
//!
//! # Why a value and not a global
//!
//! Same reason [`Remedy`](super::verdict::Remedy) names an effect instead of
//! performing one: the decision ("is this a terminal that wants color?") is a
//! bin-layer question about the real process, and the renderers are pure
//! functions that must be testable without one. [`Style::detect`] answers it
//! from three plain inputs, and every unit test can render both forms by
//! passing [`Style::PLAIN`] or [`Style::ANSI`] directly.
//!
//! # Why these colors
//!
//! ONLY the eight basic ANSI colors (31-36) plus `bold`/`dim`, and NEVER a
//! background. A terminal maps those eight to its own theme, so `red` is the
//! red that theme already chose to be legible against its own background —
//! which is the only way one palette reads correctly on both a light and a dark
//! terminal. A hardcoded 256-color or truecolor value cannot do that: it is a
//! fixed point in a space where the background moves. For the same reason
//! nothing here prints black or white, the two foregrounds guaranteed to
//! collide with one background or the other.
//!
//! `dim` is spent only on things the eye should SKIP — the `[setup]` prefix
//! repeated down the left margin, the transient scan line, the list numbering.
//! No fact anyone has to read is dim, because some themes render it very faint.

use super::verdict::Status;

/// Whether this run's output carries SGR escapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    on: bool,
}

impl Style {
    /// No escapes at all — what a pipe, a file, a `--json` document, and every
    /// unit test get.
    pub const PLAIN: Style = Style { on: false };

    /// Full color.
    pub const ANSI: Style = Style { on: true };

    /// The real-terminal decision, as a pure function of what the process can
    /// see. Colored output requires ALL THREE:
    ///
    /// * `stdout_is_tty` — a pipe or a redirect gets clean bytes. This is also
    ///   what keeps every existing test assertion true: they capture stdout
    ///   through a pipe, so they see exactly the plain report they always did.
    /// * `no_color` unset/empty — the <https://no-color.org> convention: ANY
    ///   non-empty value means no color, whatever it says.
    /// * `term` is not `dumb` — the terminal that told us it cannot do this.
    pub fn detect(stdout_is_tty: bool, no_color: Option<&str>, term: Option<&str>) -> Style {
        let suppressed =
            no_color.is_some_and(|v| !v.is_empty()) || term.is_some_and(|t| t == "dumb");
        if stdout_is_tty && !suppressed {
            Style::ANSI
        } else {
            Style::PLAIN
        }
    }

    /// Wrap in one SGR sequence, or hand back the text untouched.
    ///
    /// The reset is the FULL `\x1b[0m` rather than the attribute-specific one:
    /// these spans never nest, and a full reset cannot leave a terminal wearing
    /// an attribute if the process dies mid-line.
    fn sgr(self, code: &str, s: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    /// The `[setup]` margin and other text that is present for structure, not
    /// for reading.
    pub fn dim(self, s: &str) -> String {
        self.sgr("2", s)
    }

    /// A heading or a name — the word the eye should land on in a line.
    pub fn bold(self, s: &str) -> String {
        self.sgr("1", s)
    }

    /// An action: the `→` remedy arrow and the flags a person might type.
    pub fn cyan(self, s: &str) -> String {
        self.sgr("36", s)
    }

    /// A verdict line that is entirely good news.
    pub fn bold_green(self, s: &str) -> String {
        self.sgr("1;32", s)
    }

    /// A verdict line that is not.
    pub fn bold_red(self, s: &str) -> String {
        self.sgr("1;31", s)
    }

    /// The status glyph, colored by what it says. `Fixed` is bold because it is
    /// the only status that reports something that JUST HAPPENED to the
    /// machine, and a person scanning the after-fixes report is looking for
    /// exactly those rows.
    pub fn status(self, status: Status, s: &str) -> String {
        match status {
            Status::Ok => self.sgr("32", s),
            Status::Fixed => self.sgr("1;32", s),
            Status::Info => self.sgr("2", s),
            Status::Warn => self.sgr("33", s),
            Status::Fail => self.sgr("31", s),
            Status::Skip => self.sgr("2", s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_is_byte_for_byte_the_text_it_was_given() {
        let s = Style::PLAIN;
        assert_eq!(s.dim("x"), "x");
        assert_eq!(s.bold("x"), "x");
        assert_eq!(s.cyan("x"), "x");
        assert_eq!(s.bold_red("x"), "x");
        assert_eq!(s.status(Status::Fail, "FAIL "), "FAIL ");
    }

    #[test]
    fn ansi_wraps_and_always_resets() {
        let s = Style::ANSI;
        assert_eq!(s.bold("x"), "\x1b[1mx\x1b[0m");
        assert_eq!(s.status(Status::Ok, "ok"), "\x1b[32mok\x1b[0m");
        for out in [s.dim("x"), s.bold("x"), s.cyan("x"), s.bold_green("x"), s.bold_red("x")] {
            assert!(out.ends_with("\x1b[0m"), "unreset span: {out:?}");
        }
    }

    /// Every gate is load-bearing on its own — a pipe, a `NO_COLOR` of any
    /// value, and a terminal that says it is `dumb` each suppress color by
    /// themselves.
    #[test]
    fn color_needs_a_tty_that_has_not_asked_us_to_stop() {
        assert_eq!(Style::detect(true, None, Some("xterm-256color")), Style::ANSI);
        assert_eq!(Style::detect(false, None, Some("xterm-256color")), Style::PLAIN);
        assert_eq!(Style::detect(true, Some("1"), None), Style::PLAIN);
        assert_eq!(Style::detect(true, Some("0"), None), Style::PLAIN, "NO_COLOR is set, not parsed");
        assert_eq!(Style::detect(true, Some(""), None), Style::ANSI, "empty is unset");
        assert_eq!(Style::detect(true, None, Some("dumb")), Style::PLAIN);
        assert_eq!(Style::detect(true, None, None), Style::ANSI);
    }
}
