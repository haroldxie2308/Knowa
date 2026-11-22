//! Object-store locations (DESIGN §5.1–§5.3): S3 and S3-compatible stores, through
//! `object_store`, with the directory layout as keys under the URL's prefix.
//!
//! - There is no lock. A log batch is one new segment object, created only if absent,
//!   so of two writers racing for an actor's next seq exactly one succeeds and the
//!   other gets `conflict`. Segments are never modified, so no tail is ever torn.
//! - Blob bytes pass through this machine's blob cache (DESIGN §5.2): they are staged
//!   there while hashed before an upload, and downloaded there and checked against
//!   their id before they are read.
//! - Every API of the store is synchronous (DESIGN §8): the client runs on one private
//!   current-thread tokio runtime, and each call blocks on it.

#[cfg(test)]
mod tests;

use std::fs::{self, File};
use std::future::Future;
use std::io::{self, BufRead, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use bytes::{Buf, Bytes};
use futures::StreamExt;
use futures::stream::BoxStream;
use knowa_model::{Error, Id, IdKind, Result};
use object_store::ClientConfigKey;
use object_store::aws::{AmazonS3Builder, AmazonS3ConfigKey, S3ConditionalPut};
use object_store::path::Path as Key;
use object_store::{GetOptions, GetRange, ListResult, ObjectStore, ObjectStoreExt, PutMode, PutPayload};
use tokio::runtime::{Builder, Runtime};

use crate::backend::{Blobs, Guard, LastLine, Listing, Logs, Segment};
use crate::blob::{self, BlobHasher};
use crate::fsutil::io_err;
use crate::location::object_url;
use crate::log::{parse_segment_name, segment_rel_path};
use crate::{BLOB_DIR, GAPS_FILE, GENERATION_FILE, LOG_DIR, MANIFEST_FILE};

/// Blobs up to this size are uploaded in one PUT; larger ones in parts of at least
/// this size. 8 MiB is the AWS CLI's default threshold and part size: memory holds one
/// part at a time, a failed request resends at most one part, and S3's limit of 10 000
/// parts is reached only above 80 GiB, where parts grow instead (`part_size`).
pub(crate) const PART_SIZE: u64 = 8 * 1024 * 1024;

/// The default deadline of one request, body included, unless `AWS_TIMEOUT` sets one
/// (DESIGN §5.2). `object_store`'s own is 30 s: a GET cut by it resumes with a range
/// request, but a PUT starts over, so a part of `PART_SIZE` would need about 2.2 Mbit/s
/// of uplink. Five minutes lowers that to about 220 kbit/s and still ends a request
/// that hangs.
const REQUEST_TIMEOUT: &str = "300s";

/// S3's limit on the parts of one multipart upload.
const MAX_PARTS: u64 = 10_000;

/// How much of a segment's end `last_line` reads first; it reads 4× more until the
/// window holds a whole line.
const TAIL_WINDOW: u64 = 16 * 1024;

/// Runs `f` to completion on the backend's runtime: one current-thread runtime for the
/// process, built on first use (DESIGN §8). Several threads may block on it at once.
fn block_on<F: Future>(f: F) -> Result<F::Output> {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    let runtime = match RUNTIME.get() {
        Some(runtime) => runtime,
        None => {
            let built = Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| Error::internal(format!("starting the object-store runtime: {e}")))?;
            // Two threads may both build one; the one not kept is dropped unused.
            RUNTIME.get_or_init(|| built)
        }
    };
    Ok(runtime.block_on(f))
}

/// An S3 client for `bucket`, configured by `builder` (from the environment, outside
/// tests; DESIGN §5.2). Without an explicit region the client would assume us-east-1
/// and fail later, as something else: a missing one is a `usage` error.
fn s3_store(builder: AmazonS3Builder, bucket: &str) -> Result<Arc<dyn ObjectStore>> {
    if builder.get_config_value(&AmazonS3ConfigKey::Region).is_none() {
        return Err(Error::usage(format!(
            "s3://{bucket}: the object store has no region: set AWS_REGION or AWS_DEFAULT_REGION"
        )));
    }
    let store = builder
        .with_bucket_name(bucket)
        // Explicit, and after the environment so that AWS_CONDITIONAL_PUT cannot turn it
        // off: segment and manifest objects are created with `PutMode::Create`, which
        // `AmazonS3` sends as `If-None-Match: *` under `ETagMatch` and refuses to send at
        // all under `Disabled`.
        .with_conditional_put(S3ConditionalPut::ETagMatch)
        .build()
        .map_err(|e| Error::usage(format!("s3://{bucket}: the S3 settings in the environment are incomplete: {e}")))?;
    Ok(Arc::new(store))
}

