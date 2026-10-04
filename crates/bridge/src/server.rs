//! The accept loops: one thread per connection, at most [`MAX_CONNECTIONS`]
//! across the IPv4 and IPv6 loopback listeners.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::time::{Duration, Instant};

use crate::access::{Verdict, cors, takes_body};
use crate::api::{self, App};
use crate::http::{Conn, LINGER, MAX_BODY, REQUEST_TIMEOUT, ReadError, Refusal, Request, Response};
use crate::reply::error;

pub const MAX_CONNECTIONS: usize = 32;

/// Listens on `127.0.0.1:port` and on `[::1]` at the same port. Port 0 picks
/// a free port that both can use (tests). A machine without IPv6 loopback
/// gets the IPv4 listener alone.
pub fn bind(port: u16) -> io::Result<Vec<TcpListener>> {
    let mut attempts = if port == 0 { 20 } else { 1 };
    loop {
        attempts -= 1;
        let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
        let chosen = v4.local_addr()?.port();
        match TcpListener::bind((Ipv6Addr::LOCALHOST, chosen)) {
            Ok(v6) => return Ok(vec![v4, v6]),
            // A free IPv4 port may be taken on IPv6: pick another.
            Err(e) if e.kind() == io::ErrorKind::AddrInUse && attempts > 0 => continue,
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => return Err(e),
            // No IPv6 loopback on this machine.
            Err(_) => return Ok(vec![v4]),
        }
    }
}

/// Connections over the cap waiting for their `busy` answer; more are closed.
const BUSY_QUEUE: usize = 64;
/// Connections at most that go on reading their client after the answer
/// ([`Conn::close`]); past it a connection closes at once. A connection frees
/// its slot before its client can see it end, so the cap no longer bounds a
/// draining thread and its socket, and a client that keeps its sockets open
/// would otherwise leave a thread draining for each answer (review of #231).
const MAX_DRAINING: usize = MAX_CONNECTIONS;
/// How long after acceptance the busy answer may wait for a request head, to
/// read its `Origin`, unless [`App::busy_read`] sets another. The deadline
/// runs from acceptance, not from the moment the refusing thread reaches the
/// connection, so silent connections ahead in the queue cannot add their wait
/// to the ones behind them.
pub const BUSY_READ: Duration = Duration::from_millis(500);
/// What a connection past its deadline still gets: a read of the bytes already
/// there, enough for a request that arrived in time.
const BUSY_LAST_LOOK: Duration = Duration::from_millis(5);

/// Serves connections from every listener until they fail.
pub fn serve(listeners: Vec<TcpListener>, app: Arc<App>) -> io::Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    let draining = Arc::new(AtomicUsize::new(0));
    let (busy, refused) = mpsc::sync_channel::<(TcpStream, Instant)>(BUSY_QUEUE);
    let thread = |name: &str| std::thread::Builder::new().name(name.into()).stack_size(crate::THREAD_STACK);
    std::thread::scope(|scope| {
        let refuser = app.clone();
        thread("bridge-busy").spawn_scoped(scope, move || {
            for (stream, accepted) in refused {
                refuse_busy(stream, accepted, &refuser);
            }
        })?;
        for listener in listeners {
            let (app, active, draining, busy) = (app.clone(), active.clone(), draining.clone(), busy.clone());
            thread("bridge-accept")
                .spawn_scoped(scope, move || accept(listener, app, active, draining, busy, LINGER))?;
        }
        drop(busy);
        Ok(())
    })
}

/// Accepts connections from `listener`, at most [`MAX_CONNECTIONS`] served
/// at once counted in `active`, and at most [`MAX_DRAINING`] of them reading
/// their client for `linger` after the answer, counted in `draining`.
fn accept(
    listener: TcpListener,
    app: Arc<App>,
    active: Arc<AtomicUsize>,
    draining: Arc<AtomicUsize>,
    busy: SyncSender<(TcpStream, Instant)>,
    linger: Duration,
) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            // One thread answers them in turn; a full queue closes the connection.
            let _ = busy.try_send((stream, Instant::now()));
            continue;
        }
        let guard = Active(active.clone());
        let (app, draining) = (app.clone(), draining.clone());
        let spawned =
            std::thread::Builder::new().name("bridge-conn".into()).stack_size(crate::THREAD_STACK).spawn(move || {
                let conn = handle_connection(stream, &app);
                // The slot is free before the connection ends: a client that
                // has seen it end may open another at once and be served.
                drop(guard);
                // A drain slot is taken only while one is free, so `draining`
                // never counts past the bound, even for a moment.
                let drains =
                    draining.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |d| (d < MAX_DRAINING).then_some(d + 1));
                if drains.is_ok() {
                    let _drain = Active(draining);
                    conn.close(linger);
                } else {
                    conn.close(Duration::ZERO);
                }
            });
        // A failed spawn drops the closure, and with it the guard and the stream.
        drop(spawned);
    }
}

