//! The feed primitive (DESIGN §6): local arrival order, cursor `<epoch>:<n>`.

use knowa_model::query::{FeedResult, MAX_LIMIT};
use knowa_model::{Error, Result};

use crate::select::Space;

pub(crate) fn feed(space: &Space, cursor: Option<&str>, limit: u32) -> Result<FeedResult> {
    check_limit(limit, "limit")?;
    let parsed = cursor.map(parse_cursor).transpose()?;
    let index = space.index()?;
    let after = match parsed {
        None => 0,
        Some((epoch, n)) => {
            if epoch != index.epoch() {
                return Err(Error::cursor_expired(format!(
                    "feed cursor is from index epoch {epoch:?}, but the index of space {} was rebuilt (epoch {:?}); restart without a cursor",
                    space.name(),
                    index.epoch()
                ))
                .with_detail(serde_json::json!({ "cursor": cursor, "epoch": index.epoch() })));
            }
            n
        }
    };
    let (claims, last) = index.feed(after, limit)?;
    // An empty page means no new arrivals: the position stays where it was.
    let next = if claims.is_empty() { after } else { last };
    Ok(FeedResult { claims, cursor: format_cursor(index.epoch(), next) })
}

/// `1..=MAX_LIMIT`.
pub(crate) fn check_limit(limit: u32, field: &str) -> Result<()> {
    if limit == 0 || limit > MAX_LIMIT {
        return Err(Error::invalid(format!("{field} must be between 1 and {MAX_LIMIT}, got {limit}")));
    }
    Ok(())
}

/// `<epoch>:<n>`; the epoch is the index's opaque token (split at the last `:`),
/// `n` a decimal arrival number. Anything else → `invalid`.
pub(crate) fn parse_cursor(s: &str) -> Result<(&str, u64)> {
    let malformed = || Error::invalid(format!("malformed feed cursor {s:?}: expected <epoch>:<n>"));
    let (epoch, n) = s.rsplit_once(':').ok_or_else(malformed)?;
    if epoch.is_empty() || n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return Err(malformed());
    }
    Ok((epoch, n.parse().map_err(|_| malformed())?))
}

pub(crate) fn format_cursor(epoch: &str, n: u64) -> String {
    format!("{epoch}:{n}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use knowa_model::ErrorCode;

    #[test]
    fn cursor_round_trips() {
        let c = format_cursor("7f3a9c", 42);
        assert_eq!(parse_cursor(&c).unwrap(), ("7f3a9c", 42));
    }

    #[test]
    fn cursor_epoch_may_contain_colons() {
        assert_eq!(parse_cursor("a:b:17").unwrap(), ("a:b", 17));
    }

    #[test]
    fn malformed_cursors_are_invalid() {
        for bad in ["", "abc", ":5", "abc:", "abc:-1", "abc:+1", "abc:1x", "abc: 1", "abc:99999999999999999999999"] {
            assert_eq!(parse_cursor(bad).unwrap_err().code, ErrorCode::Invalid, "{bad:?}");
        }
    }

    #[test]
    fn limit_bounds() {
        assert!(check_limit(1, "limit").is_ok());
        assert!(check_limit(MAX_LIMIT, "limit").is_ok());
        assert_eq!(check_limit(0, "limit").unwrap_err().code, ErrorCode::Invalid);
        assert_eq!(check_limit(MAX_LIMIT + 1, "limit").unwrap_err().code, ErrorCode::Invalid);
    }
}
