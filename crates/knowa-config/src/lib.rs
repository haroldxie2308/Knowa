//! The local home (`~/.knowa` or `KNOWA_HOME`): actor keys and the space registry.
//! Pure local state — never exported, never part of a space.
//!
//! Layout:
//!   <home>/config.json            Config (below)
//!   <home>/actors/<name>.json     ActorFile, mode 0600
//!
//! Precedence for selecting a space/actor is resolved by the CLI (flag > env) and
//! handed here as `Option<&str>`; `None` falls back to `config.json` current values.

mod b64;

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use knowa_model::{ActorKind, Error, ErrorCode, Id, IdKind, Result, Signer};
use serde::{Deserialize, Serialize};

/// Name of the built-in extractor actor (DESIGN §7.2).
pub const EXTRACT_ACTOR: &str = "extract";

const CONFIG_FILE: &str = "config.json";
const ACTORS_DIR: &str = "actors";
/// The replica manifest's file name. knowa-store owns it, but this crate must not
/// depend on the store; DESIGN §5 fixes the name.
const MANIFEST_FILE: &str = "knowa.json";
const PRIVATE_DIR: u32 = 0o700;
const PRIVATE_FILE: u32 = 0o600;

/// Actor names: `[a-z0-9][a-z0-9._-]{0,63}`. Space aliases: same grammar.
pub fn validate_name(name: &str) -> Result<()> {
    let b = name.as_bytes();
    let ok = matches!(b.first(), Some(b'a'..=b'z' | b'0'..=b'9'))
        && b.len() <= 64
        && b[1..].iter().all(|c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(Error::invalid(format!("invalid name {name:?}: expected [a-z0-9][a-z0-9._-]{{0,63}}")))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpaceEntry {
    /// absolute path of the replica directory
    pub path: PathBuf,
    pub space: Id,
    #[serde(default)]
    pub readonly: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_space: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_actor: Option<String>,
    #[serde(default)]
    pub spaces: BTreeMap<String, SpaceEntry>,
}

/// On-disk actor file. `seed` is base64url (no padding) of the 32-byte Ed25519 seed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActorFile {
    pub name: String,
    pub id: Id,
    pub kind: ActorKind,
    pub seed: String,
}

pub struct Actor {
    pub name: String,
    pub id: Id,
    pub kind: ActorKind,
    pub signer: Signer,
}

/// A resolved space selection.
#[derive(Debug, Clone, PartialEq)]
pub struct SpaceRef {
    /// registry alias, if the space is registered
    pub alias: Option<String>,
    pub path: PathBuf,
    pub readonly: bool,
}

pub struct Home {
    root: PathBuf,
}

impl Home {
    /// `explicit` (flag/env, already resolved by the CLI) or `$HOME/.knowa`. Creates the
    /// directory (0700) if missing. No `$HOME` and no explicit path → `usage`.
    pub fn open(explicit: Option<&Path>) -> Result<Home> {
        let root = match explicit {
            Some(p) => p.to_path_buf(),
            None => match std::env::var_os("HOME") {
                Some(h) if !h.is_empty() => PathBuf::from(h).join(".knowa"),
                _ => return Err(Error::usage("no Knowa home: pass --home or set KNOWA_HOME ($HOME is not set)")),
            },
        };
        match fs::metadata(&root) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(Error::usage(format!(
                    "Knowa home {} is not a directory; pass --home or set KNOWA_HOME to a directory",
                    root.display()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => create_private_dir(&root)?,
            Err(e) => return Err(io_err(&root, e)),
        }
        Ok(Home { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Missing config.json → `Config::default()` (a fresh home is valid and empty).
    pub fn load_config(&self) -> Result<Config> {
        let path = self.root.join(CONFIG_FILE);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| Error::invalid(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(io_err(&path, e)),
        }
    }

    /// Atomic write (temp + rename).
    pub fn save_config(&self, c: &Config) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(c)?;
        bytes.push(b'\n');
        atomic_write(&self.root.join(CONFIG_FILE), &bytes)
    }

    fn actor_path(&self, name: &str) -> PathBuf {
        self.root.join(ACTORS_DIR).join(format!("{name}.json"))
    }

    /// Creates a new actor key. Existing name → `conflict`.
    pub fn new_actor(&self, name: &str, kind: ActorKind) -> Result<Actor> {
        validate_name(name)?;
        let dir = self.root.join(ACTORS_DIR);
        if !dir.is_dir() {
            create_private_dir(&dir)?;
        }
        let signer = Signer::generate()?;
        let id = signer.actor_id();
        let file = ActorFile { name: name.to_string(), id: id.clone(), kind, seed: b64::encode(signer.seed()) };
        let mut bytes = serde_json::to_vec_pretty(&file)?;
        bytes.push(b'\n');
        let path = self.actor_path(name);
        // create_new rather than temp + rename: an existing key must never be
        // replaced, not even by a concurrent `actor new` with the same name.
        let mut f = match OpenOptions::new().write(true).create_new(true).mode(PRIVATE_FILE).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(Error::conflict(format!("actor {name:?} already exists")));
            }
            Err(e) => return Err(io_err(&path, e)),
        };
        f.set_permissions(Permissions::from_mode(PRIVATE_FILE)).map_err(|e| io_err(&path, e))?;
        f.write_all(&bytes).and_then(|()| f.sync_all()).map_err(|e| io_err(&path, e))?;
        sync_dir(&dir)?;
        Ok(Actor { name: name.to_string(), id, kind, signer })
    }

    /// Loads an actor; verifies `id` matches the seed (else `integrity`). Missing → `not_found`.
    pub fn actor(&self, name: &str) -> Result<Actor> {
        validate_name(name)?;
        let path = self.actor_path(name);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(Error::not_found(format!(
                    "actor {name:?} not found (create it with `knowa actor new {name} --kind …`)"
                )));
            }
            Err(e) => return Err(io_err(&path, e)),
        };
        let file: ActorFile =
            serde_json::from_slice(&bytes).map_err(|e| Error::invalid(format!("{}: {e}", path.display())))?;
        if file.name != name {
            return Err(Error::integrity(format!("{}: names actor {:?}", path.display(), file.name)));
        }
        let seed = b64::decode(&file.seed)
            .map_err(|e| Error::invalid(format!("{}: seed: {}", path.display(), e.message)))?;
        let seed: [u8; 32] = seed
            .try_into()
            .map_err(|v: Vec<u8>| Error::invalid(format!("{}: seed is {} bytes, expected 32", path.display(), v.len())))?;
        let signer = Signer::from_seed(seed);
        if signer.actor_id() != file.id {
            return Err(Error::integrity(format!(
                "{}: id {} does not match its key ({})",
                path.display(),
                file.id,
                signer.actor_id()
            )));
        }
        Ok(Actor { name: file.name, id: file.id, kind: file.kind, signer })
    }

    pub fn actors(&self) -> Result<Vec<(String, Id, ActorKind)>> {
        let dir = self.root.join(ACTORS_DIR);
        let entries = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            // No actor has been created yet.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(&dir, e)),
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&dir, e))?;
            let file = entry.file_name();
            let file = file
                .to_str()
                .ok_or_else(|| Error::integrity(format!("{}: non-UTF-8 file name", entry.path().display())))?;
            // Dotfiles are our own uncommitted temp files or OS metadata (.DS_Store);
            // neither is an actor.
            if file.starts_with('.') {
                continue;
            }
            let name = file
                .strip_suffix(".json")
                .filter(|n| validate_name(n).is_ok())
                .ok_or_else(|| Error::integrity(format!("{}: not an actor file (<name>.json)", entry.path().display())))?;
            let actor = self.actor(name)?;
            out.push((actor.name, actor.id, actor.kind));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Loads the built-in `extract` actor, creating it (kind Worker) on first use.
    pub fn extract_actor(&self) -> Result<Actor> {
        let actor = match self.actor(EXTRACT_ACTOR) {
            Ok(a) => a,
            Err(e) if e.code == ErrorCode::NotFound => match self.new_actor(EXTRACT_ACTOR, ActorKind::Worker) {
                Ok(a) => a,
                // A concurrent first use created it between our read and our create.
                Err(e) if e.code == ErrorCode::Conflict => self.actor(EXTRACT_ACTOR)?,
                Err(e) => return Err(e),
            },
            Err(e) => return Err(e),
        };
        if actor.kind != ActorKind::Worker {
            return Err(Error::conflict(format!(
                "actor {EXTRACT_ACTOR:?} exists with kind {:?}; the built-in extractor needs a Worker key under that name",
                actor.kind
            )));
        }
        Ok(actor)
    }

    /// `selector` (an actor name) or `config.current_actor`. Neither → `usage` naming
    /// `--actor` / `KNOWA_ACTOR` / `knowa actor use`.
    pub fn resolve_actor(&self, selector: Option<&str>) -> Result<Actor> {
        let name = match selector {
            Some(s) => s.to_string(),
            None => self.load_config()?.current_actor.ok_or_else(|| {
                Error::usage("no actor selected: pass --actor, set KNOWA_ACTOR, or run `knowa actor use <name>`")
            })?,
        };
        self.actor(&name)
    }

    /// `selector` may be a registered alias or a directory path (containing knowa.json).
    /// `None` → `config.current_space`. Nothing → `usage` naming `--space` /
    /// `KNOWA_SPACE` / `knowa space use`. A path that is registered under an alias
    /// resolves to that entry (so `readonly` applies).
    pub fn resolve_space(&self, selector: Option<&str>) -> Result<SpaceRef> {
        let config = self.load_config()?;
        let selector = match selector {
            Some(s) => s.to_string(),
            None => config.current_space.clone().ok_or_else(|| {
                Error::usage("no space selected: pass --space, set KNOWA_SPACE, or run `knowa space use <alias>`")
            })?,
        };
        if let Some(entry) = config.spaces.get(&selector) {
            return Ok(entry_ref(&selector, entry));
        }
        let path = match fs::canonicalize(&selector) {
            Ok(p) => p,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(Error::not_found(format!(
                    "{selector:?} is neither a registered space alias nor an existing directory"
                )));
            }
            Err(e) => return Err(io_err(Path::new(&selector), e)),
        };
        if !path.join(MANIFEST_FILE).is_file() {
            return Err(Error::not_found(format!("{} is not a Knowa replica (no {MANIFEST_FILE})", path.display())));
        }
        if let Some((alias, entry)) = config.spaces.iter().find(|(_, e)| e.path == path) {
            return Ok(entry_ref(alias, entry));
        }
        Ok(SpaceRef { alias: None, path, readonly: false })
    }

    /// All registered spaces as SpaceRefs (for `spaces: ["*"]`).
    pub fn all_spaces(&self) -> Result<Vec<SpaceRef>> {
        Ok(self.load_config()?.spaces.iter().map(|(alias, e)| entry_ref(alias, e)).collect())
    }

    /// Registers `path` (canonicalized) under `alias`. Alias taken by another path →
    /// `conflict`; same path re-registered → updates `readonly`.
    pub fn register_space(&self, alias: &str, path: &Path, space: &Id, readonly: bool) -> Result<()> {
        validate_name(alias)?;
        if space.kind() != IdKind::Space {
            return Err(Error::invalid(format!("{space} is not a space id")));
        }
        let path = match fs::canonicalize(path) {
            Ok(p) => p,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(Error::not_found(format!("{} does not exist", path.display())));
            }
            Err(e) => return Err(io_err(path, e)),
        };
        let mut config = self.load_config()?;
        if let Some((other, _)) = config.spaces.iter().find(|(a, e)| e.path == path && a.as_str() != alias) {
            return Err(Error::conflict(format!("{} is already registered as {other:?}", path.display())));
        }
        match config.spaces.get_mut(alias) {
            Some(e) if e.path != path => {
                return Err(Error::conflict(format!("alias {alias:?} is taken by {}", e.path.display())));
            }
            Some(e) if e.space != *space => {
                return Err(Error::conflict(format!(
                    "alias {alias:?} at {} is registered for space {}, not {space}",
                    path.display(),
                    e.space
                )));
            }
            Some(e) => e.readonly = readonly,
            None => {
                config.spaces.insert(alias.to_string(), SpaceEntry { path, space: space.clone(), readonly });
            }
        }
        self.save_config(&config)
    }

    /// Also clears `current_space` when it names `alias`, which would otherwise
    /// point at nothing.
    pub fn unregister_space(&self, alias: &str) -> Result<()> {
        let mut config = self.load_config()?;
        if config.spaces.remove(alias).is_none() {
            return Err(Error::not_found(format!("no space registered as {alias:?}")));
        }
        if config.current_space.as_deref() == Some(alias) {
            config.current_space = None;
        }
        self.save_config(&config)
    }

    pub fn set_current_space(&self, alias: &str) -> Result<()> {
        let mut config = self.load_config()?;
        if !config.spaces.contains_key(alias) {
            return Err(Error::not_found(format!("no space registered as {alias:?} (see `knowa space ls`)")));
        }
        config.current_space = Some(alias.to_string());
        self.save_config(&config)
    }

    pub fn set_current_actor(&self, name: &str) -> Result<()> {
        self.actor(name)?;
        let mut config = self.load_config()?;
        config.current_actor = Some(name.to_string());
        self.save_config(&config)
    }
}

