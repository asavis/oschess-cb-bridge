//! HTTP/1.1, as much of it as the bridge serves: requests without bodies, JSON
//! responses, persistent connections, and bodies of JSON lines streamed as
//! chunks.

use std::io::{self, IoSlice, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::{Duration, Instant};

/// The request line and headers together may not exceed this.
pub const MAX_HEAD: usize = 16 << 10;
/// How long a connection may sit idle between requests.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a request may take to arrive once it has begun.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a closing connection goes on reading what its client still sends;
/// see [`Conn::close`].
pub const LINGER: Duration = Duration::from_secs(2);
/// At most this much is read and dropped while a connection closes.
const LINGER_BYTES: usize = 1 << 20;

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

    /// `GET target` read as the server reads it, with `Host` its only header:
    /// for the tests of what an endpoint makes of its parameters.
    #[cfg(test)]
    pub(crate) fn get(target: &str) -> Request {
        parse(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1").as_bytes()).expect("a well-formed request")
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
    /// How long to wait for a request to begin: [`IDLE_TIMEOUT`] unless
    /// [`Conn::idle`] sets another.
    idle: Duration,
}

impl Conn {
    pub fn new(stream: TcpStream) -> Self {
        // An answer's head and body are written at once, so nothing is gained
        // by holding a small segment back for the client's delayed
        // acknowledgement, which costs 40 ms a request on Linux (#142).
        let _ = stream.set_nodelay(true);
        Conn { stream, buf: Vec::new(), idle: IDLE_TIMEOUT }
    }

    /// The connection, waiting `idle` for each request to begin in place of
    /// [`IDLE_TIMEOUT`].
    pub fn idle(mut self, idle: Duration) -> Self {
        self.idle = idle;
        self
    }

    /// The next request: its first byte within the connection's idle wait,
    /// the rest within [`REQUEST_TIMEOUT`] of it.
    pub fn read_request(&mut self) -> Result<Request, Refusal> {
        self.read_request_waiting(self.idle, REQUEST_TIMEOUT)
    }

    /// [`Conn::read_request`] with `limit` in place of [`REQUEST_TIMEOUT`],
    /// and no longer than that to wait for the first byte either.
    pub fn read_request_within(&mut self, limit: Duration) -> Result<Request, Refusal> {
        self.read_request_waiting(self.idle.min(limit), limit)
    }

    /// Waits `idle` for a request to begin, then `limit` from its first byte
    /// for the rest of it.
    fn read_request_waiting(&mut self, idle: Duration, limit: Duration) -> Result<Request, Refusal> {
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
                None => idle,
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

    /// Writes `response`'s head, then the lines `body` produces as chunks of
    /// `application/x-ndjson`, and closes the body when it returns. The
    /// connection is not reused afterwards.
    pub fn write_stream(&mut self, response: &Response, body: Stream) -> io::Result<()> {
        let framing = "Content-Type: application/x-ndjson; charset=utf-8\r\nTransfer-Encoding: chunked\r\n";
        self.stream.write_all(head(response, framing, false).as_bytes())?;
        self.stream.flush()?;
        let mut chunks = Chunks { stream: &mut self.stream, failed: false };
        body(&mut chunks);
        if chunks.failed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "the client left"));
        }
        self.stream.write_all(b"0\r\n\r\n")?;
        self.stream.flush()
    }

    pub fn write(&mut self, response: &Response, keep_alive: bool) -> io::Result<()> {
        write_answer(&mut self.stream, response, keep_alive)
    }

    /// Ends the connection. A close with unread bytes resets the connection,
    /// and the reset can cost the client the answer written just before it:
    /// the rest of a refused request, still arriving, did that (#217). So the
    /// write side is shut first, which ends the answer, and what the client
    /// still sends is read and dropped until it closes, `linger` has passed,
    /// or [`LINGER_BYTES`] were dropped.
    pub fn close(mut self, linger: Duration) {
        let _ = self.stream.shutdown(Shutdown::Write);
        let deadline = Instant::now() + linger;
        let mut dropped = 0;
        let mut chunk = [0u8; 4096];
        while dropped < LINGER_BYTES {
            let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else {
                return;
            };
            if self.stream.set_read_timeout(Some(left)).is_err() {
                return;
            }
            match self.stream.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => dropped += n,
                // A read time-out counts in the kernel's ticks and can end a
                // little before the deadline; the loop looks at it again.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return,
            }
        }
    }
}

/// The head of `response`: the status line, the response's own headers, then
/// `framing`, the headers that say how its body is sent, and last the headers
/// every answer carries, ending the head.
fn head(response: &Response, framing: &str, keep_alive: bool) -> String {
    let mut head = format!("HTTP/1.1 {} {}\r\n", response.status, reason(response.status));
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(framing);
    head.push_str(&format!(
        "Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: {}\r\n\r\n",
        if keep_alive { "keep-alive" } else { "close" }
    ));
    head
}

