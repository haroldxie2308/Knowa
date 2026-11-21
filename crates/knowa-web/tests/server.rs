//! The HTTP surface of DESIGN §7.4, over real sockets: a server on port 0 in a thread,
//! raw HTTP/1.1 requests over `TcpStream`, a scratch home per test (never `~/.knowa`).

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;

use knowa_api::{Knowa, PutOptions, Sel};
use knowa_config::Home;
use knowa_model::{ActorKind, Draft, Id, Term, Value};
use knowa_web::Server;
use serde_json::{Value as Json, json};

const APP_CSP: &str =
    "default-src 'self'; img-src 'self' blob:; style-src 'self'; script-src 'self'; connect-src 'self'; frame-ancestors 'none'";
const BLOB_CSP: &str = "sandbox; default-src 'none'; img-src 'self'; style-src 'unsafe-inline'";
/// A 1×1 PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01,
    0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41,
    0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
    0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

// ---------- harness ----------

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("knowa-web-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A home where human `ada` owns the current space `main`, and agent `scout` exists.
fn home(s: &Scratch) -> Knowa {
    let home = Home::open(Some(&s.path("home"))).unwrap();
    home.new_actor("ada", ActorKind::Human).unwrap();
    home.new_actor("scout", ActorKind::Agent).unwrap();
    let k = Knowa::new(home);
    k.init(&as_actor("ada"), &s.path("main"), "main notes", Some("main")).unwrap();
    k
}

fn as_actor(name: &str) -> Sel {
    Sel { space: None, actor: Some(name.into()) }
}

/// A running server over `home`; `actor` is the serving actor (None: read-only).
struct Running {
    port: u16,
    token: String,
}

fn serve(home: &Path, actor: Option<&str>) -> Running {
    let (tx, rx) = mpsc::channel();
    let home = home.to_path_buf();
    let sel = Sel { space: None, actor: actor.map(str::to_string) };
    thread::spawn(move || {
        let knowa = Knowa::new(Home::open(Some(&home)).unwrap());
        match Server::bind(&knowa, &sel, 0) {
            Ok(server) => {
                tx.send(Ok((server.port(), server.url()))).unwrap();
                let Err(e) = server.run();
                panic!("server stopped: {e}");
            }
            Err(e) => tx.send(Err(e)).unwrap(),
        }
    });
    let (port, url) = rx.recv().unwrap().unwrap();
    assert_eq!(url, format!("http://127.0.0.1:{port}/#k={}", url.rsplit_once("#k=").unwrap().1));
    Running { port, token: url.rsplit_once("#k=").unwrap().1.to_string() }
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn json(&self) -> Json {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&self.body)))
    }
    fn error_code(&self) -> String {
        self.json()["error"]["code"].as_str().unwrap().to_string()
    }
}

impl Running {
    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
    fn get(&self, path: &str) -> Resp {
        raw(self.port, "GET", path, &self.host(), Some(&self.token), None)
    }
    fn post(&self, path: &str, body: &Json) -> Resp {
        raw(self.port, "POST", path, &self.host(), Some(&self.token), Some(&body.to_string()))
    }
}

fn raw(port: u16, method: &str, path: &str, host: &str, token: Option<&str>, body: Option<&str>) -> Resp {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
    s.write_all(req.as_bytes()).unwrap();
    let mut bytes = Vec::new();
    s.read_to_end(&mut bytes).unwrap();
    let split = bytes.windows(4).position(|w| w == b"\r\n\r\n").expect("a header block");
    let head = String::from_utf8(bytes[..split].to_vec()).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers: Vec<(String, String)> =
        lines.map(|l| l.split_once(": ").unwrap()).map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let rest = bytes[split + 4..].to_vec();
    let chunked = headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("Transfer-Encoding") && v == "chunked");
    let body = if chunked { dechunk(&rest) } else { rest };
    Resp { status, headers, body }
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let eol = b.windows(2).position(|w| w == b"\r\n").unwrap();
        let size = usize::from_str_radix(std::str::from_utf8(&b[..eol]).unwrap().trim(), 16).unwrap();
        b = &b[eol + 2..];
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

