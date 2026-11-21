//! Verb handlers: arguments → drafts / query documents → one `knowa_api` call → render.
//! Arguments are converted before the home is opened, so bad input fails without
//! side effects. No behavior lives here beyond that mapping.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use knowa_api::{Knowa, PutOptions, Sel};
use knowa_config::Home;
use knowa_model::query::{
    ContextDoc, FindDoc, GetDoc, LogDoc, Near, NearId, Order, PathDoc, QueryDoc, Target, View, WhenRange, Where,
};
use knowa_model::{Dec, Draft, Error, Id, IdKind, Result, Status, Term, Value, When, vocab};
use serde_json::json;

use crate::cli::{
    ActorCmd, AssertArgs, BlobCmd, Cli, Cmd, ContextArgs, DEFAULT_NEAR_KM, FeedArgs, GrantArgs, LogArgs, NearArgs,
    NoteArgs, PathArgs, PendingArgs, PutArgs, ReviewArgs, SearchArgs, ShareArgs, SpaceCmd, TimelineArgs, WorkerCmd,
};
use crate::parse::{self, NearTarget};
use crate::render::{self, Out, json_of};

/// `feed --follow` poll interval once caught up; the same cadence `knowa_api`
/// documents for `worker run --follow`.
const FOLLOW_POLL: Duration = Duration::from_secs(2);

/// Placeholder for the entity a `note` mints.
const NOTE_PLACEHOLDER: &str = "_:n";

struct Globals {
    home: Option<PathBuf>,
    sel: Sel,
}

impl Globals {
    fn home(&self) -> Result<Home> {
        Home::open(self.home.as_deref())
    }

    fn knowa(&self) -> Result<Knowa> {
        Ok(Knowa::new(self.home()?))
    }
}

/// How a command that did not fail ended (DESIGN §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finished {
    Complete,
    /// It finished but skipped some of its inputs; the printed result lists them.
    Skipped,
}

impl Finished {
    pub fn exit_code(self) -> i32 {
        match self {
            Finished::Complete => 0,
            Finished::Skipped => knowa_api::SKIPPED_EXIT,
        }
    }
}

pub fn run(cli: Cli) -> Result<Finished> {
    let g = Globals { home: cli.home, sel: Sel { space: cli.space, actor: cli.actor } };
    let mut out = Out::new(cli.json);
    let finished = dispatch(cli.cmd, &g, &mut out)?;
    out.flush()?;
    Ok(finished)
}

