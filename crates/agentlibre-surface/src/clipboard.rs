//! What the human's clipboard holds for a message (AGL-014, D21; AGL-127,
//! D2): files copied in a file manager, a picture, or text, by `wl-paste`.

use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The image in the clipboard, as PNG (`wl-paste`); `None` when there is
/// none or no `wl-paste` (D21).
pub fn clipboard_image() -> Option<Vec<u8>> {
    let types = wl_paste(&["--list-types"])?;
    let types = String::from_utf8_lossy(&types);
    let ty = types
        .lines()
        .find(|line| line.trim() == "image/png")
        .or_else(|| types.lines().find(|line| line.trim().starts_with("image/")))?
        .trim()
        .to_owned();
    wl_paste(&["--type", &ty]).filter(|bytes| !bytes.is_empty())
}

/// The files copied in a file manager, from the clipboard's
/// `text/uri-list` (D21).
pub fn clipboard_files() -> Vec<PathBuf> {
    let Some(types) = wl_paste(&["--list-types"]) else {
        return Vec::new();
    };
    if !String::from_utf8_lossy(&types)
        .lines()
        .any(|line| line.trim() == "text/uri-list")
    {
        return Vec::new();
    }
    wl_paste(&["--type", "text/uri-list"])
        .map(|bytes| uri_list_paths(&String::from_utf8_lossy(&bytes)))
        .unwrap_or_default()
}

/// What Ctrl-V finds in the clipboard (AGL-014, D21).
#[derive(Clone, Debug, PartialEq)]
pub enum Clip {
    /// Files copied in a file manager.
    Files(Vec<PathBuf>),
    /// A picture, as PNG.
    Image(Vec<u8>),
    Text(String),
    Empty,
}

/// The clipboard: files first, then an image, then text.
pub fn clipboard() -> Clip {
    let files = clipboard_files();
    if !files.is_empty() {
        return Clip::Files(files);
    }
    if let Some(image) = clipboard_image() {
        return Clip::Image(image);
    }
    let Some(types) = wl_paste(&["--list-types"]) else {
        return Clip::Empty;
    };
    let has_text = String::from_utf8_lossy(&types)
        .lines()
        .any(|line| line.trim().starts_with("text/plain") || line.trim() == "UTF8_STRING");
    if !has_text {
        return Clip::Empty;
    }
    match wl_paste(&["--no-newline", "--type", "text/plain"]) {
        Some(bytes) if !bytes.is_empty() => Clip::Text(String::from_utf8_lossy(&bytes).into_owned()),
        _ => Clip::Empty,
    }
}

fn wl_paste(args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new(agentlibre_common::programs::program("wl-paste").ok()?)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// The local paths of a `text/uri-list`: the `file://` lines, decoded.
pub fn uri_list_paths(list: &str) -> Vec<PathBuf> {
    list.lines()
        .filter_map(|line| line.trim().strip_prefix("file://"))
        .map(|rest| {
            let rest = rest.strip_prefix("localhost").unwrap_or(rest);
            PathBuf::from(std::ffi::OsStr::from_bytes(&percent_decode(rest)))
        })
        .collect()
}

fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

fn hex(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|d| d as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uri_list_gives_local_paths() {
        let list = "# comment\nfile:///home/u/My%20Pics/a%C3%A9.png\r\nhttp://x/y\nfile://localhost/tmp/b.mp4\n";
        assert_eq!(
            uri_list_paths(list),
            [PathBuf::from("/home/u/My Pics/aé.png"), PathBuf::from("/tmp/b.mp4")]
        );
        assert!(uri_list_paths("").is_empty());
    }
}