/// Counts a live connection until dropped, even when its thread panics.
/// Dropped before the connection's socket closes, never after.
struct Active(Arc<AtomicUsize>);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Answers a connection over the cap `503 busy`. The request head is read
/// until [`App::busy_read`] after acceptance, and its first byte no longer
/// than the app's idle wait, so that an allowed page can read the answer and
/// its retry delay through CORS.
fn refuse_busy(stream: TcpStream, accepted: Instant, app: &App) {
    let _ = stream.set_write_timeout(Some(BUSY_READ));
    let mut conn = Conn::new(stream).idle(app.idle_timeout);
    let wait = app.busy_read.saturating_sub(accepted.elapsed()).max(BUSY_LAST_LOOK);
    let origin = match conn.read_request_within(wait) {
        Ok(req) => req.header("origin").map(str::to_string),
        Err(refusal) => refusal.origin,
    };
    let response = error(503, "busy", "Too many open connections");
    let _ = conn.write(&cors(response, app.policy.allowed_origin(origin.as_deref())), false);
    // One thread answers every connection over the cap, so it waits for no
    // client; the answer still ends before a reset could cut it.
    conn.close(Duration::ZERO);
}

/// Serves the requests of one connection until it ends, and returns it still
/// open, for the caller to close.
fn handle_connection(stream: TcpStream, app: &App) -> Conn {
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let mut conn = Conn::new(stream).idle(app.idle_timeout);
    loop {
        let (mut response, keep_alive): (Response, bool) = match conn.read_request() {
            Ok(mut req) => match answer(app, &mut conn, &mut req) {
                Some(answer) => answer,
                None => return conn,
            },
            Err(Refusal { error: ReadError::Closed | ReadError::Dropped, .. }) => return conn,
            Err(Refusal { error: refused, origin }) => {
                let response = match refused {
                    ReadError::TooLarge => error(431, "headers_too_large", "Request line and headers exceed 16 KiB"),
                    ReadError::Body => error(413, "body_not_allowed", "A body is sent with Content-Length"),
                    ReadError::Malformed(what) => error(400, "bad_request", &format!("Malformed request: {what}")),
                    ReadError::Closed | ReadError::Dropped => return conn,
                };
                // The page that sent it can read the refusal when its origin is allowed.
                (cors(response, app.policy.allowed_origin(origin.as_deref())), false)
            }
        };
        if let Some(body) = response.stream.take() {
            let _ = conn.write_stream(&response, body);
            return conn;
        }
        if conn.write(&response, keep_alive).is_err() || !keep_alive {
            return conn;
        }
    }
}