/// Writes `response` whole, its head and its body, to `out`: see
/// [`write_both`].
fn write_answer(out: &mut impl Write, response: &Response, keep_alive: bool) -> io::Result<()> {
    let mut framing = String::new();
    if !response.body.is_empty() {
        framing.push_str("Content-Type: application/json; charset=utf-8\r\n");
    }
    framing.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
    write_both(out, head(response, &framing, keep_alive).as_bytes(), response.body.as_bytes())?;
    out.flush()
}

/// Writes `head` then `body` as one vectored write, so that they usually leave
/// in the same segments, without copying the body: an answer's memory is
/// reserved once, for the body (#142).
fn write_both(stream: &mut impl Write, head: &[u8], body: &[u8]) -> io::Result<()> {
    let mut slices = [IoSlice::new(head), IoSlice::new(body)];
    let mut rest = &mut slices[..];
    while !rest.is_empty() {
        match stream.write_vectored(rest) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => IoSlice::advance_slices(&mut rest, n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Where a streamed body goes, one line at a time.
pub trait Sink {
    /// Writes `text` and a line end as one chunk; an error means the client left.
    fn line(&mut self, text: &str) -> io::Result<()>;
    /// Whether the client has closed the connection or a write failed.
    fn gone(&mut self) -> bool;
}

/// A body produced while it is sent: see [`Conn::write_stream`].
pub type Stream = Box<dyn FnOnce(&mut dyn Sink) + Send>;

struct Chunks<'a> {
    stream: &'a mut TcpStream,
    failed: bool,
}

impl Sink for Chunks<'_> {
    fn line(&mut self, text: &str) -> io::Result<()> {
        let mut chunk = format!("{:x}\r\n", text.len() + 1).into_bytes();
        chunk.extend_from_slice(text.as_bytes());
        chunk.extend_from_slice(b"\n\r\n");
        let written = self.stream.write_all(&chunk).and_then(|()| self.stream.flush());
        self.failed |= written.is_err();
        written
    }

    fn gone(&mut self) -> bool {
        if self.failed || self.stream.set_nonblocking(true).is_err() {
            return true;
        }
        let mut byte = [0u8; 1];
        // A closed connection reads as its end; a live one has nothing to read.
        let gone = match self.stream.peek(&mut byte) {
            Ok(0) => true,
            Ok(_) => false,
            Err(e) => e.kind() != io::ErrorKind::WouldBlock,
        };
        gone | self.stream.set_nonblocking(false).is_err()
    }
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: String,
    /// Budget held for the body until the response is written and dropped.
    pub hold: Option<crate::budget::Reservation>,
    /// A body of JSON lines produced while it is sent, in place of `body`.
    pub stream: Option<Stream>,
}

impl Response {
    pub fn json(status: u16, body: String) -> Self {
        Response { status, headers: Vec::new(), body, hold: None, stream: None }
    }

    pub fn empty(status: u16) -> Self {
        Response { status, headers: Vec::new(), body: String::new(), hold: None, stream: None }
    }

    /// A response whose body `body` writes as lines while it is sent.
    pub fn stream(status: u16, body: impl FnOnce(&mut dyn Sink) + Send + 'static) -> Self {
        Response { status, headers: Vec::new(), body: String::new(), hold: None, stream: Some(Box::new(body)) }
    }

    /// Keeps `reservation` until the response is dropped, after its write.
    pub fn holding(mut self, reservation: crate::budget::Reservation) -> Self {
        self.hold = Some(reservation);
        self
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
        502 => "Bad Gateway",
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
    use std::net::TcpListener;

    use super::*;

    fn req(head: &str) -> Result<Request, ReadError> {
        parse(head.as_bytes())
    }

    /// A closing connection reads its client for `linger` at most when the
    /// client neither sends nor closes, and for [`LINGER_BYTES`] at most when
    /// it never stops sending.
    #[test]
    fn a_closing_connection_reads_its_client_within_bounds() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let silent = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let conn = Conn::new(listener.accept().unwrap().0);
        // The bounds a loaded machine can stall a thread for and still pass:
        // the tests' patience, and twice it for a wait told from it (#238).
        let patience = crate::search::workers::tests::PATIENCE;
        let started = Instant::now();
        conn.close(Duration::from_millis(100));
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(100) && waited < patience, "{waited:?}");
        drop(silent);
        let sender = std::thread::spawn(move || {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            while s.write_all(&[b'x'; 1 << 16]).is_ok() {}
        });
        let conn = Conn::new(listener.accept().unwrap().0);
        let started = Instant::now();
        conn.close(patience * 2);
        assert!(started.elapsed() < patience, "the close waited for the linger");
        sender.join().unwrap();
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

    /// How long a connection waits for a request to begin, told by the read
    /// time-out it sets before its first read: `idle` in full, even past
    /// [`REQUEST_TIMEOUT`] (#217), and no longer than a limit given.
    #[test]
    fn a_request_is_awaited_for_the_idle_wait() {
        let wait = |idle: Option<Duration>, limit: Option<Duration>| {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let mut conn = Conn::new(listener.accept().unwrap().0);
            if let Some(idle) = idle {
                conn = conn.idle(idle);
            }
            // The client has left, so the first read ends at once.
            drop(client);
            let read = match limit {
                Some(limit) => conn.read_request_within(limit),
                None => conn.read_request(),
            };
            assert_eq!(read.err().map(|r| r.error), Some(ReadError::Closed));
            conn.stream.read_timeout().unwrap()
        };
        assert_eq!(wait(None, None), Some(IDLE_TIMEOUT));
        let long = REQUEST_TIMEOUT * 30;
        assert_eq!(wait(Some(long), None), Some(long));
        let short = Duration::from_millis(500);
        assert_eq!(wait(Some(long), Some(short)), Some(short));
        assert_eq!(wait(None, Some(short)), Some(short));
    }

    /// A writer that takes all it is given in each write, as a socket with
    /// room for it does, and counts the writes.
    #[derive(Default)]
    struct Counting {
        writes: usize,
        bytes: Vec<u8>,
    }

    impl Write for Counting {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.write_vectored(&[IoSlice::new(buf)])
        }

        fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
            self.writes += 1;
            let before = self.bytes.len();
            bufs.iter().for_each(|b| self.bytes.extend_from_slice(b));
            Ok(self.bytes.len() - before)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// An answer is one write, its head and its body together (#142): written
    /// in two, the second waited on a kept connection for the client's
    /// delayed acknowledgement of the first, 40 ms on Linux and longer
    /// elsewhere. A test that timed round trips failed under load (#250);
    /// this one counts the writes.
    #[test]
    fn an_answer_is_one_write() {
        for (response, keep_alive) in [
            (Response::json(200, r#"{"a":1}"#.into()).header("Vary", "Origin"), true),
            (Response::json(200, "x".repeat(1 << 20)), false),
            (Response::empty(204), true),
        ] {
            let mut out = Counting::default();
            write_answer(&mut out, &response, keep_alive).unwrap();
            assert_eq!(out.writes, 1, "{}", response.status);
            let text = String::from_utf8(out.bytes).unwrap();
            assert!(text.starts_with("HTTP/1.1 ") && text.ends_with(&format!("\r\n\r\n{}", response.body)));
        }
    }

    /// A connection the bridge accepts sends each write at once, without
    /// holding a small one back for the client's acknowledgement of the one
    /// before (#142): `Conn::new`, through which the bridge serves or refuses
    /// every connection, turns Nagle's algorithm off.
    #[test]
    fn an_accepted_connection_sends_without_delay() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let accepted = listener.accept().unwrap().0;
        assert!(!accepted.nodelay().unwrap(), "a new socket delays small writes");
        assert!(Conn::new(accepted).stream.nodelay().unwrap());
    }

    /// The bytes a client reads when `send` writes to the connection accepted
    /// from it.
    fn written(send: impl FnOnce(&mut Conn)) -> String {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut conn = Conn::new(listener.accept().unwrap().0);
        send(&mut conn);
        drop(conn);
        let mut out = String::new();
        client.read_to_string(&mut out).unwrap();
        out
    }

    /// Every head is the status line with its reason phrase, `502` among them
    /// (#173), the answer's own headers, how its body is sent, and the headers
    /// every answer carries.
    #[test]
    fn heads_are_written_whole() {
        let shared = "Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n";
        let json =
            written(|c| c.write(&Response::json(502, r#"{"a":1}"#.into()).header("Vary", "Origin"), true).unwrap());
        assert_eq!(
            json,
            format!(
                "HTTP/1.1 502 Bad Gateway\r\nVary: Origin\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: 7\r\n{shared}Connection: keep-alive\r\n\r\n{{\"a\":1}}"
            )
        );
        let empty = written(|c| c.write(&Response::empty(204), false).unwrap());
        assert_eq!(empty, format!("HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n{shared}Connection: close\r\n\r\n"));
        let lines = written(|c| {
            let body: Stream = Box::new(|sink: &mut dyn Sink| sink.line("{}").unwrap());
            c.write_stream(&Response::empty(200).header("Vary", "Origin"), body).unwrap();
        });
        assert_eq!(
            lines,
            format!(
                "HTTP/1.1 200 OK\r\nVary: Origin\r\nContent-Type: application/x-ndjson; charset=utf-8\r\nTransfer-Encoding: chunked\r\n{shared}Connection: close\r\n\r\n3\r\n{{}}\n\r\n0\r\n\r\n"
            )
        );
    }
}