fn term(s: &str) -> Term {
    Term::parse(s).unwrap()
}

/// Claim ids the agent proposes: two labels on a fresh entity.
fn proposals(k: &Knowa) -> Vec<Id> {
    let e = Id::parse("_:e").unwrap();
    let drafts = vec![
        Draft::new(e.clone(), term("knowa:label"), Value::text("Kyoto trip")),
        Draft::new(e, term("knowa:summary"), Value::text("A week of temples")),
    ];
    k.write(&as_actor("scout"), drafts).unwrap().items.into_iter().map(|i| i.id).collect()
}

fn status_of(r: &Running, claim: &Id) -> String {
    let got = r.post("/api/query", &json!({"op": "get", "ids": [claim]}));
    assert_eq!(got.status, 200, "{}", String::from_utf8_lossy(&got.body));
    got.json()["items"][0]["claim"]["status"].as_str().unwrap().to_string()
}

// ---------- tests ----------

#[test]
fn api_needs_the_token_and_every_request_needs_a_loopback_host() {
    let s = Scratch::new("auth");
    home(&s);
    let r = serve(&s.path("home"), None);

    let none = raw(r.port, "GET", "/api/spaces", &r.host(), None, None);
    assert_eq!(none.status, 401);
    assert_eq!(none.header("WWW-Authenticate"), Some("Bearer"));
    assert_eq!(none.error_code(), "usage");
    let wrong = raw(r.port, "GET", "/api/spaces", &r.host(), Some(&"a".repeat(r.token.len())), None);
    assert_eq!(wrong.status, 401);

    for host in [format!("evil.example:{}", r.port), format!("127.0.0.1:{}", r.port + 1), "127.0.0.1".into()] {
        let api = raw(r.port, "GET", "/api/spaces", &host, Some(&r.token), None);
        assert_eq!(api.status, 403, "{host}");
        let page = raw(r.port, "GET", "/", &host, None, None);
        assert_eq!(page.status, 403, "{host}");
    }

    let ok = raw(r.port, "GET", "/api/spaces", &format!("localhost:{}", r.port), Some(&r.token), None);
    assert_eq!(ok.status, 200);
    assert!(ok.headers.iter().all(|(k, _)| !k.to_ascii_lowercase().starts_with("access-control-")), "no CORS headers");
}

#[test]
fn query_passes_documents_through_and_errors_map_to_statuses() {
    let s = Scratch::new("query");
    let k = home(&s);
    let w = k.write(&as_actor("ada"), vec![Draft::new(Id::parse("_:n").unwrap(), term("knowa:label"), Value::text("Kyoto trip"))]).unwrap();
    let note = w.placeholders["_:n"].clone();
    let r = serve(&s.path("home"), None);

    let found = r.post("/api/query", &json!({"op": "find", "where": {"text": "kyoto"}}));
    assert_eq!(found.status, 200);
    assert_eq!(found.header("Content-Type"), Some("application/json"));
    assert_eq!(found.header("X-Content-Type-Options"), Some("nosniff"));
    assert_eq!(found.json()["items"][0]["id"], json!(note));
    assert_eq!(found.json(), serde_json::to_value(k.query(&Sel::default(), serde_json::from_value(json!({"op": "find", "where": {"text": "kyoto"}})).unwrap()).unwrap()).unwrap());

    let cases = [
        (json!({"op": "find", "bogus": 1}), 400, "invalid"),
        (json!({"op": "find", "asOf": "2026"}), 422, "unsupported"),
        (json!({"op": "get", "ids": ["e_00000000000000000000000000"]}), 404, "not_found"),
        (json!({"op": "find", "spaces": ["nowhere"]}), 404, "not_found"),
    ];
    for (doc, status, code) in cases {
        let res = r.post("/api/query", &doc);
        assert_eq!((res.status, res.error_code().as_str()), (status, code), "{doc}");
    }
    let feed = r.get("/api/feed?limit=5");
    assert_eq!(feed.status, 200);
    assert_eq!(feed.json()["claims"].as_array().unwrap().len(), 5);
    let expired = r.get("/api/feed?cursor=gone%3A3");
    assert_eq!((expired.status, expired.error_code().as_str()), (410, "cursor_expired"));
    let bad = r.get("/api/feed?cursor=nonsense");
    assert_eq!((bad.status, bad.error_code().as_str()), (400, "invalid"));
    let unknown = r.get("/api/feed?colour=red");
    assert_eq!((unknown.status, unknown.error_code().as_str()), (400, "invalid"));

    let spaces = r.get("/api/spaces").json();
    assert_eq!(spaces, json!([{"alias": "main", "space": spaces[0]["space"], "readonly": false, "current": true}]));
    assert!(r.get("/api/schema").json()["primitives"].is_object());
    assert_eq!(r.get("/api/query").error_code(), "usage");
    assert_eq!(r.get("/api/nothing").status, 404);
}

