//! Blob access the index needs (presence, listing, text for FTS), behind a trait so
//! the indexing logic does not depend on a replica.

use knowa_model::{Id, Result};
use knowa_store::Replica;

use crate::MAX_FTS_BLOB_BYTES;

pub(crate) trait BlobSource {
    /// Fragment ignored.
    fn has_blob(&self, id: &Id) -> Result<bool>;
    fn blob_ids(&self) -> Result<Vec<Id>>;
    /// Size in bytes of a blob that `has_blob`.
    fn blob_len(&self, id: &Id) -> Result<u64>;
    /// Bytes of a blob that `has_blob`.
    fn read_blob(&self, id: &Id) -> Result<Vec<u8>>;
}

impl BlobSource for Replica {
    fn has_blob(&self, id: &Id) -> Result<bool> {
        Replica::has_blob(self, id)
    }
    fn blob_ids(&self) -> Result<Vec<Id>> {
        Replica::blob_ids(self)
    }
    fn blob_len(&self, id: &Id) -> Result<u64> {
        Replica::blob_len(self, id)
    }
    fn read_blob(&self, id: &Id) -> Result<Vec<u8>> {
        Replica::read_blob(self, id)
    }
}

/// Text of a blob for full-text indexing (DESIGN §7.2): `None` when the blob is
/// larger than `MAX_FTS_BLOB_BYTES` or absent.
pub(crate) fn fts_text(src: &dyn BlobSource, id: &Id) -> Result<Option<String>> {
    let id = id.base();
    if !src.has_blob(&id)? {
        // Fallback: a partial replica (imported from a shared/public bundle) holds
        // claims about blobs whose bytes were not exported. The text is derived data,
        // so the entity is indexed from its claims alone; nothing canonical is lost.
        return Ok(None);
    }
    if src.blob_len(&id)? > MAX_FTS_BLOB_BYTES {
        return Ok(None);
    }
    let bytes = src.read_blob(&id)?;
    Ok(Some(match String::from_utf8(bytes) {
        Ok(s) => s,
        // Fallback: `text/*` does not imply UTF-8 (e.g. Latin-1 HTML). Replacement
        // characters only affect the derived search document, never the blob.
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }))
}