/// `builder` with `REQUEST_TIMEOUT`, unless the environment set a timeout itself.
fn with_default_timeout(builder: AmazonS3Builder, set_by_env: bool) -> AmazonS3Builder {
    if set_by_env {
        return builder;
    }
    builder.with_config(AmazonS3ConfigKey::Client(ClientConfigKey::Timeout), REQUEST_TIMEOUT)
}

/// One object location: a store, and the URL and key prefix it was opened at.
#[derive(Clone)]
pub(crate) struct Remote {
    store: Arc<dyn ObjectStore>,
    /// the canonical URL, for messages
    url: String,
    bucket: String,
    prefix: Key,
    /// No key variables were set, so credentials came from the provider chain
    /// (DESIGN §5.2): failures name the variables to set.
    keyless: bool,
}

impl Remote {
    /// The S3 location `url`, with the client configured from the environment.
    pub(crate) fn s3(url: &str) -> Result<Remote> {
        let (bucket, _) = object_url(url)?;
        let builder = with_default_timeout(AmazonS3Builder::from_env(), std::env::var_os("AWS_TIMEOUT").is_some());
        let mut remote = Remote::new(s3_store(builder, &bucket)?, url)?;
        remote.keyless = std::env::var_os("AWS_ACCESS_KEY_ID").is_none();
        Ok(remote)
    }

