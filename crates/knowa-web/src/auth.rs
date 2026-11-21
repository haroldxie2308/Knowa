//! The access token and the Host check (DESIGN §7.4, "Local only").

use data_encoding::BASE32_NOPAD;
use knowa_model::{Error, Result};

/// 256 random bits, lowercase base32 without padding (the encoding Knowa ids use).
pub(crate) struct Token(String);

impl Token {
    pub fn generate() -> Result<Token> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| Error::internal(format!("OS randomness unavailable: {e}")))?;
        Ok(Token(BASE32_NOPAD.encode(&bytes).to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether an `Authorization` header value is `Bearer <this token>`. The scheme is
    /// case-insensitive (RFC 9110); the token is compared in constant time.
    pub fn admits(&self, authorization: Option<&str>) -> bool {
        let Some((scheme, presented)) = authorization.and_then(|v| v.split_once(' ')) else { return false };
        scheme.eq_ignore_ascii_case("bearer") && ct_eq(presented.trim().as_bytes(), self.0.as_bytes())
    }
}

/// Equal-length inputs are compared without an early exit. The length itself is not
/// secret: every token has the same one.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// A `Host` that is not `127.0.0.1:<port>` or `localhost:<port>` (or none at all) is
/// refused: a page on another origin that rebinds its DNS name to 127.0.0.1 still
/// sends its own name.
pub(crate) fn host_allowed(host: Option<&str>, port: u16) -> bool {
    host.is_some_and(|h| {
        h.eq_ignore_ascii_case(&format!("127.0.0.1:{port}")) || h.eq_ignore_ascii_case(&format!("localhost:{port}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_256_bits_and_differ() {
        let (a, b) = (Token::generate().unwrap(), Token::generate().unwrap());
        assert_eq!(a.as_str().len(), 52, "32 bytes in base32 without padding");
        assert!(a.as_str().bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert_ne!(a.as_str(), b.as_str());
    }

    #[test]
    fn only_the_bearer_token_is_admitted() {
        let t = Token::generate().unwrap();
        assert!(t.admits(Some(&format!("Bearer {}", t.as_str()))));
        assert!(t.admits(Some(&format!("bearer {}", t.as_str()))));
        assert!(!t.admits(None));
        assert!(!t.admits(Some(t.as_str())));
        assert!(!t.admits(Some(&format!("Basic {}", t.as_str()))));
        assert!(!t.admits(Some(&format!("Bearer {}x", t.as_str()))));
        assert!(!t.admits(Some("Bearer ")));
    }

    #[test]
    fn host_must_name_loopback_and_this_port() {
        assert!(host_allowed(Some("127.0.0.1:4747"), 4747));
        assert!(host_allowed(Some("localhost:4747"), 4747));
        assert!(!host_allowed(Some("localhost:4748"), 4747));
        assert!(!host_allowed(Some("localhost"), 4747));
        assert!(!host_allowed(Some("evil.example:4747"), 4747));
        assert!(!host_allowed(None, 4747));
    }
}
