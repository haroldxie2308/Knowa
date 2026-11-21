//! Request dispatch: the Host check for every request, the token for `/api/`, then
//! one `knowa_api` call per endpoint (DESIGN §7.4's table).

use std::io::{self, Write};

use knowa_api::ReviewDecision;
use knowa_model::query::{DEFAULT_LIMIT, QueryDoc};
use knowa_model::{Error, ErrorCode, Id, Result};
use serde::Deserialize;
use serde_json::json;
use tiny_http::{Method, Request};

use crate::http::{self, Params, Resp};
use crate::{Server, assets, auth, blob};

pub(crate) fn respond(server: &Server, mut req: Request) {
    let resp = route(server, &mut req);
    let (method, url) = (req.method().clone(), req.url().to_string());
    if let Err(e) = req.respond(resp) {
        // Fallback: the client went away (a closed tab, an aborted fetch) before the
        // response was written. Nothing is left to answer; other requests are unaffected.
        log(&format!("{method} {url}: sending the response failed: {e}"));
    }
}

fn route(server: &Server, req: &mut Request) -> Resp {
    if !auth::host_allowed(http::header_value(req, "Host"), server.port) {
        return http::forbidden();
    }
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    if let Some(endpoint) = path.strip_prefix("/api/") {
        if !server.token.admits(http::header_value(req, "Authorization")) {
            return http::unauthorized();
        }
        return api(server, req, endpoint, query).unwrap_or_else(|e| {
            if matches!(e.code, ErrorCode::Internal | ErrorCode::Integrity) {
                log(&format!("{} {url}: {e}", req.method()));
            }
            http::error(&e)
        });
    }
    match assets::get(path) {
        Some((content_type, bytes)) if *req.method() == Method::Get => http::asset(content_type, bytes),
        Some(_) => http::error(&Error::usage(format!("{path}: use GET"))),
        None => http::error(&Error::not_found(format!("{path}: no such page; the app is at /"))),
    }
}

fn api(server: &Server, req: &mut Request, endpoint: &str, query: &str) -> Result<Resp> {
    let method = req.method().clone();
    let expect = |want: Method| {
        if method == want { Ok(()) } else { Err(Error::usage(format!("/api/{endpoint}: use {want}"))) }
    };
    match endpoint {
        "schema" => {
            expect(Method::Get)?;
            Params::parse(query)?.finish()?;
            http::json(&knowa_api::schema())
        }
        "spaces" => {
            expect(Method::Get)?;
            Params::parse(query)?.finish()?;
            spaces(server)
        }
        "query" => {
            expect(Method::Post)?;
            Params::parse(query)?.finish()?;
            let doc: QueryDoc = serde_json::from_slice(&http::body(req)?)?;
            http::json(&server.knowa.query(&server.sel(None), doc)?)
        }
        "feed" => {
            expect(Method::Get)?;
            feed(server, query)
        }
        "review" => {
            expect(Method::Post)?;
            Params::parse(query)?.finish()?;
            review(server, &http::body(req)?)
        }
        _ => match endpoint.strip_prefix("blob/") {
            Some(id) => {
                expect(Method::Get)?;
                blob::respond(server, id, query)
            }
            None => Err(Error::not_found(format!("/api/{endpoint}: no such endpoint"))),
        },
    }
}

/// The registry, `[{alias, space, readonly, current}]`. `current` marks the space a
/// request without a selection reads: `--space` / `KNOWA_SPACE`, else the home's
/// current space. None is marked when nothing is selected, or when the selection is a
/// replica path that is not registered.
fn spaces(server: &Server) -> Result<Resp> {
    let home = server.knowa.home();
    let config = home.load_config()?;
    let current = if server.space.is_none() && config.current_space.is_none() {
        None
    } else {
        home.resolve_space(server.space.as_deref())?.alias
    };
    let items: Vec<_> = config
        .spaces
        .iter()
        .map(|(alias, e)| {
            json!({ "alias": alias, "space": e.space, "readonly": e.readonly, "current": current.as_deref() == Some(alias.as_str()) })
        })
        .collect();
    http::json(&items)
}

/// `?space=&cursor=&limit=`. `limit` defaults to DEFAULT_LIMIT, as `knowa feed --limit`.
fn feed(server: &Server, query: &str) -> Result<Resp> {
    let mut params = Params::parse(query)?;
    let space = params.take("space")?;
    let cursor = params.take("cursor")?;
    let limit = match params.take("limit")? {
        Some(l) => l.parse().map_err(|_| Error::invalid(format!("limit {l:?} is not a whole number")))?,
        None => DEFAULT_LIMIT,
    };
    params.finish()?;
    http::json(&server.knowa.feed(&server.sel(space), cursor.as_deref(), limit)?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewBody {
    #[serde(default)]
    space: Option<String>,
    decision: Decision,
    claims: Vec<Id>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Decision {
    Accept,
    Reject,
}

/// `review` with explicit claims only (DESIGN §7.4): the UI reviews exactly the claims
/// it shows, so the api's selectors are not reachable from here.
fn review(server: &Server, body: &[u8]) -> Result<Resp> {
    let b: ReviewBody = serde_json::from_slice(body)?;
    if server.writer.is_none() {
        return Err(Error::usage(
            "this server is read-only: it was started without an actor. Restart it with KNOWA_ACTOR=NAME (or knowa --actor NAME serve) to review claims",
        ));
    }
    if b.claims.is_empty() {
        return Err(Error::usage("review: claims must list at least one claim id (c_…)"));
    }
    let decision = match b.decision {
        Decision::Accept => ReviewDecision::Accept,
        Decision::Reject => ReviewDecision::Reject,
    };
    http::json(&server.knowa.review(&server.sel(b.space), &b.claims, None, decision, b.note)?)
}

/// Server-side failures go to the terminal running `knowa serve`; the app shows the
/// error object and points there.
fn log(line: &str) {
    // Fallback: stderr is gone (closed pipe). The response already carries the error,
    // so the client is told either way.
    let _ = writeln!(io::stderr(), "knowa serve: {line}");
}
