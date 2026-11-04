//! The output of `bash` (AGL-049): the model gets a part of a long one, its
//! start and its end, cut to a limit in tokens, and the whole is kept in a file
//! it can read when it needs to.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The most a file keeps: its first and its last half (AGL-049, D5).
const FILE_CEILING: usize = 10 * 1024 * 1024;

/// How many bytes per token of the limit are held for the cut: an output is
/// cut in tokens only at its end, by the server's count, and few tokens take
/// more than this.
const WINDOW_BYTES_PER_TOKEN: usize = 8;

/// What stands in when the server does not count: a cut in bytes.
const FALLBACK_BYTES_PER_TOKEN: usize = 4;

/// Where a call's output goes and how much of it the model sees.
pub struct Output<'a> {
    /// The limit in tokens (AGL-049, D3b).
    pub tokens: usize,
    pub file: PathBuf,
    /// The tokens of a text, as the server counts them; `None` when it does
    /// not answer.
    pub count: &'a dyn Fn(&str) -> Option<usize>,
}

/// The file of a long output, opened when an output passes its limit: a short
/// one leaves no file.
struct Kept {
    /// `None` when it could not be written: the model is then told of no file.
    file: Option<File>,
    /// The first half of the ceiling goes straight to the file.
    written: usize,
    /// The last half of the ceiling waits for the end of the output.
    last: VecDeque<u8>,
    dropped: u64,
}

impl Kept {
    fn open(path: &Path) -> Kept {
        let file = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| File::create(path));
        Kept {
            file: file.ok(),
            written: 0,
            last: VecDeque::new(),
            dropped: 0,
        }
    }

    fn write(&mut self, data: &[u8]) {
        let Some(file) = &mut self.file else { return };
        let room = (FILE_CEILING / 2 - self.written).min(data.len());
        if room > 0 {
            if file.write_all(&data[..room]).is_err() {
                self.file = None;
                return;
            }
            self.written += room;
        }
        self.last.extend(&data[room..]);
        let excess = self.last.len().saturating_sub(FILE_CEILING / 2);
        if excess > 0 {
            self.last.drain(..excess);
            self.dropped += excess as u64;
        }
    }

    /// What waits goes to the file.
    fn finish(&mut self) -> io::Result<()> {
        let Some(file) = &mut self.file else {
            return Ok(());
        };
        if self.dropped > 0 {
            writeln!(file, "\n[{} bytes dropped: the file keeps a start and an end]", self.dropped)?;
        }
        let (front, back) = self.last.as_slices();
        file.write_all(front)?;
        file.write_all(back)?;
        self.last.clear();
        file.flush()
    }
}

/// A command's output as it comes.
pub struct Capture {
    tokens: usize,
    file: PathBuf,
    /// All of it while it is within the window.
    whole: Vec<u8>,
    /// Its start and its end, 20% and 80% of the window (AGL-049, D4).
    start: Vec<u8>,
    end: VecDeque<u8>,
    total: u64,
    kept: Option<Kept>,
}

impl Capture {
    pub fn new(tokens: usize, file: PathBuf) -> Capture {
        Capture {
            tokens,
            file,
            whole: Vec::new(),
            start: Vec::new(),
            end: VecDeque::new(),
            total: 0,
            kept: None,
        }
    }

    fn window(&self) -> usize {
        self.tokens * WINDOW_BYTES_PER_TOKEN
    }

    pub fn push(&mut self, data: &[u8]) {
        self.total += data.len() as u64;
        let room = (self.window() / 5)
            .saturating_sub(self.start.len())
            .min(data.len());
        self.start.extend_from_slice(&data[..room]);
        self.end.extend(&data[room..]);
        let excess = self
            .end
            .len()
            .saturating_sub(self.window() - self.window() / 5);
        self.end.drain(..excess);
        let window = self.window();
        match &mut self.kept {
            Some(kept) => kept.write(data),
            None if self.total as usize > window => {
                let mut kept = Kept::open(&self.file);
                kept.write(&std::mem::take(&mut self.whole));
                kept.write(data);
                self.kept = Some(kept);
            }
            None => self.whole.extend_from_slice(data),
        }
    }

