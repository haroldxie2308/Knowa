//! HTTP plumbing: response builders with the headers DESIGN §7.4 requires, the error
//! code → status table, query-string parameters and request bodies.

use std::io::{Cursor, Read};

use knowa_model::{Error, ErrorCode, Result};
use serde::Serialize;
use tiny_http::{Header, Request, Response, StatusCode};

pub(crate) type Resp = Response<Box<dyn Read + Send>>;

/// The app's own pages and scripts (DESIGN §7.4).
const APP_CSP: &str =
    "default-src 'self'; img-src 'self' blob:; style-src 'self'; script-src 'self'; connect-src 'self'; frame-ancestors 'none'";

/// Request bodies are query documents and review lists: a review of the 1000 claims a
/// page can hold is about 64 KiB. A larger body is refused before it is buffered.
const MAX_BODY: u64 = 1 << 20;

/// A header from static ASCII text. tiny_http refuses only non-ASCII bytes, and every
/// name and value passed here is an ASCII literal of this crate.
pub(crate) fn header(name: &'static str, value: &'static str) -> Header {
    Header::from_bytes(name, value).expect("header names and values in knowa-web are ASCII literals")
}

fn respond(status: u16, headers: Vec<Header>, body: Vec<u8>) -> Resp {
    let len = body.len();
    Response::new(StatusCode(status), headers, Box::new(Cursor::new(body)), Some(len), None)
}

/// A response with a body of unknown length (blob bytes), sent chunked.
pub(crate) fn stream(headers: Vec<Header>, body: Box<dyn Read + Send>) -> Resp {
    Response::new(StatusCode(200), headers, body, None, None)
}

fn json_headers() -> Vec<Header> {
    vec![
        header("Content-Type", "application/json"),
        header("X-Content-Type-Options", "nosniff"),
        header("Cache-Control", "no-store"),
    ]
}

pub(crate) fn json<T: Serialize>(value: &T) -> Result<Resp> {
    let body = serde_json::to_vec(value).map_err(|e| Error::internal(format!("serializing a response: {e}")))?;
    Ok(respond(200, json_headers(), body))
}

/// DESIGN §7.4's table.
pub(crate) fn status_of(code: ErrorCode) -> u16 {
    match code {
        ErrorCode::Usage | ErrorCode::Invalid => 400,
        ErrorCode::NotFound => 404,
        ErrorCode::Conflict => 409,
        ErrorCode::CursorExpired => 410,
        ErrorCode::Unsupported => 422,
        ErrorCode::Integrity | ErrorCode::Internal => 500,
    }
}

fn error_body(e: &Error) -> Vec<u8> {
    e.to_json().to_string().into_bytes()
}

/// The §6.3 error object with its status.
pub(crate) fn error(e: &Error) -> Resp {
    respond(status_of(e.code), json_headers(), error_body(e))
}

/// 401 for an `/api/` request without the token. The body is the error object, with
/// code `usage` (a missing credential is a missing selection, not a fault), so the app
/// handles it like every other error; the status, not the code, is what says why.
pub(crate) fn unauthorized() -> Resp {
    let e = Error::usage("missing or wrong access token: open the link `knowa serve` printed (it ends in #k=…)");
    let mut headers = json_headers();
    headers.push(header("WWW-Authenticate", "Bearer"));
    respond(401, headers, error_body(&e))
}

/// 403 for a request whose `Host` is not this server's loopback address.
pub(crate) fn forbidden() -> Resp {
    let e = Error::usage("this server only answers requests for 127.0.0.1:<port> or localhost:<port>");
    respond(403, json_headers(), error_body(&e))
}

/// An embedded app file with the app's security headers. `no-cache` makes the browser
/// revalidate, so a rebuilt binary's app is never mixed with a cached one.
pub(crate) fn asset(content_type: &'static str, bytes: &'static [u8]) -> Resp {
    let headers = vec![
        header("Content-Type", content_type),
        header("Content-Security-Policy", APP_CSP),
        header("Referrer-Policy", "no-referrer"),
        header("X-Content-Type-Options", "nosniff"),
        header("Cache-Control", "no-cache"),
    ];
    respond(200, headers, bytes.to_vec())
}

/// The value of the first header named `name` (case-insensitive).
pub(crate) fn header_value<'r>(req: &'r Request, name: &'static str) -> Option<&'r str> {
    req.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str())
}

