use crate::net;
use std::fmt::Write as _;
use std::io::{self, Read};
use std::net::TcpStream;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;
const SECONDS_PER_DAY: i64 = 86_400;

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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AcceptedEncodings {
    pub br_q: u16,
    pub gzip_q: u16,
}

impl AcceptedEncodings {
    pub fn accepts_br(self) -> bool {
        self.br_q > 0
    }

    pub fn accepts_gzip(self) -> bool {
        self.gzip_q > 0
    }

    pub fn prefer_br(self) -> bool {
        self.br_q >= self.gzip_q
    }
}

#[derive(Debug)]
pub struct Request {
    pub method: Method,
    pub target: Vec<u8>,
    pub accepted_encodings: AcceptedEncodings,
    pub if_none_match: Option<String>,
    pub if_modified_since: Option<i64>,
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
    if request_line.len() > MAX_HEADER_LINE_BYTES || contains_ctl(request_line) {
        return Err(RequestError::BadRequest);
    }

    let mut fields = request_line.split(|byte| *byte == b' ');
    let method_raw = fields.next().ok_or(RequestError::BadRequest)?;
    let target = fields.next().ok_or(RequestError::BadRequest)?;
    let version = fields.next().ok_or(RequestError::BadRequest)?;
    if fields.next().is_some()
        || target.is_empty()
        || !target.starts_with(b"/")
        || !is_request_target_safe(target)
    {
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
    let mut if_modified_since = None;
    let mut host_count = 0_usize;
    for line in lines {
        let line = line.strip_suffix(b"\r").ok_or(RequestError::BadRequest)?;
        if line.is_empty() {
            break;
        }
        if line.len() > MAX_HEADER_LINE_BYTES
            || line.first().is_some_and(u8::is_ascii_whitespace)
            || value_contains_invalid_ctl(line)
        {
            return Err(RequestError::BadRequest);
        }
        let colon = line
            .iter()
            .position(|byte| *byte == b':')
            .ok_or(RequestError::BadRequest)?;
        let name = trim_ascii(&line[..colon]);
        let value = trim_ascii(&line[colon + 1..]);
        if name.is_empty() || !name.iter().copied().all(is_header_name_byte) {
            return Err(RequestError::BadRequest);
        }
        if name.eq_ignore_ascii_case(b"host") {
            host_count += 1;
            if value.is_empty() {
                return Err(RequestError::BadRequest);
            }
        } else if name.eq_ignore_ascii_case(b"accept-encoding") {
            accept_encoding.extend_from_slice(value);
            accept_encoding.push(b',');
        } else if name.eq_ignore_ascii_case(b"if-none-match") {
            if_none_match = Some(String::from_utf8_lossy(value).into_owned());
        } else if name.eq_ignore_ascii_case(b"if-modified-since") {
            if_modified_since = parse_http_date_bytes(value);
        }
    }

    if version == b"HTTP/1.1" && host_count != 1 {
        return Err(RequestError::BadRequest);
    }

    Ok(Request {
        method,
        target: target.to_vec(),
        accepted_encodings: accepted_encodings(&accept_encoding),
        if_none_match,
        if_modified_since,
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

    if decoded.contains(&0) || decoded.contains(&b'\\') || contains_ctl(&decoded) {
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
    let mut response = String::with_capacity(512 + headers.len() * 64);
    let _ = write!(
        response,
        "HTTP/1.1 {} {}\r\n",
        status.code(),
        status.reason()
    );
    response.push_str("Server: statika\r\n");
    response.push_str("Connection: close\r\n");
    response.push_str("X-Content-Type-Options: nosniff\r\n");
    let _ = write!(response, "Date: {}\r\n", http_date(now_unix_seconds()));
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
    } else if extension.eq_ignore_ascii_case(b"webmanifest") {
        "application/manifest+json; charset=utf-8"
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
    } else if extension.eq_ignore_ascii_case(b"avif") {
        "image/avif"
    } else if extension.eq_ignore_ascii_case(b"ico") {
        "image/x-icon"
    } else if extension.eq_ignore_ascii_case(b"woff") {
        "font/woff"
    } else if extension.eq_ignore_ascii_case(b"woff2") {
        "font/woff2"
    } else if extension.eq_ignore_ascii_case(b"txt") {
        "text/plain; charset=utf-8"
    } else if extension.eq_ignore_ascii_case(b"csv") {
        "text/csv; charset=utf-8"
    } else if extension.eq_ignore_ascii_case(b"xml") {
        "application/xml; charset=utf-8"
    } else if extension.eq_ignore_ascii_case(b"wasm") {
        "application/wasm"
    } else if extension.eq_ignore_ascii_case(b"pdf") {
        "application/pdf"
    } else if extension.eq_ignore_ascii_case(b"mp4") {
        "video/mp4"
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

pub fn http_date(unix_seconds: i64) -> String {
    let seconds = unix_seconds.max(0);
    let days = seconds.div_euclid(SECONDS_PER_DAY);
    let second_of_day = seconds.rem_euclid(SECONDS_PER_DAY);
    let hour = second_of_day / 3600;
    let minute = (second_of_day % 3600) / 60;
    let second = second_of_day % 60;
    let (year, month, day) = civil_from_days(days);
    let weekday = ((days + 4).rem_euclid(7)) as usize;
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        WEEKDAYS[weekday],
        day,
        MONTHS[(month - 1) as usize],
        year,
        hour,
        minute,
        second
    )
}

fn parse_http_date_bytes(value: &[u8]) -> Option<i64> {
    let value = std::str::from_utf8(value).ok()?;
    parse_http_date(value)
}

fn parse_http_date(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() != 29
        || bytes[3] != b','
        || bytes[4] != b' '
        || bytes[7] != b' '
        || bytes[11] != b' '
        || bytes[16] != b' '
        || bytes[19] != b':'
        || bytes[22] != b':'
        || bytes[25] != b' '
        || &bytes[26..29] != b"GMT"
    {
        return None;
    }
    let day = parse_2_digits(&bytes[5..7])?;
    let month = parse_month(&bytes[8..11])?;
    let year = parse_4_digits(&bytes[12..16])?;
    let hour = parse_2_digits(&bytes[17..19])?;
    let minute = parse_2_digits(&bytes[20..22])?;
    let second = parse_2_digits(&bytes[23..25])?;
    if year < 1970
        || month == 0
        || month > 12
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    Some(
        days_from_civil(year, month, day) * SECONDS_PER_DAY
            + i64::from(hour) * 3600
            + i64::from(minute) * 60
            + i64::from(second.min(59)),
    )
}

fn accepted_encodings(value: &[u8]) -> AcceptedEncodings {
    let mut br = None;
    let mut gzip = None;
    let mut wildcard = None;
    for raw_item in value.split(|byte| *byte == b',') {
        let mut fields = raw_item.split(|byte| *byte == b';');
        let token = trim_ascii(fields.next().unwrap_or_default());
        if token.is_empty() {
            continue;
        }
        let mut quality = 1000_u16;
        for field in fields {
            let field = trim_ascii(field);
            if field.len() >= 2 && field[..2].eq_ignore_ascii_case(b"q=") {
                quality = parse_quality(&field[2..]).unwrap_or(0);
            }
        }
        if token.eq_ignore_ascii_case(b"br") {
            br = Some(quality);
        } else if token.eq_ignore_ascii_case(b"gzip") {
            gzip = Some(quality);
        } else if token == b"*" {
            wildcard = Some(quality);
        }
    }
    AcceptedEncodings {
        br_q: br.or(wildcard).unwrap_or(0),
        gzip_q: gzip.or(wildcard).unwrap_or(0),
    }
}

fn parse_quality(value: &[u8]) -> Option<u16> {
    if value == b"0" || value == b"0." || value == b"0.0" || value == b"0.00" || value == b"0.000" {
        return Some(0);
    }
    if value == b"1" || value == b"1." || value == b"1.0" || value == b"1.00" || value == b"1.000" {
        return Some(1000);
    }
    let fraction = value.strip_prefix(b"0.")?;
    if fraction.is_empty() || fraction.len() > 3 || !fraction.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut quality = 0_u16;
    for digit in fraction {
        quality = quality * 10 + u16::from(digit - b'0');
    }
    for _ in fraction.len()..3 {
        quality *= 10;
    }
    Some(quality)
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

fn contains_ctl(bytes: &[u8]) -> bool {
    bytes.iter().any(|byte| matches!(*byte, 0..=31 | 127))
}

fn value_contains_invalid_ctl(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .any(|byte| matches!(*byte, 0..=8 | 10..=31 | 127))
}

fn is_request_target_safe(bytes: &[u8]) -> bool {
    !bytes
        .iter()
        .any(|byte| matches!(*byte, 0..=32 | 127) || *byte == b'#')
}

fn is_header_name_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~'
            | b'0'..=b'9'
            | b'a'..=b'z'
            | b'A'..=b'Z'
    )
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0)
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (year as i32, month as u32, day as u32)
}

fn days_from_civil(mut year: i32, month: u32, day: u32) -> i64 {
    year -= if month <= 2 { 1 } else { 0 };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i32;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era * 146_097 + doe - 719_468)
}

fn parse_2_digits(bytes: &[u8]) -> Option<u32> {
    if bytes.len() != 2 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(u32::from(bytes[0] - b'0') * 10 + u32::from(bytes[1] - b'0'))
}

fn parse_4_digits(bytes: &[u8]) -> Option<i32> {
    if bytes.len() != 4 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(
        i32::from(bytes[0] - b'0') * 1000
            + i32::from(bytes[1] - b'0') * 100
            + i32::from(bytes[2] - b'0') * 10
            + i32::from(bytes[3] - b'0'),
    )
}

fn parse_month(bytes: &[u8]) -> Option<u32> {
    match bytes {
        b"Jan" => Some(1),
        b"Feb" => Some(2),
        b"Mar" => Some(3),
        b"Apr" => Some(4),
        b"May" => Some(5),
        b"Jun" => Some(6),
        b"Jul" => Some(7),
        b"Aug" => Some(8),
        b"Sep" => Some(9),
        b"Oct" => Some(10),
        b"Nov" => Some(11),
        b"Dec" => Some(12),
        _ => None,
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
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
    use super::{
        accepted_encodings, decode_target, has_fingerprint, http_date, if_none_match_matches,
        parse_http_date, AcceptedEncodings,
    };

    #[test]
    fn encoding_quality_is_respected() {
        assert_eq!(
            accepted_encodings(b"br, gzip;q=1.0"),
            AcceptedEncodings {
                br_q: 1000,
                gzip_q: 1000,
            }
        );
        assert_eq!(
            accepted_encodings(b"gzip;q=0, br;q=1"),
            AcceptedEncodings {
                br_q: 1000,
                gzip_q: 0,
            }
        );
        assert_eq!(
            accepted_encodings(b"gzip;q=0, *;q=1"),
            AcceptedEncodings {
                br_q: 1000,
                gzip_q: 0,
            }
        );
    }

    #[test]
    fn target_decode_rejects_escapes() {
        assert!(decode_target(b"/%2e%2e/secret").is_err());
        assert!(decode_target(b"/assets/%00.js").is_err());
        assert!(decode_target(b"/assets%2f..%2fsecret").is_err());
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

    #[test]
    fn http_dates_round_trip_unix_epoch_and_known_value() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(http_date(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert!(parse_http_date("Sun, 31 Feb 1994 08:49:37 GMT").is_none());
    }
}
