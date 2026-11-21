//! Directory replicas (DESIGN §5.1, §5.3): the location is a directory, local or a
//! mounted shared filesystem, and the blobs live in it too. A batch extends the actor's
//! newest segment in place under an advisory lock on `log/<actor>/.lock`; every new
//! file is published by temp + fsync + rename.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use knowa_model::{Error, Id, IdKind, Result};

use crate::backend::{Blobs, Guard, LastLine, Listing, Logs, Segment};
use crate::fsutil::{self, TempFile, io_err};
use crate::log::{SEGMENT_LIMIT, parse_segment_name, segment_name};
use crate::{BLOB_DIR, GENERATION_FILE, LOCK_FILE, LOG_DIR, MANIFEST_FILE, blob};

/// Creates `dir` (or accepts it empty) with `log/` and `blobs/`.
pub(crate) fn prepare(dir: &Path) -> Result<()> {
    match fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(Error::conflict(format!("{} exists and is not empty", dir.display())));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) if dir.exists() && !dir.is_dir() => {
            return Err(Error::conflict(format!("{} exists and is not a directory ({e})", dir.display())));
        }
        Err(e) => return Err(io_err(dir, e)),
    }
    fsutil::ensure_dir(&dir.join(LOG_DIR))?;
    fsutil::ensure_dir(&dir.join(BLOB_DIR))
}

/// Reads `path`; `None` when it does not exist.
fn read_if_exists(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(path, e)),
    }
}

pub(crate) struct DirLogs {
    root: PathBuf,
}

impl DirLogs {
    pub(crate) fn new(root: &Path) -> DirLogs {
        DirLogs { root: root.to_path_buf() }
    }

    fn actor_dir(&self, actor: &Id) -> PathBuf {
        self.root.join(LOG_DIR).join(actor.as_str())
    }

    fn segment_path(&self, actor: &Id, first: u64) -> PathBuf {
        self.actor_dir(actor).join(segment_name(first))
    }

    fn lock_file(dir: &Path) -> Result<File> {
        let path = dir.join(LOCK_FILE);
        OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path).map_err(|e| io_err(&path, e))
    }
}

impl Logs for DirLogs {
    fn describe(&self) -> String {
        self.root.display().to_string()
    }

    fn canonical_location(&self) -> Result<String> {
        let abs = fs::canonicalize(&self.root).map_err(|e| io_err(&self.root, e))?;
        abs.into_os_string().into_string().map_err(|p| {
            Error::unsupported(format!("replica location {} is not valid UTF-8, so it has no index key", p.display()))
        })
    }

    fn read_manifest(&self) -> Result<Option<Vec<u8>>> {
        read_if_exists(&self.root.join(MANIFEST_FILE))
    }

    fn write_manifest(&self, bytes: &[u8]) -> Result<()> {
        fsutil::atomic_write(&self.root.join(MANIFEST_FILE), bytes)
    }

    fn read_generation(&self) -> Result<Option<Vec<u8>>> {
        read_if_exists(&self.root.join(GENERATION_FILE))
    }

    fn write_generation(&self, bytes: &[u8]) -> Result<()> {
        fsutil::atomic_write(&self.root.join(GENERATION_FILE), bytes)
    }

    fn lock_exclusive(&self, actor: &Id) -> Result<Guard> {
        let dir = self.actor_dir(actor);
        fsutil::ensure_dir(&dir)?;
        let file = DirLogs::lock_file(&dir)?;
        file.lock().map_err(|e| io_err(&dir.join(LOCK_FILE), e))?;
        Ok(Box::new(file))
    }

    fn lock_shared(&self, actor: &Id) -> Result<Option<Guard>> {
        let dir = self.actor_dir(actor);
        match fs::metadata(&dir) {
            Ok(_) => {}
            // No log yet: nothing to read. A writer creating it right after this check
            // publishes whole segments, which a later read takes the lock for.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(&dir, e)),
        }
        let file = DirLogs::lock_file(&dir)?;
        file.lock_shared().map_err(|e| io_err(&dir.join(LOCK_FILE), e))?;
        Ok(Some(Box::new(file)))
    }

