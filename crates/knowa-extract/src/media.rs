//! media@1: media type, size and coarse type of a blob.

use std::path::Path;

use knowa_model::value::MAX_SAFE_INT;
use knowa_model::{Draft, Result, Term, Value, vocab};

use crate::{Input, Outcome};

/// Extension → media type, consulted only when the content has no recognizable
/// magic bytes. IANA-registered types for common files; the content sniffer
/// already covers most binary formats, so this mainly names text formats.
const BY_EXTENSION: &[(&str, &str)] = &[
    ("avif", "image/avif"),
    ("bmp", "image/bmp"),
    ("csv", "text/csv"),
    ("gif", "image/gif"),
    ("heic", "image/heic"),
    ("heif", "image/heif"),
    ("htm", "text/html"),
    ("html", "text/html"),
    ("jpeg", "image/jpeg"),
    ("jpg", "image/jpeg"),
    ("json", "application/json"),
    ("markdown", "text/markdown"),
    ("md", "text/markdown"),
    ("mov", "video/quicktime"),
    ("mp3", "audio/mpeg"),
    ("mp4", "video/mp4"),
    ("pdf", "application/pdf"),
    ("png", "image/png"),
    ("svg", "image/svg+xml"),
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
    ("txt", "text/plain"),
    ("wav", "audio/wav"),
    ("webp", "image/webp"),
    ("xml", "application/xml"),
    ("zip", "application/zip"),
];

fn by_extension(name: &str) -> Option<&'static str> {
    let ext = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    BY_EXTENSION.iter().find(|(e, _)| *e == ext).map(|(_, m)| *m)
}

fn looks_like_text(head: &[u8]) -> bool {
    if head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        // `head` is a prefix of the file and may end inside a multi-byte character;
        // an incomplete final sequence (no invalid byte, `error_len() == None`) is
        // still UTF-8 text.
        Err(e) => e.error_len().is_none(),
    }
}

pub(crate) fn sniff(head: &[u8], name_hint: Option<&str>) -> String {
    if let Some(kind) = infer::get(head) {
        return kind.mime_type().to_string();
    }
    if let Some(mime) = name_hint.and_then(by_extension) {
        return mime.to_string();
    }
    if looks_like_text(head) { "text/plain" } else { "application/octet-stream" }.to_string()
}

pub(crate) fn run(input: &Input) -> Result<Outcome> {
    let size = match i64::try_from(input.size) {
        Ok(n) if n <= MAX_SAFE_INT => n,
        _ => return Ok(Err(format!("size {} exceeds the int value range", input.size))),
    };
    let kind = if input.media_type.starts_with("image/") { vocab::T_PHOTO } else { vocab::T_FILE };
    let blob = input.blob;
    Ok(Ok(vec![
        Draft::new(blob.clone(), Term::parse(vocab::MEDIA_TYPE)?, Value::text(input.media_type)),
        Draft::new(blob.clone(), Term::parse(vocab::SIZE)?, Value::Int(size)),
        Draft::new(blob.clone(), Term::parse(vocab::TYPE)?, Value::Term(Term::parse(kind)?)),
    ]))
}
