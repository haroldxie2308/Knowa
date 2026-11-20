//! RFC 4648 §5 base64url without padding, for actor seeds. knowa-config has no
//! encoding dependency and knowa-model exposes no seed codec, so this is the one
//! copy. Decoding is strict: no padding, no whitespace, zero trailing bits.

use knowa_model::{Error, Result};

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub(crate) fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        // Bytes past the end are zero bits by definition (RFC 4648 §4).
        let b = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        // A chunk of k bytes carries 8k bits → k + 1 output symbols.
        for i in 0..=chunk.len() {
            let sextet = (n >> (18 - 6 * i)) & 0x3f;
            out.push(char::from(ALPHABET[sextet as usize]));
        }
    }
    out
}

fn sextet(c: u8) -> Option<u32> {
    let v = match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'-' => 62,
        b'_' => 63,
        _ => return None,
    };
    Some(u32::from(v))
}

pub(crate) fn decode(s: &str) -> Result<Vec<u8>> {
    let bad = |why: &str| Error::invalid(format!("invalid base64url (no padding): {why}"));
    if s.len() % 4 == 1 {
        return Err(bad("impossible length"));
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.as_bytes().chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            let v = sextet(c).ok_or_else(|| bad("character outside the base64url alphabet"))?;
            n |= v << (18 - 6 * i);
        }
        // k symbols carry 6k bits → k − 1 whole bytes; the rest must be zero.
        let bytes = chunk.len() - 1;
        let unused = 24 - 8 * bytes;
        if n & ((1 << unused) - 1) != 0 {
            return Err(bad("non-zero trailing bits"));
        }
        for i in 0..bytes {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 4648 §10 vectors (identical in the url alphabet: no '+' or '/' occur).
    const VECTORS: &[(&str, &str)] =
        &[("", ""), ("f", "Zg"), ("fo", "Zm8"), ("foo", "Zm9v"), ("foob", "Zm9vYg"), ("fooba", "Zm9vYmE"), ("foobar", "Zm9vYmFy")];

    #[test]
    fn rfc4648_vectors() {
        for (plain, enc) in VECTORS {
            assert_eq!(encode(plain.as_bytes()), *enc);
            assert_eq!(decode(enc).unwrap(), plain.as_bytes());
        }
    }

    #[test]
    fn url_alphabet() {
        assert_eq!(encode(&[0xfb, 0xff]), "-_8");
        assert_eq!(decode("-_8").unwrap(), [0xfb, 0xff]);
    }

    #[test]
    fn roundtrips_every_length_and_byte() {
        let data: Vec<u8> = (0..=255u8).collect();
        for len in 0..data.len() {
            let enc = encode(&data[..len]);
            assert!(!enc.contains('='));
            assert_eq!(decode(&enc).unwrap(), &data[..len]);
        }
        assert_eq!(encode(&[0u8; 32]).len(), 43);
    }

    #[test]
    fn rejects_non_canonical_input() {
        for bad in ["Zg==", "Z", "Zh", "Zm9v Yg", "Zm+v", "Zm/v"] {
            assert!(decode(bad).is_err(), "{bad} should be rejected");
        }
    }
}
