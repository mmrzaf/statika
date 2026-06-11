use crate::net;
use std::fmt::Write as _;
use std::io::{self, Read};
use std::net::TcpStream;
use std::time::Instant;

const MAX_HEADER_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Method {
    Get,
    Head,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Ok,
    NotModified,
    BadRequest,
    NotFound,
    MethodNotAllowed,
    RequestTimeout,
    HeaderTooLarge,
    InternalServerError,
}

impl Status {
    pub fn code(self) -> u16 {
        match self {
            Self::Ok => 200,
            Self::NotModified => 304,
            Self::BadRequest => 400,
            Self::NotFound => 404,
            Self::MethodNotAllowed => 405,
            Self::RequestTimeout => 408,
            Self::HeaderTooLarge => 431,
            Self::InternalServerError => 500,
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::NotModified => "Not Modified",
            Self::BadRequest => "Bad Request",
            Self::NotFound => "Not Found",
            Self::MethodNotAllowed => "Method Not Allowed",
            Self::RequestTimeout => "Request Timeout",
            Self::HeaderTooLarge => "Request Header Fields Too Large",
            Self::InternalServerError => "Internal Server Error",
        }
    }
}

#[derive(Debug)]
pub struct Request {
    pub method: Method,
    pub target: Vec<u8>,
    pub accept_gzip: bool,
    pub if_none_match: Option<String>,
}

#[derive(Debug)]
pub enum RequestError {
    BadRequest,
    HeaderTooLarge,
    MethodNotAllowed(String),
    Timeout,
    Io,
}

#[derive(Debug)]
pub struct DecodedPath {
    pub components: Vec<Vec<u8>>,
    pub trailing_slash: bool,
}

pub fn read_request(stream: &mut TcpStream, deadline: Instant) -> Result<Request, RequestError> {
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];

    let header_end = loop {
        if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        if request.len() >= MAX_HEADER_BYTES {
            return Err(RequestError::HeaderTooLarge);
        }

        match stream.read(&mut chunk) {
            Ok(0) => return Err(RequestError::BadRequest),
            Ok(read) => {
                if request.len() + read > MAX_HEADER_BYTES {
                    return Err(RequestError::HeaderTooLarge);
                }
                request.extend_from_slice(&chunk[..read]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                net::wait_readable(stream, deadline).map_err(map_io)?;
            }
            Err(error) => return Err(map_io(error)),
        }
    };

    parse_request(&request[..header_end])
}

fn parse_request(header: &[u8]) -> Result<Request, RequestError> {
    let mut lines = header.split(|byte| *byte == b'\n');
    let request_line = lines.next().ok_or(RequestError::BadRequest)?;
    let request_line = request_line
        .strip_suffix(b"\r")
        .ok_or(RequestError::BadRequest)?;
    let mut fields = request_line.split(|byte| *byte == b' ');
    let method_raw = fields.next().ok_or(RequestError::BadRequest)?;
    let target = fields.next().ok_or(RequestError::BadRequest)?;
    let version = fields.next().ok_or(RequestError::BadRequest)?;
    if fields.next().is_some() || target.is_empty() || !target.starts_with(b"/") {
        return Err(RequestError::BadRequest);
    }
    if version != b"HTTP/1.1" && version != b"HTTP/1.0" {
        return Err(RequestError::BadRequest);
    }

    let method = match method_raw {
        b"GET" => Method::Get,
        b"HEAD" => Method::Head,
        _ => {
            return Err(RequestError::MethodNotAllowed(
                String::from_utf8_lossy(method_raw).into_owned(),
            ))
        }
    };

    let mut accept_encoding = Vec::new();
    let mut if_none_match = None;
    for line in lines {
        let line = line.strip_suffix(b"\r").ok_or(RequestError::BadRequest)?;
        if line.is_empty() {
            break;
        }
        let colon = line
            .iter()
            .position(|byte| *byte == b':')
            .ok_or(RequestError::BadRequest)?;
        let name = trim_ascii(&line[..colon]);
        let value = trim_ascii(&line[colon + 1..]);
        if name.eq_ignore_ascii_case(b"accept-encoding") {
            accept_encoding.extend_from_slice(value);
            accept_encoding.push(b',');
        } else if name.eq_ignore_ascii_case(b"if-none-match") {
            if_none_match = Some(String::from_utf8_lossy(value).into_owned());
        }
    }

    Ok(Request {
        method,
        target: target.to_vec(),
        accept_gzip: accepts_gzip(&accept_encoding),
        if_none_match,
    })
}

