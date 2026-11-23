//! A question asked of the human on this terminal: the core never asks one
//! itself, it takes these as functions (AGL-121, D22).

use std::io::{self, BufRead, Write};

use agentlibre_common::i18n::Lang;

/// The answer to a question on a terminal (D9, D13; AGL-033): the
/// question is a line, the answer a line of stdin, `y` or `yes` agrees.
pub fn ask_terminal(question: &str, _lang: Lang) -> bool {
    print!("{question} ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    agentlibre_core::tty::is_yes(&line)
}

/// Asks `question` in the terminal and returns the line answered, `None`
/// when it is empty or cannot be read (AGL-039, D24c).
pub fn ask_line(question: &str) -> Option<String> {
    print!("{question} ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).ok()?;
    let line = line.trim();
    (!line.is_empty()).then(|| line.to_owned())
}