fn dispatch(cmd: Cmd, g: &Globals, out: &mut Out) -> Result<Finished> {
    // Every verb but `put` either completes or fails.
    let completed = match cmd {
        Cmd::Init(a) => {
            let r = g.knowa()?.init(&g.sel, &a.dir, &a.name, a.alias.as_deref())?;
            out.result(|| json_of!(&r), || Ok(render::fields(&json_of!(&r)?)))
        }
        Cmd::Space(c) => space(c, g, out),
        Cmd::Actor(c) => actor(c, g, out),
        Cmd::Write => {
            let drafts = parse::read_drafts(&read_stdin()?)?;
            write(g, out, drafts)
        }
        Cmd::Query(a) => {
            let text = match a.doc.as_deref() {
                None | Some("-") => read_stdin()?,
                Some(doc) => doc.to_string(),
            };
            let doc: QueryDoc = serde_json::from_str(&text)?;
            query(g, out, doc)
        }
        Cmd::Feed(a) => feed(a, g, out),
        Cmd::Blob(BlobCmd::Put { file }) => {
            let mut f = open_input(&file)?;
            let r = g.knowa()?.blob_put(&g.sel, &mut f)?;
            out.result(|| json_of!(&r), || Ok(render::blob_put(&r)))
        }
        Cmd::Blob(BlobCmd::Cat { id, spaces }) => {
            let id = Id::parse(&id)?;
            let path = g.knowa()?.blob_path(&g.sel, &spaces.spaces, &id)?;
            let mut f = File::open(&path)?;
            io::copy(&mut f, out.raw())?;
            Ok(())
        }
        Cmd::Put(a) => return put(a, g, out),
        Cmd::Note(a) => note(a, g, out),
        Cmd::Assert(a) => assert(a, g, out),
        Cmd::Retract(a) => {
            let mut d = Draft::new(claim_id(&a.claim)?, Term::parse(vocab::RETRACT)?, Value::Bool(true));
            d.note = a.note;
            write(g, out, vec![d])
        }
        Cmd::Review(a) => review(a, g, out),
        Cmd::Grant(a) => grant(a, g, out),
        Cmd::Share(a) => share(a, g, out),
        Cmd::Pending(a) => pending(a, g, out),
        Cmd::Search(a) => search(a, g, out),
        Cmd::Timeline(a) => timeline(a, g, out),
        Cmd::Near(a) => near(a, g, out),
        Cmd::Show(a) => {
            let ids = ids(&a.ids)?;
            query(g, out, QueryDoc::Get(GetDoc { spaces: a.spaces.spaces, view: View::default(), as_of: None, ids }))
        }
        Cmd::Context(a) => context(a, g, out),
        Cmd::Path(a) => path(a, g, out),
        Cmd::Log(a) => log(a, g, out),
        Cmd::Extract(a) => {
            let r = g.knowa()?.extract(&g.sel, a.all)?;
            out.result(|| json_of!(&r), || Ok(render::fields(&json_of!(&r)?)))
        }
        Cmd::Worker(WorkerCmd::Run { actor, cmd, batch, follow }) => {
            let argv = parse::split_command(&cmd)?;
            let r = g.knowa()?.worker_run(&g.sel, &actor, &argv, batch, follow)?;
            out.result(|| json_of!(&r), || Ok(render::fields(&json_of!(&r)?)))
        }
        Cmd::Export(a) => {
            let r = g.knowa()?.export(&g.sel, &a.file, a.level.visibility())?;
            out.result(|| json_of!(&r), || Ok(render::fields(&json_of!(&r)?)))
        }
        Cmd::Import(a) => {
            let r = g.knowa()?.import(&a.file, a.dir.as_deref(), a.alias.as_deref())?;
            out.result(|| json_of!(&r), || Ok(render::fields(&json_of!(&r)?)))
        }
        Cmd::Verify => {
            let r = g.knowa()?.verify(&g.sel)?;
            out.result(|| json_of!(&r), || Ok(render::fields(&json_of!(&r)?)))
        }
        Cmd::Reindex => {
            let r = g.knowa()?.reindex(&g.sel)?;
            out.result(|| Ok(json!(r)), || Ok(vec![format!("reindexed: {r}")]))
        }
        Cmd::Schema => {
            // No home is opened: the contract is static.
            let s = knowa_api::schema();
            out.result(
                || Ok(s.clone()),
                || {
                    let pretty = serde_json::to_string_pretty(&s)
                        .map_err(|e| Error::internal(format!("serializing schema: {e}")))?;
                    Ok(vec![pretty])
                },
            )
        }
    };
    completed.map(|()| Finished::Complete)
}

/// The result is printed whether or not files were skipped: exit 9 is not an error.
fn put(a: PutArgs, g: &Globals, out: &mut Out) -> Result<Finished> {
    let opts = PutOptions { no_extract: a.no_extract, no_store: a.no_store, download_evicted: a.download_evicted };
    let r = g.knowa()?.put(&g.sel, &a.paths, opts)?;
    out.result(|| json_of!(&r), || Ok(render::put(&r)))?;
    Ok(if r.skipped.is_empty() { Finished::Complete } else { Finished::Skipped })
}

// ---------- primitives ----------

fn write(g: &Globals, out: &mut Out, drafts: Vec<Draft>) -> Result<()> {
    let r = g.knowa()?.write(&g.sel, drafts)?;
    out.result(|| json_of!(&r), || Ok(render::write(&r)))
}

fn query(g: &Globals, out: &mut Out, doc: QueryDoc) -> Result<()> {
    let r = g.knowa()?.query(&g.sel, doc)?;
    out.result(|| json_of!(&r), || Ok(render::query(&r)))
}

