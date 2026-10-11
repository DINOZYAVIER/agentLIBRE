//! Grammars that constrain what the model may generate, rendered as
//! llama.cpp GBNF (D7, D20, D41).


use std::fmt::Write;

/// A grammar expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    /// Exact text.
    Literal(String),
    /// One character from `ranges` or, when `negated`, outside them.
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
    /// Expressions one after another.
    Seq(Vec<Expr>),
    /// One of the expressions.
    Alt(Vec<Expr>),
    /// `expr` repeated `min` to `max` times; no upper limit when `max` is
    /// `None`.
    Repeat {
        expr: Box<Expr>,
        min: u32,
        max: Option<u32>,
    },
    /// A rule of the [`Grammar`].
    Rule(String),
}

impl Expr {
    pub fn literal(text: &str) -> Expr {
        Expr::Literal(text.into())
    }

    /// One of `values`, each taken literally (`enum`).
    pub fn one_of<S: AsRef<str>>(values: &[S]) -> Expr {
        Expr::Alt(values.iter().map(|v| Expr::literal(v.as_ref())).collect())
    }

    /// Whether some text matching the expression contains `c`. Rules count
    /// as able to contain anything.
    pub fn may_contain(&self, c: char) -> bool {
        match self {
            Expr::Literal(text) => text.contains(c),
            Expr::Class { negated, ranges } => {
                negated ^ ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi)
            }
            Expr::Seq(items) | Expr::Alt(items) => items.iter().any(|e| e.may_contain(c)),
            Expr::Repeat { expr, max, .. } => *max != Some(0) && expr.may_contain(c),
            Expr::Rule(_) => true,
        }
    }

    /// Writes the expression in GBNF.
    fn render(&self, out: &mut String) {
        match self {
            Expr::Literal(text) => {
                out.push('"');
                for c in text.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        _ => push_char(out, c),
                    }
                }
                out.push('"');
            }
            Expr::Class { negated, ranges } => {
                out.push('[');
                if *negated {
                    out.push('^');
                }
                for &(lo, hi) in ranges {
                    push_class_char(out, lo);
                    if lo != hi {
                        out.push('-');
                        push_class_char(out, hi);
                    }
                }
                out.push(']');
            }
            Expr::Seq(items) if items.is_empty() => out.push_str("\"\""),
            Expr::Seq(items) => {
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    if matches!(item, Expr::Alt(_)) {
                        out.push('(');
                        item.render(out);
                        out.push(')');
                    } else {
                        item.render(out);
                    }
                }
            }
            Expr::Alt(items) if items.is_empty() => {
                unreachable!("an empty choice matches nothing")
            }
            Expr::Alt(items) => {
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(" | ");
                    }
                    item.render(out);
                }
            }
            Expr::Repeat { expr, min, max } => {
                expr.render_atom(out);
                match (min, max) {
                    (0, None) => out.push('*'),
                    (1, None) => out.push('+'),
                    (0, Some(1)) => out.push('?'),
                    (min, None) => write!(out, "{{{min},}}").unwrap(),
                    (min, Some(max)) if min == max => write!(out, "{{{min}}}").unwrap(),
                    (min, Some(max)) => write!(out, "{{{min},{max}}}").unwrap(),
                }
            }
            Expr::Rule(name) => out.push_str(name),
        }
    }

    /// Writes the expression so that a repetition operator can follow it.
    fn render_atom(&self, out: &mut String) {
        let atom = match self {
            Expr::Class { .. } | Expr::Rule(_) => true,
            Expr::Literal(text) => text.chars().count() == 1,
            _ => false,
        };
        if atom {
            self.render(out);
        } else {
            out.push('(');
            self.render(out);
            out.push(')');
        }
    }
}

/// Characters that GBNF needs escaped inside literals and classes.
fn push_char(out: &mut String, c: char) {
    match c {
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        c if c.is_control() => write!(out, "\\x{:02X}", c as u32).unwrap(),
        c => out.push(c),
    }
}

