//! HTTP/1.1, as much of it as the bridge serves: requests without bodies, JSON
//! responses, persistent connections.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// The request line and headers together may not exceed this.
pub const MAX_HEAD: usize = 16 << 10;
/// How long a connection may sit idle between requests.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a request may take to arrive once it has begun.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Request {
    pub method: String,
    /// The target's path, still percent-encoded; see [`Request::segments`].
    pub path: String,
    pub query: Vec<(String, String)>,
    /// Header names in lower case, values trimmed.
    headers: Vec<(String, String)>,
    /// Whether the client may send another request on this connection.
    pub keep_alive: bool,
}

impl Request {
    /// The first value of header `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// The path's segments, percent-decoded. `None` when a segment does not
    /// decode to UTF-8.
    pub fn segments(&self) -> Option<Vec<String>> {
        self.path.split('/').skip(1).map(|s| decode(s, false)).collect()
    }
}

/// Why no request could be read from a connection.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    /// The client closed the connection, or it stayed idle, before a request began.
    Closed,
    /// A request began but did not arrive completely in time.
    Dropped,
    /// The request line and headers exceed [`MAX_HEAD`].
    TooLarge,
    /// The request carries a body.
    Body,
    /// The request is not well-formed HTTP/1.1.
    Malformed(&'static str),
}

/// A request that could not be read, with the `Origin` its bytes named, so
/// that a refusal can still carry CORS headers for an allowed page.
#[derive(Debug)]
pub struct Refusal {
    pub error: ReadError,
    pub origin: Option<String>,
}

impl ReadError {
    fn quiet(self) -> Refusal {
        Refusal { error: self, origin: None }
    }
}

/// The `Origin` header among the lines of a request head, however malformed
/// the rest of it is.
fn sniff_origin(head: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(&head[..head.len().min(MAX_HEAD)]);
    text.split("\r\n").skip(1).take_while(|l| !l.is_empty()).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim().eq_ignore_ascii_case("origin").then(|| value.trim().to_string())
    })
}

/// A connection with the bytes read past the last request.
pub struct Conn {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl Conn {
    pub fn new(stream: TcpStream) -> Self {
        Conn { stream, buf: Vec::new() }
    }

    pub fn read_request(&mut self) -> Result<Request, Refusal> {
        self.read_request_within(REQUEST_TIMEOUT)
    }