    /// The location `url` on `store`.
    pub(crate) fn new(store: Arc<dyn ObjectStore>, url: &str) -> Result<Remote> {
        let (bucket, prefix) = object_url(url)?;
        Ok(Remote { store, url: url.trim_end_matches('/').to_string(), bucket, prefix, keyless: false })
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// The key of `rel`, a `/`-separated path relative to the location (DESIGN §5.1).
    fn key(&self, rel: &str) -> Key {
        rel.split('/').fold(self.prefix.clone(), |key, part| key.join(part))
    }

    /// `key` relative to the location, as `key` builds it; `None` outside the location.
    fn rel<'k>(&self, key: &'k Key) -> Option<&'k str> {
        match self.prefix.as_ref() {
            "" => Some(key.as_ref()),
            prefix => key.as_ref().strip_prefix(prefix)?.strip_prefix('/'),
        }
    }

    /// The URL of `key`, for messages.
    fn name(&self, key: &Key) -> String {
        format!("s3://{}/{key}", self.bucket)
    }

    /// Not found maps as on a directory; refused credentials are the caller's to fix
    /// (`usage`, DESIGN §5.2); anything else is `internal`, naming the key. A request
    /// that `object_store` reports as `Generic` is classified by its HTTP status.
    fn err(&self, key: &Key, e: object_store::Error) -> Error {
        use object_store::Error::{NotFound, PermissionDenied, Unauthenticated};
        let status = generic_status(&e);
        if matches!(e, NotFound { .. }) || status == Some(404) {
            return Error::not_found(format!("{} not found", self.name(key)));
        }
        if matches!(e, Unauthenticated { .. } | PermissionDenied { .. }) || matches!(status, Some(401 | 403)) {
            return Error::usage(format!(
                "{}: access refused ({e}); set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY to credentials allowed to use {}",
                self.name(key),
                self.url
            ));
        }
        // Without key variables a failure to reach S3 is often the provider chain
        // failing (no instance metadata on a laptop); the code stays `internal`, since a
        // network fault looks the same, but the message names the fix.
        let hint = if self.keyless {
            "; AWS_ACCESS_KEY_ID is not set, so credentials were sought from web identity, the container endpoint and instance metadata: outside AWS, set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY"
        } else {
            ""
        };
        Error::internal(format!("{}: {e}{hint}", self.name(key)))
    }

    /// The object at `key`, whole; `None` when absent.
    fn get(&self, key: &Key) -> Result<Option<Bytes>> {
        match block_on(async { self.store.get(key).await?.bytes().await })? {
            Ok(bytes) => Ok(Some(bytes)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(self.err(key, e)),
        }
    }

    /// The last `n` bytes of the object at `key` (all of it when shorter), with the
    /// offset they start at.
    fn tail(&self, key: &Key, n: u64) -> Result<(u64, Bytes)> {
        let options = GetOptions { range: Some(GetRange::Suffix(n)), ..GetOptions::default() };
        let got = block_on(async {
            let got = self.store.get_opts(key, options).await?;
            let start = got.range.start;
            Ok::<_, object_store::Error>((start, got.bytes().await?))
        })?;
        got.map_err(|e| self.err(key, e))
    }

    /// The body of the object at `key`, read as it arrives; absent → `not_found`.
    fn reader(&self, key: &Key) -> Result<Body> {
        let got = block_on(self.store.get(key))?.map_err(|e| self.err(key, e))?;
        Ok(Body { stream: got.into_stream(), chunk: Bytes::new(), name: self.name(key) })
    }

    /// Size of the object at `key` (a HEAD request); `None` when absent.
    fn size(&self, key: &Key) -> Result<Option<u64>> {
        match block_on(self.store.head(key))? {
            Ok(meta) => Ok(Some(meta.size)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(self.err(key, e)),
        }
    }

    /// Creates `key` holding `bytes` only if it is absent (`If-None-Match: *` on S3).
    /// `false` when an object is already there.
    fn create(&self, key: &Key, bytes: Vec<u8>) -> Result<bool> {
        match block_on(self.store.put_opts(key, PutPayload::from(bytes), PutMode::Create.into()))? {
            Ok(_) => Ok(true),
            Err(object_store::Error::AlreadyExists { .. }) => Ok(false),
            Err(e) => Err(self.err(key, e)),
        }
    }

    /// Writes `key`, replacing any object there. A reader sees the old object or the
    /// new one, never part of either.
    fn overwrite(&self, key: &Key, bytes: Vec<u8>) -> Result<()> {
        block_on(self.store.put(key, PutPayload::from(bytes)))?.map(|_| ()).map_err(|e| self.err(key, e))
    }

    /// Uploads the file at `path` to `key` in parts, hashing it as it goes; the object
    /// appears only if the file hashes to `id`. Any failure aborts the upload.
    fn upload_parts(&self, key: &Key, path: &Path, id: &Id) -> Result<()> {
        let mut file = File::open(path).map_err(|e| io_err(path, e))?;
        let size = file.metadata().map_err(|e| io_err(path, e))?.len();
        let mut upload = block_on(self.store.put_multipart(key))?.map_err(|e| self.err(key, e))?;
        let mut send = || -> Result<()> {
            let mut hasher = BlobHasher::new();
            loop {
                let mut part = Vec::new();
                file.by_ref().take(part_size(size)).read_to_end(&mut part).map_err(|e| io_err(path, e))?;
                if part.is_empty() {
                    break;
                }
                hasher.update(&part);
                block_on(upload.put_part(PutPayload::from(part)))?.map_err(|e| self.err(key, e))?;
            }
            let got = hasher.finish();
            if got != *id {
                return Err(Error::integrity(format!(
                    "{}: changed while it was uploaded: it now hashes to {got}, not to its id {id}",
                    path.display()
                )));
            }
            Ok(())
        };
        match send() {
            Ok(()) => block_on(upload.complete())?.map(|_| ()).map_err(|e| self.err(key, e)),
            Err(mut e) => {
                let aborted = block_on(upload.abort()).and_then(|r| r.map_err(|a| self.err(key, a)));
                if let Err(a) = aborted {
                    e.message = format!(
                        "{}; aborting the multipart upload also failed, so its parts stay in the bucket until removed: {}",
                        e.message, a.message
                    );
                }
                Err(e)
            }
        }
    }

    /// The objects and sub-prefixes directly under `rel`.
    fn list_dir(&self, rel: &str) -> Result<ListResult> {
        let key = self.key(rel);
        block_on(self.store.list_with_delimiter(Some(&key)))?.map_err(|e| self.err(&key, e))
    }

    /// Every object under `prefix`, at any depth.
    fn list_all(&self, prefix: &Key) -> Result<Vec<Key>> {
        let listed = block_on(self.store.list(Some(prefix)).collect::<Vec<_>>())?;
        listed.into_iter().map(|meta| meta.map(|m| m.location).map_err(|e| self.err(prefix, e))).collect()
    }
}

/// What `object_store` puts before the status of a request the server refused.
const STATUS_PREFIX: &str = "Server returned non-2xx status code: ";

/// The HTTP status of a refused S3 request that `object_store` reports as `Generic`.
/// It maps the status of an object request (GET, HEAD, PUT) to an error variant, but
/// wraps a refused LIST, and other bucket-level requests, as `Generic`, and the types
/// that hold the status are private to it. The status survives only in a message of
/// the error's source chain, `Server returned non-2xx status code: 403 Forbidden: …`,
/// which `a_refused_list_is_classified_by_its_status` checks against the real client.
fn generic_status(e: &object_store::Error) -> Option<u16> {
    let object_store::Error::Generic { source, .. } = e else {
        return None;
    };
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&**source);
    while let Some(c) = cause {
        if let Some(rest) = c.to_string().strip_prefix(STATUS_PREFIX) {
            return rest.get(..3)?.parse().ok();
        }
        cause = c.source();
    }
    None
}

