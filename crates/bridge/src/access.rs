//! The checks every request passes before it is served, and CORS
//! (`docs/api.md`, "Access").

use crate::http::{Request, Response};
use crate::reply::error;

/// Origins the bridge always serves.
pub const DEFAULT_ORIGINS: [&str; 3] =
    ["https://oschess.org", "https://www.oschess.org", "https://staging.oschess.org"];

pub struct Policy {
    pub port: u16,
    /// Allowed `Origin` values, compared exactly.
    pub origins: Vec<String>,
    /// The pairing token.
    pub token: String,
}

pub enum Verdict {
    /// Send this response: a preflight answer or a refusal.
    Answer(Response),
    /// Serve the request, echoing `origin` in the CORS headers when present.
    Serve { origin: Option<String> },
}

impl Policy {
    pub fn check(&self, req: &Request) -> Verdict {
        if !self.host_ok(req.header("host")) {
            let refusal = error(421, "misdirected_host", "Host is not a loopback name for this bridge");
            return Verdict::Answer(cors(refusal, self.allowed_origin(req.header("origin"))));
        }
        let origin = match req.header("origin") {
            None => None,
            Some(o) => match self.allowed_origin(Some(o)) {
                Some(o) => Some(o.to_string()),
                None => return Verdict::Answer(error(403, "forbidden_origin", "Origin is not allowed")),
            },
        };
        if req.method == "OPTIONS" {
            return Verdict::Answer(match &origin {
                Some(o) => preflight(req, o),
                None => error(403, "forbidden_origin", "A preflight needs an allowed Origin"),
            });
        }
        let refuse = |r: Response| Verdict::Answer(cors(r, origin.as_deref()));
        if !self.token_ok(req.header("authorization")) {
            return refuse(error(401, "unauthorized", "The pairing token is missing or wrong"));
        }
        let methods = methods(req);
        if !methods.contains(&req.method.as_str()) {
            let allow = format!("{}, OPTIONS", methods.join(", "));
            return refuse(
                error(405, "method_not_allowed", "The method is not served at this path").header("Allow", allow),
            );
        }
        Verdict::Serve { origin }
    }

    /// `origin` when it is on the allowlist.
    pub fn allowed_origin<'a>(&self, origin: Option<&'a str>) -> Option<&'a str> {
        origin.filter(|o| self.origins.iter().any(|allowed| allowed == o))
    }

    fn host_ok(&self, host: Option<&str>) -> bool {
        let Some(host) = host else { return false };
        let port = self.port;
        [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")]
            .iter()
            .any(|h| h.eq_ignore_ascii_case(host))
    }

    fn token_ok(&self, header: Option<&str>) -> bool {
        let Some((scheme, token)) = header.and_then(|h| h.split_once(' ')) else { return false };
        scheme.eq_ignore_ascii_case("bearer") && constant_time_eq(token.trim().as_bytes(), self.token.as_bytes())
    }
}

/// The methods served at `req`'s path besides `OPTIONS`: `GET` everywhere,
/// and the writes of the games of a PGN database (`docs/api.md`, "Writing
/// games") at theirs.
fn methods(req: &Request) -> &'static [&'static str] {
    match path_kind(req) {
        Some(Games::List) => &["GET", "POST"],
        Some(Games::One) => &["GET", "PUT", "DELETE"],
        None => &["GET"],
    }
}

/// The paths that take writes.
enum Games {
    /// `/v1/databases/{id}/games`.
    List,
    /// `/v1/databases/{id}/games/{number}`.
    One,
}

fn path_kind(req: &Request) -> Option<Games> {
    let segments = req.segments()?;
    match segments.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["v1", "databases", _, "games"] => Some(Games::List),
        ["v1", "databases", _, "games", _] => Some(Games::One),
        _ => None,
    }
}

/// Whether `req` may carry a body: `POST` of a database's games and `PUT` of
/// one game, which carry the game's PGN.
pub fn takes_body(req: &Request) -> bool {
    matches!((req.method.as_str(), path_kind(req)), ("POST", Some(Games::List)) | ("PUT", Some(Games::One)))
}

/// Compares without an early exit, so the time taken tells nothing about how
/// much of the token matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn preflight(req: &Request, origin: &str) -> Response {
    let mut r = Response::empty(204)
        .header("Access-Control-Allow-Origin", origin)
        .header("Access-Control-Allow-Methods", "GET, POST, PUT, DELETE")
        .header("Access-Control-Allow-Headers", "Authorization, Content-Type, If-Match")
        .header("Access-Control-Max-Age", "600")
        .header("Vary", "Origin");
    if req.header("access-control-request-private-network").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
        r = r.header("Access-Control-Allow-Private-Network", "true");
    }
    r
}

/// Adds the CORS headers of an actual response to an allowed origin.
pub fn cors(r: Response, origin: Option<&str>) -> Response {
    let r = r.header("Vary", "Origin");
    match origin {
        Some(o) => {
            r.header("Access-Control-Allow-Origin", o).header("Access-Control-Expose-Headers", "Retry-After, ETag")
        }
        None => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes are served at the paths of a database's games alone, and only
    /// they take a body.
    #[test]
    fn methods_and_bodies_by_path() {
        let request = |method: &str, path: &str| {
            let mut r = Request::get(path);
            r.method = method.into();
            r
        };
        let list = "/v1/databases/0123456789abcdef/games";
        let one = "/v1/databases/0123456789abcdef/games/7";
        assert_eq!(methods(&request("GET", list)), ["GET", "POST"]);
        assert_eq!(methods(&request("GET", one)), ["GET", "PUT", "DELETE"]);
        for path in ["/v1/status", "/v1/databases", "/v1/databases/x/suggest", "/v1/databases/x/games/7/x"] {
            assert_eq!(methods(&request("GET", path)), ["GET"], "{path}");
        }
        assert!(takes_body(&request("POST", list)) && takes_body(&request("PUT", one)));
        for (method, path) in [("POST", one), ("PUT", list), ("DELETE", one), ("GET", list), ("POST", "/v1/status")] {
            assert!(!takes_body(&request(method, path)), "{method} {path}");
        }
    }

    #[test]
    fn constant_time_comparison() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
