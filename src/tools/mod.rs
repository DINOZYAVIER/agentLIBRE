//! The built-in tools: `bash` and `edit` (D16, D24-D28, D100-D103).

use std::fs;
use std::io::{self, Read};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::action::{Outcome, Value};
use crate::interrupt::{self, signal_group};
use crate::session::INTERRUPTED;

/// How much of a result the model sees (D28, D103).
const LIMIT: usize = 16 * 1024;

/// `timeout` when the call does not give one (D26).
const DEFAULT_TIMEOUT: u64 = 60;

/// How long a process group gets between SIGTERM and SIGKILL (D103).
const GRACE: Duration = Duration::from_secs(2);

/// What a tool call returns to the model.
#[derive(Debug, PartialEq, Eq)]
pub struct ToolResult {
    pub outcome: Outcome,
    pub content: String,
}

/// Runs the call of `tool` with `args` in `workspace`. Parsing has
/// checked the arguments against the tool (task-03).
pub fn run(workspace: &Path, tool: &str, args: &[(String, Value)]) -> ToolResult {
    let text = |name: &str| {
        args.iter()
            .find_map(|(n, value)| match value {
                Value::Text(text) if n == name => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or_default()
    };
    match tool {
        "bash" => {
            let timeout = args
                .iter()
                .find_map(|(n, value)| match value {
                    Value::Int(seconds) if n == "timeout" => Some(*seconds as u64),
                    _ => None,
                })
                .unwrap_or(DEFAULT_TIMEOUT);
            bash(workspace, text("command"), Duration::from_secs(timeout))
        }
        "edit" => edit(workspace, text("path"), text("old"), text("new")),
        _ => unreachable!("unknown tool {tool}"),
    }
}

/// The last [`LIMIT`] bytes of a stream and how many came before them.
#[derive(Default)]
struct Tail {
    bytes: Vec<u8>,
    dropped: usize,
}

impl Tail {
    fn push(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
        if self.bytes.len() > LIMIT {
            let excess = self.bytes.len() - LIMIT;
            self.bytes.drain(..excess);
            self.dropped += excess;
        }
    }

    /// The text with the truncation mark (D100). A character cut in half
    /// at the start counts as omitted.
    fn text(&self) -> String {
        let mut start = 0;
        if self.dropped > 0 {
            while start < self.bytes.len() && (self.bytes[start] & 0b1100_0000) == 0b1000_0000 {
                start += 1;
            }
        }
        let body = String::from_utf8_lossy(&self.bytes[start..]);
        match self.dropped + start {
            0 => body.into_owned(),
            omitted => format!("[output truncated: first {omitted} bytes omitted]\n{body}"),
        }
    }
}

/// Runs `bash -c command` in `workspace` and waits for it (D26, D100,
/// D102, D103).
pub fn bash(workspace: &Path, command: &str, timeout: Duration) -> ToolResult {
    let fail = |error: io::Error| ToolResult {
        outcome: Outcome::Fail,
        content: format!("Cannot run bash: {error}"),
    };
    // One pipe for stdout and stderr keeps their order.
    let (mut reader, writer) = match io::pipe() {
        Ok(pipe) => pipe,
        Err(error) => return fail(error),
    };
    let writer_err = match writer.try_clone() {
        Ok(writer) => writer,
        Err(error) => return fail(error),
    };
    let mut child = {
        let mut command_line = Command::new("bash");
        command_line
            .arg("-c")
            .arg(command)
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(writer)
            .stderr(writer_err)
            .process_group(0);
        // Dropping `command_line` closes this process's ends of the pipe.
        match command_line.spawn() {
            Ok(child) => child,
            Err(error) => return fail(error),
        }
    };

    let tail = Arc::new(Mutex::new(Tail::default()));
    let reading = {
        let tail = Arc::clone(&tail);
        thread::spawn(move || {
            let mut buffer = [0; 8192];
            while let Ok(n) = reader.read(&mut buffer)
                && n > 0
            {
                tail.lock().unwrap().push(&buffer[..n]);
            }
        })
    };

    let group = child.id();
    let status = wait_until(&mut child, Instant::now() + timeout);
    // Ctrl-C stops the command like a timeout (D111).
    let interrupted = status.is_none() && interrupt::interrupted();
    let timed_out = status.is_none() && !interrupted;
    // End what the command left running, gently first (D102, D103).
    signal_group(group, libc::SIGTERM);
    let status = status.or_else(|| wait_until(&mut child, Instant::now() + GRACE));
    let deadline = Instant::now() + GRACE;
    while !reading.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if !reading.is_finished() || status.is_none() {
        signal_group(group, libc::SIGKILL);
    }
    let status = status.or_else(|| child.wait().ok());
    // A process that left the group may still hold the pipe; do not wait
    // for it.
    let deadline = Instant::now() + GRACE;
    while !reading.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }

    if interrupted {
        return ToolResult {
            outcome: Outcome::Fail,
            content: INTERRUPTED.into(),
        };
    }
    let mut content = tail.lock().unwrap().text();
    let mut footer = Vec::new();
    let outcome = if timed_out {
        footer.push(format!("timed out after {} s", timeout.as_secs()));
        Outcome::Timeout
    } else {
        match status.map(|status| (status.code(), status.signal())) {
            Some((Some(0), _)) => Outcome::Ok,
            Some((Some(code), _)) => {
                footer.push(format!("exit code: {code}"));
                Outcome::Fail
            }
            Some((None, Some(signal))) => {
                footer.push(format!("killed by signal {signal}"));
                Outcome::Fail
            }
            _ => Outcome::Fail,
        }
    };
    if content.is_empty() && footer.is_empty() {
        content.push_str("(no output)");
    }
    for line in footer {
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&line);
    }
    ToolResult { outcome, content }
}