fn feed(a: FeedArgs, g: &Globals, out: &mut Out) -> Result<()> {
    let api = g.knowa()?;
    if !a.follow {
        let r = api.feed(&g.sel, a.cursor.as_deref(), a.limit)?;
        return out.result(|| json_of!(&r), || Ok(render::feed(&r)));
    }
    // Streaming: one line per non-empty batch. With --json each line is a full feed
    // result, so a consumer can resume from the last `cursor` it saw.
    let mut cursor = a.cursor;
    loop {
        let r = api.feed(&g.sel, cursor.as_deref(), a.limit)?;
        let caught_up = r.claims.is_empty();
        if !caught_up {
            if out.is_json() {
                out.emit(&json_of!(&r)?)?;
            } else {
                out.lines(r.claims.iter().map(render::claim))?;
            }
        }
        cursor = Some(r.cursor);
        if caught_up {
            thread::sleep(FOLLOW_POLL);
        }
    }
}

// ---------- write sugar ----------

fn note(a: NoteArgs, g: &Globals, out: &mut Out) -> Result<()> {
    let body = if a.text == "-" { parse::strip_final_newline(read_stdin()?) } else { a.text };
    let n = Id::parse(NOTE_PLACEHOLDER)?;
    let mut drafts = vec![
        Draft::new(n.clone(), Term::parse(vocab::TYPE)?, Value::Term(Term::parse(vocab::T_NOTE)?)),
        Draft::new(n.clone(), Term::parse(vocab::BODY)?, Value::text(body)),
    ];
    if let Some(title) = a.title {
        drafts.push(Draft::new(n.clone(), Term::parse(vocab::LABEL)?, Value::text(title)));
    }
    if let Some(when) = a.when {
        drafts.push(Draft::new(n.clone(), Term::parse(vocab::WHEN)?, Value::When(When::parse(&when)?)));
    }
    for about in &a.about {
        drafts.push(Draft::new(n.clone(), Term::parse(vocab::ABOUT)?, Value::Ref(Id::parse(about)?)));
    }
    write(g, out, drafts)
}

fn assert(a: AssertArgs, g: &Globals, out: &mut Out) -> Result<()> {
    let mut d = Draft::new(Id::parse(&a.s)?, Term::parse(&a.p)?, parse::parse_value(&a.value)?);
    for q in &a.q {
        let (p, v) = parse::split_pair(q, "--q")?;
        d.q.entry(Term::parse(p)?).or_default().push(parse::parse_value(v)?);
    }
    d.valid = a.valid.as_deref().map(When::parse).transpose()?;
    d.conf = a.conf.as_deref().map(Dec::parse).transpose()?;
    d.src = ids(&a.src)?;
    d.replaces = ids(&a.replaces)?;
    for kv in &a.via {
        let (k, v) = parse::split_pair(kv, "--via")?;
        if d.via.insert(k.to_string(), v.to_string()).is_some() {
            return Err(Error::invalid(format!("--via key {k:?} given more than once")));
        }
    }
    d.note = a.note;
    write(g, out, vec![d])
}

fn review(a: ReviewArgs, g: &Globals, out: &mut Out) -> Result<()> {
    let mut d = Draft::new(
        claim_id(&a.claim)?,
        Term::parse(vocab::REVIEW)?,
        Value::Term(Term::parse(a.decision.term())?),
    );
    d.note = a.note;
    write(g, out, vec![d])
}

fn grant(a: GrantArgs, g: &Globals, out: &mut Out) -> Result<()> {
    let actor = actor_id(&a.actor)?;
    let mut drafts = vec![Draft::new(actor.clone(), Term::parse(vocab::ROLE)?, Value::Term(Term::parse(a.role.term())?))];
    if a.auto_accept {
        drafts.push(Draft::new(actor, Term::parse(vocab::AUTO_ACCEPT)?, Value::Bool(true)));
    }
    write(g, out, drafts)
}

fn share(a: ShareArgs, g: &Globals, out: &mut Out) -> Result<()> {
    let d = Draft::new(Id::parse(&a.id)?, Term::parse(vocab::VISIBILITY)?, Value::Term(Term::parse(a.level.term())?));
    write(g, out, vec![d])
}

// ---------- read sugar ----------

fn pending(a: PendingArgs, g: &Globals, out: &mut Out) -> Result<()> {
    let view = View { status: vec![Status::Proposed], ..View::default() };
    let doc = FindDoc {
        spaces: a.spaces.spaces,
        view,
        as_of: None,
        target: Target::Claims,
        where_: Where::default(),
        order: None,
        limit: a.limit,
        cursor: None,
    };
    query(g, out, QueryDoc::Find(doc))
}

