//! The terminal: raw mode, key sequences and the window size (D115, D117,
//! D135).

use std::io::{self, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// What the keyboard produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    /// Enter: sends the message (D117).
    Enter,
    /// Shift-Enter, Alt-Enter or Ctrl-J: a new line in the message (D117).
    Newline,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    /// Ctrl-Left or Alt-Left.
    WordLeft,
    /// Ctrl-Right or Alt-Right.
    WordRight,
    Home,
    End,
    Tab,
    BackTab,
    Escape,
    /// Ctrl with a lowercase letter.
    Ctrl(char),
    /// Alt with a character other than Enter.
    Alt(char),
    /// Pasted text (D117).
    Paste(String),
}

const ESC: u8 = 0x1b;
const PASTE_END: &[u8] = b"\x1b[201~";

/// Turns terminal input bytes into keys. A lone Escape is only known
/// after a pause: [`Parser::timeout`] ends the wait.
#[derive(Default)]
pub struct Parser {
    pending: Vec<u8>,
    /// Inside bracketed paste; the text so far.
    paste: Option<Vec<u8>>,
}

impl Parser {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.pending.extend_from_slice(bytes);
        let mut keys = Vec::new();
        while self.step(&mut keys) {}
        keys
    }

    /// Ends the wait for the rest of an escape sequence: what is pending
    /// is a key of its own.
    pub fn timeout(&mut self) -> Vec<Key> {
        if self.paste.is_some() || self.pending.is_empty() {
            return Vec::new();
        }
        let mut keys = Vec::new();
        if self.pending == [ESC] {
            self.pending.clear();
            keys.push(Key::Escape);
        } else {
            // An unfinished sequence or character: drop it.
            self.pending.clear();
        }
        keys
    }

    /// Whether bytes are waiting for [`Parser::timeout`].
    pub fn waiting(&self) -> bool {
        self.paste.is_none() && !self.pending.is_empty()
    }

    /// Takes one key off `pending`; false when more bytes are needed.
    fn step(&mut self, keys: &mut Vec<Key>) -> bool {
        if let Some(text) = &mut self.paste {
            match find(&self.pending, PASTE_END) {
                Some(end) => {
                    text.extend_from_slice(&self.pending[..end]);
                    let text = String::from_utf8_lossy(text)
                        .replace("\r\n", "\n")
                        .replace('\r', "\n");
                    self.pending.drain(..end + PASTE_END.len());
                    self.paste = None;
                    keys.push(Key::Paste(text));
                    return true;
                }
                None => {
                    // Keep a possible start of the end marker.
                    let keep = (1..PASTE_END.len())
                        .rev()
                        .find(|n| self.pending.ends_with(&PASTE_END[..*n]))
                        .unwrap_or(0);
                    let take = self.pending.len() - keep;
                    text.extend_from_slice(&self.pending[..take]);
                    self.pending.drain(..take);
                    return false;
                }
            }
        }
        let Some(&first) = self.pending.first() else {
            return false;
        };
        if first == ESC {
            return self.escape(keys);
        }
        let (key, used) = match first {
            b'\r' => (Some(Key::Enter), 1),
            b'\n' => (Some(Key::Newline), 1),
            0x7f | 0x08 => (Some(Key::Backspace), 1),
            b'\t' => (Some(Key::Tab), 1),
            0x01..=0x1a => (Some(Key::Ctrl((b'a' + first - 1) as char)), 1),
            0x00 | 0x1c..=0x1f => (None, 1),
            _ => match decode_char(&self.pending) {
                Some((ch, used)) => (Some(Key::Char(ch)), used),
                None if utf8_incomplete(&self.pending) => return false,
                None => (None, 1),
            },
        };
        self.pending.drain(..used);
        keys.extend(key);
        true
    }

    fn escape(&mut self, keys: &mut Vec<Key>) -> bool {
        let pending = &self.pending;
        let Some(&second) = pending.get(1) else {
            return false;
        };
        match second {
            b'[' => {
                // CSI: parameters, then a final byte 0x40..=0x7e.
                let Some(end) = pending[2..]
                    .iter()
                    .position(|byte| (0x40..=0x7e).contains(byte))
                    .map(|n| n + 2)
                else {
                    return false;
                };
                let params = String::from_utf8_lossy(&pending[2..end]).into_owned();
                let final_byte = pending[end];
                self.pending.drain(..=end);
                if final_byte == b'~' && params == "200" {
                    self.paste = Some(Vec::new());
                } else {
                    keys.extend(csi_key(&params, final_byte));
                }
            }
            b'O' => {
                let Some(&last) = pending.get(2) else {
                    return false;
                };
                self.pending.drain(..3);
                keys.extend(match last {
                    b'A' => Some(Key::Up),
                    b'B' => Some(Key::Down),
                    b'C' => Some(Key::Right),
                    b'D' => Some(Key::Left),
                    b'H' => Some(Key::Home),
                    b'F' => Some(Key::End),
                    _ => None,
                });
            }
            b'\r' | b'\n' => {
                self.pending.drain(..2);
                keys.push(Key::Newline);
            }
            0x7f => {
                self.pending.drain(..2);
                keys.push(Key::Alt('\x7f'));
            }
            ESC => {
                self.pending.drain(..1);
                keys.push(Key::Escape);
            }
            _ => match decode_char(&pending[1..]) {
                Some((ch, used)) => {
                    self.pending.drain(..1 + used);
                    keys.push(Key::Alt(ch));
                }
                None if utf8_incomplete(&pending[1..]) => return false,
                None => {
                    self.pending.drain(..2);
                }
            },
        }
        true
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The first character of `bytes`, if complete.
fn decode_char(bytes: &[u8]) -> Option<(char, usize)> {
    let len = match bytes.first()? {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return None,
    };
    let text = std::str::from_utf8(bytes.get(..len)?).ok()?;
    text.chars().next().map(|ch| (ch, len))
}

/// Whether `bytes` start a UTF-8 character that is not complete yet.
fn utf8_incomplete(bytes: &[u8]) -> bool {
    let need = match bytes.first() {
        Some(0xc0..=0xdf) => 2,
        Some(0xe0..=0xef) => 3,
        Some(0xf0..=0xf7) => 4,
        _ => return false,
    };
    bytes.len() < need
}

/// Modifier bits of a CSI parameter (1 + mask).
const SHIFT: u32 = 1;
const ALT: u32 = 2;
const CTRL: u32 = 4;

fn csi_key(params: &str, final_byte: u8) -> Option<Key> {
    let mut fields = params.split(';');
    let first = fields.next().unwrap_or_default();
    let second = fields.next().unwrap_or_default();
    let number = |text: &str| text.split(':').next().and_then(|n| n.parse::<u32>().ok());
    let mask = number(second).map_or(0, |n| n.saturating_sub(1));
    // `:3` after the modifiers is a key release.
    if second.split(':').nth(1) == Some("3") {
        return None;
    }
    let word = mask & (ALT | CTRL) != 0;
    match final_byte {
        b'A' => Some(Key::Up),
        b'B' => Some(Key::Down),
        b'C' => Some(if word { Key::WordRight } else { Key::Right }),
        b'D' => Some(if word { Key::WordLeft } else { Key::Left }),
        b'H' => Some(Key::Home),
        b'F' => Some(Key::End),
        b'Z' => Some(Key::BackTab),
        b'~' => match number(first)? {
            1 | 7 => Some(Key::Home),
            3 => Some(Key::Delete),
            4 | 8 => Some(Key::End),
            _ => None,
        },
        b'u' => kitty_key(number(first)?, mask),
        _ => None,
    }
}

/// A key of the kitty keyboard protocol: `CSI code ; modifiers u`.
fn kitty_key(code: u32, mask: u32) -> Option<Key> {
    let shift = mask & SHIFT != 0;
    let alt = mask & ALT != 0;
    let ctrl = mask & CTRL != 0;
    Some(match code {
        13 | 57414 if shift || alt => Key::Newline,
        13 | 57414 if ctrl => Key::Newline,
        13 | 57414 => Key::Enter,
        27 => Key::Escape,
        9 if shift => Key::BackTab,
        9 => Key::Tab,
        127 => Key::Backspace,
        _ => {
            let ch = char::from_u32(code)?;
            if ch.is_control() || mask & !(SHIFT | ALT | CTRL) != 0 {
                return None;
            }
            if ctrl {
                Key::Ctrl(ch.to_ascii_lowercase())
            } else if alt {
                Key::Alt(ch)
            } else {
                Key::Char(ch)
            }
        }
    })
}

/// The original terminal settings, kept for [`restore`].
static ORIGINAL: Mutex<Option<libc::termios>> = Mutex::new(None);
static RESIZED: AtomicBool = AtomicBool::new(false);

/// What switching to the interactive mode turns on: bracketed paste, the
/// kitty keyboard protocol's "disambiguate" flag (D117).
const ENTER: &str = "\x1b[?2004h\x1b[>1u";
/// The reverse, plus a visible cursor and no scroll region leftovers.
const LEAVE: &str = "\x1b[<u\x1b[?2004l\x1b[?25h\x1b[0m";

/// Puts the terminal in raw mode for the line editor (D115). Output
/// post-processing stays on, so `\n` still returns the carriage.
pub fn enable_raw() -> io::Result<()> {
    let mut original: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut raw = original;
    unsafe { libc::cfmakeraw(&mut raw) };
    raw.c_oflag |= libc::OPOST | libc::ONLCR;
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
        return Err(io::Error::last_os_error());
    }
    *ORIGINAL.lock().unwrap_or_else(|e| e.into_inner()) = Some(original);
    install_resize_handler();
    let mut out = io::stdout();
    out.write_all(ENTER.as_bytes())?;
    out.flush()
}