    fn repair_tail(&self, actor: &Id) -> Result<()> {
        let (segments, _) = scan_segments(&self.actor_dir(actor))?;
        match segments.last() {
            Some(newest) => truncate_torn_tail(&self.segment_path(actor, newest.first)),
            None => Ok(()),
        }
    }

    fn actors(&self) -> Result<Listing<Id>> {
        scan_actors(&self.root)
    }

    fn segments(&self, actor: &Id) -> Result<Listing<Segment>> {
        scan_segments(&self.actor_dir(actor))
    }

    fn open_segment(&self, actor: &Id, segment: &Segment) -> Result<Box<dyn BufRead + Send>> {
        let path = self.segment_path(actor, segment.first);
        let file = File::open(&path).map_err(|e| io_err(&path, e))?;
        Ok(Box::new(BufReader::new(file)))
    }

    fn last_line(&self, actor: &Id, segment: &Segment) -> Result<LastLine> {
        read_last_line(&self.segment_path(actor, segment.first))
    }

    fn append(&self, actor: &Id, first_seq: u64, lines: &[Vec<u8>]) -> Result<()> {
        let dir = self.actor_dir(actor);
        let (segments, _) = scan_segments(&dir)?;
        // The caller read the head under the actor's lock: it is the newest segment's
        // last line, so that segment holds `first_seq - newest.first` lines.
        let tail = match segments.last() {
            Some(newest) if newest.first >= first_seq => {
                return Err(Error::conflict(format!(
                    "{}: the log of {actor} already holds seq {first_seq}",
                    self.segment_path(actor, newest.first).display()
                )));
            }
            Some(newest) => Some((self.segment_path(actor, newest.first), first_seq - newest.first)),
            None => None,
        };
        // A rotation means several files: a failure after the first has been written
        // is undone, so the append still leaves nothing on error.
        let mut changes = Vec::new();
        match write_run(&dir, tail.as_ref().map(|(p, n)| (p.as_path(), *n)), first_seq, lines, SEGMENT_LIMIT, &mut changes) {
            Ok(()) => Ok(()),
            Err(e) => Err(undo(changes, e)),
        }
    }

    fn write_run(&self, actor: &Id, first_seq: u64, lines: &[Vec<u8>]) -> Result<()> {
        // A merge that fails part-way keeps the runs already written: each is a
        // verified, chain-checked run, and the rebuild generation written before it
        // makes every index rebuild over it (DESIGN §5.4).
        write_run(&self.actor_dir(actor), None, first_seq, lines, SEGMENT_LIMIT, &mut Vec::new())
    }
}

/// Actor directories under `log/`, plus anything there that does not belong.
fn scan_actors(root: &Path) -> Result<Listing<Id>> {
    let dir = root.join(LOG_DIR);
    let (mut actors, mut problems) = (Vec::new(), Vec::new());
    // `log/` absent: tools such as git drop empty directories, and no directory
    // means no claims — the same thing an empty one means.
    let Some(entries) = fsutil::read_dir_if_exists(&dir)? else {
        return Ok((actors, problems));
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_err(&dir, e))?;
        let name = match fsutil::entry_name(&entry) {
            Ok(n) => n,
            Err(p) => {
                problems.push(p);
                continue;
            }
        };
        if fsutil::is_hidden(&name) {
            continue;
        }
        match Id::parse(&name) {
            Ok(id) if id.kind() == IdKind::Actor && entry.path().is_dir() => actors.push(id),
            _ => problems.push(format!("{}: not an actor log directory", entry.path().display())),
        }
    }
    actors.sort();
    Ok((actors, problems))
}

