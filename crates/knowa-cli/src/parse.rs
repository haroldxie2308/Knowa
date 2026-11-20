//! CLI-local syntax: value tokens, `key=value` pairs, JSONL drafts, `--cmd` splitting
//! and `lat,lon` points. These functions only reshape strings into the model's JSON
//! forms; `knowa-model` parsers own every validation rule.

use knowa_model::{Draft, Error, Id, Result, Value};
use serde_json::json;

/// DESIGN §7 value syntax → the value's JSON form (DESIGN §4.3). A token whose
/// prefix is not one of the value kinds is text in full (`https://…` stays text).
pub fn value_json(token: &str) -> Result<serde_json::Value> {
    let Some((prefix, rest)) = token.split_once(':') else {
        return Ok(json!({ "text": token }));
    };
    Ok(match prefix {
        "text" => json!({ "text": rest }),
        "term" => json!({ "term": rest }),
        "ref" => json!({ "ref": rest }),
        "int" => {
            let n: i64 = rest.parse().map_err(|e| bad_value(token, &format!("not an integer ({e})")))?;
            json!({ "int": n })
        }
        "dec" => json!({ "dec": rest }),
        "bool" => match rest {
            "true" => json!({ "bool": true }),
            "false" => json!({ "bool": false }),
            _ => return Err(bad_value(token, "expected bool:true or bool:false")),
        },
        "when" => json!({ "when": rest }),
        "geo" => match rest.split(',').collect::<Vec<_>>().as_slice() {
            [lat, lon] => json!({ "geo": { "lat": lat, "lon": lon } }),
            [lat, lon, r] => json!({ "geo": { "lat": lat, "lon": lon, "r": r } }),
            _ => return Err(bad_value(token, "expected geo:lat,lon[,r]")),
        },
        "qty" => {
            let (qty, unit) = rest.split_once(':').ok_or_else(|| bad_value(token, "expected qty:<dec>:<unit>"))?;
            json!({ "qty": qty, "unit": unit })
        }
        _ => json!({ "text": token }),
    })
}

/// A value token parsed and validated by the model; errors name the token.
pub fn parse_value(token: &str) -> Result<Value> {
    Value::from_json(&value_json(token)?).map_err(|e| Error { message: format!("value {token:?}: {}", e.message), ..e })
}

fn bad_value(token: &str, why: &str) -> Error {
    Error::invalid(format!("value {token:?}: {why}"))
}

/// `key=value` for `--q` / `--via`, split at the first `=`.
pub fn split_pair<'a>(arg: &'a str, flag: &str) -> Result<(&'a str, &'a str)> {
    match arg.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k, v)),
        _ => Err(Error::invalid(format!("{flag} {arg:?}: expected key=value"))),
    }
}

/// `lat,lon` → the two coordinate strings (validated by the api with the query).
pub fn split_point(s: &str) -> Option<(String, String)> {
    match s.split(',').collect::<Vec<_>>().as_slice() {
        [lat, lon] if !lat.is_empty() && !lon.is_empty() => Some((lat.to_string(), lon.to_string())),
        _ => None,
    }
}

pub enum NearTarget {
    Point(String, String),
    Id(Id),
}

/// `near`'s positional: an id, else a `lat,lon` point.
pub fn near_target(s: &str) -> Result<NearTarget> {
    match Id::parse(s) {
        Ok(id) => Ok(NearTarget::Id(id)),
        // Not an id: the only other accepted form is a point, so the id error is not
        // the useful one to report; a malformed point gets its own message below.
        Err(_) => split_point(s)
            .map(|(lat, lon)| NearTarget::Point(lat, lon))
            .ok_or_else(|| Error::invalid(format!("near {s:?}: expected lat,lon or an id"))),
    }
}

/// JSONL drafts (DESIGN §4.4). Blank lines are skipped; any other bad line fails the
/// whole input with its 1-based line number in the message and `detail.line`.
pub fn read_drafts(input: &str) -> Result<Vec<Draft>> {
    let mut drafts = Vec::new();
    for (i, line) in input.lines().enumerate() {
        let n = i + 1;
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_str(line).map_err(|e| at_line(n, Error::invalid(format!("json: {e}"))))?;
        drafts.push(Draft::from_json(&v).map_err(|e| at_line(n, e))?);
    }
    Ok(drafts)
}

fn at_line(n: usize, e: Error) -> Error {
    let mut detail = json!({ "line": n });
    if let Some(cause) = e.detail {
        detail["cause"] = cause;
    }
    Error { code: e.code, message: format!("line {n}: {}", e.message), detail: Some(detail) }
}

/// Splits `--cmd` into argv like a POSIX shell would, without running one and without
/// any expansion: whitespace separates words; `'…'` is literal; inside `"…"` a
/// backslash escapes only `"` `\` `$` and `` ` ``; elsewhere `\x` is a literal `x`.
pub fn split_command(s: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => word.push(ch),
                        None => return Err(cmd_error(s, "unterminated single quote")),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(ch @ ('"' | '\\' | '$' | '`')) => word.push(ch),
                            Some(ch) => {
                                word.push('\\');
                                word.push(ch);
                            }
                            None => return Err(cmd_error(s, "unterminated double quote")),
                        },
                        Some(ch) => word.push(ch),
                        None => return Err(cmd_error(s, "unterminated double quote")),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(ch) => word.push(ch),
                    None => return Err(cmd_error(s, "trailing backslash")),
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    if words.is_empty() {
        return Err(cmd_error(s, "no command"));
    }
    Ok(words)
}