/// Gives the terminal back; safe to call more than once and from any
/// thread before an exit (D135).
pub fn restore() {
    let original = ORIGINAL.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(original) = original {
        let mut out = io::stdout();
        let _ = out.write_all(LEAVE.as_bytes());
        let _ = out.flush();
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original) };
    }
}

/// Restores the terminal when dropped, also on a panic.
pub struct RawGuard;

impl RawGuard {
    pub fn new() -> io::Result<RawGuard> {
        enable_raw()?;
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            hook(info);
        }));
        Ok(RawGuard)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        restore();
    }
}

fn install_resize_handler() {
    extern "C" fn on_winch(_: libc::c_int) {
        RESIZED.store(true, Ordering::SeqCst);
    }
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_winch as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGWINCH, &action, std::ptr::null_mut());
    }
}

/// Whether the window was resized since the last call.
pub fn take_resized() -> bool {
    RESIZED.swap(false, Ordering::SeqCst)
}

/// Blocks SIGINT in the calling thread (and threads it spawns), so that
/// the wake-up signal of Ctrl-C reaches the thread running the turn.
pub fn block_sigint() {
    change_sigint_mask(libc::SIG_BLOCK);
}

pub fn unblock_sigint() {
    change_sigint_mask(libc::SIG_UNBLOCK);
}

