use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::ffi;

pub const HEADER_LIMIT: usize = 8 * 1024;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum RequestError {
    Timeout,
    BadRequest,
    MethodNotAllowed,
    Io(io::Error),
}

#[derive(Debug, Clone)]
pub struct Request {
    pub target: Vec<u8>,
    pub accept_gzip: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusCode {
    Ok = 200,
    BadRequest = 400,
    Forbidden = 403,
    NotAllowed = 405,
    InternalServerError = 500,
}

impl StatusCode {
    pub fn code(self) -> u16 {
        self as u16
    }

    pub fn reason(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::BadRequest => "Bad Request",
            Self::Forbidden => "Forbidden",
            Self::NotAllowed => "Method Not Allowed",
            Self::InternalServerError => "Internal Server Error",
        }
    }
}

pub fn read_request(stream: &mut std::net::TcpStream) -> Result<Request, RequestError> {
    let mut buf = [0u8; HEADER_LIMIT];
    let mut len = 0usize;

    loop {
        if find_header_end(&buf[..len]).is_some() {
            break;
        }
        if len == buf.len() {
            return Err(RequestError::BadRequest);
        }
        match stream.read(&mut buf[len..]) {
            Ok(0) => return Err(RequestError::BadRequest),
            Ok(n) => len += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::TimedOut => return Err(RequestError::Timeout),
            Err(e) => return Err(RequestError::Io(e)),
        }
    }

    let header_end = find_header_end(&buf[..len]).ok_or(RequestError::BadRequest)?;
    let headers = &buf[..header_end];
    parse_request(headers)
}

fn parse_request(headers: &[u8]) -> Result<Request, RequestError> {
    let mut lines = headers.split(|b| *b == b'\n');
    let request_line = lines.next().ok_or(RequestError::BadRequest)?;
    let request_line = trim_cr(request_line);

    let mut parts = request_line.split(|b| *b == b' ');
    let method = parts.next().ok_or(RequestError::BadRequest)?;
    let target = parts.next().ok_or(RequestError::BadRequest)?;
    let version = parts.next().ok_or(RequestError::BadRequest)?;
    if parts.next().is_some() {
        return Err(RequestError::BadRequest);
    }
    if method != b"GET" {
        return Err(RequestError::MethodNotAllowed);
    }
    if version != b"HTTP/1.1" {
        return Err(RequestError::BadRequest);
    }

    let mut accept_gzip = false;
    for line in lines {
        let line = trim_cr(line);
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = split_header(line)? {
            if eq_ignore_ascii_case(name, b"accept-encoding") && header_accepts_gzip(value) {
                accept_gzip = true;
            }
        } else {
            return Err(RequestError::BadRequest);
        }
    }

    Ok(Request {
        target: target.to_vec(),
        accept_gzip,
    })
}

fn split_header(line: &[u8]) -> Result<Option<(&[u8], &[u8])>, RequestError> {
    let mut parts = line.splitn(2, |b| *b == b':');
    let name = parts.next().ok_or(RequestError::BadRequest)?;
    let value = parts.next().ok_or(RequestError::BadRequest)?;
    Ok(Some((trim_ws(name), trim_ws(value))))
}

fn header_accepts_gzip(value: &[u8]) -> bool {
    value
        .split(|b| *b == b',')
        .any(|part| trim_ws(part).eq_ignore_ascii_case(b"gzip"))
}