fn search(a: SearchArgs, g: &Globals, out: &mut Out) -> Result<()> {
    let mut view = View::default();
    if !a.status.is_empty() {
        view.status = a.status.iter().map(|s| s.status()).collect();
    }
    view.actor_kinds = a.kinds.iter().map(|k| k.kind()).collect();
    let near = match a.near {
        Some(p) => {
            let (lat, lon) =
                parse::split_point(&p).ok_or_else(|| Error::invalid(format!("--near {p:?}: expected lat,lon")))?;
            Some(Near { lat, lon, km: a.km.unwrap_or_else(|| DEFAULT_NEAR_KM.to_string()) })
        }
        None => None,
    };
    let where_ = Where {
        text: Some(a.text),
        type_: terms(&a.types)?,
        when: when_range(a.from, a.to),
        near,
        ..Where::default()
    };
    let doc = FindDoc {
        spaces: a.spaces.spaces,
        view,
        as_of: None,
        target: if a.claims { Target::Claims } else { Target::Entities },
        where_,
        order: None,
        limit: a.limit,
        cursor: a.cursor,
    };
    query(g, out, QueryDoc::Find(doc))
}

fn timeline(a: TimelineArgs, g: &Globals, out: &mut Out) -> Result<()> {
    let doc = FindDoc {
        spaces: a.spaces.spaces,
        view: View::default(),
        as_of: None,
        target: Target::Entities,
        where_: Where { type_: terms(&a.types)?, when: when_range(a.from, a.to), ..Where::default() },
        order: Some(Order::When),
        limit: a.limit,
        cursor: None,
    };
    query(g, out, QueryDoc::Find(doc))
}

fn near(a: NearArgs, g: &Globals, out: &mut Out) -> Result<()> {
    let where_ = match parse::near_target(&a.target)? {
        NearTarget::Point(lat, lon) => Where { near: Some(Near { lat, lon, km: a.km }), ..Where::default() },
        NearTarget::Id(id) => Where { near_id: Some(NearId { id, km: a.km }), ..Where::default() },
    };
    let doc = FindDoc {
        spaces: a.spaces.spaces,
        view: View::default(),
        as_of: None,
        target: Target::Entities,
        where_,
        order: Some(Order::Distance),
        limit: a.limit,
        cursor: None,
    };
    query(g, out, QueryDoc::Find(doc))
}

fn context(a: ContextArgs, g: &Globals, out: &mut Out) -> Result<()> {
    let doc = ContextDoc {
        spaces: a.spaces.spaces,
        view: View::default(),
        as_of: None,
        id: Id::parse(&a.id)?,
        hops: a.hops,
        window: a.window,
        km: a.km,
        limit: None,
    };
    query(g, out, QueryDoc::Context(doc))
}

fn path(a: PathArgs, g: &Globals, out: &mut Out) -> Result<()> {
    let doc = PathDoc {
        spaces: a.spaces.spaces,
        view: View::default(),
        as_of: None,
        from: Id::parse(&a.from)?,
        to: Id::parse(&a.to)?,
        max_hops: a.max_hops,
        limit: None,
    };
    query(g, out, QueryDoc::Path(doc))
}

fn log(a: LogArgs, g: &Globals, out: &mut Out) -> Result<()> {
    let actors = a.actors.iter().map(|s| actor_id(s)).collect::<Result<_>>()?;
    let doc = LogDoc { spaces: a.spaces.spaces, actors, since: a.since, limit: a.limit, cursor: a.cursor };
    query(g, out, QueryDoc::Log(doc))
}

// ---------- local config (home only, not api ops) ----------

