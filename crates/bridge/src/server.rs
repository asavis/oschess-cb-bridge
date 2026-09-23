//! The accept loops: one thread per connection, at most [`MAX_CONNECTIONS`]
//! across the IPv4 and IPv6 loopback listeners.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::time::{Duration, Instant};

use crate::access::cors;
use crate::api::{self, App};
use crate::http::{Conn, REQUEST_TIMEOUT, ReadError, Refusal, Response};
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
/// How long after acceptance the busy answer may wait for a request head, to
/// read its `Origin`. The deadline runs from acceptance, not from the moment the
/// refusing thread reaches the connection, so silent connections ahead in the
/// queue cannot add their wait to the ones behind them.
const BUSY_READ: Duration = Duration::from_millis(500);
/// What a connection past its deadline still gets: a read of the bytes already
/// there, enough for a request that arrived in time.
const BUSY_LAST_LOOK: Duration = Duration::from_millis(5);

/// Serves connections from every listener until they fail.
pub fn serve(listeners: Vec<TcpListener>, app: Arc<App>) -> io::Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    let (busy, refused) = mpsc::sync_channel::<(TcpStream, Instant)>(BUSY_QUEUE);
    std::thread::scope(|scope| {
        let refuser = app.clone();
        scope.spawn(move || {
            for (stream, accepted) in refused {
                refuse_busy(stream, accepted, &refuser);
            }
        });
        for listener in listeners {
            let (app, active, busy) = (app.clone(), active.clone(), busy.clone());
            scope.spawn(move || accept(listener, app, active, busy));
        }
        drop(busy);
    });
    Ok(())
}

fn accept(listener: TcpListener, app: Arc<App>, active: Arc<AtomicUsize>, busy: SyncSender<(TcpStream, Instant)>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            // One thread answers them in turn; a full queue closes the connection.
            let _ = busy.try_send((stream, Instant::now()));
            continue;
        }
        let guard = Active(active.clone());
        let app = app.clone();
        let spawned = std::thread::Builder::new().name("bridge-conn".into()).spawn(move || {
            let _guard = guard;
            handle_connection(stream, &app);
        });
        // A failed spawn drops the closure, and with it the guard and the stream.
        drop(spawned);
    }
}

/// Counts a live connection until dropped, even when its thread panics.
struct Active(Arc<AtomicUsize>);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Answers a connection over the cap `503 busy`. The request head is read
/// until [`BUSY_READ`] after acceptance, so that an allowed page can read the
/// answer and its retry delay through CORS.
fn refuse_busy(stream: TcpStream, accepted: Instant, app: &App) {
    let _ = stream.set_write_timeout(Some(BUSY_READ));
    let mut conn = Conn::new(stream);
    let wait = BUSY_READ.saturating_sub(accepted.elapsed()).max(BUSY_LAST_LOOK);
    let origin = match conn.read_request_within(wait) {
        Ok(req) => req.header("origin").map(str::to_string),
        Err(refusal) => refusal.origin,
    };
    let response = error(503, "busy", "Too many open connections");
    let _ = conn.write(&cors(response, app.policy.allowed_origin(origin.as_deref())), false);
}

fn handle_connection(stream: TcpStream, app: &App) {
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let mut conn = Conn::new(stream);
    loop {
        let (response, keep_alive): (Response, bool) = match conn.read_request() {
            Ok(req) => (api::handle(app, &req), req.keep_alive),
            Err(Refusal { error: ReadError::Closed | ReadError::Dropped, .. }) => return,
            Err(Refusal { error: refused, origin }) => {
                let response = match refused {
                    ReadError::TooLarge => error(431, "headers_too_large", "Request line and headers exceed 16 KiB"),
                    ReadError::Body => error(413, "body_not_allowed", "Requests carry no body"),
                    ReadError::Malformed(what) => error(400, "bad_request", &format!("Malformed request: {what}")),
                    ReadError::Closed | ReadError::Dropped => return,
                };
                // The page that sent it can read the refusal when its origin is allowed.
                (cors(response, app.policy.allowed_origin(origin.as_deref())), false)
            }
        };
        if conn.write(&response, keep_alive).is_err() || !keep_alive {
            return;
        }
    }
}