fn push_class_char(out: &mut String, c: char) {
    match c {
        '\\' | ']' | '[' => {
            out.push('\\');
            out.push(c);
        }
        // Upstream llama.cpp rejects `\-` and `\^`; hex escapes work everywhere.
        '-' | '^' => write!(out, "\\x{:02X}", c as u32).unwrap(),
        _ => push_char(out, c),
    }
}

/// Named rules of a grammar. Rules may refer to each other.
#[derive(Debug, Default)]
pub struct Grammar {
    rules: Vec<(String, Expr)>,
    next_id: usize,
}

impl Grammar {
    pub fn new() -> Grammar {
        Grammar::default()
    }

    /// A rule name not used yet, built from `hint` (lowercase letters).
    fn fresh_name(&mut self, hint: &str) -> String {
        self.next_id += 1;
        format!("{hint}-{}", self.next_id)
    }

    /// Adds a rule for `expr` and returns a reference to it.
    pub fn rule(&mut self, hint: &str, expr: Expr) -> Expr {
        let name = self.fresh_name(hint);
        self.rules.push((name.clone(), expr));
        Expr::Rule(name)
    }

    /// Any text that does not contain `forbidden`, with at least one
    /// character when `non_empty` (D20, D73). The rules follow the
    /// Knuth-Morris-Pratt automaton of `forbidden`: state `s` means the text
    /// ends with the first `s` characters of `forbidden`.
    pub fn text_without(&mut self, hint: &str, forbidden: &str, non_empty: bool) -> Expr {
        let pattern: Vec<char> = forbidden.chars().collect();
        assert!(!pattern.is_empty(), "nothing to forbid");
        let fail = failure_function(&pattern);
        let step = |mut state: usize, c: char| {
            while state > 0 && pattern[state] != c {
                state = fail[state - 1];
            }
            if pattern[state] == c { state + 1 } else { 0 }
        };
        let mut alphabet = pattern.clone();
        alphabet.sort_unstable();
        alphabet.dedup();
        let other = Expr::Class {
            negated: true,
            ranges: alphabet.iter().map(|&c| (c, c)).collect(),
        };

        let base = self.fresh_name(hint);
        let state_name = |state: usize| format!("{base}-{state}");
        let transitions = |state: usize| {
            let mut alts = vec![Expr::Seq(vec![other.clone(), Expr::Rule(state_name(0))])];
            for &c in &alphabet {
                let next = step(state, c);
                if next < pattern.len() {
                    alts.push(Expr::Seq(vec![
                        Expr::Literal(c.into()),
                        Expr::Rule(state_name(next)),
                    ]));
                }
            }
            Expr::Alt(alts)
        };
        for state in 0..pattern.len() {
            let optional = Expr::Repeat {
                expr: Box::new(transitions(state)),
                min: 0,
                max: Some(1),
            };
            self.rules.push((state_name(state), optional));
        }
        if non_empty {
            transitions(0)
        } else {
            Expr::Rule(state_name(0))
        }
    }

    /// The grammar text with `root` as its start rule.
    pub fn render(&self, root: &Expr) -> String {
        let mut out = String::from("root ::= ");
        root.render(&mut out);
        out.push('\n');
        for (name, expr) in &self.rules {
            out.push_str(name);
            out.push_str(" ::= ");
            expr.render(&mut out);
            out.push('\n');
        }
        out
    }
}

/// `fail[i]`: length of the longest proper prefix of `pattern[..=i]` that
/// is also its suffix.
fn failure_function(pattern: &[char]) -> Vec<usize> {
    let mut fail = vec![0; pattern.len()];
    let mut k = 0;
    for i in 1..pattern.len() {
        while k > 0 && pattern[i] != pattern[k] {
            k = fail[k - 1];
        }
        if pattern[i] == pattern[k] {
            k += 1;
        }
        fail[i] = k;
    }
    fail
}

/// Integers from `min` to `max` in decimal, without leading zeros or `-0`
/// (D70). `None` when `min > max`.
pub fn int_range(min: i64, max: i64) -> Option<Expr> {
    if min > max {
        return None;
    }
    let mut alts = Vec::new();
    if min < 0 {
        let upper = max.min(-1);
        let magnitudes = uint_range(upper.unsigned_abs(), min.unsigned_abs());
        alts.push(Expr::Seq(vec![Expr::literal("-"), magnitudes]));
    }
    if max >= 0 {
        alts.push(uint_range(min.max(0) as u64, max as u64));
    }
    Some(if alts.len() == 1 {
        alts.pop().unwrap()
    } else {
        Expr::Alt(alts)
    })
}

