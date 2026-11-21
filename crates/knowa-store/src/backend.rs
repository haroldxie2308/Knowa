//! The storage seam (DESIGN §5): a replica is its manifest plus two stores, one for
//! the claim logs (which also holds the manifest and the rebuild generation, at the
//! replica's location) and one for blobs (at the blob location). Everything that is
//! not storage — claim checks, chains, merge plans, verification — is written once
//! against these traits.
//!
//! The methods are shaped so that a store with no append, no rename, no locks and no
//! local paths (an object store with create-only writes) can implement them: a log
//! batch is handed over whole, reads return bytes or readers, and a store without
//! locks returns guards that hold nothing.

use std::any::Any;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};

use knowa_model::{Id, Result};

/// A held actor lock (DESIGN §5.3); dropping it releases the lock.
pub(crate) type Guard = Box<dyn Any + Send + Sync>;

/// What a listing found, plus the entries that do not belong (for `verify` and
/// `integrity` errors).
pub(crate) type Listing<T> = (Vec<T>, Vec<String>);

/// One segment of an actor's log, as its store lists it.
pub(crate) struct Segment {
    pub(crate) first: u64,
    /// where the segment is, for messages: a path or an object key
    pub(crate) name: String,
}

/// The end of a segment: its last `\n`-terminated line and what follows it.
pub(crate) struct LastLine {
    /// the last complete line, `\n` included; `None` when the segment has none
    pub(crate) line: Option<Vec<u8>>,
    /// bytes after that line, i.e. an unterminated final line
    pub(crate) trailing: u64,
}

/// The replica's location: manifest, rebuild generation and claim logs.
pub(crate) trait Logs: Send + Sync {
    /// The location, for messages.
    fn describe(&self) -> String;

    /// Whether `lock_exclusive` keeps every other writer of the actor out (a directory).
    /// `false` for a store without locks: two writers of one actor may both read its
    /// head, and the loser's `append` fails with `conflict` (DESIGN §5.3).
    fn exclusive_locks(&self) -> bool;

    /// Readies the location for a new replica, before anything is written: it must not
    /// exist or hold nothing (`conflict` otherwise).
    fn prepare(&self) -> Result<()>;

    /// `knowa.json`; `None` when absent.
    fn read_manifest(&self) -> Result<Option<Vec<u8>>>;

    /// Writes `knowa.json` whole, atomically, once (DESIGN §5: it never changes). A
    /// store that can create an object only if absent does so: present → `conflict`.
    fn write_manifest(&self, bytes: &[u8]) -> Result<()>;

    /// `generation` (DESIGN §5.4); `None` when absent.
    fn read_generation(&self) -> Result<Option<Vec<u8>>>;

    /// Replaces `generation` whole, atomically: a reader sees the old or the new value.
    fn write_generation(&self, bytes: &[u8]) -> Result<()>;

    /// Blocks until `actor`'s write exclusivity is held. A store without locks returns
    /// a guard that holds nothing; it relies on create-only segment writes instead.
    fn lock_exclusive(&self, actor: &Id) -> Result<Guard>;

    /// Blocks until `actor`'s log may be read: no batch is being written to it.
    /// `None` when the actor has no log, so there is nothing to read.
    fn lock_shared(&self, actor: &Id) -> Result<Option<Guard>>;

    /// Torn-tail recovery for `actor`, called under its exclusive lock. A store whose
    /// segments are written whole has nothing to repair.
    fn repair_tail(&self, actor: &Id) -> Result<()>;

    /// Actors with a log, ascending, plus entries that do not belong.
    fn actors(&self) -> Result<Listing<Id>>;

    /// `actor`'s segments, ascending by first seq, plus misnamed entries.
    fn segments(&self, actor: &Id) -> Result<Listing<Segment>>;

    /// Reads one segment from its start.
    fn open_segment(&self, actor: &Id, segment: &Segment) -> Result<Box<dyn BufRead + Send>>;

    /// The last complete line of a segment and the length of what follows it.
    fn last_line(&self, actor: &Id, segment: &Segment) -> Result<LastLine>;

    /// Appends one batch of `actor`'s lines, seqs `first_seq..`, where `first_seq` is
    /// one above the head the caller read under the actor's exclusive lock. Durable on
    /// return; on error none of the batch remains. A log that already holds `first_seq`
    /// → `conflict`.
    fn append(&self, actor: &Id, first_seq: u64, lines: &[Vec<u8>]) -> Result<()>;

    /// Writes merged lines, seqs `first_seq..`, as new segments, never extending an
    /// existing one. A segment already at a name the run needs → `integrity`.
    fn write_run(&self, actor: &Id, first_seq: u64, lines: &[Vec<u8>]) -> Result<()>;
}

/// The blob location: raw bytes addressed by their hash.
pub(crate) trait Blobs: Send + Sync {
    /// Readies the blob location of a new replica. It may already hold blobs: they are
    /// addressed by their hash, so replicas can share one.
    fn prepare(&self) -> Result<()>;

    /// Accepts a `b_` id with or without fragment; the fragment is ignored. Another
    /// kind of id → `invalid`.
    fn has(&self, id: &Id) -> Result<bool>;

    /// Size of a stored blob; absent → `not_found`.
    fn len(&self, id: &Id) -> Result<u64>;

    /// The bytes of a stored blob, checked against its id where they pass through a
    /// cache (`integrity` otherwise); absent → `not_found`.
    fn open(&self, id: &Id) -> Result<Box<dyn Read + Send>>;

    /// The bytes as the blob location holds them, never from a cache and unchecked: for
    /// `verify`, which hashes them itself; absent → `not_found`.
    fn open_raw(&self, id: &Id) -> Result<Box<dyn Read + Send>>;

    /// A local file holding the bytes of a stored blob, for readers that need a path;
    /// absent → `not_found`. Callers must not modify it.
    fn local_file(&self, id: &Id) -> Result<PathBuf>;

    /// Stores `r`'s bytes, hashed while they are staged (DESIGN §5.3 Blob path).
    /// Returns (id, created, size).
    fn put(&self, r: &mut dyn Read) -> Result<(Id, bool, u64)>;

    /// Stores the file at `src`, as `put`.
    fn put_file(&self, src: &Path) -> Result<(Id, bool, u64)>;

    /// Every stored blob with its name for messages, plus entries that do not belong.
    fn scan(&self) -> Result<Listing<(Id, String)>>;
}
