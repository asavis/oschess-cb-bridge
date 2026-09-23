//! The accept loop: one thread per connection, at most [`MAX_CONNECTIONS`].

use std::io;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::api::{self, App};
use crate::http::{Conn, REQUEST_TIMEOUT, ReadError, Response};
use crate::reply::error;

pub const MAX_CONNECTIONS: usize = 32;

/// Listens on `127.0.0.1:port`; port 0 picks a free one (tests).
pub fn bind(port: u16) -> io::Result<TcpListener> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port))
}

/// Serves connections from `listener` until it fails.
pub fn serve(listener: TcpListener, app: Arc<App>) -> io::Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
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
    Ok(())
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
            Err(ReadError::Closed | ReadError::Dropped) => return,
            Err(ReadError::TooLarge) => {
                (error(431, "headers_too_large", "Request line and headers exceed 16 KiB"), false)
            }
            Err(ReadError::Body) => (error(413, "body_not_allowed", "Requests carry no body"), false),
            Err(ReadError::Malformed(what)) => {
                let message = format!("Malformed request: {what}");
                (error(400, "bad_request", &message), false)
            }
        };
        if conn.write(&response, keep_alive).is_err() || !keep_alive {
            return;
        }
    }
}