/// Unsigned integers from `lo` to `hi`, `lo <= hi`.
fn uint_range(lo: u64, hi: u64) -> Expr {
    let (lo_digits, hi_digits) = (lo.to_string(), hi.to_string());
    let mut alts = Vec::new();
    for len in lo_digits.len()..=hi_digits.len() {
        let from = if len == lo_digits.len() {
            lo_digits.clone()
        } else {
            format!("1{}", "0".repeat(len - 1))
        };
        let to = if len == hi_digits.len() {
            hi_digits.clone()
        } else {
            "9".repeat(len)
        };
        alts.push(same_length_range(from.as_bytes(), to.as_bytes()));
    }
    if alts.len() == 1 {
        alts.pop().unwrap()
    } else {
        Expr::Alt(alts)
    }
}

/// Digit strings from `lo` to `hi`, both of the same length.
fn same_length_range(lo: &[u8], hi: &[u8]) -> Expr {
    let digits = |lo: u8, hi: u8| Expr::Class {
        negated: false,
        ranges: vec![(lo as char, hi as char)],
    };
    let any_digits = |count: usize| match count {
        0 => Expr::Seq(Vec::new()),
        1 => digits(b'0', b'9'),
        n => Expr::Repeat {
            expr: Box::new(digits(b'0', b'9')),
            min: n as u32,
            max: Some(n as u32),
        },
    };
    let seq = |head: Expr, tail: Expr| match tail {
        Expr::Seq(items) if items.is_empty() => head,
        tail => Expr::Seq(vec![head, tail]),
    };
    let (first_lo, first_hi) = (lo[0], hi[0]);
    let (rest_lo, rest_hi) = (&lo[1..], &hi[1..]);
    if first_lo == first_hi {
        let head = Expr::Literal((first_lo as char).into());
        return if rest_lo.is_empty() {
            head
        } else {
            seq(head, same_length_range(rest_lo, rest_hi))
        };
    }
    let all = |digits: &[u8], d: u8| digits.iter().all(|&x| x == d);
    // `lo` rounded down and `hi` rounded up to whole blocks of the first digit.
    if all(rest_lo, b'0') && all(rest_hi, b'9') {
        return seq(digits(first_lo, first_hi), any_digits(rest_lo.len()));
    }
    let mut alts = Vec::new();
    let mut middle_lo = first_lo;
    if !all(rest_lo, b'0') {
        let nines = vec![b'9'; rest_lo.len()];
        alts.push(seq(
            Expr::Literal((first_lo as char).into()),
            same_length_range(rest_lo, &nines),
        ));
        middle_lo += 1;
    }
    let mut middle_hi = first_hi;
    let tail_hi = if all(rest_hi, b'9') {
        None
    } else {
        middle_hi -= 1;
        Some(first_hi)
    };
    if middle_lo <= middle_hi {
        alts.push(seq(digits(middle_lo, middle_hi), any_digits(rest_lo.len())));
    }
    if let Some(first) = tail_hi {
        let zeros = vec![b'0'; rest_hi.len()];
        alts.push(seq(
            Expr::Literal((first as char).into()),
            same_length_range(&zeros, rest_hi),
        ));
    }
    if alts.len() == 1 {
        alts.pop().unwrap()
    } else {
        Expr::Alt(alts)
    }
}

/// A test-only matcher: the oracle for grammar tests.
#[cfg(test)]
pub mod matcher {
    use super::*;
    use std::collections::BTreeSet;

    /// Whether all of `text` matches `expr` within `grammar`.
    pub fn matches(grammar: &Grammar, expr: &Expr, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        ends(grammar, expr, &chars, 0).contains(&chars.len())
    }

