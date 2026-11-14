//! The regex subset of `pattern` (D69, D75): characters, escapes, `.`,
//! classes, `\d \w \s` (ASCII), groups, `|` and `* + ? {n} {n,} {n,m}`. A
//! pattern covers the whole value.

use super::Expr;

/// Why a pattern is rejected, at byte `offset` of the pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatternError {
    pub offset: usize,
    pub kind: PatternErrorKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternErrorKind {
    /// `^` or `$`: a pattern always covers the whole value (D75).
    Anchor,
    /// `(?`: lookaround, flags, non-capturing or named groups.
    GroupSyntax,
    /// `\` followed by a character outside the subset, such as `\b`,
    /// `\1` or `\p`.
    Escape(char),
    /// The pattern ends right after `\`.
    TrailingBackslash,
    /// A repetition with nothing to repeat, or right after another one
    /// (`a**`, lazy `a*?`).
    NothingToRepeat,
    /// `{` that is not `{n}`, `{n,}` or `{n,m}` with `n <= m`.
    BadRepetition,
    /// `(` without `)` or `)` without `(`.
    UnbalancedGroup,
    /// `[` without `]`, an empty class, or `[` inside a class.
    BadClass,
    /// A class range that ends before it starts, such as `z-a`, or whose
    /// end is `\d`, `\w` or `\s`.
    BadRange,
}

/// Parses `pattern` into an expression.
pub fn parse(pattern: &str) -> Result<Expr, PatternError> {
    let mut parser = Parser {
        chars: pattern.char_indices().collect(),
        pos: 0,
        len: pattern.len(),
    };
    let expr = parser.alternation()?;
    if parser.pos < parser.chars.len() {
        // `alternation` stops only at the end or at `)`.
        return Err(parser.error(PatternErrorKind::UnbalancedGroup));
    }
    Ok(expr)
}

/// What an escape stands for.
enum Escaped {
    Char(char),
    Ranges(Vec<(char, char)>),
}