/// Segments in an actor directory, ascending by first seq, plus misnamed entries.
/// Hidden entries (the lock file, in-flight temp files) are skipped.
fn scan_segments(dir: &Path) -> Result<Listing<Segment>> {
    let (mut segments, mut problems) = (Vec::new(), Vec::new());
    let Some(entries) = fsutil::read_dir_if_exists(dir)? else {
        return Ok((segments, problems));
    };
    for entry in entries {
        let entry = entry.map_err(|e| io_err(dir, e))?;
        let name = match fsutil::entry_name(&entry) {
            Ok(n) => n,
            Err(p) => {
                problems.push(p);
                continue;
            }
        };
        if fsutil::is_hidden(&name) {
            continue;
        }
        match parse_segment_name(&name) {
            Some(first) => segments.push(Segment { first, name: entry.path().display().to_string() }),
            None => problems.push(format!("{}: not a segment file (<12-digit first seq>.jsonl)", entry.path().display())),
        }
    }
    segments.sort_by_key(|s| s.first);
    Ok((segments, problems))
}

fn read_last_line(path: &Path) -> Result<LastLine> {
    let mut f = File::open(path).map_err(|e| io_err(path, e))?;
    let len = f.metadata().map_err(|e| io_err(path, e))?.len();
    let Some(nl) = last_newline_before(&mut f, len, path)? else {
        return Ok(LastLine { line: None, trailing: len });
    };
    let start = last_newline_before(&mut f, nl, path)?.map_or(0, |prev| prev + 1);
    let mut line = vec![0u8; (nl + 1 - start) as usize];
    f.seek(SeekFrom::Start(start)).map_err(|e| io_err(path, e))?;
    f.read_exact(&mut line).map_err(|e| io_err(path, e))?;
    Ok(LastLine { line: Some(line), trailing: len - (nl + 1) })
}

fn read_byte(f: &mut File, at: u64, path: &Path) -> Result<u8> {
    let mut b = [0u8; 1];
    f.seek(SeekFrom::Start(at)).map_err(|e| io_err(path, e))?;
    f.read_exact(&mut b).map_err(|e| io_err(path, e))?;
    Ok(b[0])
}

/// Offset of the last `\n` among the first `end` bytes of `f`, scanning backwards.
fn last_newline_before(f: &mut File, end: u64, path: &Path) -> Result<Option<u64>> {
    const CHUNK: u64 = 8 * 1024;
    let mut hi = end;
    while hi > 0 {
        let lo = hi.saturating_sub(CHUNK);
        let mut chunk = vec![0u8; (hi - lo) as usize];
        f.seek(SeekFrom::Start(lo)).map_err(|e| io_err(path, e))?;
        f.read_exact(&mut chunk).map_err(|e| io_err(path, e))?;
        if let Some(i) = chunk.iter().rposition(|&b| b == b'\n') {
            return Ok(Some(lo + i as u64));
        }
        hi = lo;
    }
    Ok(None)
}

/// Torn tail recovery (DESIGN §5.3). The caller holds the actor's exclusive lock.
///
/// Why truncating is safe: `append` is the only writer that extends an existing
/// segment, it always extends the actor's newest one, and it writes whole lines and
/// fsyncs them before returning, all under the actor's exclusive lock. With that lock
/// held here no append is in flight, so an unterminated final line in the newest
/// segment is what is left of an append that never returned success: no caller was
/// given its id, the index never applied it (readers stop before it, and a writer
/// applies only after append returns), and no bundle holds it. Removing it loses
/// nothing that was acknowledged.
///
/// Everything else is left for readers and `verify` to report rather than repaired:
/// stray or misnamed entries (the scans' problem lists), and segments with no complete
/// line, which no crash can produce because new segments are published whole by rename.
fn truncate_torn_tail(path: &Path) -> Result<()> {
    let mut f = OpenOptions::new().read(true).write(true).open(path).map_err(|e| io_err(path, e))?;
    let len = f.metadata().map_err(|e| io_err(path, e))?.len();
    if len == 0 || read_byte(&mut f, len - 1, path)? == b'\n' {
        return Ok(());
    }
    let Some(last_complete) = last_newline_before(&mut f, len, path)? else {
        return Ok(());
    };
    f.set_len(last_complete + 1).map_err(|e| io_err(path, e))?;
    f.sync_all().map_err(|e| io_err(path, e))
}

