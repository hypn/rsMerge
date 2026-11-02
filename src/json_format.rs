//! Canonical JSON layout for comparing: keys sorted at every level, 4-space indent, numbers exact
//! (no float rounding) and array order kept.

use serde::Serialize;
use serde_json::{Value, ser::PrettyFormatter};
use std::path::Path;

pub fn is_json_path(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"))
}

/// Formats JSON text into lines. Blank input (e.g. a missing side) gives no lines.
pub fn format(text: &str) -> Result<Vec<String>, String> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, PrettyFormatter::with_indent(b"    "));
    sorted(value).serialize(&mut ser).map_err(|e| e.to_string())?;
    let text = String::from_utf8(out).expect("serde_json writes UTF-8");
    Ok(text.lines().map(String::from).collect())
}

/// A multi-line object or array that can be collapsed: lines `start..=end` hold it, from the
/// line ending in the opening bracket to the line starting with the closing one.
#[derive(Clone, Debug, PartialEq)]
pub struct Fold {
    pub start: usize,
    pub end: usize,
    /// Property names and array indices leading here. It ignores whether the value is an object
    /// or an array, so the same property folds on both sides of a comparison.
    pub path: String,
}

/// Finds the foldable regions of JSON text, tolerating text that isn't formatted or valid.
/// Sorted by `start`; the root value has an empty path.
pub fn folds(lines: &[String]) -> Vec<Fold> {
    struct Frame {
        path: String,
        array: bool,
        index: usize,
        key: Option<String>,
        open_line: usize,
        /// The opening bracket is the last thing on its line.
        ends_line: bool,
    }
    let mut out = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut string: Option<String> = None;
    let mut last_string: Option<String> = None;
    let mut escaped = false;
    for (li, line) in lines.iter().enumerate() {
        for (byte, c) in line.char_indices() {
            if let Some(s) = &mut string {
                if escaped {
                    escaped = false;
                    s.push(c);
                } else if c == '\\' {
                    escaped = true;
                    s.push(c);
                } else if c == '"' {
                    last_string = string.take();
                } else {
                    s.push(c);
                }
                continue;
            }
            match c {
                '"' => string = Some(String::new()),
                ':' => {
                    if let Some(top) = stack.last_mut() {
                        top.key = last_string.take();
                    }
                }
                ',' => {
                    if let Some(top) = stack.last_mut() {
                        top.index += 1;
                        top.key = None;
                    }
                }
                '{' | '[' => {
                    let path = match stack.last() {
                        None => String::new(),
                        Some(p) if p.array => format!("{}\0{}", p.path, p.index),
                        Some(p) => format!("{}\0{}", p.path, p.key.as_deref().unwrap_or("")),
                    };
                    let ends_line = line[byte + 1..].trim().is_empty();
                    stack.push(Frame { path, array: c == '[', index: 0, key: None, open_line: li, ends_line });
                }
                '}' | ']' => {
                    let Some(frame) = stack.pop() else { continue };
                    if frame.ends_line && frame.open_line != li && line.trim_start().starts_with(c) {
                        out.push(Fold { start: frame.open_line, end: li, path: frame.path });
                    }
                }
                _ => {}
            }
        }
    }
    out.sort_by_key(|f| f.start);
    out
}

/// Sorts object keys explicitly so the order doesn't depend on serde_json's map features.
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_nested_keys_and_indents() {
        let lines = format(r#"{"b":1,"a":{"z":[3,{"y":1,"x":2}],"c":null}}"#).unwrap();
        assert_eq!(
            lines,
            [
                "{",
                r#"    "a": {"#,
                r#"        "c": null,"#,
                r#"        "z": ["#,
                "            3,",
                "            {",
                r#"                "x": 2,"#,
                r#"                "y": 1"#,
                "            }",
                "        ]",
                "    },",
                r#"    "b": 1"#,
                "}",
            ]
        );
    }

    #[test]
    fn keeps_numbers_and_text_as_written() {
        let lines = format(r#"[1.50, 1e400, 12345678901234567890123, "éé", {}, []]"#).unwrap();
        assert_eq!(lines[1..4], ["    1.50,", "    1e+400,", "    12345678901234567890123,"]);
        assert_eq!(lines[4], r#"    "éé","#);
        assert_eq!(lines[5..7], ["    {},", "    []"]);
    }

    #[test]
    fn finds_folds_by_path() {
        let lines = format(r#"{"a":{"s":"}{\"[","z":[1,{"y":1}]},"b":[],"c":[[2]]}"#).unwrap();
        let found: Vec<_> = folds(&lines).into_iter().map(|f| (f.start, f.end, f.path.replace('\0', "/"))).collect();
        assert_eq!(
            found,
            [
                (0, lines.len() - 1, String::new()),
                (1, 9, "/a".to_string()),
                (3, 8, "/a/z".to_string()),
                (5, 7, "/a/z/1".to_string()),
                (11, 15, "/c".to_string()),
                (12, 14, "/c/0".to_string()),
            ]
        );
    }

    #[test]
    fn ignores_single_line_values_and_garbage() {
        let lines: Vec<String> = ["{\"a\": [1, 2],", "\"b\": {", "}}}", "]"].map(String::from).to_vec();
        let found: Vec<_> = folds(&lines).into_iter().map(|f| (f.start, f.end)).collect();
        assert_eq!(found, [(1, 2)]);
    }

    #[test]
    fn blank_and_invalid_input() {
        assert_eq!(format(" \n"), Ok(Vec::new()));
        assert!(format("{\"a\": }").unwrap_err().contains("line 1"));
        assert!(is_json_path(Path::new("x/Data.JSON")));
        assert!(!is_json_path(Path::new("x.jsonl")));
    }
}
