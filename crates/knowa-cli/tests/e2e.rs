//! End-to-end tests of the `knowa` binary through its `--json` contract. Every test
//! runs in its own temporary directory with its own `KNOWA_HOME` (never `~/.knowa`),
//! and `KNOWA_SPACE` / `KNOWA_ACTOR` from the outer environment are removed.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

// ---------- harness ----------

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Sandbox {
        static N: AtomicUsize = AtomicUsize::new(0);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("knowa-cli-e2e-{tag}-{}-{nanos}-{n}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        Sandbox { root }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// A home (KNOWA_HOME) inside this sandbox, with no acting actor.
    fn home(&self, rel: &str) -> Kn {
        Kn { home: self.path(rel), actor: None }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // A failing test keeps its directory for inspection. Fallback: if removal fails,
        // a stray temp dir is left behind, which affects no other test (unique names).
        if !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

/// The binary bound to one home and (optionally) one acting actor via env.
#[derive(Clone)]
struct Kn {
    home: PathBuf,
    actor: Option<String>,
}

impl Kn {
    fn as_actor(&self, name: &str) -> Kn {
        Kn { home: self.home.clone(), actor: Some(name.to_string()) }
    }

    fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_knowa"));
        c.args(args)
            .env("KNOWA_HOME", &self.home)
            .env_remove("KNOWA_SPACE")
            .env_remove("KNOWA_ACTOR")
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(a) = &self.actor {
            c.env("KNOWA_ACTOR", a);
        }
        let mut child = c.spawn().unwrap();
        if let Some(bytes) = stdin {
            child.stdin.take().unwrap().write_all(bytes).unwrap();
        }
        child.wait_with_output().unwrap()
    }

    fn json_args<'a>(args: &[&'a str]) -> Vec<&'a str> {
        let mut v = vec!["--json"];
        v.extend_from_slice(args);
        v
    }

    /// `--json` call that must succeed; returns stdout as one JSON document.
    fn ok(&self, args: &[&str]) -> Value {
        self.ok_stdin(args, None)
    }

    fn ok_stdin(&self, args: &[&str], stdin: Option<&[u8]>) -> Value {
        let out = self.run(&Self::json_args(args), stdin);
        assert!(
            out.status.success(),
            "knowa {args:?} failed with {:?}\nstderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("knowa {args:?}: stdout is not one JSON document ({e}): {}", String::from_utf8_lossy(&out.stdout)))
    }

    /// `--json` call that must fail; returns (exit code, stderr JSON envelope).
    fn fail(&self, args: &[&str]) -> (i32, Value) {
        self.fail_stdin(args, None)
    }

    fn fail_stdin(&self, args: &[&str], stdin: Option<&[u8]>) -> (i32, Value) {
        let out = self.run(&Self::json_args(args), stdin);
        assert!(!out.status.success(), "knowa {args:?} unexpectedly succeeded: {}", String::from_utf8_lossy(&out.stdout));
        let err: Value = serde_json::from_slice(&out.stderr)
            .unwrap_or_else(|e| panic!("knowa {args:?}: stderr is not the JSON envelope ({e}): {}", String::from_utf8_lossy(&out.stderr)));
        (out.status.code().unwrap(), err)
    }
}

/// A home with a human actor `name` owning a fresh space `<rel>-space` (alias `alias`),
/// which `init` makes current. Returns the home bound to that actor.
fn setup_home(sb: &Sandbox, rel: &str, name: &str, alias: &str) -> Kn {
    let kn = sb.home(rel);
    kn.ok(&["actor", "new", name, "--kind", "human"]);
    let me = kn.as_actor(name);
    let dir = sb.path(&format!("{rel}-space"));
    let r = me.ok(&["init", s(&dir), "--name", &format!("{name} notes"), "--alias", alias]);
    assert_eq!(r["alias"], alias);
    assert!(r["space"].as_str().unwrap().starts_with("s_"));
    me
}