pub fn decode_target(target: &[u8]) -> Result<DecodedPath, RequestError> {
    let path = target.split(|byte| *byte == b'?').next().unwrap_or(target);
    if !path.starts_with(b"/") {
        return Err(RequestError::BadRequest);
    }

    let trailing_slash = path.ends_with(b"/");
    let mut decoded = Vec::with_capacity(path.len());
    let mut cursor = 0;
    while cursor < path.len() {
        match path[cursor] {
            b'%' => {
                if cursor + 2 >= path.len() {
                    return Err(RequestError::BadRequest);
                }
                let high = hex(path[cursor + 1]).ok_or(RequestError::BadRequest)?;
                let low = hex(path[cursor + 2]).ok_or(RequestError::BadRequest)?;
                decoded.push((high << 4) | low);
                cursor += 3;
            }
            byte => {
                decoded.push(byte);
                cursor += 1;
            }
        }
    }

    if decoded.contains(&0) || decoded.contains(&b'\\') {
        return Err(RequestError::BadRequest);
    }

    let mut components = Vec::new();
    for component in decoded.split(|byte| *byte == b'/') {
        if component.is_empty() {
            continue;
        }
        if component == b"." || component == b".." {
            return Err(RequestError::BadRequest);
        }
        components.push(component.to_vec());
    }

    Ok(DecodedPath {
        components,
        trailing_slash,
    })
}

pub fn if_none_match_matches(value: Option<&str>, etag: &str) -> bool {
    value.is_some_and(|header| {
        header.split(',').any(|candidate| {
            let candidate = candidate.trim();
            candidate == "*" || candidate == etag || candidate.strip_prefix("W/") == Some(etag)
        })
    })
}

pub fn response_head(
    status: Status,
    content_length: u64,
    content_type: Option<&str>,
    headers: &[(&str, &str)],
) -> Vec<u8> {
    let mut response = String::with_capacity(512);
    let _ = write!(
        response,
        "HTTP/1.1 {} {}\r\n",
        status.code(),
        status.reason()
    );
    response.push_str("Server: statika\r\n");
    response.push_str("Connection: close\r\n");
    response.push_str("X-Content-Type-Options: nosniff\r\n");
    let _ = write!(response, "Content-Length: {content_length}\r\n");
    if let Some(content_type) = content_type {
        let _ = write!(response, "Content-Type: {content_type}\r\n");
    }
    for (name, value) in headers {
        let _ = write!(response, "{name}: {value}\r\n");
    }
    response.push_str("\r\n");
    response.into_bytes()
}

