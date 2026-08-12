use std::fs;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::InferenceFailure;

const MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;
pub(super) const MAX_HTTP_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_STREAM_FRAME_BYTES: usize = 32 * 1024 * 1024;
const MAX_STREAM_WIRE_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENGINE_DIAGNOSTIC_BYTES: usize = 4 * 1024 * 1024;
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(super) struct HttpConnection {
    stream: UnixStream,
    buffer: Vec<u8>,
}

#[derive(Debug)]
pub(super) struct HttpResponse {
    pub(super) status: u16,
    pub(super) body: Vec<u8>,
}

impl HttpConnection {
    pub(super) fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            buffer: Vec::new(),
        }
    }

    pub(super) fn request(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<HttpResponse, InferenceFailure> {
        self.write_request(method, path, body, "application/json")?;
        self.read_response(None, None)
    }

    pub(super) fn request_with_control(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        content_type: &str,
        cancellation: &crate::InferenceCancellation,
        deadline: Option<Instant>,
    ) -> Result<HttpResponse, InferenceFailure> {
        self.write_request(method, path, body, content_type)?;
        self.read_response(Some(cancellation), deadline)
    }

    pub(super) fn write_request(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        content_type: &str,
    ) -> Result<(), InferenceFailure> {
        let body = body.unwrap_or_default();
        write!(
            self.stream,
            "{method} {path} HTTP/1.1\r\nHost: agentlibre.internal\r\nX-AGL-Protocol: 1\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
            body.len(),
        )
        .map_err(protocol_io)?;
        self.stream.write_all(body).map_err(protocol_io)?;
        self.stream.flush().map_err(protocol_io)?;
        Ok(())
    }

    fn read_response(
        &mut self,
        cancellation: Option<&crate::InferenceCancellation>,
        deadline: Option<Instant>,
    ) -> Result<HttpResponse, InferenceFailure> {
        let header_end = loop {
            if let Some(position) = self
                .buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
            {
                break position + 4;
            }
            if self.buffer.len() >= MAX_HTTP_HEADER_BYTES {
                return Err(protocol("HTTP response headers exceed 64 KiB"));
            }
            let mut chunk = [0_u8; 8192];
            wait_readable_controlled(&self.stream, cancellation, deadline)?;
            let read = self.stream.read(&mut chunk).map_err(protocol_io)?;
            if read == 0 {
                return Err(protocol(
                    "engine closed the private connection before a response",
                ));
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        };
        let header = std::str::from_utf8(&self.buffer[..header_end])
            .map_err(|_| protocol("HTTP response headers are not UTF-8"))?;
        let mut lines = header.split("\r\n");
        let status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u16>().ok())
            .ok_or_else(|| protocol("invalid HTTP response status"))?;
        let mut content_length = None;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.eq_ignore_ascii_case("content-length") {
                if content_length.is_some() {
                    return Err(protocol("duplicate Content-Length header"));
                }
                content_length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| protocol("invalid Content-Length header"))?,
                );
            }
            if name.eq_ignore_ascii_case("transfer-encoding") {
                return Err(protocol("chunked private responses are unsupported"));
            }
        }
        let content_length =
            content_length.ok_or_else(|| protocol("missing Content-Length header"))?;
        if content_length > MAX_HTTP_RESPONSE_BYTES {
            return Err(protocol("HTTP response body exceeds 16 MiB"));
        }
        let required = header_end
            .checked_add(content_length)
            .ok_or_else(|| protocol("HTTP response length overflow"))?;
        while self.buffer.len() < required {
            let mut chunk = [0_u8; 8192];
            wait_readable_controlled(&self.stream, cancellation, deadline)?;
            let read = self.stream.read(&mut chunk).map_err(protocol_io)?;
            if read == 0 {
                return Err(protocol("partial HTTP response body"));
            }
            self.buffer.extend_from_slice(&chunk[..read]);
            if self.buffer.len() > required + MAX_HTTP_HEADER_BYTES {
                return Err(protocol("HTTP response framing exceeded its bound"));
            }
        }
        let body = self.buffer[header_end..required].to_vec();
        self.buffer.drain(..required);
        Ok(HttpResponse { status, body })
    }

    pub(super) fn read_generation_response(
        &mut self,
        cancellation: Option<&crate::InferenceCancellation>,
        deadline: Option<Instant>,
        mut on_frame: impl FnMut(&[u8]) -> Result<(), InferenceFailure>,
    ) -> Result<HttpResponse, InferenceFailure> {
        let header_end = loop {
            if let Some(position) = self
                .buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
            {
                break position + 4;
            }
            if self.buffer.len() >= MAX_HTTP_HEADER_BYTES {
                return Err(protocol("HTTP response headers exceed 64 KiB"));
            }
            let mut chunk = [0_u8; 8192];
            wait_readable_controlled(&self.stream, cancellation, deadline)?;
            let read = self.stream.read(&mut chunk).map_err(protocol_io)?;
            if read == 0 {
                return Err(protocol(
                    "engine closed the private connection before a response",
                ));
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        };
        let header = std::str::from_utf8(&self.buffer[..header_end])
            .map_err(|_| protocol("HTTP response headers are not UTF-8"))?;
        let mut lines = header.split("\r\n");
        let status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u16>().ok())
            .ok_or_else(|| protocol("invalid HTTP response status"))?;
        let mut content_length = None;
        let mut chunked = false;
        let mut content_type = None;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                if content_length.is_some() {
                    return Err(protocol("duplicate Content-Length header"));
                }
                content_length = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| protocol("invalid Content-Length header"))?,
                );
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                if chunked || !value.eq_ignore_ascii_case("chunked") {
                    return Err(protocol("invalid private Transfer-Encoding header"));
                }
                chunked = true;
            } else if name.eq_ignore_ascii_case("content-type")
                && content_type.replace(value.to_owned()).is_some()
            {
                return Err(protocol("duplicate Content-Type header"));
            }
        }
        self.buffer.drain(..header_end);

        if !chunked {
            let content_length =
                content_length.ok_or_else(|| protocol("missing Content-Length header"))?;
            if content_length > MAX_HTTP_RESPONSE_BYTES {
                return Err(protocol("HTTP response body exceeds 16 MiB"));
            }
            while self.buffer.len() < content_length {
                let mut chunk = [0_u8; 8192];
                wait_readable_controlled(&self.stream, cancellation, deadline)?;
                let read = self.stream.read(&mut chunk).map_err(protocol_io)?;
                if read == 0 {
                    return Err(protocol("partial HTTP response body"));
                }
                self.buffer.extend_from_slice(&chunk[..read]);
            }
            let body = self.buffer[..content_length].to_vec();
            self.buffer.drain(..content_length);
            return Ok(HttpResponse { status, body });
        }
        if content_length.is_some() {
            return Err(protocol(
                "private response cannot combine Content-Length and chunked encoding",
            ));
        }
        if status != 200 || content_type.as_deref() != Some("application/x-ndjson") {
            return Err(protocol(
                "chunked generation response has an invalid status or content type",
            ));
        }

        let mut wire_bytes = 0_usize;
        let mut line_buffer = Vec::new();
        'chunks: loop {
            let chunk_header_end = loop {
                if let Some(position) = self.buffer.windows(2).position(|window| window == b"\r\n")
                {
                    break position;
                }
                if self.buffer.len() > 64 {
                    return Err(protocol("chunk-size line exceeds its bound"));
                }
                let mut bytes = [0_u8; 8192];
                wait_readable_controlled(&self.stream, cancellation, deadline)?;
                let read = self.stream.read(&mut bytes).map_err(protocol_io)?;
                if read == 0 {
                    if self.buffer.is_empty() && line_buffer.is_empty() {
                        // Some cpp-httplib builds close a completed chunked
                        // provider without flushing the redundant HTTP zero
                        // chunk. The generation layer still requires and
                        // validates one typed final frame before accepting the
                        // response, so a pre-terminal close remains fatal.
                        break 'chunks;
                    }
                    return Err(protocol("partial chunk-size line"));
                }
                self.buffer.extend_from_slice(&bytes[..read]);
            };
            let size_text = std::str::from_utf8(&self.buffer[..chunk_header_end])
                .map_err(|_| protocol("chunk size is not ASCII"))?;
            if size_text.is_empty() || size_text.contains(';') {
                return Err(protocol("chunk extensions and empty sizes are unavailable"));
            }
            let chunk_size =
                usize::from_str_radix(size_text, 16).map_err(|_| protocol("invalid chunk size"))?;
            self.buffer.drain(..chunk_header_end + 2);
            if chunk_size == 0 {
                while self.buffer.len() < 2 {
                    let mut bytes = [0_u8; 2];
                    wait_readable_controlled(&self.stream, cancellation, deadline)?;
                    let read = self.stream.read(&mut bytes).map_err(protocol_io)?;
                    if read == 0 {
                        return Err(protocol("partial final chunk"));
                    }
                    self.buffer.extend_from_slice(&bytes[..read]);
                }
                if &self.buffer[..2] != b"\r\n" {
                    return Err(protocol("private chunked response contains trailers"));
                }
                self.buffer.drain(..2);
                break;
            }
            if chunk_size > MAX_STREAM_FRAME_BYTES {
                return Err(protocol("HTTP stream chunk exceeds 32 MiB"));
            }
            let required = chunk_size
                .checked_add(2)
                .ok_or_else(|| protocol("chunk length overflow"))?;
            while self.buffer.len() < required {
                let mut bytes = [0_u8; 8192];
                wait_readable_controlled(&self.stream, cancellation, deadline)?;
                let read = self.stream.read(&mut bytes).map_err(protocol_io)?;
                if read == 0 {
                    return Err(protocol("partial HTTP stream chunk"));
                }
                self.buffer.extend_from_slice(&bytes[..read]);
            }
            if &self.buffer[chunk_size..required] != b"\r\n" {
                return Err(protocol("HTTP stream chunk omitted its terminator"));
            }
            wire_bytes = wire_bytes
                .checked_add(chunk_size)
                .ok_or_else(|| protocol("generation stream byte count overflow"))?;
            if wire_bytes > MAX_STREAM_WIRE_BYTES {
                return Err(protocol("generation stream exceeds 64 MiB"));
            }
            line_buffer.extend_from_slice(&self.buffer[..chunk_size]);
            self.buffer.drain(..required);
            while let Some(newline) = line_buffer.iter().position(|byte| *byte == b'\n') {
                let mut frame = line_buffer.drain(..=newline).collect::<Vec<_>>();
                frame.pop();
                if frame.last() == Some(&b'\r') {
                    frame.pop();
                }
                if frame.is_empty() || frame.len() > MAX_STREAM_FRAME_BYTES {
                    return Err(protocol("generation stream frame size is invalid"));
                }
                on_frame(&frame)?;
            }
            if line_buffer.len() > MAX_STREAM_FRAME_BYTES {
                return Err(protocol("generation stream frame exceeds 32 MiB"));
            }
        }
        if !line_buffer.is_empty() {
            return Err(protocol("generation stream ended with a partial frame"));
        }
        Ok(HttpResponse {
            status,
            body: Vec::new(),
        })
    }
}