pub fn eq_ignore_ascii_case(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

pub fn trim_ws(input: &[u8]) -> &[u8] {
    let mut start = 0usize;
    let mut end = input.len();
    while start < end && (input[start] == b' ' || input[start] == b'\t') {
        start += 1;
    }
    while end > start && (input[end - 1] == b' ' || input[end - 1] == b'\t') {
        end -= 1;
    }
    &input[start..end]
}

fn trim_cr(input: &[u8]) -> &[u8] {
    if input.ends_with(b"\r") {
        &input[..input.len() - 1]
    } else {
        input
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

pub fn read_request_target(raw_target: &[u8]) -> Result<Vec<u8>, RequestError> {
    let path = match raw_target.iter().position(|b| *b == b'?') {
        Some(idx) => &raw_target[..idx],
        None => raw_target,
    };
    if path.is_empty() || path[0] != b'/' {
        return Err(RequestError::BadRequest);
    }
    percent_decode(path)
}

pub fn percent_decode(input: &[u8]) -> Result<Vec<u8>, RequestError> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0usize;
    while i < input.len() {
        match input[i] {
            b'%' => {
                if i + 2 >= input.len() {
                    return Err(RequestError::BadRequest);
                }
                let hi = hex_value(input[i + 1]).ok_or(RequestError::BadRequest)?;
                let lo = hex_value(input[i + 2]).ok_or(RequestError::BadRequest)?;
                let byte = (hi << 4) | lo;
                if byte == 0 {
                    return Err(RequestError::BadRequest);
                }
                out.push(byte);
                i += 3;
            }
            b => {
                if b == 0 {
                    return Err(RequestError::BadRequest);
                }
                out.push(b);
                i += 1;
            }
        }
    }
    Ok(out)
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn normalize_path(decoded: &[u8]) -> Result<Vec<Vec<u8>>, RequestError> {
    let mut segments: Vec<Vec<u8>> = Vec::new();
    for seg in decoded.split(|b| *b == b'/') {
        if seg.is_empty() || seg == b"." {
            continue;
        }
        if seg == b".." {
            segments.pop();
            continue;
        }
        if seg.iter().any(|b| *b == 0) {
            return Err(RequestError::BadRequest);
        }
        segments.push(seg.to_vec());
    }
    Ok(segments)
}

pub fn is_health_endpoint(path: &[u8]) -> bool {
    path == b"/health"
}

pub fn write_simple_response(
    stream: &mut std::net::TcpStream,
    status: StatusCode,
    body: &[u8],
    extra_headers: &[(&str, &str)],
) -> io::Result<u64> {
    let mut header = Vec::with_capacity(256);
    header.extend_from_slice(b"HTTP/1.1 ");
    header.extend_from_slice(status.code().to_string().as_bytes());
    header.push(b' ');
    header.extend_from_slice(status.reason().as_bytes());
    header.extend_from_slice(b"\r\nConnection: close\r\nContent-Length: ");
    header.extend_from_slice(body.len().to_string().as_bytes());
    header.extend_from_slice(b"\r\n");
    for (k, v) in extra_headers {
        header.extend_from_slice(k.as_bytes());
        header.extend_from_slice(b": ");
        header.extend_from_slice(v.as_bytes());
        header.extend_from_slice(b"\r\n");
    }
    header.extend_from_slice(b"\r\n");
    stream.write_all(&header)?;
    stream.write_all(body)?;
    Ok(header.len() as u64 + body.len() as u64)
}

pub fn write_file_response(
    stream: &mut std::net::TcpStream,
    status: StatusCode,
    content_length: u64,
    mime: &str,
    cache_immutable: bool,
    gzip: bool,
    file: &std::fs::File,
) -> io::Result<u64> {
    let mut header = Vec::with_capacity(256);
    header.extend_from_slice(b"HTTP/1.1 ");
    header.extend_from_slice(status.code().to_string().as_bytes());
    header.push(b' ');
    header.extend_from_slice(status.reason().as_bytes());
    header.extend_from_slice(b"\r\nConnection: close\r\nContent-Length: ");
    header.extend_from_slice(content_length.to_string().as_bytes());
    header.extend_from_slice(b"\r\nContent-Type: ");
    header.extend_from_slice(mime.as_bytes());
    header.extend_from_slice(b"\r\n");
    if gzip {
        header.extend_from_slice(b"Content-Encoding: gzip\r\nVary: Accept-Encoding\r\n");
    }
    if cache_immutable {
        header.extend_from_slice(b"Cache-Control: public, max-age=31536000, immutable\r\n");
    }
    header.extend_from_slice(b"\r\n");
    stream.write_all(&header)?;
    let body_bytes = send_file(stream, file, content_length)?;
    Ok(header.len() as u64 + body_bytes)
}

pub fn send_file(
    stream: &mut std::net::TcpStream,
    file: &std::fs::File,
    mut count: u64,
) -> io::Result<u64> {
    let mut sent = 0u64;
    let out_fd = stream.as_raw_fd();
    let in_fd = file.as_raw_fd();

    while count > 0 {
        let to_send = count.min(usize::MAX as u64) as usize;
        let mut offset: ffi::off_t = sent as ffi::off_t;
        let rc = unsafe { ffi::sendfile(out_fd, in_fd, &mut offset as *mut ffi::off_t, to_send) };
        if rc > 0 {
            let n = rc as u64;
            sent += n;
            count -= n;
            continue;
        }
        if rc == 0 {
            break;
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => continue,
            io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported if sent == 0 => {
                let mut reader = file;
                let copied = io::copy(&mut reader, stream)?;
                return Ok(copied);
            }
            _ => return Err(err),
        }
    }
    Ok(sent)
}

pub fn log_request(
    remote: Option<SocketAddr>,
    method: &str,
    path: &str,
    status: StatusCode,
    bytes: u64,
    duration: Duration,
    error: Option<&str>,
) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut line = String::with_capacity(256);
    line.push('{');
    push_json_kv_u128(&mut line, "ts", ts);
    push_json_kv_str(
        &mut line,
        "remote",
        remote.map(|r| r.to_string()).as_deref().unwrap_or(""),
    );
    push_json_kv_str(&mut line, "method", method);
    push_json_kv_str(&mut line, "path", path);
    push_json_kv_u64(&mut line, "status", status.code() as u64);
    push_json_kv_u64(&mut line, "bytes", bytes);
    push_json_kv_u64(&mut line, "duration_ms", duration.as_millis() as u64);
    if let Some(err) = error {
        push_json_kv_str(&mut line, "error", err);
    } else if line.ends_with(',') {
        line.pop();
    }
    if line.ends_with(',') {
        line.pop();
    }
    line.push('}');
    let mut stderr = io::stderr().lock();
    let _ = stderr.write_all(line.as_bytes());
    let _ = stderr.write_all(b"\n");
}

fn push_json_kv_str(line: &mut String, key: &str, value: &str) {
    line.push('"');
    line.push_str(key);
    line.push_str("\":\"");
    escape_json_string(line, value);
    line.push_str("\",");
}

fn push_json_kv_u64(line: &mut String, key: &str, value: u64) {
    line.push('"');
    line.push_str(key);
    line.push_str("\":");
    line.push_str(&value.to_string());
    line.push(',');
}

fn push_json_kv_u128(line: &mut String, key: &str, value: u128) {
    line.push('"');
    line.push_str(key);
    line.push_str("\":");
    line.push_str(&value.to_string());
    line.push(',');
}

fn escape_json_string(out: &mut String, input: &str) {
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = std::fmt::Write::write_fmt(out, format_args!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

pub fn mime_for_path(path: &Path) -> &'static str {
    match path.extension().and_then(|s| s.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "mjs" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_valid() {
        let out = percent_decode(b"/a%20b/c").unwrap();
        assert_eq!(out, b"/a b/c");
    }

    #[test]
    fn percent_decode_invalid() {
        assert!(matches!(
            percent_decode(b"/%ZZ"),
            Err(RequestError::BadRequest)
        ));
        assert!(matches!(
            percent_decode(b"/%00"),
            Err(RequestError::BadRequest)
        ));
    }

    #[test]
    fn normalize_path_drops_dots() {
        let decoded = b"/a/./b/../c";
        let segs = normalize_path(decoded).unwrap();
        let rendered: Vec<Vec<u8>> = segs;
        assert_eq!(rendered, vec![b"a".to_vec(), b"c".to_vec()]);
    }

    #[test]
    fn request_target_requires_origin_form() {
        assert!(matches!(
            read_request_target(b"http://example.com/"),
            Err(RequestError::BadRequest)
        ));
    }

    #[test]
    fn gzip_header_detection() {
        assert!(header_accepts_gzip(b"br, gzip, deflate"));
        assert!(!header_accepts_gzip(b"br, deflate"));
    }
}