fn setup(sb: &Sandbox) -> Kn {
    setup_home(sb, "home", "alice", "main")
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// Writes a note and returns the minted entity id.
fn note(kn: &Kn, args: &[&str]) -> String {
    let mut v = vec!["note"];
    v.extend_from_slice(args);
    let r = kn.ok(&v);
    let id = r["placeholders"]["_:n"].as_str().unwrap().to_string();
    assert!(id.starts_with("e_"), "{r}");
    id
}

#[test]
fn the_timeline_holds_only_dated_entities() {
    let sb = Sandbox::new("timeline-dated");
    let alice = setup(&sb);
    let dated = note(&alice, &["met the reading group", "--title", "Reading group"]);
    // a concept with no stated time, as an agent creates one while reading
    let w = alice.ok_stdin(&["write"], Some(br#"{"s":"_:c","p":"knowa:label","o":{"text":"Undated concept"}}"#));
    let undated = w["placeholders"]["_:c"].as_str().unwrap().to_string();
    let on_timeline = ids(&alice.ok(&["timeline"])["items"]);
    assert!(on_timeline.contains(&dated), "{on_timeline:?}");
    assert!(!on_timeline.contains(&undated), "an entity without a when is not on the timeline: {on_timeline:?}");
    assert!(ids(&alice.ok(&["search", "undated"])["items"]).contains(&undated), "it is still found by search");
}

/// The id of the claim a single-draft write produced. The api may prepend the actor's
/// `knowa:actorKind` claim on its first write, so the drafted claim is the last item.
fn last_id(write: &Value) -> String {
    write["items"].as_array().unwrap().last().unwrap()["id"].as_str().unwrap().to_string()
}

fn ids(items: &Value) -> Vec<String> {
    items.as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).collect()
}

fn show_claim(kn: &Kn, id: &str) -> Value {
    let r = kn.ok(&["show", id]);
    let item = &r["items"][0];
    assert_eq!(item["kind"], "claim", "{r}");
    item["claim"].clone()
}

fn actor_id(kn: &Kn, name: &str) -> String {
    kn.ok(&["actor", "show", name])["id"].as_str().unwrap().to_string()
}

// ---------- CLI-only contracts (no api call) ----------

#[test]
fn clap_usage_errors_use_the_json_envelope_and_exit_2() {
    let sb = Sandbox::new("clap");
    let kn = sb.home("home");
    let (code, err) = kn.fail(&["frobnicate"]);
    assert_eq!(code, 2);
    assert_eq!(err["error"]["code"], "usage");
    let (code, err) = kn.fail(&["search"]);
    assert_eq!(code, 2);
    assert_eq!(err["error"]["code"], "usage");
}

#[test]
fn put_help_says_hidden_files_are_skipped() {
    let sb = Sandbox::new("help");
    let out = sb.home("home").run(&["put", "--help"], None);
    assert!(out.status.success());
    let help = String::from_utf8(out.stdout).unwrap();
    assert!(help.contains("Hidden files"), "{help}");
}

// ---------- flows ----------

#[test]
fn schema_needs_no_home() {
    let sb = Sandbox::new("schema");
    let kn = sb.home("home");
    let schema = kn.ok(&["schema"]);
    assert!(schema.is_object());
    assert!(!kn.home.exists(), "schema must not create the home directory");
}

#[test]
fn note_then_search_finds_it_including_two_char_chinese_substrings() {
    let sb = Sandbox::new("search");
    let alice = setup(&sb);
    let fox = note(&alice, &["the quick brown fox jumps", "--title", "Fox", "--when", "2024-05-01"]);
    let tokyo = note(&alice, &["今天去了东京塔看夜景"]);

    let hits = alice.ok(&["search", "brown"]);
    assert!(ids(&hits["items"]).contains(&fox), "{hits}");
    let item = hits["items"].as_array().unwrap().iter().find(|i| i["id"] == fox.as_str()).unwrap();
    assert_eq!(item["label"], "Fox");
    assert!(item["types"].as_array().unwrap().contains(&json!("knowa:Note")));
    assert_eq!(item["when"]["value"], "2024-05-01");

    let hits = alice.ok(&["search", "东京"]);
    assert_eq!(ids(&hits["items"]), [tokyo], "{hits}");

    // Text output is not contract, but must work and mention the hit.
    let out = alice.run(&["search", "brown"], None);
    assert!(out.status.success());
    assert!(String::from_utf8(out.stdout).unwrap().contains(&fox));
}

#[test]
fn agent_claim_is_pending_until_the_owner_accepts_it() {
    let sb = Sandbox::new("review");
    let alice = setup(&sb);
    alice.ok(&["actor", "new", "bot", "--kind", "agent"]);
    let e = note(&alice, &["meeting with Bob about the roadmap"]);

    let bot = alice.as_actor("bot");
    let w = bot.ok(&["assert", &e, "knowa:summary", "text:Roadmap meeting", "--conf", "0.8", "--via", "model=test"]);
    let c = last_id(&w);
    assert_eq!(w["items"].as_array().unwrap().last().unwrap()["status"], "proposed");

    let pending = alice.ok(&["pending"]);
    let item = pending["items"].as_array().unwrap().iter().find(|i| i["id"] == c.as_str());
    let item = item.unwrap_or_else(|| panic!("{c} not pending: {pending}"));
    assert_eq!(item["status"], "proposed");
    assert_eq!(item["via"]["model"], "test");

    alice.ok(&["review", "accept", &c, "--note", "looks right"]);
    assert_eq!(show_claim(&alice, &c)["status"], "accepted");
    assert!(!ids(&alice.ok(&["pending"])["items"]).contains(&c));
}

#[test]
fn review_by_author_accepts_every_pending_claim_of_theirs_at_once() {
    let sb = Sandbox::new("bulk-review");
    let alice = setup(&sb);
    alice.ok(&["actor", "new", "bot", "--kind", "agent"]);
    alice.ok(&["actor", "new", "bot2", "--kind", "agent"]);
    let e = note(&alice, &["planning meeting"]);
    let b1 = last_id(&alice.as_actor("bot").ok(&["assert", &e, "ex:topic", "text:roadmap"]));
    let b2 = last_id(&alice.as_actor("bot").ok(&["assert", &e, "ex:topic", "text:budget"]));
    let other = last_id(&alice.as_actor("bot2").ok(&["assert", &e, "ex:topic", "text:hiring"]));

    // pending takes the same selectors: a preview of what review will select
    let by_bot = ids(&alice.ok(&["pending", "--by", "bot"])["items"]);
    assert!(by_bot.contains(&b1) && by_bot.contains(&b2) && !by_bot.contains(&other), "{by_bot:?}");
    let by_id = ids(&alice.ok(&["pending", "--by", &actor_id(&alice, "bot")])["items"]);
    assert_eq!(by_id, by_bot, "an actor id selects what its local name does");
    assert!(ids(&alice.ok(&["pending"])["items"]).contains(&other));

    let w = alice.ok(&["review", "accept", "--by", "bot", "--note", "checked"]);
    let items = w["items"].as_array().unwrap();
    assert_eq!(items.len(), by_bot.len(), "one review per selected claim: {w}");
    assert!(items.iter().all(|i| i["created"] == true && i["status"] == "accepted"), "{w}");
    assert_eq!((show_claim(&alice, &b1)["status"].clone(), show_claim(&alice, &b2)["status"].clone()), (json!("accepted"), json!("accepted")));
    assert_eq!(show_claim(&alice, &other)["status"], "proposed", "another author's claim");
    assert!(ids(&alice.ok(&["pending", "--by", "bot"])["items"]).is_empty());

    // text output counts what was written and what was already there
    let out = alice.run(&["review", "accept", &b1, &b2], None);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.trim(), "reviews: 0 written, 2 already present");
}

#[test]
fn review_needs_a_claim_or_a_selector() {
    let sb = Sandbox::new("review-usage");
    let alice = setup(&sb);
    let (code, err) = alice.fail(&["review", "accept"]);
    assert_eq!((code, &err["error"]["code"]), (2, &json!("usage")), "{err}");
    let (code, err) = alice.fail(&["review", "accept", "--by", "nobody"]);
    assert_eq!((code, &err["error"]["code"]), (3, &json!("not_found")), "{err}");
    assert!(err["error"]["message"].as_str().unwrap().contains("nobody"), "{err}");
}

#[test]
fn retract_marks_the_claim_retracted() {
    let sb = Sandbox::new("retract");
    let alice = setup(&sb);
    let e = note(&alice, &["a passing mood"]);
    let c = last_id(&alice.ok(&["assert", &e, "ex:mood", "term:ex:Happy"]));
    assert_eq!(show_claim(&alice, &c)["status"], "accepted");

    let w = alice.ok(&["retract", &c, "--note", "wrong"]);
    let r = last_id(&w);
    let retraction = show_claim(&alice, &r);
    assert_eq!(retraction["p"], "knowa:retract");
    assert_eq!(retraction["o"], json!({ "bool": true }));
    assert_eq!(show_claim(&alice, &c)["status"], "retracted");
}

#[test]
fn assert_with_replaces_supersedes_the_old_claim() {
    let sb = Sandbox::new("replaces");
    let alice = setup(&sb);
    let e = note(&alice, &["draft of the plan"]);
    let c1 = last_id(&alice.ok(&["assert", &e, "knowa:label", "text:Old title"]));
    let c2 = last_id(&alice.ok(&["assert", &e, "knowa:label", "text:New title", "--replaces", &c1]));

    assert_eq!(show_claim(&alice, &c1)["status"], "superseded");
    assert_eq!(show_claim(&alice, &c2)["status"], "accepted");
    assert_eq!(show_claim(&alice, &c2)["replaces"], json!([c1]));
    let entity = alice.ok(&["show", &e]);
    assert_eq!(entity["items"][0]["kind"], "entity");
    assert_eq!(entity["items"][0]["entity"]["label"], "New title");
}

#[test]
fn assert_value_syntax_reaches_the_claim() {
    let sb = Sandbox::new("values");
    let alice = setup(&sb);
    let e = note(&alice, &["value holder"]);
    let ref_token = format!("ref:{e}");
    let cases = [
        ("int:42", json!({ "int": 42 })),
        ("dec:12.50", json!({ "dec": "12.50" })),
        ("bool:false", json!({ "bool": false })),
        ("when:2024-05~", json!({ "when": "2024-05~" })),
        ("geo:35.68,139.76", json!({ "geo": { "lat": "35.68", "lon": "139.76" } })),
        ("qty:12.5:kg", json!({ "qty": "12.5", "unit": "kg" })),
        ("bare words", json!({ "text": "bare words" })),
        (ref_token.as_str(), json!({ "ref": e })),
    ];
    for (i, (token, want)) in cases.iter().enumerate() {
        let p = format!("ex:p{i}");
        let c = last_id(&alice.ok(&["assert", &e, &p, token, "--q", "ex:unit=term:ex:Thing"]));
        let claim = show_claim(&alice, &c);
        assert_eq!(&claim["o"], want, "{token}");
        assert_eq!(claim["q"], json!({ "ex:unit": [{ "term": "ex:Thing" }] }));
    }
}

#[test]
fn bad_value_syntax_is_invalid_exit_4() {
    let sb = Sandbox::new("badvalue");
    let alice = setup(&sb);
    let e = note(&alice, &["target"]);
    for bad in ["int:abc", "bool:maybe", "geo:1", "qty:3", "term:not a term", "ref:nope"] {
        let (code, err) = alice.fail(&["assert", &e, "ex:p", bad]);
        assert_eq!(code, 4, "{bad}: {err}");
        assert_eq!(err["error"]["code"], "invalid", "{bad}");
    }
    let (code, err) = alice.fail(&["assert", &e, "ex:p", "int:1", "--q", "ex:q"]);
    assert_eq!((code, &err["error"]["code"]), (4, &json!("invalid")));
}

#[test]
fn write_reads_jsonl_and_names_the_bad_line() {
    let sb = Sandbox::new("write");
    let alice = setup(&sb);

    let good = "{\"s\":\"_:x\",\"p\":\"knowa:type\",\"o\":{\"term\":\"knowa:Event\"}}\n\n\
                {\"s\":\"_:x\",\"p\":\"knowa:body\",\"o\":{\"text\":\"lantern festival\"}}\n";
    let r = alice.ok_stdin(&["write"], Some(good.as_bytes()));
    let x = r["placeholders"]["_:x"].as_str().unwrap().to_string();
    assert!(x.starts_with("e_"));
    assert_eq!(r["items"].as_array().unwrap().len(), 2, "{r}");

    let bad = "{\"s\":\"_:y\",\"p\":\"knowa:body\",\"o\":{\"text\":\"marmalade\"}}\n\nnot json\n";
    let (code, err) = alice.fail_stdin(&["write"], Some(bad.as_bytes()));
    assert_eq!(code, 4);
    assert_eq!(err["error"]["code"], "invalid");
    assert_eq!(err["error"]["detail"]["line"], 3);
    assert!(err["error"]["message"].as_str().unwrap().contains("line 3"));
    // All-or-nothing: the good first line was not written either.
    assert!(alice.ok(&["search", "marmalade"])["items"].as_array().unwrap().is_empty());
}

#[test]
fn put_text_file_is_searchable_and_hidden_files_are_skipped() {
    let sb = Sandbox::new("put");
    let alice = setup(&sb);
    let dir = sb.path("files");
    fs::create_dir_all(dir.join(".secret")).unwrap();
    fs::write(dir.join("hello.txt"), "zebra crossing at dawn\n").unwrap();
    fs::write(dir.join(".hidden.txt"), "quokka hidden file\n").unwrap();
    fs::write(dir.join(".secret").join("inner.txt"), "quokka hidden dir\n").unwrap();

    let r = alice.ok(&["put", s(&dir)]);
    let items = r["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{r}");
    assert_eq!(r["skipped"], json!([]), "always present");
    let blob = items[0]["blob"].as_str().unwrap().to_string();
    assert!(blob.starts_with("b_"));

    let hits = alice.ok(&["search", "zebra"]);
    assert!(ids(&hits["items"]).contains(&blob), "{hits}");
    assert!(alice.ok(&["search", "quokka"])["items"].as_array().unwrap().is_empty());

    let out = alice.run(&["blob", "cat", &blob], None);
    assert!(out.status.success());
    assert_eq!(out.stdout, b"zebra crossing at dawn\n");
}

#[test]
fn put_no_store_records_files_whose_bytes_blob_cat_cannot_find() {
    let sb = Sandbox::new("put-no-store");
    let alice = setup(&sb);
    let dir = sb.path("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("exam.txt"), "listening section, track 4\n").unwrap();

    let r = alice.ok(&["put", "--no-store", s(&dir)]);
    let item = &r["items"][0];
    assert_eq!((&item["stored"], &item["created"]), (&json!(false), &json!(false)), "{r}");
    let blob = item["blob"].as_str().unwrap();
    let (_, err) = alice.fail(&["blob", "cat", blob]);
    assert_eq!(err["error"]["code"], "not_found", "{err}");
}

#[test]
fn put_skips_an_unreadable_file_prints_the_result_and_exits_9() {
    let sb = Sandbox::new("put-skip");
    let alice = setup(&sb);
    let dir = sb.path("files");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), "aardvark at noon\n").unwrap();
    fs::write(dir.join("b.txt"), "badger at dusk\n").unwrap();
    let locked = dir.join("c.txt");
    fs::write(&locked, "capybara never\n").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::File::open(&locked).is_ok() {
        eprintln!("skipped: a mode 000 file is readable here (root?)");
        return;
    }

    let out = alice.run(&["--json", "put", s(&dir)], None);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(9), "stderr: {stderr}");
    assert!(stderr.is_empty(), "exit 9 is not an error: no envelope, got {stderr}");
    let r: Value = serde_json::from_slice(&out.stdout).unwrap();
    let paths: Vec<&str> = r["items"].as_array().unwrap().iter().map(|i| i["path"].as_str().unwrap()).collect();
    assert_eq!(paths, vec![s(&dir.join("a.txt")), s(&dir.join("b.txt"))], "{r}");
    let skipped = r["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1, "{r}");
    assert_eq!((&skipped[0]["path"], &skipped[0]["code"]), (&json!(s(&locked)), &json!("internal")), "{r}");
    assert!(skipped[0]["message"].as_str().unwrap().contains(s(&locked)), "{r}");

    // The other two were recorded.
    for (item, word) in r["items"].as_array().unwrap().iter().zip(["aardvark", "badger"]) {
        let hits = alice.ok(&["search", word]);
        assert!(ids(&hits["items"]).contains(&item["blob"].as_str().unwrap().to_string()), "{hits}");
        let claim = item["claims"][0].as_str().unwrap();
        assert_eq!(show_claim(&alice, claim)["id"], claim);
    }
}