struct Parser {
    chars: Vec<(usize, char)>,
    pos: usize,
    len: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).map(|&(_, c)| c)
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek();
        self.pos += 1;
        c
    }

    /// Byte offset of the current character.
    fn offset(&self) -> usize {
        self.chars
            .get(self.pos)
            .map_or(self.len, |&(offset, _)| offset)
    }

    fn error(&self, kind: PatternErrorKind) -> PatternError {
        PatternError {
            offset: self.offset(),
            kind,
        }
    }

    fn alternation(&mut self) -> Result<Expr, PatternError> {
        let mut alts = vec![self.sequence()?];
        while self.peek() == Some('|') {
            self.pos += 1;
            alts.push(self.sequence()?);
        }
        Ok(if alts.len() == 1 {
            alts.pop().unwrap()
        } else {
            Expr::Alt(alts)
        })
    }

    fn sequence(&mut self) -> Result<Expr, PatternError> {
        let mut items: Vec<Expr> = Vec::new();
        while let Some(c) = self.peek()
            && c != '|'
            && c != ')'
        {
            let atom = self.atom()?;
            let item = self.repetition(atom)?;
            // Join neighbouring characters into one literal.
            match (items.last_mut(), item) {
                (Some(Expr::Literal(text)), Expr::Literal(more)) => text.push_str(&more),
                (_, item) => items.push(item),
            }
        }
        Ok(if items.len() == 1 {
            items.pop().unwrap()
        } else {
            Expr::Seq(items)
        })
    }

    fn atom(&mut self) -> Result<Expr, PatternError> {
        let start = self.offset();
        let error = |kind| {
            Err(PatternError {
                offset: start,
                kind,
            })
        };
        match self.next().unwrap() {
            '(' => {
                if self.peek() == Some('?') {
                    return error(PatternErrorKind::GroupSyntax);
                }
                let inner = self.alternation()?;
                if self.next() != Some(')') {
                    return error(PatternErrorKind::UnbalancedGroup);
                }
                Ok(inner)
            }
            '[' => self.class(start),
            '.' => Ok(Expr::Class {
                negated: true,
                ranges: vec![('\n', '\n')],
            }),
            '\\' => Ok(match self.escape(start)? {
                Escaped::Char(c) => Expr::Literal(c.into()),
                Escaped::Ranges(ranges) => Expr::Class {
                    negated: false,
                    ranges,
                },
            }),
            '^' | '$' => error(PatternErrorKind::Anchor),
            '*' | '+' | '?' | '{' => error(PatternErrorKind::NothingToRepeat),
            c => Ok(Expr::Literal(c.into())),
        }
    }

    /// Applies a repetition operator after `atom`, if there is one.
    fn repetition(&mut self, atom: Expr) -> Result<Expr, PatternError> {
        let (min, max) = match self.peek() {
            Some('{') => self.braces()?,
            Some(op @ ('*' | '+' | '?')) => {
                self.pos += 1;
                match op {
                    '*' => (0, None),
                    '+' => (1, None),
                    _ => (0, Some(1)),
                }
            }
            _ => return Ok(atom),
        };
        if matches!(self.peek(), Some('*' | '+' | '?' | '{')) {
            return Err(self.error(PatternErrorKind::NothingToRepeat));
        }
        Ok(Expr::Repeat {
            expr: Box::new(atom),
            min,
            max,
        })
    }

    /// Parses `{n}`, `{n,}` or `{n,m}` at the current `{`.
    fn braces(&mut self) -> Result<(u32, Option<u32>), PatternError> {
        let start = self.offset();
        let bad = || PatternError {
            offset: start,
            kind: PatternErrorKind::BadRepetition,
        };
        self.pos += 1;
        let min = self.number().ok_or_else(bad)?;
        let max = match self.next() {
            Some('}') => return Ok((min, Some(min))),
            Some(',') if self.peek() == Some('}') => None,
            Some(',') => Some(self.number().ok_or_else(bad)?),
            _ => return Err(bad()),
        };
        if self.next() != Some('}') || max.is_some_and(|max| max < min) {
            return Err(bad());
        }
        Ok((min, max))
    }

    fn number(&mut self) -> Option<u32> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        let digits: String = self.chars[start..self.pos]
            .iter()
            .map(|&(_, c)| c)
            .collect();
        digits.parse().ok()
    }

    /// Parses the escape after `\` at byte `start`.
    fn escape(&mut self, start: usize) -> Result<Escaped, PatternError> {
        let Some(c) = self.next() else {
            return Err(PatternError {
                offset: start,
                kind: PatternErrorKind::TrailingBackslash,
            });
        };
        Ok(match c {
            'd' => Escaped::Ranges(vec![('0', '9')]),
            'w' => Escaped::Ranges(vec![('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')]),
            // ASCII whitespace: tab, newline, vertical tab, form feed, carriage return, space.
            's' => Escaped::Ranges(vec![('\t', '\r'), (' ', ' ')]),
            'n' => Escaped::Char('\n'),
            't' => Escaped::Char('\t'),
            'r' => Escaped::Char('\r'),
            c if c.is_ascii_punctuation() => Escaped::Char(c),
            c => {
                return Err(PatternError {
                    offset: start,
                    kind: PatternErrorKind::Escape(c),
                });
            }
        })
    }

    /// Parses a class after `[` at byte `start`.
    fn class(&mut self, start: usize) -> Result<Expr, PatternError> {
        let bad_class = PatternError {
            offset: start,
            kind: PatternErrorKind::BadClass,
        };
        let negated = self.peek() == Some('^');
        if negated {
            self.pos += 1;
        }
        let mut ranges = Vec::new();
        loop {
            let item_start = self.offset();
            let lo = match self.next() {
                None | Some('[') => return Err(bad_class),
                Some(']') if ranges.is_empty() => return Err(bad_class),
                Some(']') => break,
                Some('\\') => match self.escape(item_start)? {
                    Escaped::Char(c) => c,
                    Escaped::Ranges(more) => {
                        ranges.extend(more);
                        continue;
                    }
                },
                Some(c) => c,
            };
            // `-` before `]` is a literal dash.
            let is_range = self.peek() == Some('-')
                && self.chars.get(self.pos + 1).is_some_and(|&(_, c)| c != ']');
            if !is_range {
                ranges.push((lo, lo));
                continue;
            }
            self.pos += 1;
            let hi_start = self.offset();
            let bad_range = PatternError {
                offset: item_start,
                kind: PatternErrorKind::BadRange,
            };
            let hi = match self.next() {
                Some('[') => return Err(bad_class),
                Some('\\') => match self.escape(hi_start)? {
                    Escaped::Char(c) => c,
                    Escaped::Ranges(_) => return Err(bad_range),
                },
                Some(c) => c,
                None => return Err(bad_class),
            };
            if hi < lo {
                return Err(bad_range);
            }
            ranges.push((lo, hi));
        }
        Ok(Expr::Class { negated, ranges })
    }
}

#[cfg(test)]
mod tests {
    use super::super::Grammar;
    use super::super::matcher::matches;
    use super::*;

    fn accepts(pattern: &str, text: &str) -> bool {
        matches(&Grammar::new(), &parse(pattern).unwrap(), text)
    }