    /// What the model sees; the file is complete after it. The limit is
    /// applied here, once, so it does not touch the prompt's cache.
    pub fn text(&mut self, count: &dyn Fn(&str) -> Option<usize>) -> String {
        if self.kept.is_none() {
            let whole = String::from_utf8_lossy(&self.whole).into_owned();
            // Few bytes are few tokens: the server need not be asked.
            let fits = whole.len() <= self.tokens
                || match count(&whole) {
                    Some(tokens) => tokens <= self.tokens,
                    None => whole.len() <= self.tokens * FALLBACK_BYTES_PER_TOKEN,
                };
            if fits {
                return whole;
            }
            let mut kept = Kept::open(&self.file);
            kept.write(&self.whole);
            self.kept = Some(kept);
        }
        let kept = self.kept.as_mut().expect("just set");
        let written = kept.finish().is_ok() && kept.file.is_some();
        let end: Vec<u8> = self.end.iter().copied().collect();
        cut(
            start_of(&self.start),
            end_of(&end),
            self.total,
            self.tokens,
            written.then_some(self.file.as_path()),
            count,
        )
    }
}

/// `text`, which is in no file yet, as the model sees it: the same limit, and
/// the whole kept in the output's file when it is cut (AGL-049).
pub fn shown(text: &str, output: &Output<'_>) -> String {
    let mut capture = Capture::new(output.tokens, output.file.clone());
    capture.push(text.as_bytes());
    capture.text(output.count)
}

/// `text`, which is whole in `file` already, as the model sees it: the same
/// limit and the same cut as a command's output (AGL-049).
pub fn limited(
    text: &str,
    tokens: usize,
    file: &Path,
    count: &dyn Fn(&str) -> Option<usize>,
) -> String {
    let fits = text.len() <= tokens
        || match count(text) {
            Some(counted) => counted <= tokens,
            None => text.len() <= tokens * FALLBACK_BYTES_PER_TOKEN,
        };
    if fits {
        return text.to_owned();
    }
    let window = tokens * WINDOW_BYTES_PER_TOKEN;
    let bytes = text.as_bytes();
    let start = &bytes[..(window / 5).min(bytes.len())];
    let end = &bytes[bytes.len().saturating_sub(window - window / 5)..];
    cut(
        start_of(start),
        end_of(end),
        bytes.len() as u64,
        tokens,
        Some(file),
        count,
    )
}

/// The start and the end of an output of `total` bytes, in their shares of the
/// limit (AGL-049, D4), and the mark between them.
fn cut(
    start: &str,
    end: &str,
    total: u64,
    tokens: usize,
    file: Option<&Path>,
    count: &dyn Fn(&str) -> Option<usize>,
) -> String {
    let start_tokens = tokens / 5;
    let head = fit(start, start_tokens, count, Keep::Start);
    let tail = fit(end, tokens - start_tokens, count, Keep::End);
    let omitted = total.saturating_sub((head.len() + tail.len()) as u64);
    let mark = match file {
        Some(file) => format!(
            "[… {omitted} bytes omitted; the full output: {}; read it with grep or sed]",
            file.display()
        ),
        None => format!("[… {omitted} bytes omitted]"),
    };
    let joint = if head.is_empty() || head.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    format!("{head}{joint}{mark}\n{tail}")
}

/// The whole characters at the start of `bytes`: a window can end in one.
fn start_of(bytes: &[u8]) -> &str {
    match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap_or_default(),
    }
}

/// The whole characters at the end of `bytes`: a window can start in one.
/// What is not text is cut there too, as a binary output says nothing here.
fn end_of(bytes: &[u8]) -> &str {
    let skip = bytes
        .iter()
        .take_while(|byte| (**byte & 0b1100_0000) == 0b1000_0000)
        .count();
    start_of(&bytes[skip..])
}

