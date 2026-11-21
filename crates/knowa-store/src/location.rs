//! Locations (DESIGN §5.2): where a replica, or its blobs, live.

use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

use knowa_model::{Error, Result};
use object_store::path::Path as ObjectPath;

use crate::fsutil::io_err;

/// The object-store scheme (DESIGN §5.2): S3 and S3-compatible stores.
const S3_SCHEME: &str = "s3";

/// A replica's location, or the separate location of its blobs (DESIGN §5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A directory: local disk, or a mounted shared filesystem.
    Dir(PathBuf),
    /// An object-store URL, `s3://bucket[/prefix]`, as `parse` gives it: without a
    /// trailing `/`.
    Object(String),
}

impl Location {
    /// `s3://bucket[/prefix]` is an object location, kept with a lowercase scheme and
    /// without trailing `/`s; a malformed one (no bucket, an empty or `.`/`..` prefix
    /// segment) → `invalid`. Any other `scheme://` → `unsupported`. Anything else is a
    /// directory path, which may not exist yet.
    pub fn parse(s: &str) -> Result<Location> {
        let Some((scheme, rest)) = split_scheme(s) else {
            return Ok(Location::Dir(PathBuf::from(s)));
        };
        // RFC 3986: schemes are case-insensitive.
        if !scheme.eq_ignore_ascii_case(S3_SCHEME) {
            return Err(Error::unsupported(format!(
                "{s}: locations with scheme {scheme}:// are not supported (a location is a directory path or s3://bucket/prefix)"
            )));
        }
        let url = format!("{S3_SCHEME}://{}", rest.trim_end_matches('/'));
        object_url(&url)?;
        Ok(Location::Object(url))
    }

    /// The canonical location (DESIGN §5.2): the absolute path of a directory, with
    /// every symlink resolved, or the URL without a trailing `/`. A directory that does
    /// not exist → `not_found`; one whose path is not UTF-8 → `unsupported`.
    pub fn canonical(&self) -> Result<String> {
        match self {
            Location::Dir(path) => {
                let abs = fs::canonicalize(path).map_err(|e| match e.kind() {
                    io::ErrorKind::NotFound => Error::not_found(format!("{} does not exist", path.display())),
                    _ => io_err(path, e),
                })?;
                abs.into_os_string().into_string().map_err(|p| {
                    Error::unsupported(format!("location {} is not valid UTF-8, so it has no canonical form", p.display()))
                })
            }
            Location::Object(url) => Ok(url.trim_end_matches('/').to_string()),
        }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Location::Dir(path) => write!(f, "{}", path.display()),
            Location::Object(url) => f.write_str(url),
        }
    }
}

/// `scheme` and the rest when `s` starts with `scheme://` (RFC 3986: a letter, then
/// letters, digits, `+`, `-` or `.`).
fn split_scheme(s: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = s.split_once("://")?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    (first.is_ascii_alphabetic() && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
        .then_some((scheme, rest))
}

/// The bucket and key prefix of an `s3://bucket[/prefix]` URL. The prefix must be a
/// valid object path: no empty, `.` or `..` segment.
pub(crate) fn object_url(url: &str) -> Result<(String, ObjectPath)> {
    let rest = url
        .trim_end_matches('/')
        .strip_prefix(S3_SCHEME)
        .and_then(|r| r.strip_prefix("://"))
        .ok_or_else(|| Error::invalid(format!("{url}: not an s3:// URL")))?;
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    if bucket.is_empty() {
        return Err(Error::invalid(format!("{url}: no bucket (expected s3://bucket/prefix)")));
    }
    let parsed = ObjectPath::parse(prefix).map_err(|e| Error::invalid(format!("{url}: {e}")))?;
    // `parse` drops a leading `/`, which would give one location two spellings.
    if parsed.as_ref() != prefix {
        return Err(Error::invalid(format!("{url}: empty path segment")));
    }
    Ok((bucket.to_string(), parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use knowa_model::ErrorCode;

    #[test]
    fn parse_tells_directories_from_object_urls() {
        assert_eq!(Location::parse("s3://bucket").unwrap(), Location::Object("s3://bucket".into()));
        assert_eq!(Location::parse("s3://bucket/a/b/").unwrap(), Location::Object("s3://bucket/a/b".into()));
        assert_eq!(Location::parse("s3://bucket/a//").unwrap(), Location::Object("s3://bucket/a".into()));
        assert_eq!(Location::parse("S3://bucket/a").unwrap(), Location::Object("s3://bucket/a".into()));
        for path in ["notes", "./notes", "/srv/knowa/notes", "a:b", "./x://y", "1s3://bucket", ""] {
            assert_eq!(Location::parse(path).unwrap(), Location::Dir(PathBuf::from(path)), "{path}");
        }
        for other in ["gs://bucket/x", "file:///tmp/x", "https://example.com/x", "s3a://bucket"] {
            assert_eq!(Location::parse(other).unwrap_err().code, ErrorCode::Unsupported, "{other}");
        }
        for bad in ["s3://", "s3:///prefix", "s3://bucket//prefix", "s3://bucket/a/../b", "s3://bucket/./a"] {
            assert_eq!(Location::parse(bad).unwrap_err().code, ErrorCode::Invalid, "{bad}");
        }
    }

    #[test]
    fn object_urls_split_into_bucket_and_prefix() {
        let (bucket, prefix) = object_url("s3://bucket/a/b").unwrap();
        assert_eq!((bucket.as_str(), prefix.as_ref()), ("bucket", "a/b"));
        let (bucket, prefix) = object_url("s3://bucket").unwrap();
        assert_eq!((bucket.as_str(), prefix.as_ref()), ("bucket", ""));
    }

    /// DESIGN §5.2: the canonical form of a URL drops its trailing `/`, so both
    /// spellings share one index key; a directory's is its resolved absolute path.
    #[test]
    fn canonical_locations() {
        let url = Location::parse("s3://bucket/notes/").unwrap();
        assert_eq!(url.canonical().unwrap(), "s3://bucket/notes");
        assert_eq!(Location::Object("s3://bucket/notes/".into()).canonical().unwrap(), "s3://bucket/notes");
        assert_eq!(crate::location_key("s3://bucket/notes"), crate::location_key(&url.canonical().unwrap()));
        assert_ne!(crate::location_key("s3://bucket/notes"), crate::location_key("s3://bucket/other"));

        let dir = std::env::temp_dir().join(format!("knowa-location-{}", std::process::id()));
        fs::create_dir_all(dir.join("a")).unwrap();
        let canonical = Location::Dir(dir.join("a/../a")).canonical().unwrap();
        assert_eq!(canonical, fs::canonicalize(dir.join("a")).unwrap().to_str().unwrap());
        assert_eq!(Location::Dir(dir.join("missing")).canonical().unwrap_err().code, ErrorCode::NotFound);
        fs::remove_dir_all(&dir).unwrap();
    }
}
