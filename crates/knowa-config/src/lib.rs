//! The local home (`~/.knowa` or `KNOWA_HOME`): actor keys and the space registry.
//! Pure local state — never exported, never part of a space.
//!
//! Layout:
//!   <home>/config.json            Config (below)
//!   <home>/actors/<name>.json     ActorFile, mode 0600
//!
//! Precedence for selecting a space/actor is resolved by the CLI (flag > env) and
//! handed here as `Option<&str>`; `None` falls back to `config.json` current values.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use knowa_model::{ActorKind, Id, Result, Signer};
use serde::{Deserialize, Serialize};

/// Name of the built-in extractor actor (DESIGN §7.2).
pub const EXTRACT_ACTOR: &str = "extract";

/// Actor names: `[a-z0-9][a-z0-9._-]{0,63}`. Space aliases: same grammar.
pub fn validate_name(name: &str) -> Result<()> {
    let _ = name;
    todo!()
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
        let _ = explicit;
        todo!()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Missing config.json → `Config::default()` (a fresh home is valid and empty).
    pub fn load_config(&self) -> Result<Config> {
        todo!()
    }

    /// Atomic write (temp + rename).
    pub fn save_config(&self, c: &Config) -> Result<()> {
        let _ = c;
        todo!()
    }

    /// Creates a new actor key. Existing name → `conflict`.
    pub fn new_actor(&self, name: &str, kind: ActorKind) -> Result<Actor> {
        let _ = (name, kind);
        todo!()
    }

    /// Loads an actor; verifies `id` matches the seed (else `integrity`). Missing → `not_found`.
    pub fn actor(&self, name: &str) -> Result<Actor> {
        let _ = name;
        todo!()
    }

    pub fn actors(&self) -> Result<Vec<(String, Id, ActorKind)>> {
        todo!()
    }

    /// Loads the built-in `extract` actor, creating it (kind Worker) on first use.
    pub fn extract_actor(&self) -> Result<Actor> {
        todo!()
    }

    /// `selector` (alias or path) or `config.current_actor`. Neither → `usage` naming
    /// `--actor` / `KNOWA_ACTOR` / `knowa actor use`.
    pub fn resolve_actor(&self, selector: Option<&str>) -> Result<Actor> {
        let _ = selector;
        todo!()
    }

    /// `selector` may be a registered alias or a directory path (containing knowa.json).
    /// `None` → `config.current_space`. Nothing → `usage` naming `--space` /
    /// `KNOWA_SPACE` / `knowa space use`. A path that is registered under an alias
    /// resolves to that entry (so `readonly` applies).
    pub fn resolve_space(&self, selector: Option<&str>) -> Result<SpaceRef> {
        let _ = selector;
        todo!()
    }

    /// All registered spaces as SpaceRefs (for `spaces: ["*"]`).
    pub fn all_spaces(&self) -> Result<Vec<SpaceRef>> {
        todo!()
    }

    /// Registers `path` (canonicalized) under `alias`. Alias taken by another path →
    /// `conflict`; same path re-registered → updates `readonly`.
    pub fn register_space(&self, alias: &str, path: &Path, space: &Id, readonly: bool) -> Result<()> {
        let _ = (alias, path, space, readonly);
        todo!()
    }

    pub fn unregister_space(&self, alias: &str) -> Result<()> {
        let _ = alias;
        todo!()
    }

    pub fn set_current_space(&self, alias: &str) -> Result<()> {
        let _ = alias;
        todo!()
    }

    pub fn set_current_actor(&self, name: &str) -> Result<()> {
        let _ = name;
        todo!()
    }
}
