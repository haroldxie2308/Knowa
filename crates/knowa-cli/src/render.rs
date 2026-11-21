//! Output. `--json` prints the api result as one compact JSON document (contract);
//! text output is concise and for humans only (not contract).

use std::io::{self, Write};

use knowa_model::query::{
    BlobPutResult, ContextResult, FeedResult, FindItems, FindResult, GetItem, GetResult, LogResult, PathResult,
    QueryResult, WriteResult,
};
use knowa_model::{ActorInfo, ActorKind, ClaimView, Dir, EntityView, Result, Role, Value};

/// `serde_json::to_value` for api results. The CLI has no direct serde dependency, so
/// the `Serialize` bound can only be satisfied at call sites with concrete types.
macro_rules! json_of {
    ($v:expr) => {
        serde_json::to_value($v).map_err(|e| knowa_model::Error::internal(format!("serializing result: {e}")))
    };
}
pub(crate) use json_of;

const TEXT_PREVIEW_CHARS: usize = 80;

pub struct Out {
    json: bool,
    w: io::StdoutLock<'static>,
}

impl Out {
    pub fn new(json: bool) -> Out {
        Out { json, w: io::stdout().lock() }
    }

    pub fn is_json(&self) -> bool {
        self.json
    }

    /// One JSON document on its own line.
    pub fn emit(&mut self, v: &serde_json::Value) -> Result<()> {
        writeln!(self.w, "{v}")?;
        Ok(())
    }

    pub fn lines(&mut self, lines: impl IntoIterator<Item = String>) -> Result<()> {
        for l in lines {
            writeln!(self.w, "{l}")?;
        }
        Ok(())
    }

    /// The JSON form with `--json`, else the text lines.
    pub fn result(
        &mut self,
        json: impl FnOnce() -> Result<serde_json::Value>,
        text: impl FnOnce() -> Result<Vec<String>>,
    ) -> Result<()> {
        if self.json {
            let v = json()?;
            self.emit(&v)
        } else {
            let lines = text()?;
            self.lines(lines)
        }
    }

    /// Raw byte sink (`blob cat`).
    pub fn raw(&mut self) -> &mut io::StdoutLock<'static> {
        &mut self.w
    }

    pub fn flush(&mut self) -> Result<()> {
        self.w.flush()?;
        Ok(())
    }
}

/// `id  label  [types]  when  where`, plus distance/score when the query produced them.
pub fn entity(e: &EntityView) -> String {
    let types: Vec<&str> = e.types.iter().map(|t| t.as_str()).collect();
    let mut line = format!(
        "{}  {}  [{}]  {}  {}",
        e.id,
        e.label.as_deref().unwrap_or("-"),
        types.join(", "),
        e.when.as_ref().map_or("-", |w| w.value.as_str()),
        e.where_.as_ref().map_or_else(|| "-".to_string(), |w| format!("{},{}", w.lat, w.lon)),
    );
    if let Some(d) = e.distance_km {
        line.push_str(&format!("  {d:.3}km"));
    }
    if let Some(s) = e.score {
        line.push_str(&format!("  score={s:.3}"));
    }
    line
}

/// `id  s  p  o  (status[, stale])`
pub fn claim(c: &ClaimView) -> String {
    let b = &c.claim.body;
    let stale = if c.stale { ", stale" } else { "" };
    format!("{}  {}  {}  {}  ({}{stale})", c.claim.id, b.s, b.p, value(&b.o), c.status.as_str())
}

pub fn value(v: &Value) -> String {
    match v {
        Value::Text { text, lang } => {
            let mut shown: String = text.chars().take(TEXT_PREVIEW_CHARS).collect();
            if text.chars().nth(TEXT_PREVIEW_CHARS).is_some() {
                shown.push('…');
            }
            let quoted = serde_json::Value::String(shown).to_string();
            match lang {
                Some(l) => format!("{quoted}@{l}"),
                None => quoted,
            }
        }
        Value::Term(t) => t.to_string(),
        Value::Ref(id) => id.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Dec(d) => d.as_str().to_string(),
        Value::Bool(b) => b.to_string(),
        Value::When(w) => w.raw.clone(),
        Value::Geo(g) => match &g.r {
            Some(r) => format!("{},{} ±{}m", g.lat.as_str(), g.lon.as_str(), r.as_str()),
            None => format!("{},{}", g.lat.as_str(), g.lon.as_str()),
        },
        Value::Qty { qty, unit } => format!("{} {unit}", qty.as_str()),
    }
}

pub fn write(r: &WriteResult) -> Vec<String> {
    let mut out: Vec<String> = r
        .items
        .iter()
        .map(|i| format!("{}  {}  ({})", i.id, if i.created { "created" } else { "exists" }, i.status.as_str()))
        .collect();
    out.extend(r.placeholders.iter().map(|(ph, id)| format!("{ph} = {id}")));
    out
}

pub fn query(r: &QueryResult) -> Vec<String> {
    match r {
        QueryResult::Find(f) => find(f),
        QueryResult::Get(g) => get(g),
        QueryResult::Context(c) => context(c),
        QueryResult::Path(p) => path(p),
        QueryResult::Log(l) => log(l),
    }
}

