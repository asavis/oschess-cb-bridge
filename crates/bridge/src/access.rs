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
        if req.method != "GET" {
            return refuse(error(405, "method_not_allowed", "Only GET is served").header("Allow", "GET, OPTIONS"));
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

/// Compares without an early exit, so the time taken tells nothing about how
/// much of the token matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn preflight(req: &Request, origin: &str) -> Response {
    let mut r = Response::empty(204)
        .header("Access-Control-Allow-Origin", origin)
        .header("Access-Control-Allow-Methods", "GET")
        .header("Access-Control-Allow-Headers", "Authorization")
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
        Some(o) => r.header("Access-Control-Allow-Origin", o).header("Access-Control-Expose-Headers", "Retry-After"),
        None => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_comparison() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