fn wait_readable_controlled(
    stream: &UnixStream,
    cancellation: Option<&crate::InferenceCancellation>,
    deadline: Option<Instant>,
) -> Result<(), InferenceFailure> {
    loop {
        if cancellation.is_some_and(crate::InferenceCancellation::is_cancelled) {
            return Err(InferenceFailure::Cancelled);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(InferenceFailure::DeadlineExceeded);
        }
        let timeout = if cancellation.is_some() || deadline.is_some() {
            Duration::from_millis(50)
        } else {
            Duration::from_secs(600)
        };
        if poll_readable(stream, timeout)? {
            return Ok(());
        }
        if cancellation.is_none() && deadline.is_none() {
            return Err(protocol("private engine response timed out"));
        }
    }
}

pub(super) fn create_private_directory() -> Result<PathBuf, InferenceFailure> {
    for _ in 0..32 {
        let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("agl-inference-{}-{id}", std::process::id()));
        let mut builder = fs::DirBuilder::new();
        match builder.mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(protocol_io(error)),
        }
    }
    Err(protocol(
        "could not allocate a unique private engine directory",
    ))
}

pub(super) fn duplicate(source: RawFd, target: RawFd) -> std::io::Result<()> {
    if source != target && unsafe { libc::dup2(source, target) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    clear_cloexec(target)
}

pub(super) fn mark_cloexec_range(first: RawFd, last: RawFd) -> std::io::Result<()> {
    if first > last {
        return Ok(());
    }
    let result = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            first as libc::c_uint,
            last as libc::c_uint,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn poll_readable(stream: &UnixStream, timeout: Duration) -> Result<bool, InferenceFailure> {
    let millis: i32 = timeout.as_millis().try_into().unwrap_or(i32::MAX);
    let mut descriptor = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut descriptor, 1, millis) };
    if result < 0 {
        Err(protocol_io(std::io::Error::last_os_error()))
    } else if result == 0 {
        Ok(false)
    } else {
        Ok(true)
    }
}