/// The answer to `req`, and whether the connection may serve another
/// request after it: the policy's verdict first, then the body, read when the
/// request may carry one. `None` when the body did not arrive in time. A
/// request refused with its body unread ends the connection, which reads and
/// drops the body as it closes.
fn answer(app: &App, conn: &mut Conn, req: &mut Request) -> Option<(Response, bool)> {
    let origin = match api::admit(app, req) {
        Verdict::Answer(response) => return Some((response, req.keep_alive && req.body_len == 0)),
        Verdict::Serve { origin } => origin,
    };
    if req.body_len > 0 {
        let refusal = if !takes_body(req) {
            Some(error(413, "body_not_allowed", "This request carries no body"))
        } else if req.body_len > MAX_BODY {
            Some(error(413, "body_too_large", "The body is over 4 MiB"))
        } else {
            None
        };
        if let Some(refusal) = refusal {
            return Some((cors(refusal, origin.as_deref()), false));
        }
        req.body = conn.read_body(req.body_len as usize).ok()?;
    }
    Some((api::serve(app, req, origin.as_deref()), req.keep_alive))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;
    use crate::access::{DEFAULT_ORIGINS, Policy};
    use crate::catalog::Catalog;
    use crate::search::workers::tests::PATIENCE;

    /// A test's app on `port`, whose connections wait [`PATIENCE`] for a
    /// request: a loaded machine can stall a test between its connect and its
    /// write for longer than [`crate::http::IDLE_TIMEOUT`], which these tests
    /// are not about (#217).
    fn app_on(port: u16) -> App {
        let policy = Policy { port, origins: DEFAULT_ORIGINS.map(String::from).to_vec(), token: "t".repeat(43) };
        App { idle_timeout: PATIENCE, ..App::new("test", policy, Catalog::new(Vec::new())) }
    }

    /// A client that has read a connection to its end may open another at
    /// once: the slot is free before the socket closes. Freed after the close,
    /// a descheduled connection thread kept it counted while its client went
    /// on, and a request at the cap was refused (seen on Windows).
    #[test]
    fn a_connection_stops_counting_before_its_client_sees_it_close() {
        let listener = bind(0).unwrap().remove(0);
        let port = listener.local_addr().unwrap().port();
        let app = Arc::new(app_on(port));
        let active = Arc::new(AtomicUsize::new(0));
        let (busy, _refused) = mpsc::sync_channel(BUSY_QUEUE);
        let counted = active.clone();
        std::thread::spawn(move || accept(listener, app, counted, Arc::default(), busy, LINGER));
        let request = format!("GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
        let mut late = 0;
        for _ in 0..500 {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream.write_all(request.as_bytes()).unwrap();
            let mut answer = Vec::new();
            stream.read_to_end(&mut answer).unwrap();
            assert!(answer.starts_with(b"HTTP/1.1 401"));
            late += usize::from(active.load(Ordering::SeqCst) != 0);
        }
        assert_eq!(late, 0, "connections still counted after their client saw them close");
    }

    /// Connections that read their client after the answer are bounded too
    /// (review of #231): a client that keeps every socket it was answered on
    /// open leaves [`MAX_DRAINING`] draining and no more, and is answered
    /// whole every time. With a long linger, the first ones drain for the
    /// whole test.
    #[test]
    fn draining_connections_are_bounded() {
        let listener = bind(0).unwrap().remove(0);
        let port = listener.local_addr().unwrap().port();
        let app = Arc::new(app_on(port));
        let draining = Arc::new(AtomicUsize::new(0));
        let (busy, _refused) = mpsc::sync_channel(BUSY_QUEUE);
        let counted = draining.clone();
        let linger = Duration::from_secs(300);
        std::thread::spawn(move || accept(listener, app, Arc::default(), counted, busy, linger));
        let mut kept = Vec::new();
        for _ in 0..MAX_DRAINING * 3 {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream.write_all(b"bad\r\n\r\n").unwrap();
            let mut answer = Vec::new();
            stream.read_to_end(&mut answer).unwrap();
            assert!(answer.starts_with(b"HTTP/1.1 400"), "{}", String::from_utf8_lossy(&answer));
            assert!(draining.load(Ordering::SeqCst) <= MAX_DRAINING);
            kept.push(stream);
        }
        assert_eq!(draining.load(Ordering::SeqCst), MAX_DRAINING, "the first ones drain, the rest closed at once");
    }

    /// The busy answer waits for a request until [`App::busy_read`] after
    /// the connection was accepted, not after the busy thread reaches it: a
    /// connection accepted as long ago gets a last look only, so silent
    /// connections queued ahead of a request add nothing to its wait, and an
    /// allowed page's request that arrived in time is still read, its
    /// `Origin` with it (#238). A busy read twice the test's patience tells
    /// the two apart however slow the machine is: from acceptance, every
    /// connection is answered at once; from the moment the thread reaches it,
    /// the first silent one alone would hold it for the whole busy read, past
    /// the patience of the request behind. The app's idle wait, which also
    /// bounds the wait for a first byte, is as long. A test that timed the
    /// answer failed under load.
    #[test]
    fn the_busy_wait_runs_from_acceptance() {
        let busy_read = PATIENCE * 2;
        let Some(accepted_at) = Instant::now().checked_sub(busy_read) else {
            eprintln!("skipped: this computer's clock does not go back {busy_read:?}");
            return;
        };
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Arc::new(App { busy_read, idle_timeout: busy_read, ..app_on(port) });
        // The silent ones first, then the request: each accepted before the
        // next connects, so that none overtakes another.
        let (mut silent, mut accepted) = (Vec::new(), Vec::new());
        for _ in 0..12 {
            silent.push(TcpStream::connect(("127.0.0.1", port)).unwrap());
            accepted.push(listener.accept().unwrap().0);
        }
        let mut asking = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let origin = DEFAULT_ORIGINS[0];
        let request = format!("GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: {origin}\r\n\r\n");
        asking.write_all(request.as_bytes()).unwrap();
        asking.set_read_timeout(Some(PATIENCE)).unwrap();
        accepted.push(listener.accept().unwrap().0);
        let refusing = std::thread::spawn(move || {
            for stream in accepted {
                refuse_busy(stream, accepted_at, &app);
            }
        });
        let mut answer = String::new();
        let read = asking.read_to_string(&mut answer);
        assert!(read.is_ok(), "no answer within {PATIENCE:?}: {read:?}");
        assert!(answer.starts_with("HTTP/1.1 503 "), "{answer}");
        assert!(answer.contains(&format!("\r\nAccess-Control-Allow-Origin: {origin}\r\n")), "{answer}");
        refusing.join().unwrap();
        drop(silent);
    }
}
