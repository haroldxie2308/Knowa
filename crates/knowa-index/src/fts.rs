//! Full-text documents and queries over FTS5 with the `unicode61` tokenizer.
//!
//! `unicode61` does not segment CJK text, so documents get a space between CJK
//! characters (each becomes a token) and CJK runs in queries become phrase queries:
//! any substring of a CJK run then matches. User input never reaches FTS syntax:
//! the query is split into words, each word is quoted.

use knowa_model::{Error, Result};

/// Han, Hiragana, Katakana and Hangul (incl. their extension and half-width blocks).
pub(crate) fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11FF       // Hangul Jamo
        | 0x2E80..=0x2FDF     // CJK and Kangxi radicals
        | 0x3005 | 0x3007     // ideographic iteration mark, ideographic zero
        | 0x3021..=0x3029     // Hangzhou numerals
        | 0x3038..=0x303B     // Han marks
        | 0x3041..=0x309F     // Hiragana
        | 0x30A0..=0x30FF     // Katakana
        | 0x3130..=0x318F     // Hangul compatibility Jamo
        | 0x31F0..=0x31FF     // Katakana phonetic extensions
        | 0x3400..=0x4DBF     // CJK extension A
        | 0x4E00..=0x9FFF     // CJK unified ideographs
        | 0xA960..=0xA97F     // Hangul Jamo extended-A
        | 0xAC00..=0xD7FF     // Hangul syllables, Jamo extended-B
        | 0xF900..=0xFAFF     // CJK compatibility ideographs
        | 0xFF66..=0xFF9F     // half-width Katakana
        | 0xFFA0..=0xFFDC     // half-width Hangul
        | 0x1B000..=0x1B16F   // Kana supplement/extended
        | 0x20000..=0x3FFFF   // CJK extensions B–I, compatibility supplement
    )
}

/// Document text with every CJK character surrounded by spaces.
pub(crate) fn document(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 2);
    for c in text.chars() {
        if is_cjk(c) {
            out.push(' ');
            out.push(c);
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

/// FTS5 MATCH expression for free text: every word must match (AND), the last one
/// as a prefix; CJK runs are phrases of single characters. `invalid` when the text
/// has no word characters at all.
pub(crate) fn match_expr(text: &str) -> Result<String> {
    let mut terms: Vec<String> = Vec::new();
    for word in text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
        let mut run = String::new();
        let mut run_cjk = false;
        for c in word.chars() {
            let cjk = is_cjk(c);
            if !run.is_empty() && cjk != run_cjk {
                terms.push(quoted(&run, run_cjk));
                run.clear();
            }
            run_cjk = cjk;
            run.push(c);
        }
        terms.push(quoted(&run, run_cjk));
    }
    let Some(last) = terms.last_mut() else {
        return Err(Error::invalid(format!("text {text:?} contains no searchable words")));
    };
    last.push('*');
    Ok(terms.join(" AND "))
}

/// A word as an FTS5 string; a CJK run as a phrase of its characters. Words only
/// contain alphanumeric characters, so there is never a `"` to escape.
fn quoted(run: &str, cjk: bool) -> String {
    if cjk {
        let spaced: Vec<String> = run.chars().map(String::from).collect();
        format!("\"{}\"", spaced.join(" "))
    } else {
        format!("\"{run}\"")
    }
}
