//! Tiny TTY helpers shared by the bin verbs (bootstrap's relay prompt, setup's
//! apply prompt, and `qd start`'s harness question).
//!
//! These are bin-layer effects (interactivity detection + a visible `[y/N]`
//! prompt). The pure deciders in [`dispatch::bootstrap`] never call these — they take
//! `interactive` + a `prompt_yes_no` closure — so unit tests stay hermetic; this
//! module is the production wiring only.

use std::io::{BufRead, Write};

/// Is the session interactive? (Both stdin AND stdout must be TTYs — the TS
/// `Boolean(process.stdin.isTTY && process.stdout.isTTY)`,
/// 0d0fa9e:src/commands/bootstrap.ts:843.) A non-TTY NEVER prompts.
pub fn stdin_and_stdout_are_tty() -> bool {
    // SAFETY: isatty on a valid fd is always safe.
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDOUT_FILENO) == 1 }
}

/// Visible yes/no prompt, DEFAULT NO (port of the TS `promptYesNo`,
/// bootstrap.ts:838-852): write the question, read a line from stdin, and treat
/// ONLY an explicit `y`/`yes` (case-insensitive, trimmed) as yes. A bare Enter /
/// anything else / EOF = No. Callers guard on `interactive` BEFORE calling this,
/// so a non-TTY never reaches here.
pub fn prompt_yes_no_default_no(question: &str) -> bool {
    print!("{question}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let stdin = std::io::stdin();
    if stdin.lock().read_line(&mut line).is_err() {
        // Read error / EOF → default No (never hang).
        return false;
    }
    let ans = line.trim().to_ascii_lowercase();
    ans == "y" || ans == "yes"
}

/// Ask a question and read ONE line back, trimmed. `None` on EOF or a read
/// error — which is the whole reason this returns an `Option` rather than a
/// `String`: a caller must be able to tell "the human pressed Enter" (an empty
/// `Some`, meaning *take the default*) apart from "there is nobody there" (a
/// closed stdin), and both have to resolve without hanging.
///
/// Like [`prompt_yes_no_default_no`], callers guard on interactivity BEFORE
/// calling — a non-TTY never reaches here. Added for FTUE punch R20 (`qd
/// start`'s "which harness?" question), which needs a free-text answer rather
/// than a `[y/N]`.
pub fn prompt_line(question: &str) -> Option<String> {
    print!("{question}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let stdin = std::io::stdin();
    match stdin.lock().read_line(&mut line) {
        // read_line answers Ok(0) at EOF with nothing appended — a closed stdin,
        // not an empty answer. Distinguishing them is the point of the Option.
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}
