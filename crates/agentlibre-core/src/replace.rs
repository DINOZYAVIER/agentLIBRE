//! The replace of the one occurrence of a text in a file: the edit tool's
//! and the memory's (D25, D27, D101, D103; AGL-121, D80).

use std::fs;
use std::io;
use std::path::Path;

/// Why a replace was not made (AGL-121, D80).
#[derive(Debug)]
pub enum Refusal {
    /// The file cannot be read.
    Read(io::Error),
    /// The file is not UTF-8 text.
    NotUtf8,
    /// `old` is not in the file.
    NotFound,
    /// `old` is in the file `count` times; `lines` are the lines of the
    /// first ten.
    Many { count: usize, lines: Vec<usize> },
    /// The file cannot be written.
    Write(io::Error),
}

impl Refusal {
    /// The refusal of a replace in `path`, in English, as the tools have
    /// always answered it.
    pub fn text(self, path: &str) -> String {
        match self {
            Refusal::Read(error) => format!("Cannot read {path}: {error}"),
            Refusal::NotUtf8 => format!("{path} is not UTF-8 text."),
            Refusal::NotFound => format!("`old` was not found in {path}."),
            Refusal::Many { count, lines } => {
                let mut lines: Vec<String> = lines.iter().map(usize::to_string).collect();
                if count > 10 {
                    lines.push("…".into());
                }
                format!(
                    "`old` appears {count} times in {path} (lines {}); add context to make it unique.",
                    lines.join(", ")
                )
            }
            Refusal::Write(error) => format!("Cannot write {path}: {error}"),
        }
    }
}

/// The byte offsets where `old` starts in `text`, overlapping ones too.
pub fn occurrences(text: &str, old: &str) -> Vec<usize> {
    text.char_indices()
        .map(|(i, _)| i)
        .chain([text.len()])
        .filter(|&i| text[i..].starts_with(old))
        .collect()
}

/// Replaces the only occurrence of `old` in the file `file` with `new`.
pub fn once(file: &Path, old: &str, new: &str) -> Result<(), Refusal> {
    let bytes = fs::read(file).map_err(Refusal::Read)?;
    let text = String::from_utf8(bytes).map_err(|_| Refusal::NotUtf8)?;
    let starts = occurrences(&text, old);
    match starts.as_slice() {
        [] => Err(Refusal::NotFound),
        [start] => {
            let mut edited = String::with_capacity(text.len() + new.len());
            edited.push_str(&text[..*start]);
            edited.push_str(new);
            edited.push_str(&text[start + old.len()..]);
            fs::write(file, edited).map_err(Refusal::Write)
        }
        starts => Err(Refusal::Many {
            count: starts.len(),
            lines: starts
                .iter()
                .take(10)
                .map(|&i| text[..i].matches('\n').count() + 1)
                .collect(),
        }),
    }
}