fn clear_cloexec(fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(super) fn protocol_io(error: std::io::Error) -> InferenceFailure {
    protocol(&error.to_string())
}

pub(super) fn protocol_io_context(context: &str, error: std::io::Error) -> InferenceFailure {
    protocol(&format!("{context}: {error}"))
}

pub(super) fn protocol(reason: &str) -> InferenceFailure {
    InferenceFailure::EngineProtocol {
        reason: reason.to_owned(),
    }
}

pub(super) fn bounded_body(body: &[u8]) -> String {
    String::from_utf8_lossy(&body[..body.len().min(4096)]).into_owned()
}

pub(super) fn bounded_json(value: &Value) -> String {
    let encoded = serde_json::to_vec(value).unwrap_or_else(|_| b"<invalid-json>".to_vec());
    bounded_body(&encoded)
}

pub(super) fn drain_diagnostics(mut stderr: std::process::ChildStderr, sink: &Mutex<Vec<u8>>) {
    let mut buffer = [0_u8; 8192];
    while let Ok(read) = stderr.read(&mut buffer) {
        if read == 0 {
            break;
        }
        let mut collected = sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remaining = MAX_ENGINE_DIAGNOSTIC_BYTES.saturating_sub(collected.len());
        collected.extend_from_slice(&buffer[..read.min(remaining)]);
    }
}
