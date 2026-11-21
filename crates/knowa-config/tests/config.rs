//! Home layout, key files, and space/actor resolution.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use knowa_config::{EXTRACT_ACTOR, Home, SpaceEntry, SpaceRef, validate_name};
use knowa_model::{ActorKind, Error, ErrorCode, Id, Result};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        Scratch(std::env::temp_dir().join(format!("knowa-config-{tag}-{}-{n}", std::process::id())))
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

fn open_home(s: &Scratch) -> Home {
    Home::open(Some(s.path("home").as_path())).unwrap()
}

fn mode(p: &Path) -> u32 {
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

/// Stands in for knowa-store's `Location::canonical`, which this crate cannot use:
/// drops trailing `/`s, and knows nothing at `missing`.
fn canonical(selector: &str) -> Result<String> {
    if selector.starts_with("s3://bucket/missing") {
        return Err(Error::not_found(format!("{selector} does not exist")));
    }
    Ok(selector.trim_end_matches('/').to_string())
}

fn unreachable_canonical(selector: &str) -> Result<String> {
    panic!("an alias was looked up as a location: {selector}")
}

fn space_ref(alias: Option<&str>, location: &str, blobs: Option<&str>, readonly: bool) -> SpaceRef {
    SpaceRef {
        alias: alias.map(str::to_string),
        location: location.to_string(),
        blobs: blobs.map(str::to_string),
        readonly,
    }
}

#[test]
fn name_grammar() {
    for ok in ["a", "0", "alice", "my-agent.v2_x", &"a".repeat(64)] {
        validate_name(ok).unwrap();
    }
    for bad in ["", "-a", ".a", "_a", "Alice", "a b", "a/b", &"a".repeat(65)] {
        assert_eq!(validate_name(bad).unwrap_err().code, ErrorCode::Invalid, "{bad:?}");
    }
}

#[test]
fn keys_are_private_and_round_trip() {
    let s = Scratch::new("keys");
    let home = open_home(&s);
    assert_eq!(mode(home.root()), 0o700);

    let alice = home.new_actor("alice", ActorKind::Human).unwrap();
    assert_eq!(alice.id, alice.signer.actor_id());
    assert_eq!(mode(&s.path("home/actors")), 0o700);
    assert_eq!(mode(&s.path("home/actors/alice.json")), 0o600);

    let loaded = home.actor("alice").unwrap();
    assert_eq!((loaded.id.clone(), loaded.kind), (alice.id.clone(), ActorKind::Human));
    assert_eq!(loaded.signer.seed(), alice.signer.seed());

    assert_eq!(home.new_actor("alice", ActorKind::Agent).err().unwrap().code, ErrorCode::Conflict);
    assert_eq!(home.actor("bob").err().unwrap().code, ErrorCode::NotFound);
    assert_eq!(home.actor("Bad Name").err().unwrap().code, ErrorCode::Invalid);

    let bot = home.new_actor("bot", ActorKind::Agent).unwrap();
    let listed = home.actors().unwrap();
    assert_eq!(listed, vec![("alice".into(), alice.id.clone(), ActorKind::Human), ("bot".into(), bot.id, ActorKind::Agent)]);
}

#[test]
fn a_key_file_whose_id_disagrees_with_its_seed_is_rejected() {
    let s = Scratch::new("tamper");
    let home = open_home(&s);
    let alice = home.new_actor("alice", ActorKind::Human).unwrap();
    let mallory = home.new_actor("mallory", ActorKind::Human).unwrap();
    let path = s.path("home/actors/alice.json");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replace(alice.id.as_str(), mallory.id.as_str())).unwrap();
    assert_eq!(home.actor("alice").err().unwrap().code, ErrorCode::Integrity);
}

#[test]
fn extract_actor_is_one_stable_worker() {
    let s = Scratch::new("extract");
    let home = open_home(&s);
    let first = home.extract_actor().unwrap();
    assert_eq!((first.name.as_str(), first.kind), (EXTRACT_ACTOR, ActorKind::Worker));
    assert_eq!(home.extract_actor().unwrap().id, first.id);

    let other = Scratch::new("extract-human");
    let home = open_home(&other);
    home.new_actor(EXTRACT_ACTOR, ActorKind::Human).unwrap();
    assert_eq!(home.extract_actor().err().unwrap().code, ErrorCode::Conflict);
}

#[test]
fn actor_selection() {
    let s = Scratch::new("select-actor");
    let home = open_home(&s);
    assert_eq!(home.resolve_actor(None).err().unwrap().code, ErrorCode::Usage);
    let alice = home.new_actor("alice", ActorKind::Human).unwrap();
    assert_eq!(home.set_current_actor("nobody").unwrap_err().code, ErrorCode::NotFound);
    home.set_current_actor("alice").unwrap();
    assert_eq!(home.resolve_actor(None).unwrap().id, alice.id);
    home.new_actor("bot", ActorKind::Agent).unwrap();
    assert_eq!(home.resolve_actor(Some("bot")).unwrap().kind, ActorKind::Agent);
}