/// The part size of a multipart upload of `size` bytes: `PART_SIZE`, or larger when
/// that many parts would exceed `MAX_PARTS`.
fn part_size(size: u64) -> u64 {
    PART_SIZE.max(size.div_ceil(MAX_PARTS))
}

/// An object's body as a blocking reader: each refill waits for the next chunk.
struct Body {
    stream: BoxStream<'static, object_store::Result<Bytes>>,
    chunk: Bytes,
    name: String,
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = {
            let available = self.fill_buf()?;
            let n = available.len().min(buf.len());
            buf[..n].copy_from_slice(&available[..n]);
            n
        };
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for Body {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        while self.chunk.is_empty() {
            match block_on(self.stream.next()).map_err(|e| io::Error::other(e.message))? {
                Some(Ok(chunk)) => self.chunk = chunk,
                Some(Err(e)) => return Err(io::Error::other(format!("{}: {e}", self.name))),
                None => break,
            }
        }
        Ok(&self.chunk)
    }

    fn consume(&mut self, amt: usize) {
        self.chunk.advance(amt);
    }
}

/// The manifest, generations and claim logs at an object location.
pub(crate) struct ObjectLogs {
    remote: Remote,
}

impl ObjectLogs {
    pub(crate) fn new(remote: Remote) -> ObjectLogs {
        ObjectLogs { remote }
    }

    fn segment_key(&self, actor: &Id, first: u64) -> Key {
        self.remote.key(&segment_rel_path(actor, first))
    }
}

impl Logs for ObjectLogs {
    fn describe(&self) -> String {
        self.remote.url.clone()
    }

    fn exclusive_locks(&self) -> bool {
        false
    }

    /// The location must hold no object at all.
    fn prepare(&self) -> Result<()> {
        let prefix = &self.remote.prefix;
        match block_on(async { self.remote.store.list(Some(prefix)).next().await })? {
            None => Ok(()),
            Some(Ok(meta)) => Err(Error::conflict(format!(
                "{} is not empty: it holds {}",
                self.remote.url,
                self.remote.name(&meta.location)
            ))),
            Some(Err(e)) => Err(self.remote.err(prefix, e)),
        }
    }