    /// Positions where a match of `expr` starting at `start` can end.
    fn ends(grammar: &Grammar, expr: &Expr, text: &[char], start: usize) -> BTreeSet<usize> {
        match expr {
            Expr::Literal(lit) => {
                let lit: Vec<char> = lit.chars().collect();
                let end = start + lit.len();
                if text.get(start..end) == Some(&lit[..]) {
                    BTreeSet::from([end])
                } else {
                    BTreeSet::new()
                }
            }
            Expr::Class { .. } => match text.get(start) {
                Some(&c) if class_matches(expr, c) => BTreeSet::from([start + 1]),
                _ => BTreeSet::new(),
            },
            Expr::Seq(items) => items.iter().fold(BTreeSet::from([start]), |starts, item| {
                starts
                    .into_iter()
                    .flat_map(|s| ends(grammar, item, text, s))
                    .collect()
            }),
            Expr::Alt(items) => items
                .iter()
                .flat_map(|item| ends(grammar, item, text, start))
                .collect(),
            Expr::Repeat { expr, min, max } => {
                let mut result = BTreeSet::new();
                let mut current = BTreeSet::from([start]);
                let mut count = 0;
                loop {
                    if count >= *min {
                        result.extend(current.iter().copied());
                    }
                    if max.is_some_and(|max| count >= max) || current.is_empty() {
                        break;
                    }
                    let next: BTreeSet<usize> = current
                        .iter()
                        .flat_map(|&s| ends(grammar, expr, text, s))
                        .collect();
                    // Without an upper limit, stop once no new end appears.
                    if max.is_none() && count >= *min && next.is_subset(&result) {
                        break;
                    }
                    current = next;
                    count += 1;
                }
                result
            }
            Expr::Rule(name) => {
                let (_, rule) = grammar
                    .rules
                    .iter()
                    .find(|(n, _)| n == name)
                    .expect("undefined rule");
                ends(grammar, rule, text, start)
            }
        }
    }

    fn class_matches(class: &Expr, c: char) -> bool {
        let Expr::Class { negated, ranges } = class else {
            unreachable!()
        };
        negated ^ ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi)
    }
}

#[cfg(test)]
mod tests {
    use super::matcher::matches;
    use super::*;

    #[test]
    fn literals_and_classes_are_escaped() {
        let expr = Expr::Seq(vec![
            Expr::literal("say \"hi\"\\\n\t\u{1}"),
            Expr::Class {
                negated: true,
                ranges: vec![
                    ('\\', '\\'),
                    (']', ']'),
                    ('-', '-'),
                    ('\n', '\n'),
                    ('a', 'z'),
                ],
            },
        ]);
        assert_eq!(
            Grammar::new().render(&expr),
            "root ::= \"say \\\"hi\\\"\\\\\\n\\t\\x01\" [^\\\\\\]\\x2D\\na-z]\n"
        );
    }

    #[test]
    fn repetitions_and_groups() {
        let digit = Expr::Class {
            negated: false,
            ranges: vec![('0', '9')],
        };
        let repeat = |min, max| Expr::Repeat {
            expr: Box::new(digit.clone()),
            min,
            max,
        };
        let expr = Expr::Seq(vec![
            repeat(0, None),
            repeat(1, None),
            repeat(0, Some(1)),
            repeat(2, None),
            repeat(3, Some(3)),
            repeat(1, Some(4)),
            Expr::Repeat {
                expr: Box::new(Expr::one_of(&["ab", "c"])),
                min: 0,
                max: None,
            },
        ]);
        assert_eq!(
            Grammar::new().render(&expr),
            "root ::= [0-9]* [0-9]+ [0-9]? [0-9]{2,} [0-9]{3} [0-9]{1,4} (\"ab\" | \"c\")*\n"
        );
    }

    #[test]
    fn rules_are_rendered_after_root() {
        let mut grammar = Grammar::new();
        let word = grammar.rule("word", Expr::literal("hi"));
        let root = Expr::Seq(vec![word.clone(), Expr::literal(" "), word]);
        assert_eq!(
            grammar.render(&root),
            "root ::= word-1 \" \" word-1\nword-1 ::= \"hi\"\n"
        );
    }

