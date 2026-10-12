//! Text for the terminal: widths, wrapping and cleaning (D123, D138).

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Tab stops are replaced by this many spaces.
const TAB: &str = "    ";

/// The columns `text` takes, without escape sequences.
pub fn width(text: &str) -> usize {
    let mut total = 0;
    let mut rest = text;
    while let Some(start) = rest.find('\x1b') {
        total += rest[..start].width();
        rest = &rest[start..];
        // CSI sequences end with a byte in 0x40..=0x7e.
        let end = rest
            .strip_prefix("\x1b[")
            .and_then(|tail| tail.find(|c: char| ('@'..='~').contains(&c)).map(|n| n + 3))
            .unwrap_or(1);
        rest = &rest[end.min(rest.len())..];
    }
    total + rest.width()
}

/// Removes terminal controls from untrusted text but keeps line breaks;
/// tabs become spaces (D138).
pub fn clean(text: &str) -> String {
    let mut clean = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\n' => clean.push('\n'),
            '\t' => clean.push_str(TAB),
            '\r' => {}
            ch if ch.is_control() => {}
            ch => clean.push(ch),
        }
    }
    clean
}

/// Splits `text` into pieces of at most `width` columns, at characters.
pub fn hard_wrap(text: &str, width_limit: usize) -> Vec<String> {
    let limit = width_limit.max(1);
    let mut rows = vec![String::new()];
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > limit && used > 0 {
            rows.push(String::new());
            used = 0;
        }
        rows.last_mut().expect("a row").push(ch);
        used += w;
    }
    rows
}

/// Cuts `text` to `limit` columns, ending with `…` when it was longer.
pub fn truncate(text: &str, limit: usize) -> String {
    if width(text) <= limit {
        return text.into();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w + 1 > limit {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Cuts `text` to its last `limit` columns, starting with `…` when it was
/// longer.
pub fn truncate_start(text: &str, limit: usize) -> String {
    if width(text) <= limit {
        return text.into();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut used = 1;
    for ch in text.chars().rev() {
        let w = ch.width().unwrap_or(0);
        if used + w > limit {
            break;
        }
        kept.push(ch);
        used += w;
    }
    kept.push('…');
    kept.into_iter().rev().collect()
}

/// Wraps streamed text into lines of at most `width` columns as it
/// arrives. Finished lines are final; the unfinished one is shown live.
pub struct StreamWrap {
    width: usize,
    current: String,
    current_width: usize,
}

impl StreamWrap {
    pub fn new(width: usize) -> StreamWrap {
        StreamWrap {
            width: width.max(1),
            current: String::new(),
            current_width: 0,
        }
    }

    /// Adds a piece; returns the lines it finished.
    pub fn push(&mut self, piece: &str) -> Vec<String> {
        let mut done = Vec::new();
        for ch in clean(piece).chars() {
            if ch == '\n' {
                done.push(self.take());
                continue;
            }
            let w = ch.width().unwrap_or(0);
            if self.current_width + w > self.width {
                // Break at the last space when there is one.
                match self.current.rfind(' ') {
                    Some(space) if space > 0 => {
                        let rest = self.current[space + 1..].to_owned();
                        self.current.truncate(space);
                        let line = self.take();
                        done.push(line);
                        self.current_width = width(&rest);
                        self.current = rest;
                    }
                    _ => done.push(self.take()),
                }
            }
            self.current.push(ch);
            self.current_width += w;
        }
        done
    }

    /// The unfinished line.
    pub fn live(&self) -> &str {
        &self.current
    }

    /// Ends the text: the unfinished line, if any.
    pub fn finish(&mut self) -> Option<String> {
        (!self.current.is_empty()).then(|| self.take())
    }

    fn take(&mut self) -> String {
        self.current_width = 0;
        std::mem::take(&mut self.current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("привіт"), 6);
        assert_eq!(width("日本"), 4);
        assert_eq!(width("\x1b[1;38;2;1;2;3mab\x1b[0mc"), 3);
    }

    #[test]
    fn cleaning() {
        assert_eq!(clean("a\x1b[31mb\r\nc\td\x07"), "a[31mb\nc    d");
    }

    #[test]
    fn truncating() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate_start("hello world", 6), "…world");
        assert_eq!(truncate_start("hi", 6), "hi");
        assert_eq!(hard_wrap("abcdefg", 3), ["abc", "def", "g"]);
        assert_eq!(hard_wrap("日本語", 4), ["日本", "語"]);
    }

    #[test]
    fn streamed_text_is_wrapped_at_words() {
        let mut wrap = StreamWrap::new(10);
        let mut lines = Vec::new();
        for piece in ["hello wor", "ld and more", " text\nnext"] {
            lines.extend(wrap.push(piece));
        }
        assert_eq!(lines, ["hello", "world and", "more text"]);
        assert_eq!(wrap.live(), "next");
        assert_eq!(wrap.finish().as_deref(), Some("next"));
        assert_eq!(wrap.finish(), None);
    }

    #[test]
    fn long_words_break_inside() {
        let mut wrap = StreamWrap::new(4);
        let lines = wrap.push("abcdefghij");
        assert_eq!(lines, ["abcd", "efgh"]);
        assert_eq!(wrap.live(), "ij");
        // The same text in other pieces gives the same lines.
        let mut other = StreamWrap::new(4);
        let mut all = Vec::new();
        for ch in "abcdefghij".chars() {
            all.extend(other.push(&ch.to_string()));
        }
        assert_eq!(all, lines);
    }
}
