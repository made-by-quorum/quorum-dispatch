//! REAL `qd init <shell>` backend — print the shell-integration script (the
//! eval-init pattern; see [`dispatch::shell_init`] module docs for the why).
//!
//! Thin binding: parse the shell name, resolve the zmx dir through the real env
//! seam, print the emission. Exit 0 on success, 1 on an unknown shell.

use clap::ArgMatches;

use dispatch::effects::RealEnv;
use dispatch::shell_init::{init_script, Shell};
use dispatch::zmx_dir::resolve_zmx_dir;

pub fn run(matches: &ArgMatches) -> i32 {
    let raw = matches
        .get_one::<String>("shell")
        .map(String::as_str)
        .unwrap_or("");
    let shell = match Shell::from_name(raw) {
        Some(s) => s,
        None => {
            eprintln!("init: unknown shell '{raw}' (supported: bash, zsh, fish)");
            return 1;
        }
    };
    let zmx_dir = resolve_zmx_dir(&RealEnv);
    print!("{}", init_script(shell, &zmx_dir.to_string_lossy()));
    0
}
