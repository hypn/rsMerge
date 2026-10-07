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
    fn blank_and_invalid_input() {
        assert_eq!(format(" \n"), Ok(Vec::new()));
        assert!(format("{\"a\": }").unwrap_err().contains("line 1"));
        assert!(is_json_path(Path::new("x/Data.JSON")));
        assert!(!is_json_path(Path::new("x.jsonl")));
    }
}