pub fn content_type(components: &[Vec<u8>]) -> &'static str {
    let extension = components
        .last()
        .and_then(|name| name.rsplit(|byte| *byte == b'.').next())
        .unwrap_or_default();

    if extension.eq_ignore_ascii_case(b"html") || extension.eq_ignore_ascii_case(b"htm") {
        "text/html; charset=utf-8"
    } else if extension.eq_ignore_ascii_case(b"css") {
        "text/css; charset=utf-8"
    } else if extension.eq_ignore_ascii_case(b"js") || extension.eq_ignore_ascii_case(b"mjs") {
        "text/javascript; charset=utf-8"
    } else if extension.eq_ignore_ascii_case(b"json") || extension.eq_ignore_ascii_case(b"map") {
        "application/json; charset=utf-8"
    } else if extension.eq_ignore_ascii_case(b"svg") {
        "image/svg+xml"
    } else if extension.eq_ignore_ascii_case(b"png") {
        "image/png"
    } else if extension.eq_ignore_ascii_case(b"jpg") || extension.eq_ignore_ascii_case(b"jpeg") {
        "image/jpeg"
    } else if extension.eq_ignore_ascii_case(b"gif") {
        "image/gif"
    } else if extension.eq_ignore_ascii_case(b"webp") {
        "image/webp"
    } else if extension.eq_ignore_ascii_case(b"ico") {
        "image/x-icon"
    } else if extension.eq_ignore_ascii_case(b"woff") {
        "font/woff"
    } else if extension.eq_ignore_ascii_case(b"woff2") {
        "font/woff2"
    } else if extension.eq_ignore_ascii_case(b"txt") {
        "text/plain; charset=utf-8"
    } else if extension.eq_ignore_ascii_case(b"xml") {
        "application/xml; charset=utf-8"
    } else if extension.eq_ignore_ascii_case(b"wasm") {
        "application/wasm"
    } else if extension.eq_ignore_ascii_case(b"pdf") {
        "application/pdf"
    } else {
        "application/octet-stream"
    }
}

pub fn has_fingerprint(components: &[Vec<u8>]) -> bool {
    components.last().is_some_and(|name| {
        name.split(|byte| matches!(*byte, b'.' | b'-' | b'_'))
            .any(|part| part.len() >= 8 && part.iter().all(u8::is_ascii_hexdigit))
    })
}

fn accepts_gzip(value: &[u8]) -> bool {
    let mut gzip = None;
    let mut wildcard = None;
    for raw_item in value.split(|byte| *byte == b',') {
        let mut fields = raw_item.split(|byte| *byte == b';');
        let token = trim_ascii(fields.next().unwrap_or_default());
        if token.is_empty() {
            continue;
        }
        let mut quality = 1.0_f32;
        for field in fields {
            let field = trim_ascii(field);
            if field.len() >= 2 && field[..2].eq_ignore_ascii_case(b"q=") {
                quality = std::str::from_utf8(&field[2..])
                    .ok()
                    .and_then(|text| text.parse::<f32>().ok())
                    .filter(|quality| (0.0..=1.0).contains(quality))
                    .unwrap_or(0.0);
            }
        }
        if token.eq_ignore_ascii_case(b"gzip") {
            gzip = Some(quality);
        } else if token == b"*" {
            wildcard = Some(quality);
        }
    }
    gzip.or(wildcard).unwrap_or(0.0) > 0.0
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn map_io(error: io::Error) -> RequestError {
    if error.kind() == io::ErrorKind::TimedOut {
        RequestError::Timeout
    } else {
        RequestError::Io
    }
}

#[cfg(test)]
mod tests {
    use super::{accepts_gzip, decode_target, has_fingerprint, if_none_match_matches};

    #[test]
    fn gzip_quality_is_respected() {
        assert!(accepts_gzip(b"br, gzip;q=1.0"));
        assert!(!accepts_gzip(b"gzip;q=0, *;q=1"));
        assert!(accepts_gzip(b"*;q=0.5"));
    }

    #[test]
    fn target_decode_rejects_escapes() {
        assert!(decode_target(b"/%2e%2e/secret").is_err());
        assert!(decode_target(b"/assets/%00.js").is_err());
        assert_eq!(
            decode_target(b"/route?q=1").unwrap().components,
            vec![b"route".to_vec()]
        );
    }

    #[test]
    fn fingerprint_detection_is_conservative() {
        assert!(has_fingerprint(&[
            b"assets".to_vec(),
            b"app.0123abcd.js".to_vec()
        ]));
        assert!(!has_fingerprint(&[b"assets".to_vec(), b"app.js".to_vec()]));
    }

    #[test]
    fn weak_etag_matches() {
        assert!(if_none_match_matches(Some("W/\"abc\""), "\"abc\""));
        assert!(if_none_match_matches(Some("*"), "\"abc\""));
        assert!(!if_none_match_matches(Some("\"def\""), "\"abc\""));
    }
}