/// One change `write_run` makes to the log, recorded *before* it is attempted, so a
/// write that fails part-way is still covered.
enum Change {
    /// lines are appended to `path`, which was `len` bytes long
    Extended { path: PathBuf, len: u64 },
    /// a new segment is published at `path`
    Created(PathBuf),
}

/// Reverts `changes` (newest first) after `err` and returns `err`, extended with any
/// step of the undo that failed. The caller holds the actor's exclusive lock, so
/// nothing else has touched these files since. Each step is independent, so one
/// failing does not stop the others.
fn undo(changes: Vec<Change>, mut err: Error) -> Error {
    let failed: Vec<String> = changes.into_iter().rev().filter_map(|c| revert(c).err()).map(|e| e.message).collect();
    if !failed.is_empty() {
        err.message = format!(
            "{}; undoing the partial append also failed, so the log may keep part of it: {}",
            err.message,
            failed.join("; ")
        );
    }
    err
}

fn revert(change: Change) -> Result<()> {
    match change {
        Change::Extended { path, len } => {
            let f = OpenOptions::new().write(true).open(&path).map_err(|e| io_err(&path, e))?;
            f.set_len(len).map_err(|e| io_err(&path, e))?;
            f.sync_all().map_err(|e| io_err(&path, e))
        }
        Change::Created(path) => match fs::remove_file(&path) {
            Ok(()) => sync_parent(&path),
            // The publish failed before its rename: there is nothing to remove.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(&path, e)),
        },
    }
}

fn sync_parent(path: &Path) -> Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => fsutil::sync_dir(parent),
        _ => fsutil::sync_dir(Path::new(".")),
    }
}

/// Writes `lines` (consecutive seqs from `first_seq`): first into the open `tail`
/// segment (its path and line count) while it has room under `limit`, then into new
/// segments of ≤ `limit` lines, each named by its first seq. Every change is recorded
/// in `changes`.
fn write_run(
    dir: &Path,
    tail: Option<(&Path, u64)>,
    first_seq: u64,
    lines: &[Vec<u8>],
    limit: usize,
    changes: &mut Vec<Change>,
) -> Result<()> {
    let mut rest = lines;
    let mut seq = first_seq;
    if let Some((path, count)) = tail {
        // ≤ rest.len(), so the cast back to usize is lossless.
        let room = (limit as u64).saturating_sub(count).min(rest.len() as u64) as usize;
        if room > 0 {
            append_to_segment(path, &rest[..room], changes)?;
            rest = &rest[room..];
            seq += room as u64;
        }
    }
    for part in rest.chunks(limit.max(1)) {
        write_new_segment(dir, seq, part, changes)?;
        seq += part.len() as u64;
    }
    Ok(())
}

fn append_to_segment(path: &Path, lines: &[Vec<u8>], changes: &mut Vec<Change>) -> Result<()> {
    let mut f = OpenOptions::new().append(true).open(path).map_err(|e| io_err(path, e))?;
    let len = f.metadata().map_err(|e| io_err(path, e))?.len();
    changes.push(Change::Extended { path: path.to_path_buf(), len });
    f.write_all(&lines.concat()).map_err(|e| io_err(path, e))?;
    f.sync_all().map_err(|e| io_err(path, e))
}

fn write_new_segment(dir: &Path, first: u64, lines: &[Vec<u8>], changes: &mut Vec<Change>) -> Result<()> {
    // Taking the actor's lock created the directory; it is not the log's to remove on undo.
    fsutil::ensure_dir(dir)?;
    let dest = dir.join(segment_name(first));
    // Callers hold the actor's exclusive lock, so no other Knowa writer can race this check.
    if dest.try_exists().map_err(|e| io_err(&dest, e))? {
        return Err(Error::integrity(format!(
            "{}: segment already exists but does not contain seq {first}",
            dest.display()
        )));
    }
    let mut tmp = TempFile::create_in(dir)?;
    let tmp_path = tmp.path().to_path_buf();
    tmp.file().write_all(&lines.concat()).map_err(|e| io_err(&tmp_path, e))?;
    changes.push(Change::Created(dest.clone()));
    tmp.commit(&dest)
}