#[test]
fn context_shows_claims_inbound_links_and_neighbors() {
    let sb = Sandbox::new("context");
    let alice = setup(&sb);
    let trip = note(&alice, &["Trip to Kyoto", "--title", "Kyoto trip", "--when", "2024-04"]);
    let photo = note(&alice, &["Photo at Fushimi Inari", "--about", &trip]);

    let ctx = alice.ok(&["context", &trip]);
    assert_eq!(ctx["entity"]["id"], trip.as_str());
    let claims = ctx["claims"].as_array().unwrap();
    assert!(!claims.is_empty());
    assert!(claims.iter().all(|c| c["s"] == trip.as_str()), "{ctx}");
    assert!(
        ctx["inbound"].as_array().unwrap().iter().any(|c| c["p"] == "knowa:about" && c["s"] == photo.as_str()),
        "{ctx}"
    );
    assert!(
        ctx["neighbors"].as_array().unwrap().iter().any(|n| n["entity"]["id"] == photo.as_str() && n["dir"] == "in"),
        "{ctx}"
    );

    let paths = alice.ok(&["path", &photo, &trip]);
    let first = &paths["paths"][0];
    assert_eq!(first[0]["id"], photo.as_str());
    assert_eq!(first[1]["id"], trip.as_str());
}

#[test]
fn shared_export_imports_into_another_home_and_cross_space_search_finds_both() {
    let sb = Sandbox::new("share");
    let alice = setup_home(&sb, "home-a", "alice", "alice-space");
    let shared = note(&alice, &["sunset walk along the river"]);
    let private = note(&alice, &["sunset private diary"]);
    alice.ok(&["share", &shared, "shared"]);
    let bundle = sb.path("alice.knowa");
    let ex = alice.ok(&["export", s(&bundle), "--level", "shared"]);
    assert_eq!(ex["partial"], true);

    let bob = setup_home(&sb, "home-b", "bob", "bob-space");
    let own = note(&bob, &["sunset over the bay"]);
    let im = bob.ok(&["import", s(&bundle)]);
    assert!(im["added"].as_u64().unwrap() > 0, "{im}");

    let hits = bob.ok(&["search", "sunset", "--spaces", "*"]);
    let found = ids(&hits["items"]);
    assert!(found.contains(&shared), "{hits}");
    assert!(found.contains(&own), "{hits}");
    assert!(!found.contains(&private), "private note leaked: {hits}");
    let mut spaces: Vec<String> = hits["items"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|i| i["spaces"].as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()))
        .collect();
    spaces.sort();
    spaces.dedup();
    assert_eq!(spaces.len(), 2, "{hits}");

    // The imported replica is read-only.
    let (code, err) = bob.fail(&["--space", im["path"].as_str().unwrap(), "note", "no"]);
    assert_eq!(code, 2, "{err}");
}