fn cmd_error(s: &str, why: &str) -> Error {
    Error::usage(format!("--cmd {s:?}: {why}"))
}

/// Text read from stdin for `note -`: one trailing line break (what `echo` and editors
/// add) is not part of the note.
pub fn strip_final_newline(mut s: String) -> String {
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use knowa_model::ErrorCode;
    use serde_json::json;

    use super::*;

    #[test]
    fn value_prefixes_map_to_json_forms() {
        let cases = [
            ("text:hello world", json!({"text": "hello world"})),
            ("text:", json!({"text": ""})),
            ("text:a:b", json!({"text": "a:b"})),
            ("term:knowa:Note", json!({"term": "knowa:Note"})),
            ("ref:e_01h", json!({"ref": "e_01h"})),
            ("int:-42", json!({"int": -42})),
            ("dec:12.50", json!({"dec": "12.50"})),
            ("bool:true", json!({"bool": true})),
            ("bool:false", json!({"bool": false})),
            ("when:2024-05~", json!({"when": "2024-05~"})),
            ("geo:35.68,139.76", json!({"geo": {"lat": "35.68", "lon": "139.76"}})),
            ("geo:35.68,139.76,25", json!({"geo": {"lat": "35.68", "lon": "139.76", "r": "25"}})),
            ("qty:12.5:kg", json!({"qty": "12.5", "unit": "kg"})),
            ("qty:1:m/s", json!({"qty": "1", "unit": "m/s"})),
        ];
        for (token, want) in cases {
            assert_eq!(value_json(token).unwrap(), want, "{token}");
        }
    }

    #[test]
    fn unprefixed_tokens_are_text_in_full() {
        for token in ["hello", "knowa:Note", "https://example.com/a", "e_01h", "", "42"] {
            assert_eq!(value_json(token).unwrap(), json!({ "text": token }), "{token}");
        }
    }

    #[test]
    fn malformed_values_are_invalid() {
        for token in ["int:abc", "int:", "int:1.5", "bool:yes", "bool:", "geo:1", "geo:1,2,3,4", "qty:12.5"] {
            let e = value_json(token).unwrap_err();
            assert_eq!(e.code, ErrorCode::Invalid, "{token}");
            assert!(e.message.contains(token), "{token}: {}", e.message);
        }
    }

    #[test]
    fn pairs_split_at_the_first_equals() {
        assert_eq!(split_pair("ex:p=text:a=b", "--q").unwrap(), ("ex:p", "text:a=b"));
        assert_eq!(split_pair("model=", "--via").unwrap(), ("model", ""));
        assert_eq!(split_pair("novalue", "--q").unwrap_err().code, ErrorCode::Invalid);
        assert_eq!(split_pair("=v", "--via").unwrap_err().code, ErrorCode::Invalid);
    }

    #[test]
    fn points_need_exactly_two_parts() {
        assert_eq!(split_point("35.6,139.7"), Some(("35.6".into(), "139.7".into())));
        assert_eq!(split_point("35.6"), None);
        assert_eq!(split_point("1,2,3"), None);
        assert_eq!(split_point(",2"), None);
    }

    #[test]
    fn commands_split_like_a_shell_without_expansion() {
        let cases: [(&str, &[&str]); 8] = [
            ("worker.sh", &["worker.sh"]),
            ("  python3   -u  w.py ", &["python3", "-u", "w.py"]),
            ("sh -c 'echo $HOME; ls'", &["sh", "-c", "echo $HOME; ls"]),
            (r#"tool "a b" c"#, &["tool", "a b", "c"]),
            (r#"tool "say \"hi\" \n""#, &["tool", r#"say "hi" \n"#]),
            (r"tool a\ b", &["tool", "a b"]),
            ("tool '' x", &["tool", "", "x"]),
            ("a'b'\"c\"d", &["abcd"]),
        ];
        for (cmd, want) in cases {
            assert_eq!(split_command(cmd).unwrap(), want, "{cmd}");
        }
    }

    #[test]
    fn bad_commands_are_usage_errors() {
        for cmd in ["", "   ", "tool 'open", "tool \"open", "tool \\"] {
            assert_eq!(split_command(cmd).unwrap_err().code, ErrorCode::Usage, "{cmd:?}");
        }
    }

    #[test]
    fn bad_jsonl_names_the_line() {
        let e = read_drafts("\n   \n{not json\n").unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid);
        assert!(e.message.starts_with("line 3: "), "{}", e.message);
        assert_eq!(e.detail, Some(json!({ "line": 3 })));
    }

    #[test]
    fn blank_input_has_no_drafts() {
        assert!(read_drafts("\n \r\n").unwrap().is_empty());
    }

    #[test]
    fn line_errors_keep_code_and_cause() {
        let e = at_line(2, Error::integrity("x").with_detail(json!({ "k": 1 })));
        assert_eq!(e.code, ErrorCode::Integrity);
        assert_eq!(e.detail, Some(json!({ "line": 2, "cause": { "k": 1 } })));
    }

    #[test]
    fn only_one_final_newline_is_stripped() {
        assert_eq!(strip_final_newline("a\n".into()), "a");
        assert_eq!(strip_final_newline("a\r\n".into()), "a");
        assert_eq!(strip_final_newline("a\n\n".into()), "a\n");
        assert_eq!(strip_final_newline("a".into()), "a");
    }
}