/// Blobs in `<root>/blobs/` (DESIGN §5.1).
pub(crate) struct DirBlobs {
    root: PathBuf,
}

impl DirBlobs {
    pub(crate) fn new(root: &Path) -> DirBlobs {
        DirBlobs { root: root.to_path_buf() }
    }

    /// The stored file of a present blob; absent → `not_found`.
    fn stored(&self, id: &Id) -> Result<PathBuf> {
        let path = blob::path(&self.root, id)?;
        if !path.is_file() {
            return Err(Error::not_found(format!("blob {} not found in {}", id.base(), self.root.display())));
        }
        Ok(path)
    }
}

impl Blobs for DirBlobs {
    fn has(&self, id: &Id) -> Result<bool> {
        Ok(blob::path(&self.root, id)?.is_file())
    }

    fn len(&self, id: &Id) -> Result<u64> {
        let path = self.stored(id)?;
        Ok(fs::metadata(&path).map_err(|e| io_err(&path, e))?.len())
    }

    fn open(&self, id: &Id) -> Result<Box<dyn Read + Send>> {
        let path = self.stored(id)?;
        Ok(Box::new(File::open(&path).map_err(|e| io_err(&path, e))?))
    }

    fn local_file(&self, id: &Id) -> Result<PathBuf> {
        self.stored(id)
    }

    fn put(&self, r: &mut dyn Read) -> Result<(Id, bool, u64)> {
        blob::put(&self.root, r)
    }

    fn put_file(&self, src: &Path) -> Result<(Id, bool, u64)> {
        blob::put_file(&self.root, src)
    }

    fn scan(&self) -> Result<Listing<(Id, String)>> {
        let (blobs, problems) = blob::scan(&self.root)?;
        Ok((blobs.into_iter().map(|(id, path)| (id, path.display().to_string())).collect(), problems))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(range: std::ops::RangeInclusive<u64>) -> Vec<Vec<u8>> {
        range.map(|n| format!("{n}\n").into_bytes()).collect()
    }

    fn layout(dir: &Path) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (e.file_name().into_string().unwrap(), fs::read_to_string(e.path()).unwrap_or_default())
            })
            .collect();
        out.sort();
        out
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("knowa-store-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn write_run_fills_the_tail_then_rotates() {
        let dir = scratch("rotate");
        write_run(&dir, None, 1, &lines(1..=7), 3, &mut Vec::new()).unwrap();
        assert_eq!(
            layout(&dir),
            [
                (segment_name(1), "1\n2\n3\n".to_string()),
                (segment_name(4), "4\n5\n6\n".to_string()),
                (segment_name(7), "7\n".to_string()),
            ]
        );
        let tail = dir.join(segment_name(7));
        write_run(&dir, Some((&tail, 1)), 8, &lines(8..=11), 3, &mut Vec::new()).unwrap();
        let after = layout(&dir);
        assert_eq!(after.len(), 4);
        assert_eq!(after[2], (segment_name(7), "7\n8\n9\n".to_string()));
        assert_eq!(after[3], (segment_name(10), "10\n11\n".to_string()));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// One actor's batch spans files when it rotates; a failure after the tail was
    /// extended must take the extension back, so a failed append leaves nothing.
    #[test]
    fn a_rotating_write_that_fails_part_way_is_undone() {
        let dir = scratch("undo");
        write_run(&dir, None, 1, &lines(1..=2), 3, &mut Vec::new()).unwrap();
        let tail = dir.join(segment_name(1));
        // Something already occupies the name of the segment the rotation needs.
        fs::create_dir(dir.join(segment_name(4))).unwrap();
        let mut changes = Vec::new();
        let err = write_run(&dir, Some((&tail, 2)), 3, &lines(3..=5), 3, &mut changes).unwrap_err();
        let err = undo(changes, err);
        assert_eq!(err.code, knowa_model::ErrorCode::Integrity, "{err:?}");
        assert_eq!(fs::read_to_string(&tail).unwrap(), "1\n2\n");
        fs::remove_dir_all(&dir).unwrap();
    }
}
