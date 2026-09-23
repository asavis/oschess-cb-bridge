//! Responses in the shape `docs/api.md` specifies.

use crate::http::Response;
use crate::json::Obj;

/// An error response: `{"error": {"code": ..., "message": ...}}`.
pub fn error(status: u16, code: &str, message: &str) -> Response {
    error_with(status, code, message, |o| o)
}

/// An error response whose error object carries further fields after `code`
/// and `message`, added by `extra`.
pub fn error_with(status: u16, code: &str, message: &str, extra: impl FnOnce(Obj) -> Obj) -> Response {
    let body = extra(Obj::new().str("code", code).str("message", message)).done();
    let r = Response::json(status, Obj::new().raw("error", &body).done());
    if status == 503 { r.header("Retry-After", "1") } else { r }
}

pub fn ok(body: String) -> Response {
    Response::json(200, body)
}

pub fn bad_parameter(parameter: &str, message: &str) -> Response {
    error_with(400, "bad_request", message, |o| o.str("parameter", parameter))
}

pub fn not_found() -> Response {
    error(404, "not_found", "No such resource")
}