fn entry_ref(alias: &str, e: &SpaceEntry) -> SpaceRef {
    SpaceRef { alias: Some(alias.to_string()), path: e.path.clone(), readonly: e.readonly }
}

fn io_err(path: &Path, e: io::Error) -> Error {
    Error::internal(format!("io: {}: {e}", path.display()))
}

/// Creates `dir` (parents with default permissions) and forces mode 0700 on it,
/// whatever the umask.
fn create_private_dir(dir: &Path) -> Result<()> {
    if let Some(parent) = dir.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
    }
    match fs::DirBuilder::new().mode(PRIVATE_DIR).create(dir) {
        Ok(()) => {}
        // A concurrent process created it first; the mode is forced below either way.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => {}
        Err(e) => return Err(io_err(dir, e)),
    }
    fs::set_permissions(dir, Permissions::from_mode(PRIVATE_DIR)).map_err(|e| io_err(dir, e))
}

fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir).and_then(|d| d.sync_all()).map_err(|e| io_err(dir, e))
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Replaces `dest` atomically: a 0600 temp file in the same directory, fsync,
/// rename, directory fsync.
fn atomic_write(dest: &Path, bytes: &[u8]) -> Result<()> {
    let dir = dest
        .parent()
        .ok_or_else(|| Error::internal(format!("{} has no parent directory", dest.display())))?;
    let (tmp, mut file) = loop {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".tmp-{}-{n}", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).mode(PRIVATE_FILE).open(&tmp) {
            Ok(f) => break (tmp, f),
            // A crashed process with the same pid left this name behind; its content
            // was never committed, so take the next name.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_err(&tmp, e)),
        }
    };
    let written = file.write_all(bytes).and_then(|()| file.sync_all()).and_then(|()| fs::rename(&tmp, dest));
    if let Err(e) = written {
        // Error path: the failed write is what the caller needs to see. A temp file we
        // cannot remove is dot-prefixed and ignored by every reader of this directory.
        let _ = fs::remove_file(&tmp);
        return Err(io_err(dest, e));
    }
    sync_dir(dir)
}