    fn error(pattern: &str) -> (usize, PatternErrorKind) {
        let error = parse(pattern).unwrap_err();
        (error.offset, error.kind)
    }

    const VERIFY: &str = "cargo (test|check)( --?[a-z-]+)*";

    #[test]
    fn verify_command_pattern() {
        for text in ["cargo test", "cargo check --release -q", "cargo test --lib"] {
            assert!(accepts(VERIFY, text), "{text:?}");
        }
        for text in [
            "ls -la /etc",
            "cargo  test",
            "cargo test 2>&1",
            "cargo test\n",
            "cargo",
        ] {
            assert!(!accepts(VERIFY, text), "{text:?}");
        }
        assert_eq!(
            Grammar::new().render(&parse(VERIFY).unwrap()),
            "root ::= \"cargo \" (\"test\" | \"check\") (\" -\" \"-\"? [a-z\\x2D]+)*\n"
        );
    }

    #[test]
    fn dot_excludes_newline() {
        assert!(accepts("a.c", "abc"));
        assert!(accepts("a.c", "a<c"));
        assert!(!accepts("a.c", "a\nc"));
    }

    #[test]
    fn escapes_and_shorthands() {
        assert!(accepts(r"\.\*\\", r".*\"));
        assert!(accepts(r"a\nb\t", "a\nb\t"));
        assert!(accepts(r"\d{2,3}", "12") && accepts(r"\d{2,3}", "123"));
        assert!(!accepts(r"\d{2,3}", "1") && !accepts(r"\d{2,3}", "1234"));
        assert!(accepts(r"\w+", "snake_Case9"));
        assert!(!accepts(r"\w+", "кирилиця"));
        assert!(accepts(r"a\sb", "a b") && accepts(r"a\sb", "a\nb"));
        assert!(accepts(r"[\d.]+", "1.25"));
    }

    #[test]
    fn classes() {
        assert!(accepts("[a-]", "-") && accepts("[-a]", "-"));
        assert!(accepts("[^0-9]", "\n") && !accepts("[^0-9]", "5"));
        assert!(accepts(r"[\]\[]", "]"));
    }

    #[test]
    fn groups_alternatives_and_repetitions() {
        assert!(accepts("a|", "") && accepts("a|", "a"));
        assert!(accepts("(ab)+", "abab") && !accepts("(ab)+", "aba"));
        assert!(accepts("x{2,}", "xxxx") && !accepts("x{2,}", "x"));
        assert!(accepts("x{0}", "") && !accepts("x{0}", "x"));
        assert!(accepts("(a|b)?c", "c") && accepts("(a|b)?c", "bc"));
        assert!(accepts("}]", "}]"));
    }

    #[test]
    fn errors() {
        use PatternErrorKind::*;
        assert_eq!(error("^a"), (0, Anchor));
        assert_eq!(error("a$"), (1, Anchor));
        assert_eq!(error("(?:a)"), (0, GroupSyntax));
        assert_eq!(error("(?=a)"), (0, GroupSyntax));
        assert_eq!(error(r"\b"), (0, Escape('b')));
        assert_eq!(error(r"(a)\1"), (3, Escape('1')));
        assert_eq!(error(r"\p{L}"), (0, Escape('p')));
        assert_eq!(error("a\\"), (1, TrailingBackslash));
        assert_eq!(error("*a"), (0, NothingToRepeat));
        assert_eq!(error("a**"), (2, NothingToRepeat));
        assert_eq!(error("a*?"), (2, NothingToRepeat));
        assert_eq!(error("a{2"), (1, BadRepetition));
        assert_eq!(error("a{3,1}"), (1, BadRepetition));
        assert_eq!(error("a{x}"), (1, BadRepetition));
        assert_eq!(error("(a"), (0, UnbalancedGroup));
        assert_eq!(error("a)"), (1, UnbalancedGroup));
        assert_eq!(error("[a"), (0, BadClass));
        assert_eq!(error("[]"), (0, BadClass));
        assert_eq!(error("[]a]"), (0, BadClass));
        assert_eq!(error("[[:alpha:]]"), (0, BadClass));
        assert_eq!(error("x[z-a]"), (2, BadRange));
        assert_eq!(error(r"[a-\d]"), (1, BadRange));
        assert_eq!(error("ж^"), (2, Anchor));
    }

    #[test]
    fn newline_detection() {
        let may_contain_newline = |p: &str| parse(p).unwrap().may_contain('\n');
        assert!(!may_contain_newline(VERIFY));
        assert!(!may_contain_newline(".*"));
        assert!(may_contain_newline("[^;]*"));
        assert!(may_contain_newline(r"a\sb"));
        assert!(may_contain_newline(r"a\n"));
    }
}
