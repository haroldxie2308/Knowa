//! The local web UI (DESIGN §7.4): an HTTP server on 127.0.0.1 that serves the
//! embedded app and the five primitives' documents over HTTP. It adds no semantics:
//! every endpoint is one `knowa_api` call, and every error is the §6.3 error object.

mod assets;
mod auth;
mod blob;
mod http;
mod routes;

use std::convert::Infallible;
use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::sync::{Mutex, PoisonError, mpsc};
use std::thread;

use knowa_api::{Knowa, Sel};
use knowa_model::{Error, Id, Result};

/// `knowa serve` listens here unless `--port` says otherwise (DESIGN §7.4).
pub const DEFAULT_PORT: u16 = 4747;

/// Requests are handled by this many threads, so one slow query does not hold up the
/// app's other calls (the space list, images).
const WORKERS: usize = 4;

/// The actor that signs reviews, resolved once at startup: what the startup line
/// announces is what every review is signed with, even if `knowa actor use` changes
/// the home's current actor while the server runs.
struct Writer {
    name: String,
    id: Id,
}

/// A bound server. `bind` validates the selection and takes the port; `run` serves.
pub struct Server<'k> {
    knowa: &'k Knowa,
    http: tiny_http::Server,
    port: u16,
    token: auth::Token,
    /// `--space` / `KNOWA_SPACE`, passed through so a request without a space
    /// selection reads the same space the CLI would
    space: Option<String>,
    writer: Option<Writer>,
}

impl<'k> Server<'k> {
    /// Fails fast on a selection that names nothing: a `--space` that does not resolve,
    /// or an actor (`--actor`, `KNOWA_ACTOR`, else the home's current actor) without a
    /// key. No actor at all is valid: the UI is then read-only (DESIGN §7.4). Port 0
    /// picks a free port.
    pub fn bind(knowa: &'k Knowa, sel: &Sel, port: u16) -> Result<Server<'k>> {
        let home = knowa.home();
        if let Some(space) = &sel.space {
            home.resolve_space(Some(space))?;
        }
        let selected = sel.actor.is_some() || home.load_config()?.current_actor.is_some();
        let writer = if selected {
            let a = home.resolve_actor(sel.actor.as_deref())?;
            Some(Writer { name: a.name, id: a.id })
        } else {
            None
        };
        let token = auth::Token::generate()?;
        let http = tiny_http::Server::http((Ipv4Addr::LOCALHOST, port)).map_err(|e| {
            Error::usage(format!("cannot listen on 127.0.0.1:{port}: {e}; pass --port N, or --port 0 for a free port"))
        })?;
        let port = http
            .server_addr()
            .to_ip()
            .ok_or_else(|| Error::internal("web: the server is bound to a TCP port but reports no IP address"))?
            .port();
        Ok(Server { knowa, http, port, token, space: sel.space.clone(), writer })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The app's address with the access token in the fragment, which a browser never
    /// sends to a server (DESIGN §7.4).
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/#k={}", self.port, self.token.as_str())
    }

    /// Serves until accepting a connection fails, which is the only way it returns.
    pub fn run(&self) -> Result<Infallible> {
        let (tx, rx) = mpsc::sync_channel::<tiny_http::Request>(WORKERS);
        let rx = Mutex::new(rx);
        thread::scope(|s| {
            for _ in 0..WORKERS {
                s.spawn(|| self.work(&rx));
            }
            let failure = loop {
                match self.http.recv() {
                    Ok(req) => {
                        if tx.send(req).is_err() {
                            break Error::internal("web: every request worker has stopped");
                        }
                    }
                    Err(e) => break Error::internal(format!("web: accepting a connection failed: {e}")),
                }
            };
            // Closing the channel stops the workers once they finish their requests.
            drop(tx);
            Err(failure)
        })
    }

    fn work(&self, rx: &Mutex<mpsc::Receiver<tiny_http::Request>>) {
        loop {
            // The lock only serializes the hand-off: no code that could panic runs
            // while it is held, so a poisoned lock still guards a consistent receiver.
            let next = rx.lock().unwrap_or_else(PoisonError::into_inner).recv();
            match next {
                Ok(req) => routes::respond(self, req),
                // The acceptor closed the channel.
                Err(mpsc::RecvError) => return,
            }
        }
    }

    /// The selection a request runs under: the request's own space when it names one
    /// (feed, review), else the server's, and the startup writer.
    fn sel(&self, space: Option<String>) -> Sel {
        Sel { space: space.or_else(|| self.space.clone()), actor: self.writer.as_ref().map(|w| w.name.clone()) }
    }
}

/// `knowa serve`: binds, prints the app's URL on stdout and who signs reviews on
/// stderr, then serves until the process is killed.
pub fn serve(knowa: &Knowa, sel: &Sel, port: u16) -> Result<Infallible> {
    let server = Server::bind(knowa, sel, port)?;
    let who = match &server.writer {
        Some(w) => format!("Reviews are signed by {} ({}).", w.name, w.id),
        None => "Read-only: no actor selected. To review claims, restart with --actor NAME or KNOWA_ACTOR=NAME.".to_string(),
    };
    writeln!(io::stderr(), "{who}")?;
    writeln!(io::stdout(), "{}", server.url())?;
    server.run()
}