    #[test]
    fn one_of_matches_exactly_its_values() {
        let grammar = Grammar::new();
        let expr = Expr::one_of(&["src/main.rs", "Cargo.toml"]);
        assert!(matches(&grammar, &expr, "Cargo.toml"));
        assert!(!matches(&grammar, &expr, "Cargo.tom"));
        assert!(!matches(&grammar, &expr, "/etc/passwd"));
    }

    #[test]
    fn int_range_matches_exactly_the_range() {
        let grammar = Grammar::new();
        let bounds = [
            (0, 0),
            (1, 50),
            (1, 20),
            (0, 9),
            (7, 7),
            (5, 123),
            (10, 99),
            (99, 1000),
            (37, 412),
            (-10, 10),
            (-123, -45),
            (-9, -1),
            (-1000, 0),
            (-5, 250),
        ];
        for (min, max) in bounds {
            let expr = int_range(min, max).unwrap();
            for n in -1100..=1100 {
                assert_eq!(
                    matches(&grammar, &expr, &n.to_string()),
                    (min..=max).contains(&n),
                    "{n} in [{min}, {max}]"
                );
            }
            for text in ["", "-", "-0", "00", "007", "+5", "1.0"] {
                assert!(
                    !matches(&grammar, &expr, text),
                    "{text:?} in [{min}, {max}]"
                );
            }
        }
        assert!(int_range(2, 1).is_none());
    }

    #[test]
    fn int_range_handles_the_widest_bounds() {
        let grammar = Grammar::new();
        let expr = int_range(i64::MIN, i64::MAX).unwrap();
        for n in [i64::MIN, -1, 0, 1, i64::MAX] {
            assert!(matches(&grammar, &expr, &n.to_string()));
        }
        assert!(!matches(&grammar, &expr, "9223372036854775808"));
        assert!(!matches(&grammar, &expr, "-9223372036854775809"));
    }

    /// All strings over `alphabet` up to `max_len` characters.
    fn strings(alphabet: &[char], max_len: usize) -> Vec<String> {
        let mut all = vec![String::new()];
        let mut last = vec![String::new()];
        for _ in 0..max_len {
            last = last
                .iter()
                .flat_map(|s| alphabet.iter().map(move |&c| format!("{s}{c}")))
                .collect();
            all.extend(last.iter().cloned());
        }
        all
    }

    #[test]
    fn text_without_rejects_exactly_the_forbidden_string() {
        for forbidden in ["aab", "abab", "a", "\n</p>"] {
            for non_empty in [false, true] {
                let mut grammar = Grammar::new();
                let expr = grammar.text_without("text", forbidden, non_empty);
                let mut alphabet: Vec<char> = forbidden.chars().collect();
                alphabet.push('x');
                alphabet.sort_unstable();
                alphabet.dedup();
                for text in strings(&alphabet, 6) {
                    let expected = !text.contains(forbidden) && !(non_empty && text.is_empty());
                    assert_eq!(
                        matches(&grammar, &expr, &text),
                        expected,
                        "{text:?} without {forbidden:?}, non_empty={non_empty}"
                    );
                }
            }
        }
    }

    #[test]
    fn text_without_allows_code() {
        let mut grammar = Grammar::new();
        let expr = grammar.text_without("value", "\n</parameter>", false);
        let code = "fn f(v: Vec<&str>) -> Option<u8> {\n    None // </param\n}";
        assert!(matches(&grammar, &expr, code));
        assert!(!matches(&grammar, &expr, "ls\n</parameter>\nx"));
    }

    #[test]
    fn may_contain() {
        let mut grammar = Grammar::new();
        assert!(Expr::literal("a\nb").may_contain('\n'));
        assert!(!Expr::one_of(&["a", "b"]).may_contain('\n'));
        let not_semicolon = Expr::Class {
            negated: true,
            ranges: vec![(';', ';')],
        };
        assert!(not_semicolon.may_contain('\n'));
        let never = Expr::Repeat {
            expr: Box::new(Expr::literal("\n")),
            min: 0,
            max: Some(0),
        };
        assert!(!never.may_contain('\n'));
        assert!(grammar.text_without("t", "x", false).may_contain('\n'));
    }
}