#[test]
fn blobs_render_inline_only_for_accepted_raster_and_text_types() {
    let s = Scratch::new("blob");
    let k = home(&s);
    fs::write(s.path("photo.png"), PNG).unwrap();
    fs::write(s.path("page.html"), "<!doctype html><script>alert(1)</script>").unwrap();
    fs::write(s.path("plain.txt"), "plain words").unwrap();
    let put = k.put(&as_actor("ada"), &[s.path("photo.png"), s.path("page.html")], PutOptions::default()).unwrap();
    let blob = |name: &str| put.items.iter().find(|i| i.path.ends_with(name)).unwrap().blob.clone();
    // Stored without extractors: its only media type is the agent's proposal.
    let plain = k.put(&as_actor("ada"), &[s.path("plain.txt")], PutOptions { no_extract: true, ..PutOptions::default() }).unwrap().items[0].blob.clone();
    k.write(&as_actor("scout"), vec![Draft::new(plain.clone(), term("knowa:mediaType"), Value::text("text/plain"))]).unwrap();
    let r = serve(&s.path("home"), None);

    let png = r.get(&format!("/api/blob/{}", blob("photo.png")));
    assert_eq!(png.status, 200);
    assert_eq!(png.header("Content-Type"), Some("image/png"));
    assert_eq!(png.header("Content-Disposition"), None);
    assert_eq!(png.header("Content-Security-Policy"), Some(BLOB_CSP));
    assert_eq!(png.header("X-Content-Type-Options"), Some("nosniff"));
    assert_eq!(png.body, PNG);

    let html = r.get(&format!("/api/blob/{}?spaces=main", blob("page.html")));
    assert_eq!(html.status, 200);
    assert_eq!(html.header("Content-Type"), Some("application/octet-stream"));
    assert_eq!(html.header("Content-Disposition"), Some("attachment"));
    assert_eq!(html.header("Content-Security-Policy"), Some(BLOB_CSP));
    assert_eq!(html.header("X-Content-Type-Options"), Some("nosniff"));

    let proposed = r.get(&format!("/api/blob/{plain}"));
    assert_eq!(proposed.header("Content-Type"), Some("application/octet-stream"), "a proposed media type never decides");
    assert_eq!(proposed.header("Content-Disposition"), Some("attachment"));
    assert_eq!(proposed.body, b"plain words");

    let not_blob = r.get("/api/blob/e_00000000000000000000000000");
    assert_eq!((not_blob.status, not_blob.error_code().as_str()), (400, "invalid"));
    let absent = r.get(&format!("/api/blob/{}", Id::blob_of(b"never stored")));
    assert_eq!((absent.status, absent.error_code().as_str()), (404, "not_found"));
}