/// At most `MAX_BODY` bytes, else `invalid`.
pub(crate) fn body(req: &mut Request) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    req.as_reader()
        .take(MAX_BODY + 1)
        .read_to_end(&mut buf)
        .map_err(|e| Error::invalid(format!("reading the request body: {e}")))?;
    if buf.len() as u64 > MAX_BODY {
        return Err(Error::invalid(format!("request body is larger than {MAX_BODY} bytes")));
    }
    Ok(buf)
}

/// Query-string parameters. Each endpoint takes the ones it knows and then calls
/// `finish`, so an unknown or repeated parameter fails instead of being ignored, as an
/// unknown field of a query document does (DESIGN §6.1).
pub(crate) struct Params(Vec<(String, String)>);

impl Params {
    pub fn parse(query: &str) -> Result<Params> {
        let mut out = Vec::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            // A name without `=` has the empty value (URL standard), which `take` refuses.
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            out.push((decode(k)?, decode(v)?));
        }
        Ok(Params(out))
    }

    /// The parameter's value; repeated → `invalid`, empty → `invalid` (omit it instead).
    pub fn take(&mut self, name: &str) -> Result<Option<String>> {
        let mut found: Vec<String> = Vec::new();
        self.0.retain(|(k, v)| {
            if k == name {
                found.push(v.clone());
                false
            } else {
                true
            }
        });
        match found.as_slice() {
            [] => Ok(None),
            [v] if v.is_empty() => Err(Error::invalid(format!("parameter {name} is empty; omit it instead"))),
            [v] => Ok(Some(v.clone())),
            _ => Err(Error::invalid(format!("parameter {name} is given more than once"))),
        }
    }

    pub fn finish(self) -> Result<()> {
        match self.0.first() {
            None => Ok(()),
            Some((k, _)) => Err(Error::invalid(format!("unknown parameter {k:?}"))),
        }
    }
}

/// `application/x-www-form-urlencoded` decoding: `+` is a space, `%XX` a byte, and the
/// result must be UTF-8.
fn decode(s: &str) -> Result<String> {
    let bad = || Error::invalid(format!("malformed query parameter {s:?}"));
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        match b {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = [bytes.next().ok_or_else(bad)?, bytes.next().ok_or_else(bad)?];
                let hex = std::str::from_utf8(&hex).map_err(|_| bad())?;
                out.push(u8::from_str_radix(hex, 16).map_err(|_| bad())?);
            }
            b => out.push(b),
        }
    }
    String::from_utf8(out).map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_code_has_its_status() {
        let table = [
            (ErrorCode::Usage, 400),
            (ErrorCode::Invalid, 400),
            (ErrorCode::NotFound, 404),
            (ErrorCode::Conflict, 409),
            (ErrorCode::CursorExpired, 410),
            (ErrorCode::Unsupported, 422),
            (ErrorCode::Integrity, 500),
            (ErrorCode::Internal, 500),
        ];
        for (code, status) in table {
            assert_eq!(status_of(code), status, "{code:?}");
        }
    }

    #[test]
    fn params_decode_and_refuse_unknown_repeated_and_empty() {
        let mut p = Params::parse("cursor=ep%3A12&limit=5&space=my+notes").unwrap();
        assert_eq!(p.take("cursor").unwrap().as_deref(), Some("ep:12"));
        assert_eq!(p.take("space").unwrap().as_deref(), Some("my notes"));
        assert_eq!(p.take("missing").unwrap(), None);
        assert_eq!(p.take("limit").unwrap().as_deref(), Some("5"));
        p.finish().unwrap();

        assert_eq!(Params::parse("bogus=1").unwrap().finish().unwrap_err().code, ErrorCode::Invalid);
        assert_eq!(Params::parse("a=1&a=2").unwrap().take("a").unwrap_err().code, ErrorCode::Invalid);
        assert_eq!(Params::parse("a=").unwrap().take("a").unwrap_err().code, ErrorCode::Invalid);
        assert_eq!(Params::parse("a=%zz").err().map(|e| e.code), Some(ErrorCode::Invalid));
        assert_eq!(Params::parse("a=%ff").err().map(|e| e.code), Some(ErrorCode::Invalid));
    }
}
