//! The terminal as the program that asks questions sees it: whether stdin
//! and stdout are terminals, whether an answer is a yes (AGL-067, F4), and a
//! question asked of the human on this terminal: the core never asks one
//! itself, it takes these as functions (AGL-121, D22; AGL-131, D6).

use std::io::{self, BufRead, Write};

use crate::i18n::Lang;

/// Exit status after Ctrl-C (AGL-063, D112; AGL-131, D6).
pub const INTERRUPTED_EXIT: u8 = 130;

/// Whether stdin and stdout are terminals (D128).
pub fn is_interactive() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDOUT_FILENO) == 1 }
}

/// Whether the answer to a yes/no question is yes.
pub fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

/// The answer to a question on a terminal (D9, D13; AGL-033): the
/// question is a line, the answer a line of stdin, `y` or `yes` agrees.
pub fn ask_yes(question: &str, _lang: Lang) -> bool {
    print!("{question} ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    is_yes(&line)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_is_yes_only_for_y_and_yes() {
        for answer in ["y", "Y\n", " yes ", "YES"] {
            assert!(is_yes(answer), "{answer:?}");
        }
        for answer in ["", "\n", "n", "no", "yep", "ye"] {
            assert!(!is_yes(answer), "{answer:?}");
        }
    }
}