#[derive(Clone, Copy)]
enum Keep {
    Start,
    End,
}

/// `text` cut to `budget` tokens at its far side. The count of a part is close
/// to its share of the whole, so a few rounds are enough.
fn fit(mut text: &str, budget: usize, count: &dyn Fn(&str) -> Option<usize>, keep: Keep) -> String {
    let cut = |text: &'_ str, bytes: usize| -> usize {
        match keep {
            Keep::Start => {
                let mut at = bytes.min(text.len());
                while !text.is_char_boundary(at) {
                    at -= 1;
                }
                at
            }
            Keep::End => {
                let mut at = text.len().saturating_sub(bytes);
                while !text.is_char_boundary(at) {
                    at += 1;
                }
                at
            }
        }
    };
    for _ in 0..4 {
        let target = match count(text) {
            Some(tokens) if tokens <= budget => break,
            Some(tokens) => text.len() * budget / tokens * 19 / 20,
            None => budget * FALLBACK_BYTES_PER_TOKEN,
        };
        if target >= text.len() {
            break;
        }
        let at = cut(text, target);
        text = match keep {
            Keep::Start => &text[..at],
            Keep::End => &text[at..],
        };
    }
    text.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Text of a token per byte, as a run of digits is.
    fn dense(text: &str) -> Option<usize> {
        Some(text.len())
    }

    /// Text of four bytes per token.
    fn sparse(text: &str) -> Option<usize> {
        Some(text.len() / 4)
    }

    fn no_server(_: &str) -> Option<usize> {
        None
    }

    fn file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentLIBRE-output-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir.join("deep/c1.txt")
    }

    #[test]
    fn what_fits_is_whole_and_leaves_no_file() {
        let path = file("fits");
        let mut capture = Capture::new(100, path.clone());
        capture.push(&[b'a'; 60]);
        capture.push(&[b'b'; 40]);
        // Within the limit in bytes, the server is not even asked.
        let text = capture.text(&|_| panic!("asked"));
        assert_eq!(text, "a".repeat(60) + &"b".repeat(40));
        assert!(!path.exists());
        // More bytes than the limit, but within it in tokens.
        let mut capture = Capture::new(100, path.clone());
        capture.push(&[b'a'; 300]);
        assert_eq!(capture.text(&sparse), "a".repeat(300));
        assert!(!path.exists());
    }

    #[test]
    fn the_limit_is_in_tokens() {
        // The same bytes, cut harder when they take more tokens.
        let data = vec![b'7'; 4000];
        let of = |count: fn(&str) -> Option<usize>, name: &str| {
            let mut capture = Capture::new(100, file(name));
            capture.push(&data);
            capture.text(&count)
        };
        let dense = of(dense, "dense");
        let sparse = of(sparse, "sparse");
        assert!(dense.contains("bytes omitted"));
        assert!(sparse.contains("bytes omitted"));
        let digits = |text: &str| text.chars().filter(|c| *c == '7').count();
        assert!(digits(&dense) <= 100, "{}", digits(&dense));
        assert!(digits(&dense) >= 80, "{}", digits(&dense));
        assert!(digits(&sparse) <= 400, "{}", digits(&sparse));
        assert!(digits(&sparse) >= 300, "{}", digits(&sparse));
    }

    #[test]
    fn a_long_output_shows_its_start_and_end_and_keeps_the_whole() {
        let path = file("long");
        let mut capture = Capture::new(100, path.clone());
        let data: Vec<u8> = (0..4000u32).map(|n| b'a' + (n % 26) as u8).collect();
        for piece in data.chunks(37) {
            capture.push(piece);
        }
        let text = capture.text(&sparse);
        let (head, tail) = text.split_once("[… ").unwrap();
        let tail = tail.split_once("sed]\n").unwrap().1;
        assert!(text.contains(&format!("the full output: {}", path.display())));
        assert!(data.starts_with(head.trim_end().as_bytes()));
        assert!(data.ends_with(tail.as_bytes()));
        // A fifth of the limit for the start, the rest for the end.
        assert!(head.len() <= 20 * 4 && head.len() >= 15 * 4, "{}", head.len());
        assert!(tail.len() <= 80 * 4 && tail.len() >= 60 * 4, "{}", tail.len());
        assert_eq!(fs::read(&path).unwrap(), data);
    }

    #[test]
    fn an_output_within_the_window_that_does_not_fit_is_kept_too() {
        let path = file("window");
        let mut capture = Capture::new(100, path.clone());
        capture.push(&[b'7'; 500]);
        let text = capture.text(&dense);
        assert!(text.contains("the full output"), "{text}");
        assert_eq!(fs::read(&path).unwrap(), vec![b'7'; 500]);
    }

    #[test]
    fn without_the_server_it_is_cut_in_bytes() {
        let path = file("noserver");
        let mut capture = Capture::new(100, path.clone());
        capture.push(&[b'a'; 300]);
        assert_eq!(capture.text(&no_server), "a".repeat(300));
        let mut capture = Capture::new(100, path.clone());
        capture.push(&[b'a'; 5000]);
        let text = capture.text(&no_server);
        assert!(text.contains("bytes omitted"));
        assert!(text.len() <= 100 * 4 + 300, "{}", text.len());
    }

    #[test]
    fn the_cuts_keep_whole_characters() {
        let path = file("chars");
        let mut capture = Capture::new(101, path.clone());
        capture.push("я".repeat(1000).as_bytes());
        let text = capture.text(&dense);
        assert!(!text.contains('\u{fffd}'), "{text}");
        assert!(text.contains('я'));
        assert_eq!(fs::read_to_string(&path).unwrap(), "я".repeat(1000));
    }

    #[test]
    fn a_file_stops_at_its_ceiling_and_keeps_both_ends() {
        let path = file("ceiling");
        let mut capture = Capture::new(100, path.clone());
        let block = vec![b'x'; 1024 * 1024];
        capture.push(b"FIRST\n");
        for _ in 0..12 {
            capture.push(&block);
        }
        capture.push(b"\nLAST\n");
        let _ = capture.text(&sparse);
        let kept = fs::read(&path).unwrap();
        assert!(kept.len() < FILE_CEILING + 200, "{}", kept.len());
        assert!(kept.starts_with(b"FIRST\n"));
        assert!(kept.ends_with(b"\nLAST\n"));
        assert!(String::from_utf8_lossy(&kept).contains("bytes dropped"));
    }

    #[test]
    fn a_text_in_no_file_yet_is_kept_when_it_is_cut() {
        let path = file("shown");
        let output = Output {
            tokens: 100,
            file: path.clone(),
            count: &sparse,
        };
        assert_eq!(shown("short", &output), "short");
        assert!(!path.exists());
        let text = "a".repeat(2000) + &"z".repeat(2000);
        let cut = shown(&text, &output);
        assert!(cut.contains("the full output"), "{cut}");
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn a_text_that_is_in_a_file_already_is_cut_the_same() {
        let path = Path::new("/store/spec.md");
        let short = limited("short", 100, path, &|_| panic!("asked"));
        assert_eq!(short, "short");
        let text = "a".repeat(2000) + &"z".repeat(2000);
        let cut = limited(&text, 100, path, &sparse);
        assert!(cut.starts_with('a') && cut.ends_with('z'));
        assert!(cut.contains("the full output: /store/spec.md"), "{cut}");
        assert!(cut.len() <= 100 * 4 + 200, "{}", cut.len());
        assert!(!path.exists());
    }

    #[test]
    fn a_file_that_cannot_be_written_is_not_named() {
        let mut capture = Capture::new(10, PathBuf::from("/proc/no/such/dir/c1.txt"));
        capture.push(&[b'a'; 1000]);
        let text = capture.text(&sparse);
        assert!(text.contains("bytes omitted]"), "{text}");
        assert!(!text.contains("full output"), "{text}");
    }
}