fn change_sigint_mask(how: libc::c_int) {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::pthread_sigmask(how, &set, std::ptr::null_mut());
    }
}

/// Columns and rows of the terminal on stdout.
pub fn size() -> (usize, usize) {
    let mut winsize: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut winsize) } == 0;
    if ok && winsize.ws_col > 0 && winsize.ws_row > 0 {
        (usize::from(winsize.ws_col), usize::from(winsize.ws_row))
    } else {
        (80, 24)
    }
}

/// Whether stdin and stdout are terminals (D128).
pub fn is_interactive() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDOUT_FILENO) == 1 }
}

/// Reads input bytes, waiting at most `timeout_ms`: `Ok(None)` after a
/// timeout, `Ok(Some(0))` at the end of input.
pub fn read_input(buffer: &mut [u8], timeout_ms: i32) -> io::Result<Option<usize>> {
    let mut poll = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut poll, 1, timeout_ms) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(None)
        } else {
            Err(error)
        };
    }
    if ready == 0 {
        return Ok(None);
    }
    let n = unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
    if n < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(None)
        } else {
            Err(error)
        };
    }
    Ok(Some(n as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(input: &[u8]) -> Vec<Key> {
        let mut parser = Parser::default();
        let mut keys = parser.feed(input);
        keys.extend(parser.timeout());
        keys
    }

    #[test]
    fn plain_keys() {
        assert_eq!(
            keys(b"a\xd0\xb1\r\x7f\t"),
            [
                Key::Char('a'),
                Key::Char('б'),
                Key::Enter,
                Key::Backspace,
                Key::Tab
            ]
        );
        assert_eq!(
            keys(b"\x03\x04\x01\x17"),
            [
                Key::Ctrl('c'),
                Key::Ctrl('d'),
                Key::Ctrl('a'),
                Key::Ctrl('w')
            ]
        );
        assert_eq!(keys(b"\n"), [Key::Newline]);
    }

    #[test]
    fn arrows_and_editing_keys() {
        assert_eq!(
            keys(b"\x1b[A\x1b[B\x1b[C\x1b[D\x1b[H\x1b[F\x1b[3~"),
            [
                Key::Up,
                Key::Down,
                Key::Right,
                Key::Left,
                Key::Home,
                Key::End,
                Key::Delete
            ]
        );
        assert_eq!(keys(b"\x1bOA\x1bOH"), [Key::Up, Key::Home]);
        assert_eq!(keys(b"\x1b[1;5D\x1b[1;3C"), [Key::WordLeft, Key::WordRight]);
        assert_eq!(keys(b"\x1b[Z"), [Key::BackTab]);
    }

    #[test]
    fn escape_alt_and_the_pause() {
        assert_eq!(keys(b"\x1b"), [Key::Escape]);
        assert_eq!(keys(b"\x1b\r"), [Key::Newline]);
        assert_eq!(keys(b"\x1bb"), [Key::Alt('b')]);
        let mut parser = Parser::default();
        assert!(parser.feed(b"\x1b").is_empty());
        assert!(parser.waiting());
        assert!(parser.feed(b"[").is_empty());
        assert_eq!(parser.feed(b"A"), [Key::Up]);
        assert!(!parser.waiting());
    }

    #[test]
    fn kitty_protocol() {
        // Shift-Enter is a new line, Enter still sends (D117).
        assert_eq!(keys(b"\x1b[13;2u"), [Key::Newline]);
        assert_eq!(keys(b"\x1b[13u"), [Key::Enter]);
        assert_eq!(keys(b"\x1b[13;3u"), [Key::Newline]);
        assert_eq!(keys(b"\x1b[99;5u"), [Key::Ctrl('c')]);
        assert_eq!(keys(b"\x1b[100;5u"), [Key::Ctrl('d')]);
        assert_eq!(keys(b"\x1b[27u"), [Key::Escape]);
        assert_eq!(keys(b"\x1b[127u"), [Key::Backspace]);
        assert_eq!(keys(b"\x1b[97;3u"), [Key::Alt('a')]);
        assert_eq!(keys(b"\x1b[1;5D"), [Key::WordLeft]);
        // A release is ignored.
        assert_eq!(keys(b"\x1b[13;2:3u"), []);
    }

    #[test]
    fn bracketed_paste() {
        assert_eq!(
            keys(b"\x1b[200~line 1\r\nline 2\x1b[201~x"),
            [Key::Paste("line 1\nline 2".into()), Key::Char('x')]
        );
        // Split anywhere, also inside the markers.
        let mut parser = Parser::default();
        let mut all = Vec::new();
        for chunk in [&b"\x1b[20"[..], b"0~a", b"\x1b[2", b"01", b"~b"] {
            all.extend(parser.feed(chunk));
        }
        assert_eq!(all, [Key::Paste("a".into()), Key::Char('b')]);
        // Text with escape bytes inside is kept.
        assert_eq!(
            keys(b"\x1b[200~a\x1b[Ab\x1b[201~"),
            [Key::Paste("a\x1b[Ab".into())]
        );
    }

    #[test]
    fn utf8_split_over_reads() {
        let mut parser = Parser::default();
        assert!(parser.feed(&[0xd0]).is_empty());
        assert_eq!(parser.feed(&[0xb1]), [Key::Char('б')]);
        assert!(parser.feed(&[0xf0, 0x9f]).is_empty());
        assert_eq!(parser.feed(&[0x98, 0x80]), [Key::Char('😀')]);
    }

    #[test]
    fn unknown_sequences_are_dropped() {
        assert_eq!(keys(b"\x1b[5~\x1b[99;9;9zx"), [Key::Char('x')]);
        assert_eq!(keys(b"\x00x"), [Key::Char('x')]);
    }
}