pub fn find(r: &FindResult) -> Vec<String> {
    let mut out: Vec<String> = match &r.items {
        FindItems::Entities(es) => es.iter().map(entity).collect(),
        FindItems::Claims(cs) => cs.iter().map(claim).collect(),
    };
    if let Some(c) = &r.cursor {
        out.push(format!("cursor: {c}"));
    }
    out
}

pub fn get(r: &GetResult) -> Vec<String> {
    let mut out = Vec::new();
    for item in &r.items {
        match item {
            GetItem::Entity { entity: e, claims } => {
                out.push(entity(e));
                out.extend(claims.iter().map(|c| format!("  {}", claim(c))));
            }
            GetItem::Claim { claim: c } => out.push(claim(c)),
            GetItem::Blob { blob } => {
                let spaces: Vec<&str> = blob.spaces.iter().map(|s| s.as_str()).collect();
                out.push(format!(
                    "{}  {} bytes  {}  [{}]",
                    blob.id,
                    blob.size,
                    blob.media_type.as_deref().unwrap_or("-"),
                    spaces.join(", ")
                ));
            }
        }
    }
    out
}

pub fn context(r: &ContextResult) -> Vec<String> {
    let mut out = vec![format!("entity: {}", entity(&r.entity))];
    section(&mut out, "claims", r.claims.iter().map(claim));
    section(&mut out, "inbound", r.inbound.iter().map(claim));
    section(
        &mut out,
        "neighbors",
        r.neighbors.iter().map(|n| format!("{}  {}  {}  {}", n.hop, dir(n.dir), n.p, entity(&n.entity))),
    );
    section(&mut out, "temporal", r.temporal.iter().map(entity));
    section(&mut out, "spatial", r.spatial.iter().map(entity));
    section(&mut out, "actors", r.actors.iter().map(actor_info));
    out
}

fn section(out: &mut Vec<String>, title: &str, lines: impl Iterator<Item = String>) {
    out.push(format!("{title}:"));
    let before = out.len();
    out.extend(lines.map(|l| format!("  {l}")));
    if out.len() == before {
        out.push("  (none)".to_string());
    }
}

fn dir(d: Dir) -> &'static str {
    match d {
        Dir::Out => "out",
        Dir::In => "in",
    }
}

fn actor_info(a: &ActorInfo) -> String {
    format!(
        "{}  {}  {}  autoAccept={}  claims={}",
        a.id,
        a.kind.map_or("-", kind),
        a.role.map_or("-", role),
        a.auto_accept,
        a.claims
    )
}

pub fn kind(k: ActorKind) -> &'static str {
    match k {
        ActorKind::Human => "human",
        ActorKind::Agent => "agent",
        ActorKind::Worker => "worker",
    }
}

fn role(r: Role) -> &'static str {
    match r {
        Role::Owner => "owner",
        Role::Member => "member",
        Role::Agent => "agent",
        Role::Worker => "worker",
        Role::Viewer => "viewer",
    }
}

/// `a -[p]-> b <-[p]- c`, one path per line, shortest first.
pub fn path(r: &PathResult) -> Vec<String> {
    if r.paths.is_empty() {
        return vec!["no path".to_string()];
    }
    r.paths
        .iter()
        .map(|steps| {
            let mut line = String::new();
            for step in steps {
                match (&step.p, step.dir) {
                    (Some(p), Some(Dir::Out)) => line.push_str(&format!(" -[{p}]-> ")),
                    (Some(p), Some(Dir::In)) => line.push_str(&format!(" <-[{p}]- ")),
                    _ => {}
                }
                line.push_str(step.id.as_str());
            }
            line
        })
        .collect()
}

pub fn log(r: &LogResult) -> Vec<String> {
    let mut out: Vec<String> = r.items.iter().map(claim).collect();
    if let Some(c) = &r.cursor {
        out.push(format!("cursor: {c}"));
    }
    out
}

pub fn feed(r: &FeedResult) -> Vec<String> {
    let mut out: Vec<String> = r.claims.iter().map(claim).collect();
    out.push(format!("cursor: {}", r.cursor));
    out
}

pub fn blob_put(r: &BlobPutResult) -> Vec<String> {
    vec![format!("{}  {} bytes  {}", r.id, r.size, if r.created { "created" } else { "exists" })]
}

pub fn put(r: &knowa_api::PutResult) -> Vec<String> {
    let mut out = Vec::new();
    for i in &r.items {
        out.push(format!(
            "{}  {}  {} bytes  {}  {} claims",
            i.path.display(),
            i.blob,
            i.size,
            match (i.created, i.stored) {
                (true, _) => "created",
                (false, true) => "exists",
                (false, false) => "catalog only",
            },
            i.claims.len()
        ));
        out.extend(i.errors.iter().map(|e| format!("  error: {e}")));
    }
    out.extend(r.skipped.iter().map(|s| format!("{}  skipped  {}: {}", s.path.display(), s.code.as_str(), s.message)));
    out
}

/// `key: value` per top-level field of an admin result; scalars bare, the rest as JSON.
pub fn fields(v: &serde_json::Value) -> Vec<String> {
    match v {
        serde_json::Value::Object(m) => m.iter().map(|(k, v)| format!("{k}: {}", scalar(v))).collect(),
        other => vec![scalar(other)],
    }
}

fn scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
