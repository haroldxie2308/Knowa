//! `GET /api/blob/<b_…>` (DESIGN §7.4): blob bytes are untrusted content, so the
//! response is sandboxed and only a few raster types and plain text render inline.

use std::io::Read;

use knowa_api::{Knowa, Sel};
use knowa_model::query::{GetDoc, GetItem, QueryDoc, QueryResult, View};
use knowa_model::{Error, Id, IdKind, Result, Status, Value, vocab};
use tiny_http::Header;

use crate::Server;
use crate::http::{self, Params, Resp, header};

/// Sent with every blob response: a blob may be HTML or SVG with scripts.
const BLOB_CSP: &str = "sandbox; default-src 'none'; img-src 'self'; style-src 'unsafe-inline'";

/// The accepted media types that are served as themselves.
const INLINE_IMAGES: [&str; 5] = ["image/jpeg", "image/png", "image/gif", "image/webp", "image/avif"];

/// `spaces` is a comma-separated selection as in a query document's `spaces` (absent:
/// the server's space), the "selected spaces" of blob.get (DESIGN §6).
pub(crate) fn respond(server: &Server, raw_id: &str, query: &str) -> Result<Resp> {
    let id = Id::parse(raw_id)?;
    if id.kind() != IdKind::Blob {
        return Err(Error::invalid(format!("blob: {id} is not a blob id (b_…)")));
    }
    let mut params = Params::parse(query)?;
    let spaces: Vec<String> = params.take("spaces")?.map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
    params.finish()?;
    let sel = server.sel(None);
    let bytes = open(server.knowa, &sel, &spaces, &id)?;
    let media_type = accepted_media_type(server.knowa, &sel, &spaces, &id)?;
    Ok(http::stream(headers(media_type.as_deref()), bytes))
}

/// Every read of blob bytes goes through here.
fn open(knowa: &Knowa, sel: &Sel, spaces: &[String], id: &Id) -> Result<Box<dyn Read + Send>> {
    knowa.blob_open(sel, spaces, id)
}

/// The display-chosen `knowa:mediaType` among the blob's **accepted** claims (latest
/// `at`, then claim id: DESIGN §4.7's display rule), read with a `get` under a view of
/// accepted claims only. A proposed media type never decides how bytes render.
fn accepted_media_type(knowa: &Knowa, sel: &Sel, spaces: &[String], id: &Id) -> Result<Option<String>> {
    let doc = GetDoc {
        spaces: spaces.to_vec(),
        view: View { status: vec![Status::Accepted], ..View::default() },
        as_of: None,
        ids: vec![id.clone()],
    };
    let QueryResult::Get(got) = knowa.query(sel, QueryDoc::Get(doc))? else {
        return Err(Error::internal("blob: a get query returned another op's result"));
    };
    match got.items.into_iter().next() {
        Some(GetItem::Entity { claims, .. }) => Ok(claims
            .into_iter()
            .filter(|c| c.status == Status::Accepted && c.claim.body.s == *id && c.claim.body.p.as_str() == vocab::MEDIA_TYPE)
            .filter_map(|c| match c.claim.body.o {
                Value::Text { text, .. } => Some((c.claim.body.at, c.claim.id, text)),
                _ => None,
            })
            .max()
            .map(|(_, _, text)| text)),
        // Stored bytes that no accepted claim describes.
        Some(GetItem::Blob { .. }) => Ok(None),
        Some(GetItem::Claim { .. }) | None => Err(Error::internal(format!("blob: get {id} returned no blob or entity item"))),
    }
}

/// DESIGN §7.4: the five raster types as themselves, `text/plain` as UTF-8 text, and
/// everything else (SVG and HTML included) as a download. Media types compare by their
/// essence, case-insensitively, without parameters (RFC 9110 §8.3.1).
fn headers(media_type: Option<&str>) -> Vec<Header> {
    let essence = media_type.map(|m| m.split_once(';').map_or(m, |(e, _)| e).trim());
    let inline = essence.and_then(|e| {
        if e.eq_ignore_ascii_case("text/plain") {
            return Some("text/plain; charset=utf-8");
        }
        INLINE_IMAGES.into_iter().find(|t| e.eq_ignore_ascii_case(t))
    });
    let mut out = vec![
        header("Content-Security-Policy", BLOB_CSP),
        header("X-Content-Type-Options", "nosniff"),
        header("Cache-Control", "no-store"),
    ];
    match inline {
        Some(content_type) => out.push(header("Content-Type", content_type)),
        None => {
            out.push(header("Content-Type", "application/octet-stream"));
            out.push(header("Content-Disposition", "attachment"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'h>(hs: &'h [Header], name: &'static str) -> Option<&'h str> {
        hs.iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str())
    }

    #[test]
    fn raster_types_and_plain_text_render_inline() {
        for (claimed, served) in [
            ("image/png", "image/png"),
            ("IMAGE/JPEG", "image/jpeg"),
            ("image/avif", "image/avif"),
            ("text/plain", "text/plain; charset=utf-8"),
            ("text/plain; charset=latin1", "text/plain; charset=utf-8"),
        ] {
            let hs = headers(Some(claimed));
            assert_eq!(get(&hs, "Content-Type"), Some(served), "{claimed}");
            assert_eq!(get(&hs, "Content-Disposition"), None, "{claimed}");
            assert_eq!(get(&hs, "Content-Security-Policy"), Some(BLOB_CSP));
            assert_eq!(get(&hs, "X-Content-Type-Options"), Some("nosniff"));
        }
    }

    #[test]
    fn everything_else_downloads() {
        for claimed in [Some("image/svg+xml"), Some("text/html"), Some("application/pdf"), Some("text/markdown"), None] {
            let hs = headers(claimed);
            assert_eq!(get(&hs, "Content-Type"), Some("application/octet-stream"), "{claimed:?}");
            assert_eq!(get(&hs, "Content-Disposition"), Some("attachment"), "{claimed:?}");
            assert_eq!(get(&hs, "Content-Security-Policy"), Some(BLOB_CSP));
        }
    }
}
