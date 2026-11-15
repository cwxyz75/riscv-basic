//! HTTP/1 for the demo server and the fetch tool (S9-SPEC 10): a request parser that treats its input as hostile, the
//! sanitising of a request target into path segments, the building of a response head, and the parsing of a response head.
//! Pure and total: no input can make any function panic, loop or allocate beyond the stated limits.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

pub const MAX_HEAD: usize = 4096;
pub const MAX_TARGET: usize = 1024;
pub const MAX_HEADERS: usize = 32;
pub const MAX_RESPONSE_HEAD: usize = 8192;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    Get,
    Head,
}

/// Why a request was refused, with the status the server answers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reject {
    /// 400: bad syntax, a bad escape, `..`, a body announced.
    BadRequest,
    /// 405 (`Allow: GET, HEAD`).
    MethodNotAllowed,
    /// 414.
    TargetTooLong,
    /// 431.
    HeadTooLarge,
    /// 505.
    VersionNotSupported,
}

impl Reject {
    pub fn status(self) -> u16 {
        match self {
            Reject::BadRequest => 400,
            Reject::MethodNotAllowed => 405,
            Reject::TargetTooLong => 414,
            Reject::HeadTooLarge => 431,
            Reject::VersionNotSupported => 505,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    /// The target as sent (origin form), query not yet cut.
    pub target: Vec<u8>,
}

/// Find the end of the head (the first empty line): the offset just past `\r\n\r\n`. `None` if it has not arrived yet.
pub fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn is_token(b: &[u8]) -> bool {
    !b.is_empty()
        && b.iter()
            .all(|&c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c))
}

/// Parse the head of a request (everything up to and including the empty line). Only `GET` and `HEAD`; HTTP/1.0 and 1.1;
/// headers are syntax-checked and ignored except that a body is refused.
pub fn parse_request(head: &[u8]) -> Result<Request, Reject> {
    if head.len() > MAX_HEAD {
        return Err(Reject::HeadTooLarge);
    }
    let end = head_end(head).ok_or(Reject::BadRequest)?;
    let head = &head[..end - 4];
    let mut lines = head
        .split(|&c| c == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    // a bare CR or LF inside a line, or a NUL, is a smuggling attempt: refused
    if head.contains(&0)
        || head.windows(2).any(|w| w[1] == b'\n' && w[0] != b'\r')
        || head.contains(&b'\r') && head.windows(2).any(|w| w[0] == b'\r' && w[1] != b'\n')
    {
        return Err(Reject::BadRequest);
    }
    let line = lines.next().ok_or(Reject::BadRequest)?;
    let mut parts = line.split(|&c| c == b' ');
    let (m, t, v) = (parts.next(), parts.next(), parts.next());
    if parts.next().is_some() {
        return Err(Reject::BadRequest);
    }
    let (Some(m), Some(t), Some(v)) = (m, t, v) else {
        return Err(Reject::BadRequest);
    };
    if !is_token(m) {
        return Err(Reject::BadRequest);
    }
    let method = match m {
        b"GET" => Method::Get,
        b"HEAD" => Method::Head,
        _ => return Err(Reject::MethodNotAllowed),
    };
    match v {
        b"HTTP/1.0" | b"HTTP/1.1" => {}
        v if v.starts_with(b"HTTP/") => return Err(Reject::VersionNotSupported),
        _ => return Err(Reject::BadRequest),
    }
    if t.len() > MAX_TARGET {
        return Err(Reject::TargetTooLong);
    }
    if t.first() != Some(&b'/') || t.iter().any(|&c| c <= b' ' || c == 0x7f) {
        return Err(Reject::BadRequest);
    }
    let mut n = 0;
    for h in lines {
        n += 1;
        if n > MAX_HEADERS {
            return Err(Reject::HeadTooLarge);
        }
        let Some(colon) = h.iter().position(|&c| c == b':') else {
            return Err(Reject::BadRequest);
        };
        let (name, value) = (&h[..colon], &h[colon + 1..]);
        if !is_token(name) {
            return Err(Reject::BadRequest);
        }
        if name.eq_ignore_ascii_case(b"transfer-encoding") {
            return Err(Reject::BadRequest);
        }
        if name.eq_ignore_ascii_case(b"content-length") {
            let v = value
                .iter()
                .copied()
                .filter(|c| *c != b' ' && *c != b'\t')
                .collect::<Vec<u8>>();
            if v.is_empty() || !v.iter().all(|c| c.is_ascii_digit()) || v.iter().any(|&c| c != b'0')
            {
                return Err(Reject::BadRequest); // a body (or an unparseable length)
            }
        }
    }
    Ok(Request {
        method,
        target: t.to_vec(),
    })
}

/// Turn a request target into path segments: the query and fragment are cut off, percent-escapes decoded once (`%00` and an
/// encoded `/` or `\` refused), empty and `.` segments dropped, any `..` refused. `Err` is a 400.
pub fn path_segments(target: &[u8]) -> Result<Vec<Vec<u8>>, Reject> {
    let end = target
        .iter()
        .position(|&c| c == b'?' || c == b'#')
        .unwrap_or(target.len());
    let path = &target[..end];
    if path.first() != Some(&b'/') {
        return Err(Reject::BadRequest);
    }
    let mut segs: Vec<Vec<u8>> = Vec::new();
    for raw in path.split(|&c| c == b'/') {
        let mut seg = Vec::with_capacity(raw.len());
        let mut i = 0;
        while i < raw.len() {
            if raw[i] == b'%' {
                let (Some(&h), Some(&l)) = (raw.get(i + 1), raw.get(i + 2)) else {
                    return Err(Reject::BadRequest);
                };
                let hex = |c: u8| (c as char).to_digit(16);
                let (Some(h), Some(l)) = (hex(h), hex(l)) else {
                    return Err(Reject::BadRequest);
                };
                let v = (h * 16 + l) as u8;
                if v == 0 || v == b'/' || v == b'\\' {
                    return Err(Reject::BadRequest);
                }
                seg.push(v);
                i += 3;
            } else {
                if raw[i] == b'\\' {
                    return Err(Reject::BadRequest);
                }
                seg.push(raw[i]);
                i += 1;
            }
        }
        match seg.as_slice() {
            b"" | b"." => {}
            b".." => return Err(Reject::BadRequest),
            _ => segs.push(seg),
        }
    }
    Ok(segs)
}

/// The `Content-Type` for a file name (by extension, case-insensitive).
pub fn content_type(name: &[u8]) -> &'static str {
    let ext = name.rsplit(|&c| c == b'.').next().unwrap_or(b"");
    if ext.len() == name.len() {
        return "application/octet-stream";
    }
    let mut e = [0u8; 8];
    if ext.len() > 8 {
        return "application/octet-stream";
    }
    for (o, c) in e.iter_mut().zip(ext) {
        *o = c.to_ascii_lowercase();
    }
    match &e[..ext.len()] {
        b"html" | b"htm" => "text/html; charset=utf-8",
        b"txt" => "text/plain; charset=utf-8",
        b"css" => "text/css",
        b"js" => "text/javascript",
        b"json" => "application/json",
        b"xml" => "application/xml",
        b"png" => "image/png",
        b"jpg" | b"jpeg" => "image/jpeg",
        b"gif" => "image/gif",
        b"svg" => "image/svg+xml",
        b"bin" => "application/octet-stream",
        _ => "application/octet-stream",
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        414 => "URI Too Long",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        505 => "HTTP Version Not Supported",
        _ => "Status",
    }
}

fn push_dec(out: &mut Vec<u8>, mut n: u64) {
    let mut d = [0u8; 20];
    let mut i = d.len();
    loop {
        i -= 1;
        d[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&d[i..]);
}

/// A response head: status line, `Content-Length`, `Content-Type`, `Connection: close`, `Server`, and `Allow` for a 405.
pub fn response_head(status: u16, content_type: &str, length: u64) -> Vec<u8> {
    let mut h = Vec::with_capacity(160);
    h.extend_from_slice(b"HTTP/1.1 ");
    push_dec(&mut h, u64::from(status));
    h.push(b' ');
    h.extend_from_slice(reason(status).as_bytes());
    h.extend_from_slice(b"\r\nContent-Length: ");
    push_dec(&mut h, length);
    h.extend_from_slice(b"\r\nContent-Type: ");
    h.extend_from_slice(content_type.as_bytes());
    h.extend_from_slice(b"\r\nConnection: close\r\nServer: riscv-os-httpd\r\n");
    if status == 405 {
        h.extend_from_slice(b"Allow: GET, HEAD\r\n");
    }
    h.extend_from_slice(b"\r\n");
    h
}

/// A parsed response head (for `fetch`).
#[derive(Debug, PartialEq, Eq)]
pub struct ResponseHead {
    pub status: u16,
    /// `Content-Length`, if the head had one.
    pub content_length: Option<u64>,
    /// The head's length in bytes (the body starts here).
    pub head_len: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResponseError {
    /// Not enough bytes yet (and fewer than `MAX_RESPONSE_HEAD`).
    Incomplete,
    /// The head is malformed or too large.
    Bad,
}

pub fn parse_response_head(buf: &[u8]) -> Result<ResponseHead, ResponseError> {
    let Some(end) = head_end(buf) else {
        return Err(if buf.len() >= MAX_RESPONSE_HEAD {
            ResponseError::Bad
        } else {
            ResponseError::Incomplete
        });
    };
    if end > MAX_RESPONSE_HEAD {
        return Err(ResponseError::Bad);
    }
    let head = &buf[..end - 4];
    let mut lines = head
        .split(|&c| c == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    let first = lines.next().ok_or(ResponseError::Bad)?;
    let mut parts = first.splitn(3, |&c| c == b' ');
    let version = parts.next().ok_or(ResponseError::Bad)?;
    if version != b"HTTP/1.0" && version != b"HTTP/1.1" {
        return Err(ResponseError::Bad);
    }
    let code = parts.next().ok_or(ResponseError::Bad)?;
    if code.len() != 3 || !code.iter().all(|c| c.is_ascii_digit()) {
        return Err(ResponseError::Bad);
    }
    let status = u16::from(code[0] - b'0') * 100
        + u16::from(code[1] - b'0') * 10
        + u16::from(code[2] - b'0');
    let mut content_length = None;
    for h in lines {
        let Some(colon) = h.iter().position(|&c| c == b':') else {
            return Err(ResponseError::Bad);
        };
        if h[..colon].eq_ignore_ascii_case(b"content-length") {
            let v: Vec<u8> = h[colon + 1..]
                .iter()
                .copied()
                .filter(|c| *c != b' ' && *c != b'\t')
                .collect();
            if v.is_empty() || v.len() > 18 || !v.iter().all(|c| c.is_ascii_digit()) {
                return Err(ResponseError::Bad);
            }
            let n = v.iter().fold(0u64, |a, &d| a * 10 + u64::from(d - b'0'));
            if content_length.is_some_and(|old| old != n) {
                return Err(ResponseError::Bad); // two different lengths
            }
            content_length = Some(n);
        }
        if h[..colon].eq_ignore_ascii_case(b"transfer-encoding") {
            return Err(ResponseError::Bad); // fetch speaks HTTP/1.0 and refuses chunked bodies
        }
    }
    Ok(ResponseHead {
        status,
        content_length,
        head_len: end,
    })
}
