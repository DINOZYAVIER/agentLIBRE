//! Markdown for the terminal: headings, emphasis, code, lists, quotes,
//! links, rules and tables, wrapped to the window width (D123).

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use super::text::{clean, hard_wrap, width};
use crate::theme::{Role, Theme};

/// The narrowest column text is wrapped in.
const MIN_WIDTH: usize = 8;

/// Renders `source` as terminal lines of at most `columns` columns.
pub fn render(source: &str, columns: usize, theme: &Theme) -> Vec<String> {
    let mut renderer = Renderer {
        theme,
        columns: columns.max(MIN_WIDTH),
        lines: Vec::new(),
        inline: Vec::new(),
        roles: Vec::new(),
        quote: 0,
        lists: Vec::new(),
        items: Vec::new(),
        code: None,
        links: Vec::new(),
        table: None,
    };
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(source, options) {
        renderer.event(event);
    }
    renderer.flush();
    while renderer.lines.last().is_some_and(String::is_empty) {
        renderer.lines.pop();
    }
    renderer.lines
}

/// Words without a space between them, with their roles.
type Atom = Vec<(String, Option<Role>)>;

/// A piece of inline text.
enum Piece {
    Word(String, Option<Role>),
    Space,
    Break,
}

struct Item {
    marker: String,
    shown: bool,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<Cell>>,
    row: Vec<Cell>,
    header_rows: usize,
}

struct Cell {
    styled: String,
    plain: String,
}

struct Renderer<'t> {
    theme: &'t Theme,
    columns: usize,
    lines: Vec<String>,
    inline: Vec<Piece>,
    roles: Vec<Role>,
    quote: usize,
    /// The next number of each open ordered list; `None` for bullets.
    lists: Vec<Option<u64>>,
    items: Vec<Item>,
    code: Option<String>,
    links: Vec<String>,
    table: Option<Table>,
}

