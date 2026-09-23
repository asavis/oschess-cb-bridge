//! The accept loops: one thread per connection, at most [`MAX_CONNECTIONS`]
//! across the IPv4 and IPv6 loopback listeners.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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

/// Serves connections from every listener until they fail.
pub fn serve(listeners: Vec<TcpListener>, app: Arc<App>) -> io::Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for listener in listeners {
            let (app, active) = (app.clone(), active.clone());
            scope.spawn(move || accept(listener, app, active));
        }
    });
    Ok(())
}

fn accept(listener: TcpListener, app: Arc<App>, active: Arc<AtomicUsize>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            refuse_busy(stream);
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

fn refuse_busy(stream: TcpStream) {
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let _ = Conn::new(stream).write(&error(503, "busy", "Too many open connections"), false);
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