/// Waits for `child` until `deadline` or Ctrl-C; `None` if it is still
/// running.
fn wait_until(child: &mut Child, deadline: Instant) -> Option<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline && !interrupt::interrupted() => {
                thread::sleep(Duration::from_millis(10))
            }
            _ => return None,
        }
    }
}

/// Replaces the only occurrence of `old` in the file at `path` with `new`
/// (D25, D27, D101, D103).
pub fn edit(workspace: &Path, path: &str, old: &str, new: &str) -> ToolResult {
    let fail = |content: String| ToolResult {
        outcome: Outcome::Fail,
        content,
    };
    let file = workspace.join(path);
    let bytes = match fs::read(&file) {
        Ok(bytes) => bytes,
        Err(error) => return fail(format!("Cannot read {path}: {error}")),
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return fail(format!("{path} is not UTF-8 text."));
    };
    // Overlapping occurrences count too: `aa` in `aaa` is ambiguous.
    let starts: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain([text.len()])
        .filter(|&i| text[i..].starts_with(old))
        .collect();
    match starts.as_slice() {
        [] => fail(format!("`old` was not found in {path}.")),
        [start] => {
            let mut edited = String::with_capacity(text.len() + new.len());
            edited.push_str(&text[..*start]);
            edited.push_str(new);
            edited.push_str(&text[start + old.len()..]);
            match fs::write(&file, edited) {
                Ok(()) => ToolResult {
                    outcome: Outcome::Ok,
                    content: format!("Replaced 1 occurrence in {path}."),
                },
                Err(error) => fail(format!("Cannot write {path}: {error}")),
            }
        }
        starts => {
            let mut lines: Vec<String> = starts
                .iter()
                .take(10)
                .map(|&i| (text[..i].matches('\n').count() + 1).to_string())
                .collect();
            if starts.len() > 10 {
                lines.push("…".into());
            }
            fail(format!(
                "`old` appears {} times in {path} (lines {}); add context to make it unique.",
                starts.len(),
                lines.join(", ")
            ))
        }
    }
}

#[cfg(test)]
mod tests;