    fn read_manifest(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.remote.get(&self.remote.key(MANIFEST_FILE))?.map(|b| b.to_vec()))
    }

    fn write_manifest(&self, bytes: &[u8]) -> Result<()> {
        let key = self.remote.key(MANIFEST_FILE);
        if !self.remote.create(&key, bytes.to_vec())? {
            return Err(Error::conflict(format!("{} already exists", self.remote.name(&key))));
        }
        Ok(())
    }

    fn read_generation(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.remote.get(&self.remote.key(GENERATION_FILE))?.map(|b| b.to_vec()))
    }

    fn write_generation(&self, bytes: &[u8]) -> Result<()> {
        self.remote.overwrite(&self.remote.key(GENERATION_FILE), bytes.to_vec())
    }

    fn read_gaps(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.remote.get(&self.remote.key(GAPS_FILE))?.map(|b| b.to_vec()))
    }

    fn write_gaps(&self, bytes: &[u8]) -> Result<()> {
        self.remote.overwrite(&self.remote.key(GAPS_FILE), bytes.to_vec())
    }

    /// No lock: segment objects are created only if absent instead (DESIGN §5.3).
    fn lock_exclusive(&self, _actor: &Id) -> Result<Guard> {
        Ok(Box::new(()))
    }

    /// Nothing to wait for: a segment object is published whole.
    fn lock_shared(&self, _actor: &Id) -> Result<Option<Guard>> {
        Ok(Some(Box::new(())))
    }

    /// Segment objects are written whole: there is no torn tail.
    fn repair_tail(&self, _actor: &Id) -> Result<()> {
        Ok(())
    }

    fn actors(&self) -> Result<Listing<Id>> {
        let listed = self.remote.list_dir(LOG_DIR)?;
        let mut problems: Vec<String> =
            listed.objects.iter().map(|o| format!("{}: not an actor log", self.remote.name(&o.location))).collect();
        let mut actors = Vec::new();
        for prefix in &listed.common_prefixes {
            match prefix.filename().map(Id::parse) {
                Some(Ok(id)) if id.kind() == IdKind::Actor => actors.push(id),
                _ => problems.push(format!("{}: not an actor log", self.remote.name(prefix))),
            }
        }
        actors.sort();
        Ok((actors, problems))
    }

    fn segments(&self, actor: &Id) -> Result<Listing<Segment>> {
        let listed = self.remote.list_dir(&format!("{LOG_DIR}/{actor}"))?;
        let mut problems: Vec<String> = listed
            .common_prefixes
            .iter()
            .map(|p| format!("{}: not a segment object", self.remote.name(p)))
            .collect();
        let mut segments = Vec::new();
        for object in &listed.objects {
            let name = self.remote.name(&object.location);
            match object.location.filename().and_then(parse_segment_name) {
                Some(first) => segments.push(Segment { first, name }),
                None => problems.push(format!("{name}: not a segment object (<12-digit first seq>.jsonl)")),
            }
        }
        segments.sort_by_key(|s| s.first);
        Ok((segments, problems))
    }

    fn open_segment(&self, actor: &Id, segment: &Segment) -> Result<Box<dyn BufRead + Send>> {
        Ok(Box::new(self.remote.reader(&self.segment_key(actor, segment.first))?))
    }

    /// Reads the segment's end only, widening the window until it holds a whole line.
    fn last_line(&self, actor: &Id, segment: &Segment) -> Result<LastLine> {
        let key = self.segment_key(actor, segment.first);
        let mut window = TAIL_WINDOW;
        loop {
            let (start, bytes) = self.remote.tail(&key, window)?;
            let whole = start == 0;
            match bytes.iter().rposition(|&b| b == b'\n') {
                None if whole => return Ok(LastLine { line: None, trailing: bytes.len() as u64 }),
                None => {}
                Some(nl) => {
                    let trailing = (bytes.len() - nl - 1) as u64;
                    match bytes[..nl].iter().rposition(|&b| b == b'\n') {
                        Some(prev) => return Ok(LastLine { line: Some(bytes[prev + 1..=nl].to_vec()), trailing }),
                        None if whole => return Ok(LastLine { line: Some(bytes[..=nl].to_vec()), trailing }),
                        None => {}
                    }
                }
            }
            window = window.saturating_mul(4);
        }
    }

    /// One new segment object holding the whole batch, created only if absent: a
    /// writer that raced this one for `first_seq` and won has created it already.
    fn append(&self, actor: &Id, first_seq: u64, lines: &[Vec<u8>]) -> Result<()> {
        let key = self.segment_key(actor, first_seq);
        if !self.remote.create(&key, lines.concat())? {
            return Err(Error::conflict(format!(
                "{}: another writer of {actor} appended seq {first_seq} first",
                self.remote.name(&key)
            )));
        }
        Ok(())
    }

    /// The run as one new segment object; nothing rotates on an object store.
    fn write_run(&self, actor: &Id, first_seq: u64, lines: &[Vec<u8>]) -> Result<()> {
        let key = self.segment_key(actor, first_seq);
        if !self.remote.create(&key, lines.concat())? {
            return Err(Error::integrity(format!(
                "{}: segment already exists but does not contain seq {first_seq}",
                self.remote.name(&key)
            )));
        }
        Ok(())
    }
}