#[test]
fn space_registry_and_resolution() {
    let s = Scratch::new("spaces");
    let home = open_home(&s);
    let photos = "s3://bucket/photos";
    let bulk = "s3://bucket/bulk";
    let space = Id::new_space().unwrap();
    assert_eq!(home.resolve_space(None, canonical).unwrap_err().code, ErrorCode::Usage);

    home.register_space("pics", photos, Some(bulk), &space, false).unwrap();
    let by_alias = home.resolve_space(Some("pics"), unreachable_canonical).unwrap();
    assert_eq!(by_alias, space_ref(Some("pics"), photos, Some(bulk), false));

    // A location that is registered resolves to its entry, matched in canonical form,
    // so `readonly` and the blob location apply.
    home.register_space("pics", photos, Some(bulk), &space, true).unwrap();
    let by_location = home.resolve_space(Some("s3://bucket/photos/"), canonical).unwrap();
    assert_eq!(by_location, space_ref(Some("pics"), photos, Some(bulk), true));

    let elsewhere = "s3://bucket/elsewhere";
    assert_eq!(home.register_space("pics", elsewhere, None, &space, false).unwrap_err().code, ErrorCode::Conflict);
    assert_eq!(home.register_space("again", photos, Some(bulk), &space, false).unwrap_err().code, ErrorCode::Conflict);
    assert_eq!(
        home.register_space("pics", photos, Some(bulk), &Id::new_space().unwrap(), false).unwrap_err().code,
        ErrorCode::Conflict
    );
    // DESIGN §5.2: every registration of a replica gives the same blob location.
    assert_eq!(home.register_space("pics", photos, None, &space, true).unwrap_err().code, ErrorCode::Conflict);

    // Unregistered location: resolves without an alias, its blobs at the location.
    let loose = home.resolve_space(Some(elsewhere), canonical).unwrap();
    assert_eq!(loose, space_ref(None, elsewhere, None, false));

    // Neither an alias nor a location that exists.
    let err = home.resolve_space(Some("s3://bucket/missing"), canonical).unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);

    assert_eq!(home.set_current_space("nope").unwrap_err().code, ErrorCode::NotFound);
    home.set_current_space("pics").unwrap();
    assert_eq!(home.resolve_space(None, unreachable_canonical).unwrap().alias.as_deref(), Some("pics"));
    assert_eq!(home.all_spaces().unwrap().len(), 1);

    home.unregister_space("pics").unwrap();
    assert_eq!(home.resolve_space(None, canonical).unwrap_err().code, ErrorCode::Usage);
    assert_eq!(home.unregister_space("pics").unwrap_err().code, ErrorCode::NotFound);
    assert!(home.all_spaces().unwrap().is_empty());
}

/// A v0 home registered a directory under `path`; it reads as that location, and the
/// next save writes `location`.
#[test]
fn v0_registry_entries_read_their_path_as_the_location() {
    let s = Scratch::new("v0-home");
    let home = open_home(&s);
    let space = Id::new_space().unwrap();
    let v0 = format!(r#"{{"currentSpace":"main","spaces":{{"main":{{"path":"/srv/knowa/main","space":"{space}"}}}}}}"#);
    fs::write(s.path("home/config.json"), v0).unwrap();
    let config = home.load_config().unwrap();
    let entry = SpaceEntry { location: "/srv/knowa/main".into(), blobs: None, space: space.clone(), readonly: false };
    assert_eq!(config.spaces["main"], entry);
    assert_eq!(home.resolve_space(None, unreachable_canonical).unwrap(), space_ref(Some("main"), "/srv/knowa/main", None, false));

    home.save_config(&config).unwrap();
    let saved = fs::read_to_string(s.path("home/config.json")).unwrap();
    assert!(saved.contains(r#""location": "/srv/knowa/main""#) && !saved.contains(r#""path""#), "{saved}");
}

#[test]
fn config_file_is_strict_and_private() {
    let s = Scratch::new("config");
    let home = open_home(&s);
    assert_eq!(home.load_config().unwrap(), Default::default());
    home.set_current_actor("x").unwrap_err();
    home.new_actor("x", ActorKind::Human).unwrap();
    home.set_current_actor("x").unwrap();
    assert_eq!(mode(&s.path("home/config.json")), 0o600);
    assert_eq!(home.load_config().unwrap().current_actor.as_deref(), Some("x"));

    fs::write(s.path("home/config.json"), br#"{"spaces":{},"surprise":1}"#).unwrap();
    assert_eq!(home.load_config().unwrap_err().code, ErrorCode::Invalid);

    fs::write(s.path("not-a-dir"), b"").unwrap();
    assert_eq!(Home::open(Some(s.path("not-a-dir").as_path())).err().unwrap().code, ErrorCode::Usage);
}
