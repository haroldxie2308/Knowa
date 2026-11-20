//! RFC 8785 (JCS) canonical JSON for the subset Knowa emits.

use crate::error::Result;

/// Serializes `v` per RFC 8785. Knowa restricts input so JCS is exact and simple:
/// - numbers must be integers with |n| ≤ 2^53 − 1 (floats → `invalid`);
/// - object keys must be ASCII (so UTF-16 code-unit order == byte order) → else `invalid`;
/// - strings: escape only `"`, `\`, and U+0000–U+001F (`\b \f \n \r \t` short forms,
///   others `\u00xx` lowercase); everything else, including U+2028/2029 and `/`, verbatim.
pub fn canonical_json(v: &serde_json::Value) -> Result<Vec<u8>> {
    let _ = v;
    todo!()
}