/// Blobs at an object location, through this machine's blob cache (DESIGN §5.2, §5.3).
pub(crate) struct ObjectBlobs {
    remote: Remote,
    /// the blob cache, `$KNOWA_HOME/cache/blobs/`: a blob directory
    cache: PathBuf,
}

impl ObjectBlobs {
    pub(crate) fn new(remote: Remote, cache: &Path) -> ObjectBlobs {
        ObjectBlobs { remote, cache: cache.to_path_buf() }
    }

    fn blob_key(&self, id: &Id) -> Result<Key> {
        Ok(self.remote.key(&blob::rel_path(id)?))
    }

    /// Uploads a blob just staged in the cache, unless the store has it already.
    /// Two machines uploading one blob at once write identical objects, so a plain PUT
    /// is enough (DESIGN §5.3).
    fn upload(&self, id: Id, size: u64) -> Result<(Id, bool, u64)> {
        let key = self.blob_key(&id)?;
        if self.remote.size(&key)?.is_some() {
            return Ok((id, false, size));
        }
        let staged = blob::path(&self.cache, &id)?;
        if size > PART_SIZE {
            self.remote.upload_parts(&key, &staged, &id)?;
            return Ok((id, true, size));
        }
        let bytes = fs::read(&staged).map_err(|e| io_err(&staged, e))?;
        let mut hasher = BlobHasher::new();
        hasher.update(&bytes);
        let got = hasher.finish();
        if got != id {
            return Err(Error::integrity(format!(
                "{}: changed after it was staged: it now hashes to {got}, not to its id {id}",
                staged.display()
            )));
        }
        self.remote.overwrite(&key, bytes)?;
        Ok((id, true, size))
    }
}

impl Blobs for ObjectBlobs {
    /// Nothing to ready: keys need no directories, and the cache is made on first use.
    fn prepare(&self) -> Result<()> {
        Ok(())
    }

    fn has(&self, id: &Id) -> Result<bool> {
        Ok(self.remote.size(&self.blob_key(id)?)?.is_some())
    }

    fn len(&self, id: &Id) -> Result<u64> {
        self.remote
            .size(&self.blob_key(id)?)?
            .ok_or_else(|| Error::not_found(format!("blob {} not found in {}", id.base(), self.remote.url)))
    }

    fn open(&self, id: &Id) -> Result<Box<dyn Read + Send>> {
        let path = self.local_file(id)?;
        Ok(Box::new(File::open(&path).map_err(|e| io_err(&path, e))?))
    }

    fn open_raw(&self, id: &Id) -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(self.remote.reader(&self.blob_key(id)?)?))
    }

    /// The cached copy, downloaded on a miss: into a temp file in the cache, hashed,
    /// and renamed to its id only if it hashes to that id (`integrity` otherwise).
    fn local_file(&self, id: &Id) -> Result<PathBuf> {
        let cached = blob::path(&self.cache, id)?;
        if cached.is_file() {
            return Ok(cached);
        }
        let key = self.blob_key(id)?;
        let mut body = self.remote.reader(&key)?;
        blob::put_expected(&self.cache, &mut body, id, &|| self.remote.name(&key))
    }

    fn put(&self, r: &mut dyn Read) -> Result<(Id, bool, u64)> {
        let (id, _, size) = blob::put(&self.cache, r)?;
        self.upload(id, size)
    }

    fn put_file(&self, src: &Path) -> Result<(Id, bool, u64)> {
        let (id, _, size) = blob::put_file(&self.cache, src)?;
        self.upload(id, size)
    }

    fn scan(&self) -> Result<Listing<(Id, String)>> {
        let (mut blobs, mut problems) = (Vec::new(), Vec::new());
        for key in self.remote.list_all(&self.remote.key(BLOB_DIR))? {
            match self.remote.rel(&key).and_then(blob::parse_rel_path) {
                Some(id) => blobs.push((id, self.remote.name(&key))),
                None => problems.push(format!("{}: not a blob object", self.remote.name(&key))),
            }
        }
        blobs.sort();
        Ok((blobs, problems))
    }
}