    /// [`Conn::read_request`] with `limit` in place of [`REQUEST_TIMEOUT`],
    /// and no longer than that to wait for the first byte either.
    pub fn read_request_within(&mut self, limit: Duration) -> Result<Request, Refusal> {
        let mut started = (!self.buf.is_empty()).then(Instant::now);
        loop {
            let too_large = |buf: &[u8]| Refusal { error: ReadError::TooLarge, origin: sniff_origin(buf) };
            if let Some(end) = find_head_end(&self.buf) {
                if end > MAX_HEAD {
                    return Err(too_large(&self.buf));
                }
                let head: Vec<u8> = self.buf.drain(..end + 4).collect();
                return parse(&head[..end]).map_err(|error| Refusal { error, origin: sniff_origin(&head) });
            }
            if self.buf.len() > MAX_HEAD {
                return Err(too_large(&self.buf));
            }
            let timeout = match started {
                None => IDLE_TIMEOUT.min(limit),
                Some(t) => match limit.checked_sub(t.elapsed()).filter(|d| !d.is_zero()) {
                    Some(left) => left,
                    None => return Err(ReadError::Dropped.quiet()),
                },
            };
            let lost = if started.is_some() { ReadError::Dropped } else { ReadError::Closed };
            if self.stream.set_read_timeout(Some(timeout)).is_err() {
                return Err(lost.quiet());
            }
            let mut chunk = [0u8; 4096];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(lost.quiet()),
                Ok(n) => {
                    started.get_or_insert_with(Instant::now);
                    self.buf.extend_from_slice(&chunk[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return Err(lost.quiet()),
            }
        }
    }

    pub fn write(&mut self, response: &Response, keep_alive: bool) -> io::Result<()> {
        let mut head = format!("HTTP/1.1 {} {}\r\n", response.status, reason(response.status));
        for (name, value) in &response.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if !response.body.is_empty() {
            head.push_str("Content-Type: application/json; charset=utf-8\r\n");
        }
        head.push_str(&format!(
            "Content-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: {}\r\n\r\n",
            response.body.len(),
            if keep_alive { "keep-alive" } else { "close" }
        ));
        self.stream.write_all(head.as_bytes())?;
        self.stream.write_all(response.body.as_bytes())?;
        self.stream.flush()
    }
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: String,
}

impl Response {
    pub fn json(status: u16, body: String) -> Self {
        Response { status, headers: Vec::new(), body }
    }

    pub fn empty(status: u16) -> Self {
        Response { status, headers: Vec::new(), body: String::new() }
    }

    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Content Too Large",
        421 => "Misdirected Request",
        422 => "Unprocessable Content",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn parse(head: &[u8]) -> Result<Request, ReadError> {
    let bad = ReadError::Malformed;
    let head = std::str::from_utf8(head).map_err(|_| bad("not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let line = lines.next().unwrap_or_default();
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad("request line"));
    };
    if !is_token(method) {
        return Err(bad("method"));
    }
    let http10 = match version {
        "HTTP/1.1" => false,
        "HTTP/1.0" => true,
        _ => return Err(bad("version")),
    };
    if !target.starts_with('/') || target.bytes().any(|b| !(0x21..0x7f).contains(&b)) {
        return Err(bad("target"));
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut headers = Vec::new();
    for line in lines {
        if line.starts_with([' ', '\t']) {
            return Err(bad("folded header"));
        }
        let (name, value) = line.split_once(':').ok_or(bad("header"))?;
        if !is_token(name) || value.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f) {
            return Err(bad("header"));
        }
        headers.push((name.to_ascii_lowercase(), value.trim_matches([' ', '\t']).to_string()));
    }
    let count = |name: &str| headers.iter().filter(|(n, _)| n == name).count();
    if count("host") != 1 && !http10 {
        return Err(bad("host"));
    }
    if count("content-length") > 1 {
        return Err(bad("content-length"));
    }
    let request =
        Request { method: method.into(), path: path.into(), query: parse_query(query)?, headers, keep_alive: false };
    if request.header("transfer-encoding").is_some() {
        return Err(ReadError::Body);
    }
    if let Some(n) = request.header("content-length") {
        match n.parse::<u64>() {
            Ok(0) => {}
            Ok(_) => return Err(ReadError::Body),
            Err(_) => return Err(bad("content-length")),
        }
    }
    let connection = request.header("connection").unwrap_or_default().to_ascii_lowercase();
    let keep_alive = if http10 { connection == "keep-alive" } else { connection != "close" };
    Ok(Request { keep_alive, ..request })
}

fn parse_query(query: &str) -> Result<Vec<(String, String)>, ReadError> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match (decode(k, true), decode(v, true)) {
                (Some(k), Some(v)) => Ok((k, v)),
                _ => Err(ReadError::Malformed("query")),
            }
        })
        .collect()
}

/// Percent-decodes `s`, and `+` as a space when `plus` is set.
fn decode(s: &str, plus: bool) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' if plus => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(head: &str) -> Result<Request, ReadError> {
        parse(head.as_bytes())
    }

    #[test]
    fn parses_requests() {
        let r = req("GET /v1/databases/ab%20c/games?q=white%3Amorphy+x&limit=5&flag HTTP/1.1\r\nHost: 127.0.0.1:1\r\nOrigin: https://oschess.org").unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.segments().unwrap(), ["v1", "databases", "ab c", "games"]);
        assert_eq!(r.param("q"), Some("white:morphy x"));
        assert_eq!(r.param("limit"), Some("5"));
        assert_eq!(r.param("flag"), Some(""));
        assert_eq!(r.header("origin"), Some("https://oschess.org"));
        assert!(r.keep_alive);
        let r = req("GET / HTTP/1.1\r\nHost: h\r\nConnection: close").unwrap();
        assert!(!r.keep_alive);
        assert!(!req("GET / HTTP/1.0").unwrap().keep_alive);
    }

    #[test]
    fn finds_the_origin_of_a_refused_request() {
        let head = b"GET /?q=%zz HTTP/1.1\r\nHost: h\r\norigin:  https://oschess.org \r\n\r\nOrigin: later";
        assert_eq!(sniff_origin(head).as_deref(), Some("https://oschess.org"));
        assert_eq!(sniff_origin(b"GET / HTTP/1.1\r\nHost: h"), None);
        assert_eq!(sniff_origin(b"Origin: first-line-is-the-request-line"), None);
    }

    #[test]
    fn refuses_malformed_and_bodies() {
        for head in [
            "GET / HTTP/2.0\r\nHost: h",
            "GET  / HTTP/1.1\r\nHost: h",
            "GET http://x/ HTTP/1.1\r\nHost: h",
            "GET / HTTP/1.1",
            "GET / HTTP/1.1\r\nHost: a\r\nHost: b",
            "GET / HTTP/1.1\r\nHost: h\r\n folded",
            "GET / HTTP/1.1\r\nHost: h\r\nBad Name: x",
            "GET /?q=%zz HTTP/1.1\r\nHost: h",
            "GET /?q=%ff HTTP/1.1\r\nHost: h",
            "GET / HTTP/1.1\r\nHost: h\r\nContent-Length: x",
        ] {
            assert!(matches!(req(head), Err(ReadError::Malformed(_))), "{head:?}");
        }
        assert_eq!(req("GET / HTTP/1.1\r\nHost: h\r\nContent-Length: 3").err(), Some(ReadError::Body));
        assert_eq!(req("GET / HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked").err(), Some(ReadError::Body));
        assert!(req("GET / HTTP/1.1\r\nHost: h\r\nContent-Length: 0").is_ok());
    }
}