#[test]
fn review_writes_one_signed_review_per_listed_claim() {
    let s = Scratch::new("review");
    let k = home(&s);
    let claims = proposals(&k);
    let r = serve(&s.path("home"), Some("ada"));
    assert_eq!(status_of(&r, &claims[0]), "proposed");

    let res = r.post("/api/review", &json!({"decision": "accept", "claims": claims, "note": "checked"}));
    assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.body));
    let items = res.json()["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 2);
    for (item, claim) in items.iter().zip(&claims) {
        let review = r.post("/api/query", &json!({"op": "get", "ids": [item["id"]]})).json();
        let review = &review["items"][0]["claim"];
        assert_eq!(review["p"], "knowa:review");
        assert_eq!(review["s"], json!(claim));
        assert_eq!(review["o"], json!({"term": "knowa:Accept"}));
        assert_eq!(review["note"], "checked");
        assert_eq!(review["actor"], json!(k.home().actor("ada").unwrap().id));
        assert_eq!(status_of(&r, claim), "accepted");
    }

    let reject = r.post("/api/review", &json!({"space": "main", "decision": "reject", "claims": [claims[1]]}));
    assert_eq!(reject.status, 200);
    assert_eq!(reject.json()["items"].as_array().unwrap().len(), 1);
    assert_eq!(status_of(&r, &claims[1]), "rejected");

    let empty = r.post("/api/review", &json!({"decision": "accept", "claims": []}));
    assert_eq!((empty.status, empty.error_code().as_str()), (400, "usage"));
    let selector = r.post("/api/review", &json!({"decision": "accept", "claims": [claims[0]], "by": ["scout"]}));
    assert_eq!((selector.status, selector.error_code().as_str()), (400, "invalid"), "selectors are not part of the HTTP review");
    let not_claim = r.post("/api/review", &json!({"decision": "accept", "claims": ["e_00000000000000000000000000"]}));
    assert_eq!((not_claim.status, not_claim.error_code().as_str()), (400, "invalid"));
}

#[test]
fn review_without_a_serving_actor_is_a_usage_error_naming_the_variable() {
    let s = Scratch::new("readonly");
    let k = home(&s);
    let claims = proposals(&k);
    let r = serve(&s.path("home"), None);

    let res = r.post("/api/review", &json!({"decision": "accept", "claims": claims}));
    assert_eq!((res.status, res.error_code().as_str()), (400, "usage"));
    assert!(res.json()["error"]["message"].as_str().unwrap().contains("KNOWA_ACTOR"));
    assert_eq!(status_of(&r, &claims[0]), "proposed", "nothing was written");
}

#[test]
fn app_pages_carry_the_csp_and_nothing_else_is_served() {
    let s = Scratch::new("app");
    home(&s);
    let r = serve(&s.path("home"), None);

    for (path, content_type) in [("/", "text/html; charset=utf-8"), ("/js/main.js", "text/javascript; charset=utf-8"), ("/app.css", "text/css; charset=utf-8")] {
        let page = raw(r.port, "GET", path, &r.host(), None, None);
        assert_eq!(page.status, 200, "{path}");
        assert_eq!(page.header("Content-Type"), Some(content_type), "{path}");
        assert_eq!(page.header("Content-Security-Policy"), Some(APP_CSP), "{path}");
        assert_eq!(page.header("Referrer-Policy"), Some("no-referrer"), "{path}");
        assert_eq!(page.header("X-Content-Type-Options"), Some("nosniff"), "{path}");
    }
    assert_eq!(raw(r.port, "GET", "/../Cargo.toml", &r.host(), None, None).status, 404);
    assert_eq!(raw(r.port, "GET", "/index.html", &r.host(), None, None).status, 404);
}

#[test]
fn bind_refuses_an_actor_without_a_key() {
    let s = Scratch::new("bind");
    let k = home(&s);
    let err = Server::bind(&k, &as_actor("nobody"), 0).err().expect("an unknown actor must not start a server");
    assert_eq!(err.code, knowa_model::ErrorCode::NotFound);
}