#[cfg(unix)]
#[test]
fn worker_run_writes_command_output_as_the_worker() {
    use std::os::unix::fs::PermissionsExt;

    let sb = Sandbox::new("worker");
    let alice = setup(&sb);
    let a = note(&alice, &["alpha"]);
    let b = note(&alice, &["beta"]);
    let linker = alice.ok(&["actor", "new", "linker", "--kind", "worker"])["id"].as_str().unwrap().to_string();

    let script = sb.path("worker dir").join("link.sh");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    let draft = json!({ "s": a, "p": "knowa:related", "o": { "ref": b }, "note": "linked by the test worker" });
    fs::write(&script, format!("#!/bin/sh\ncat > /dev/null\nprintf '%s\\n' '{draft}'\n")).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    // The path contains a space: --cmd quoting must keep it one word.
    let r = alice.ok(&["worker", "run", "linker", "--cmd", &format!("'{}'", s(&script)), "--batch", "1000"]);
    assert!(r["claimsOut"].as_u64().unwrap() >= 1, "{r}");

    let found = alice.ok(&["query", r#"{"op":"find","target":"claims","where":{"p":["knowa:related"]}}"#]);
    let items = found["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{found}");
    assert_eq!(items[0]["actor"], linker.as_str());
    assert_eq!(items[0]["s"], a.as_str());
    assert_eq!(items[0]["o"]["ref"], b.as_str());
    assert_eq!(items[0]["via"]["worker"], "linker");
}

#[test]
fn feed_and_log_list_claims() {
    let sb = Sandbox::new("feed");
    let alice = setup(&sb);
    note(&alice, &["something to feed"]);
    let me = actor_id(&alice, "alice");

    let feed = alice.ok(&["feed", "--limit", "1000"]);
    assert!(!feed["claims"].as_array().unwrap().is_empty());
    let cursor = feed["cursor"].as_str().unwrap();
    // A cursor records per-actor seqs, not positions in this index: reindex keeps it.
    alice.ok(&["reindex"]);
    let rest = alice.ok(&["feed", "--cursor", cursor]);
    assert!(rest["claims"].as_array().unwrap().is_empty(), "{rest}");
    assert_eq!(rest["cursor"], cursor);
    note(&alice, &["after the reindex"]);
    let next = alice.ok(&["feed", "--cursor", cursor]);
    let fresh = next["claims"].as_array().unwrap();
    assert!(!fresh.is_empty() && fresh.iter().all(|c| c["actor"] == me.as_str()), "{next}");
    let (code, err) = alice.fail(&["feed", "--cursor", "not-a-cursor"]);
    assert_eq!(code, 4, "a malformed cursor is invalid: {err}");

    let log = alice.ok(&["log", "--actor", &me]);
    let items = log["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert!(items.iter().all(|c| c["actor"] == me.as_str()));
}

#[test]
fn verify_passes_and_reindex_reproduces_search_results() {
    let sb = Sandbox::new("reindex");
    let alice = setup(&sb);
    note(&alice, &["lantern by the lake", "--title", "Lantern"]);
    note(&alice, &["paper lantern workshop"]);
    let before = alice.ok(&["search", "lantern"]);
    assert_eq!(before["items"].as_array().unwrap().len(), 2, "{before}");

    let v = alice.ok(&["verify"]);
    assert!(v["claims"].as_u64().unwrap() > 0, "{v}");
    assert!(v["problems"].as_array().unwrap().is_empty(), "{v}");

    alice.ok(&["reindex"]);
    assert_eq!(alice.ok(&["search", "lantern"]), before);
}

// ---------- audit regressions (DESIGN §5) ----------

/// The newest segment of `actor`'s log in the replica at `space_dir`.
fn newest_segment(space_dir: &Path, actor: &str) -> PathBuf {
    let dir = space_dir.join("log").join(actor);
    let mut names: Vec<PathBuf> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    names.sort();
    names.pop().unwrap()
}

fn append_bytes(path: &Path, bytes: &[u8]) {
    fs::OpenOptions::new().append(true).open(path).unwrap().write_all(bytes).unwrap();
}

#[test]
fn a_forged_log_line_is_an_integrity_error_not_a_grant() {
    use knowa_model::{Claim, ClaimBody, Draft, Id, Term, Timestamp, Value};

    let sb = Sandbox::new("forged");
    let alice = setup(&sb);
    alice.ok(&["actor", "new", "bot", "--kind", "agent"]);
    let e = note(&alice, &["something for the bot"]);
    let bot = alice.as_actor("bot");
    let proposal = last_id(&bot.ok(&["assert", &e, "knowa:summary", "text:a bot summary"]));
    assert_eq!(show_claim(&alice, &proposal)["status"], "proposed");

    // A line appended to alice's log: `bot knowa:role knowa:Owner` with a zero signature.
    let segment = newest_segment(&sb.path("home-space"), &actor_id(&alice, "alice"));
    let text = fs::read_to_string(&segment).unwrap();
    let last = Claim::from_json(&serde_json::from_str(text.lines().last().unwrap()).unwrap()).unwrap();
    let grant = Draft::new(
        Id::parse(&actor_id(&alice, "bot")).unwrap(),
        Term::parse("knowa:role").unwrap(),
        Value::Term(Term::parse("knowa:Owner").unwrap()),
    );
    let b = last.body;
    let body = ClaimBody::from_draft(grant, b.space, b.actor, b.seq + 1, Some(last.id), Timestamp::now());
    let id = Id::claim_of(&body.canonical_bytes().unwrap());
    append_bytes(&segment, &Claim { body, id, sig: "A".repeat(86) }.to_line().unwrap());

    // Before the fix the index caught up on the forged grant and the proposal showed
    // as accepted (bot an Owner).
    let (code, err) = alice.fail(&["show", &proposal]);
    assert_eq!((code, &err["error"]["code"]), (7, &json!("integrity")), "{err}");
}

#[test]
fn a_torn_tail_left_by_a_crashed_append_is_repaired_by_the_next_writer() {
    let sb = Sandbox::new("torn");
    let alice = setup(&sb);
    note(&alice, &["written before the crash"]);
    let segment = newest_segment(&sb.path("home-space"), &actor_id(&alice, "alice"));
    let intact = fs::read(&segment).unwrap();
    // What a crash in the middle of an append leaves: half a line, no newline.
    let last_line = intact[..intact.len() - 1].rsplit(|&b| b == b'\n').next().unwrap().to_vec();
    append_bytes(&segment, &last_line[..last_line.len() / 2]);

    let after = note(&alice, &["written after the crash"]);
    assert!(ids(&alice.ok(&["search", "after the crash"])["items"]).contains(&after));
    assert!(alice.ok(&["verify"])["problems"].as_array().unwrap().is_empty());
}

/// DESIGN §5.4: the import that fills the gap writes a new rebuild generation, and the
/// importing home's index rebuilds on its next open. The index stays under the home.
#[test]
fn an_import_that_fills_a_gap_is_searchable() {
    let sb = Sandbox::new("gapfill");
    let alice = setup_home(&sb, "home-a", "alice", "alice-space");
    let first = note(&alice, &["first public walk"]);
    let hidden = note(&alice, &["hidden harbour diary"]);
    let last = note(&alice, &["last public walk"]);
    alice.ok(&["share", &first, "shared"]);
    alice.ok(&["share", &last, "shared"]);
    let bundle = sb.path("alice-1.knowa");
    alice.ok(&["export", s(&bundle), "--level", "shared"]);

    let bob = setup_home(&sb, "home-b", "bob", "bob-space");
    let im = bob.ok(&["import", s(&bundle)]);
    assert!(im["gaps"].as_u64().unwrap() > 0, "{im}");
    let replica = im["path"].as_str().unwrap().to_string();
    assert!(ids(&bob.ok(&["search", "walk", "--spaces", &replica])["items"]).contains(&last));

    // The hidden note's claims sit below alice's head in bob's replica: catch-up alone
    // would never index them.
    alice.ok(&["share", &hidden, "shared"]);
    let bundle = sb.path("alice-2.knowa");
    alice.ok(&["export", s(&bundle), "--level", "shared"]);
    let im = bob.ok(&["import", s(&bundle)]);
    assert!(im["added"].as_u64().unwrap() > 0, "{im}");
    let hits = bob.ok(&["search", "harbour", "--spaces", &replica]);
    assert_eq!(ids(&hits["items"]), [hidden], "{hits}");
    assert!(Path::new(&replica).join("generation").is_file());
    assert!(!Path::new(&replica).join(".index").exists(), "the replica holds nothing machine-local");
    let space = im["space"].as_str().unwrap();
    let index_dirs: Vec<PathBuf> =
        fs::read_dir(sb.path("home-b").join("index").join(space)).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(index_dirs.len(), 1, "{index_dirs:?}");
    assert!(index_dirs[0].join("index.sqlite").is_file());
}

/// DESIGN §5.2/§5.4: two homes (two machines, in effect) registered on one replica
/// directory each keep their own index. When one imports a bundle that fills a gap
/// below the other's indexed head, the other's next query sees the filled claims: the
/// new rebuild generation is in the replica, where every reader sees it.
#[test]
fn a_gap_filled_through_one_home_is_seen_by_another_home_on_the_same_replica() {
    let sb = Sandbox::new("two-homes");
    let alice = setup_home(&sb, "home-a", "alice", "alice-space");
    let first = note(&alice, &["first public walk"]);
    let hidden = note(&alice, &["hidden harbour diary"]);
    let last = note(&alice, &["last public walk"]);
    alice.ok(&["share", &first, "shared"]);
    alice.ok(&["share", &last, "shared"]);
    let bundle = sb.path("alice-1.knowa");
    alice.ok(&["export", s(&bundle), "--level", "shared"]);

    let carol = sb.home("home-c");
    let replica = carol.ok(&["import", s(&bundle)])["path"].as_str().unwrap().to_string();
    let dave = sb.home("home-d");
    dave.ok(&["space", "add", &replica, "--alias", "shared", "--readonly"]);
    assert!(ids(&dave.ok(&["search", "walk", "--spaces", "shared"])["items"]).contains(&last));
    assert!(ids(&dave.ok(&["search", "harbour", "--spaces", "shared"])["items"]).is_empty());

    alice.ok(&["share", &hidden, "shared"]);
    let bundle = sb.path("alice-2.knowa");
    alice.ok(&["export", s(&bundle), "--level", "shared"]);
    let im = carol.ok(&["import", s(&bundle)]);
    assert_eq!(im["path"].as_str().unwrap(), replica);
    assert!(im["added"].as_u64().unwrap() > 0, "{im}");

    let hits = dave.ok(&["search", "harbour", "--spaces", "shared"]);
    assert_eq!(ids(&hits["items"]), [hidden], "{hits}");
}

/// `blob cat` streams the bytes, also past any one read buffer.
#[test]
fn blob_cat_streams_a_blob_larger_than_a_buffer() {
    let sb = Sandbox::new("blob-cat");
    let alice = setup(&sb);
    let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let file = sb.path("data.bin");
    fs::write(&file, &bytes).unwrap();
    let id = alice.ok(&["blob", "put", s(&file)])["id"].as_str().unwrap().to_string();
    let out = alice.run(&["blob", "cat", &id], None);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout == bytes, "blob cat returned {} bytes, not the {} stored", out.stdout.len(), bytes.len());
}

#[test]
fn no_current_space_is_a_usage_error() {
    let sb = Sandbox::new("nospace");
    let kn = sb.home("home");
    let (code, err) = kn.fail(&["search", "anything"]);
    assert_eq!(code, 2);
    assert_eq!(err["error"]["code"], "usage");
    assert!(err["error"]["message"].as_str().unwrap().contains("KNOWA_SPACE"), "{err}");
}

/// DESIGN §5.2 `--blobs`: the blob location belongs to a registration. The bytes go
/// there, not into the replica, and another home that registers the replica with the
/// same blob location reads them.
#[test]
fn blobs_registered_at_their_own_directory_are_stored_and_read_there() {
    let sb = Sandbox::new("blobs-dir");
    let kn = sb.home("home");
    kn.ok(&["actor", "new", "alice", "--kind", "human"]);
    let alice = kn.as_actor("alice");
    let (replica, bulk) = (sb.path("notes"), sb.path("bulk"));
    let r = alice.ok(&["init", s(&replica), "--name", "notes", "--blobs", s(&bulk)]);
    let (replica, bulk) = (fs::canonicalize(&replica).unwrap(), fs::canonicalize(&bulk).unwrap());
    assert_eq!((&r["alias"], &r["path"], &r["blobs"]), (&json!("notes"), &json!(s(&replica)), &json!(s(&bulk))), "{r}");
    let ls = alice.ok(&["space", "ls"]);
    assert_eq!(ls["spaces"][0]["blobs"], json!(s(&bulk)), "{ls}");

    let file = sb.path("hello.txt");
    fs::write(&file, "hello, bulk").unwrap();
    let id = alice.ok(&["blob", "put", s(&file)])["id"].as_str().unwrap().to_string();
    assert!(bulk.join("blobs").join(&id[6..8]).join(&id).is_file());
    assert!(!replica.join("blobs").exists(), "the replica got a blob directory");

    let bob = sb.home("home-b");
    bob.ok(&["space", "add", s(&replica), "--alias", "shared", "--readonly", "--blobs", s(&bulk)]);
    let out = bob.run(&["--space", "shared", "blob", "cat", &id], None);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.stdout, b"hello, bulk");
    // Registered without its blob location, the replica holds no bytes.
    let (code, err) = sb.home("home-c").fail(&["--space", s(&replica), "blob", "cat", &id]);
    assert_eq!((code, &err["error"]["code"]), (3, &json!("not_found")), "{err}");
}

/// Object-store locations fail fast on what the CLI can check without a request: an
/// unknown scheme is `unsupported`, and a missing region is `usage` naming both
/// variables (DESIGN §5.2), before anything is registered.
#[test]
fn object_locations_need_a_known_scheme_and_a_region() {
    let sb = Sandbox::new("s3-region");
    let kn = sb.home("home");
    kn.ok(&["actor", "new", "alice", "--kind", "human"]);
    let alice = kn.as_actor("alice");
    let (code, err) = alice.fail(&["init", "gs://bucket/notes", "--name", "notes"]);
    assert_eq!((code, &err["error"]["code"]), (6, &json!("unsupported")), "{err}");

    let out = Command::new(env!("CARGO_BIN_EXE_knowa"))
        .args(["--json", "init", "s3://bucket/notes", "--name", "notes"])
        .env("KNOWA_HOME", &kn.home)
        .env("KNOWA_ACTOR", "alice")
        .env_remove("KNOWA_SPACE")
        .env_remove("AWS_REGION")
        .env_remove("AWS_DEFAULT_REGION")
        .output()
        .unwrap();
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!((out.status.code(), &err["error"]["code"]), (Some(2), &json!("usage")), "{err}");
    let message = err["error"]["message"].as_str().unwrap();
    assert!(message.contains("AWS_REGION") && message.contains("AWS_DEFAULT_REGION"), "{message}");
    assert_eq!(kn.ok(&["space", "ls"])["spaces"], json!([]));
}