impl Renderer<'_> {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => self.text(&text),
            Event::Code(text) => {
                let text = clean(&text).replace('\n', " ");
                self.inline
                    .push(Piece::Word(text, Some(Role::MarkdownInlineCode)));
            }
            Event::SoftBreak => self.inline.push(Piece::Space),
            Event::HardBreak => self.inline.push(Piece::Break),
            Event::Rule => {
                self.flush();
                self.blank_before();
                let (first, _, used) = self.prefixes();
                let rule = "─".repeat((self.columns - used).min(40));
                self.lines.push(format!(
                    "{first}{}",
                    self.theme.paint(Role::MarkdownRule, &rule)
                ));
                self.mark_shown();
            }
            Event::TaskListMarker(done) => {
                let mark = if done { "[x]" } else { "[ ]" };
                self.inline.push(Piece::Word(mark.into(), None));
                self.inline.push(Piece::Space);
            }
            Event::FootnoteReference(_) | Event::InlineMath(_) | Event::DisplayMath(_) => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                self.flush();
                self.blank_before();
            }
            Tag::Heading { .. } => {
                self.flush();
                self.blank_before();
                self.roles.push(Role::MarkdownHeading);
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.blank_before();
                self.quote += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                self.blank_before();
                let _ = matches!(kind, CodeBlockKind::Fenced(_));
                self.code = Some(String::new());
            }
            Tag::List(first) => {
                self.flush();
                self.blank_before();
                self.lists.push(first);
            }
            Tag::Item => {
                self.flush();
                let marker = match self.lists.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}. ");
                        *number += 1;
                        marker
                    }
                    _ => "• ".to_owned(),
                };
                self.items.push(Item {
                    marker,
                    shown: false,
                });
            }
            Tag::Table(_) => {
                self.flush();
                self.blank_before();
                self.table = Some(Table::default());
            }
            Tag::TableHead | Tag::TableRow | Tag::TableCell => {}
            Tag::Emphasis => self.roles.push(Role::MarkdownEmphasis),
            Tag::Strong => self.roles.push(Role::MarkdownStrong),
            Tag::Strikethrough => {}
            Tag::Link { dest_url, .. } => {
                self.roles.push(Role::MarkdownLink);
                self.links.push(dest_url.into_string());
            }
            Tag::Image { .. } => {}
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Heading(_) => {
                self.flush();
                self.roles.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                if let Some(code) = self.code.take() {
                    self.code_block(&code);
                }
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush();
                self.items.pop();
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.table_block(table);
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                    table.header_rows = table.rows.len();
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::TableCell => self.cell(),
            TagEnd::Emphasis | TagEnd::Strong => {
                self.roles.pop();
            }
            TagEnd::Link => {
                self.roles.pop();
                let url = self.links.pop().unwrap_or_default();
                let same = matches!(self.inline.last(), Some(Piece::Word(text, _)) if *text == url);
                if !url.is_empty() && !same {
                    self.inline.push(Piece::Space);
                    self.inline
                        .push(Piece::Word(format!("({})", clean(&url)), Some(Role::Muted)));
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        if let Some(code) = &mut self.code {
            code.push_str(&clean(text));
            return;
        }
        let role = self.roles.last().copied();
        let text = clean(text);
        let mut words = text.split_whitespace().peekable();
        if text.starts_with(char::is_whitespace) {
            self.inline.push(Piece::Space);
        }
        while let Some(word) = words.next() {
            self.inline.push(Piece::Word(word.into(), role));
            if words.peek().is_some() {
                self.inline.push(Piece::Space);
            }
        }
        if text.ends_with(char::is_whitespace) && !text.trim().is_empty() {
            self.inline.push(Piece::Space);
        }
    }

    /// A blank line before a top-level block.
    fn blank_before(&mut self) {
        if self.items.is_empty() && self.lines.last().is_some_and(|line| !line.is_empty()) {
            self.lines.push(String::new());
        }
    }

    /// The prefix of the first and of the other lines of the block being
    /// written, and its width.
    fn prefixes(&self) -> (String, String, usize) {
        let quote = self
            .theme
            .paint(Role::MarkdownQuote, "│ ")
            .repeat(self.quote);
        let mut used = self.quote * 2;
        let mut indent = 0;
        let mut first_marker = None;
        for (index, item) in self.items.iter().enumerate() {
            let marker_width = width(&item.marker);
            if index + 1 == self.items.len() && !item.shown {
                first_marker = Some(item.marker.as_str());
            }
            indent += marker_width;
        }
        used += indent;
        let spaces = |n: usize| " ".repeat(n);
        let rest = format!("{quote}{}", spaces(indent));
        let first = match first_marker {
            Some(marker) => {
                let outer = indent - width(marker);
                format!(
                    "{quote}{}{}",
                    spaces(outer),
                    self.theme.paint(Role::MarkdownBullet, marker)
                )
            }
            None => rest.clone(),
        };
        (first, rest, used)
    }

    fn mark_shown(&mut self) {
        if let Some(item) = self.items.last_mut() {
            item.shown = true;
        }
    }

    /// Writes the inline text collected so far as wrapped lines.
    fn flush(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let pieces = std::mem::take(&mut self.inline);
        let (first, rest, used) = self.prefixes();
        let avail = self.columns.saturating_sub(used).max(MIN_WIDTH);
        for (index, line) in self.flow(pieces, avail).into_iter().enumerate() {
            let prefix = if index == 0 { &first } else { &rest };
            self.lines.push(format!("{prefix}{line}"));
        }
        self.mark_shown();
    }

    fn paint(&self, text: &str, role: Option<Role>) -> String {
        match role {
            Some(role) => self.theme.paint(role, text),
            None => text.to_owned(),
        }
    }

    /// Wraps `pieces` into lines of at most `avail` columns.
    fn flow(&self, pieces: Vec<Piece>, avail: usize) -> Vec<String> {
        // Words without a space between them stay together.
        let mut atoms: Vec<Option<Atom>> = Vec::new();
        let mut open = false;
        for piece in pieces {
            match piece {
                Piece::Word(text, role) => {
                    if open {
                        if let Some(Some(atom)) = atoms.last_mut() {
                            atom.push((text, role));
                        }
                    } else {
                        atoms.push(Some(vec![(text, role)]));
                        open = true;
                    }
                }
                Piece::Space => open = false,
                Piece::Break => {
                    atoms.push(None);
                    open = false;
                }
            }
        }
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut used = 0;
        for atom in atoms {
            let Some(atom) = atom else {
                lines.push(std::mem::take(&mut current));
                used = 0;
                continue;
            };
            let atom_width: usize = atom.iter().map(|(text, _)| width(text)).sum();
            let gap = usize::from(used > 0);
            if used > 0 && used + gap + atom_width > avail {
                lines.push(std::mem::take(&mut current));
                used = 0;
            }
            if atom_width > avail {
                // Longer than a line: break it by characters.
                for (text, role) in atom {
                    for chunk in hard_wrap(&text, avail.saturating_sub(used).max(1)) {
                        let w = width(&chunk);
                        if used + w > avail && used > 0 {
                            lines.push(std::mem::take(&mut current));
                            used = 0;
                        }
                        current.push_str(&self.paint(&chunk, role));
                        used += w;
                    }
                }
                continue;
            }
            if used > 0 {
                current.push(' ');
                used += 1;
            }
            for (text, role) in atom {
                current.push_str(&self.paint(&text, role));
            }
            used += atom_width;
        }
        if used > 0 || lines.is_empty() {
            lines.push(current);
        }
        lines
    }

    fn code_block(&mut self, code: &str) {
        let (first, rest, used) = self.prefixes();
        let avail = self.columns.saturating_sub(used + 2).max(1);
        let mut first_line = true;
        for line in code.trim_end_matches('\n').split('\n') {
            for row in hard_wrap(line, avail) {
                let prefix = if first_line { &first } else { &rest };
                first_line = false;
                self.lines.push(format!(
                    "{prefix}  {}",
                    self.theme.paint(Role::MarkdownCode, &row)
                ));
            }
        }
        self.mark_shown();
    }

    /// Ends a table cell: its inline text becomes one line.
    fn cell(&mut self) {
        let pieces = std::mem::take(&mut self.inline);
        let mut styled = String::new();
        let mut plain = String::new();
        let mut space = false;
        for piece in pieces {
            match piece {
                Piece::Word(text, role) => {
                    if space {
                        styled.push(' ');
                        plain.push(' ');
                        space = false;
                    }
                    styled.push_str(&self.paint(&text, role));
                    plain.push_str(&text);
                }
                Piece::Space | Piece::Break => space = !plain.is_empty(),
            }
        }
        if let Some(table) = &mut self.table {
            table.row.push(Cell { styled, plain });
        }
    }

    fn table_block(&mut self, table: Table) {
        let (first, rest, used) = self.prefixes();
        let avail = self.columns.saturating_sub(used).max(MIN_WIDTH);
        let count = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0; count];
        for row in &table.rows {
            for (column, cell) in row.iter().enumerate() {
                widths[column] = widths[column].max(width(&cell.plain));
            }
        }
        let total = widths.iter().sum::<usize>() + 3 * count.saturating_sub(1);
        let mut out: Vec<String> = Vec::new();
        if total <= avail {
            for (index, row) in table.rows.iter().enumerate() {
                let cells: Vec<String> = (0..count)
                    .map(|column| match row.get(column) {
                        Some(cell) => {
                            let pad = " ".repeat(widths[column] - width(&cell.plain));
                            let text = if index < table.header_rows {
                                self.theme.paint(Role::MarkdownStrong, &cell.plain)
                            } else {
                                cell.styled.clone()
                            };
                            format!("{text}{pad}")
                        }
                        None => " ".repeat(widths[column]),
                    })
                    .collect();
                let bar = self.theme.paint(Role::MarkdownRule, " │ ");
                out.push(cells.join(&bar).trim_end().to_owned());
                if index + 1 == table.header_rows {
                    let rule = widths
                        .iter()
                        .map(|w| "─".repeat(*w))
                        .collect::<Vec<_>>()
                        .join("─┼─");
                    out.push(self.theme.paint(Role::MarkdownRule, &rule));
                }
            }
        } else {
            // Too wide for a grid: one wrapped line per row.
            for row in &table.rows {
                let line = row
                    .iter()
                    .map(|c| c.plain.as_str())
                    .collect::<Vec<_>>()
                    .join(" │ ");
                out.extend(hard_wrap(&line, avail));
            }
        }
        for (index, line) in out.into_iter().enumerate() {
            let prefix = if index == 0 { &first } else { &rest };
            self.lines.push(format!("{prefix}{line}"));
        }
        self.mark_shown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(source: &str, columns: usize) -> Vec<String> {
        render(source, columns, &Theme::plain())
    }

    #[test]
    fn paragraphs_wrap_and_are_separated() {
        assert_eq!(
            plain("one two three four five\n\nsecond paragraph", 10),
            ["one two", "three four", "five", "", "second", "paragraph"]
        );
        // A soft break is a space, a hard break a line break.
        assert_eq!(plain("a\nb", 20), ["a b"]);
        assert_eq!(plain("a  \nb", 20), ["a", "b"]);
    }

    #[test]
    fn headings_emphasis_and_code_spans() {
        let theme = Theme::new(true, &[]);
        let lines = render("# Title\n\ntext **bold** and `code`", 40, &theme);
        assert_eq!(lines[0], theme.paint(Role::MarkdownHeading, "Title"));
        assert!(lines[2].contains(&theme.paint(Role::MarkdownStrong, "bold")));
        assert!(lines[2].contains(&theme.paint(Role::MarkdownInlineCode, "code")));
        // Punctuation stays with the word before it.
        assert_eq!(plain("**bold**, next", 40), ["bold, next"]);
    }

    #[test]
    fn lists() {
        assert_eq!(
            plain("- one\n- two\n  - inner\n- three", 40),
            ["• one", "• two", "  • inner", "• three"]
        );
        assert_eq!(plain("1. a\n2. b", 40), ["1. a", "2. b"]);
        // Wrapped item text lines up under the first word.
        assert_eq!(
            plain("- alpha beta gamma delta", 14),
            ["• alpha beta", "  gamma delta"]
        );
        assert_eq!(
            plain("- [x] done\n- [ ] todo", 40),
            ["• [x] done", "• [ ] todo"]
        );
    }

    #[test]
    fn quotes_and_rules() {
        assert_eq!(plain("> quoted text", 40), ["│ quoted text"]);
        assert_eq!(
            plain("a\n\n---\n\nb", 40),
            ["a", "", &"─".repeat(40), "", "b"]
        );
    }

    #[test]
    fn code_blocks_keep_lines() {
        assert_eq!(
            plain("```rust\nfn main() {\n    run();\n}\n```", 40),
            ["  fn main() {", "      run();", "  }"]
        );
        assert_eq!(plain("```\nabcdefghij\n```", 8), ["  abcdef", "  ghij"]);
    }

    #[test]
    fn links() {
        assert_eq!(
            plain("[docs](https://example.com/x)", 60),
            ["docs (https://example.com/x)"]
        );
        assert_eq!(plain("<https://example.com>", 60), ["https://example.com"]);
    }

    #[test]
    fn tables() {
        assert_eq!(
            plain("| a | bb |\n|---|----|\n| ccc | d |", 40),
            ["a   │ bb", "────┼───", "ccc │ d"]
        );
        // Too wide: one line per row.
        assert_eq!(
            plain("| aaaa | bbbb |\n|---|---|\n| cccc | dddd |", 8),
            ["aaaa │ b", "bbb", "cccc │ d", "ddd"]
        );
    }

    #[test]
    fn terminal_controls_are_removed() {
        assert_eq!(plain("a \x1b[31mred\x1b[0m", 40), ["a [31mred[0m"]);
        assert!(plain("", 40).is_empty());
    }

    #[test]
    fn long_words_are_broken() {
        assert_eq!(plain("abcdefghijkl", 8), ["abcdefgh", "ijkl"]);
    }
}