fn space(c: SpaceCmd, g: &Globals, out: &mut Out) -> Result<()> {
    match c {
        SpaceCmd::Ls => {
            let cfg = g.home()?.load_config()?;
            let current = cfg.current_space.as_deref();
            let mut items = Vec::new();
            let mut lines = Vec::new();
            for (alias, e) in &cfg.spaces {
                items.push(json!({
                    "alias": alias,
                    "space": e.space,
                    "path": json_of!(&e.path)?,
                    "readonly": e.readonly,
                }));
                lines.push(format!(
                    "{} {alias}  {}  {}{}",
                    if current == Some(alias.as_str()) { "*" } else { " " },
                    e.space,
                    e.path.display(),
                    if e.readonly { "  (readonly)" } else { "" },
                ));
            }
            out.result(|| Ok(json!({ "current": current, "spaces": items })), || Ok(lines))
        }
        SpaceCmd::Add { dir, alias, readonly } => {
            let r = g.knowa()?.space_add(&dir, alias.as_deref(), readonly)?;
            let line = format!("added space {} ({}) at {}", r.alias, r.space, r.path.display());
            out.result(|| json_of!(&r), || Ok(vec![line]))
        }
        SpaceCmd::Rm { alias } => {
            g.home()?.unregister_space(&alias)?;
            out.result(|| Ok(json!({ "removed": alias })), || Ok(vec![format!("removed space {alias}")]))
        }
        SpaceCmd::Use { alias } => {
            g.home()?.set_current_space(&alias)?;
            out.result(|| Ok(json!({ "current": alias })), || Ok(vec![format!("current space: {alias}")]))
        }
    }
}

fn actor(c: ActorCmd, g: &Globals, out: &mut Out) -> Result<()> {
    match c {
        ActorCmd::Ls => {
            let home = g.home()?;
            let current = home.load_config()?.current_actor;
            let mut items = Vec::new();
            let mut lines = Vec::new();
            for (name, id, kind) in home.actors()? {
                let mark = if current.as_deref() == Some(name.as_str()) { "*" } else { " " };
                lines.push(format!("{mark} {name}  {id}  {}", render::kind(kind)));
                items.push(json!({ "name": name, "id": id, "kind": json_of!(&kind)? }));
            }
            out.result(|| Ok(json!({ "current": current, "actors": items })), || Ok(lines))
        }
        ActorCmd::New { name, kind } => {
            let a = g.home()?.new_actor(&name, kind.kind())?;
            actor_out(out, &a.name, &a.id, a.kind)
        }
        ActorCmd::Use { name } => {
            g.home()?.set_current_actor(&name)?;
            out.result(|| Ok(json!({ "current": name })), || Ok(vec![format!("current actor: {name}")]))
        }
        ActorCmd::Show { name } => {
            let home = g.home()?;
            let a = match name {
                Some(n) => home.actor(&n)?,
                None => home.resolve_actor(g.sel.actor.as_deref())?,
            };
            actor_out(out, &a.name, &a.id, a.kind)
        }
    }
}

/// Public identity only; the signing seed is never printed.
fn actor_out(out: &mut Out, name: &str, id: &Id, kind: knowa_model::ActorKind) -> Result<()> {
    out.result(
        || Ok(json!({ "name": name, "id": id, "kind": json_of!(&kind)? })),
        || Ok(vec![format!("{name}  {id}  {}", render::kind(kind))]),
    )
}

// ---------- helpers ----------

fn ids(raw: &[String]) -> Result<Vec<Id>> {
    raw.iter().map(|s| Id::parse(s)).collect()
}

fn terms(raw: &[String]) -> Result<Vec<Term>> {
    raw.iter().map(|s| Term::parse(s)).collect()
}

/// An id argument that the verb defines as one kind (e.g. `retract <claim>`).
fn claim_id(s: &str) -> Result<Id> {
    id_of_kind(s, IdKind::Claim, "a claim id (c_…)")
}

fn actor_id(s: &str) -> Result<Id> {
    id_of_kind(s, IdKind::Actor, "an actor id (a_…)")
}

fn id_of_kind(s: &str, kind: IdKind, what: &str) -> Result<Id> {
    let id = Id::parse(s)?;
    if id.kind() != kind {
        return Err(Error::invalid(format!("{s:?} is not {what}")));
    }
    Ok(id)
}

fn when_range(from: Option<String>, to: Option<String>) -> Option<WhenRange> {
    (from.is_some() || to.is_some()).then_some(WhenRange { from, to })
}

fn read_stdin() -> Result<String> {
    let mut s = String::new();
    io::stdin().read_to_string(&mut s).map_err(|e| match e.kind() {
        io::ErrorKind::InvalidData => Error::invalid("stdin is not valid UTF-8"),
        _ => Error::from(e),
    })?;
    Ok(s)
}

/// A file the user named: absent is `not_found`, not `internal`.
fn open_input(p: &Path) -> Result<File> {
    File::open(p).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => Error::not_found(format!("{}: no such file", p.display())),
        _ => Error::internal(format!("{}: {e}", p.display())),
    })
}
