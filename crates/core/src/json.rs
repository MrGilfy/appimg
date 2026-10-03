//! Just enough JSON for two jobs: pulling a handful of string fields out of
//! GitHub release responses and writing machine-readable output. A full serde
//! stack would be a lot of dependency for that.

/// Values of every `"key": "value"` pair in the document, in order.
pub fn string_fields(json: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{key}\"");
    let bytes = json.as_bytes();
    let mut values = Vec::new();
    let mut position = 0;

    while let Some(found) = json[position..].find(&needle) {
        let mut cursor = position + found + needle.len();
        position = cursor;

        while cursor < bytes.len() && (bytes[cursor] as char).is_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b':' {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && (bytes[cursor] as char).is_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'"' {
            continue;
        }
        if let Some((value, end)) = read_string(json, cursor) {
            values.push(value);
            position = end;
        }
    }
    values
}

/// The first value for `key`.
pub fn string_field(json: &str, key: &str) -> Option<String> {
    string_fields(json, key).into_iter().next()
}

/// The first `"key": true` or `"key": false` in the document.
pub fn bool_field(json: &str, key: &str) -> Option<bool> {
    let needle = format!("\"{key}\"");
    let mut position = 0;

    while let Some(found) = json[position..].find(&needle) {
        position += found + needle.len();
        let Some(value) = json[position..].trim_start().strip_prefix(':') else {
            continue;
        };
        let value = value.trim_start();
        if value.starts_with("true") {
            return Some(true);
        }
        if value.starts_with("false") {
            return Some(false);
        }
    }
    None
}

/// The first `"key": <number>` in the document, a whole number that is not
/// negative.
pub fn number_field(json: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\"");
    let mut position = 0;

    while let Some(found) = json[position..].find(&needle) {
        position += found + needle.len();
        let Some(value) = json[position..].trim_start().strip_prefix(':') else {
            continue;
        };
        let value = value.trim_start();
        let digits = value.len() - value.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        let rest = value[digits..].trim_start();
        let ends = rest.is_empty() || rest.starts_with([',', '}', ']']);
        if digits > 0 && ends {
            return value[..digits].parse().ok();
        }
    }
    None
}

/// The strings of the array the first `"key": [...]` in the document holds.
/// `None` when there is no such array, or something in it is no string.
pub fn string_array_field(json: &str, key: &str) -> Option<Vec<String>> {
    let needle = format!("\"{key}\"");
    let mut position = 0;

    while let Some(found) = json[position..].find(&needle) {
        position += found + needle.len();
        let rest = json[position..].trim_start();
        let Some(value) = rest.strip_prefix(':') else {
            continue;
        };
        let Some(mut rest) = value.trim_start().strip_prefix('[') else {
            continue;
        };
        let mut values = Vec::new();
        loop {
            rest = rest.trim_start();
            if rest.starts_with(']') {
                return Some(values);
            }
            let start = json.len() - rest.len();
            let (value, end) = read_string(json, start)?;
            values.push(value);
            rest = json[end..].trim_start();
            rest = match rest.strip_prefix(',') {
                Some(after) => after,
                None if rest.starts_with(']') => rest,
                None => return None,
            };
        }
    }
    None
}

/// The objects of a top-level array, each as the part of the document it
/// spans, so the fields of one can be read without those of the next.
/// Brackets inside strings do not count.
pub fn array_objects(json: &str) -> Vec<&str> {
    let bytes = json.as_bytes();
    let mut objects = Vec::new();
    let Some(open) = json.find(|c: char| !c.is_whitespace()).filter(|&i| bytes[i] == b'[') else {
        return objects;
    };

    let mut depth = 0usize;
    let mut start = None;
    let mut in_string = false;
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if in_string => i += 1,
            b'"' => in_string = !in_string,
            _ if in_string => {}
            b'{' | b'[' => {
                if depth == 0 && bytes[i] == b'{' {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' | b']' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = start.take() {
                        objects.push(&json[start..=i]);
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    objects
}

/// The objects of the array the first `"key": [...]` in the document holds,
/// the way [`array_objects`] splits them: the assets of a release, each
/// with its own download URL and digest.
pub fn array_field_objects<'a>(json: &'a str, key: &str) -> Vec<&'a str> {
    let needle = format!("\"{key}\"");
    let mut position = 0;

    while let Some(found) = json[position..].find(&needle) {
        position += found + needle.len();
        let rest = json[position..].trim_start();
        let Some(value) = rest.strip_prefix(':') else {
            continue;
        };
        let value = value.trim_start();
        if value.starts_with('[') {
            return array_objects(value);
        }
    }
    Vec::new()
}

/// Escapes a string for JSON output.
pub fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Reads the JSON string starting at the opening quote and returns it
/// together with the index just past the closing quote.
fn read_string(json: &str, start: usize) -> Option<(String, usize)> {
    let bytes = json.as_bytes();
    if bytes.get(start) != Some(&b'"') {
        return None;
    }

    let mut out = String::new();
    let mut i = start + 1;

    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Some((out, i + 1)),
            b'\\' => {
                i += 1;
                match bytes.get(i)? {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'u' => {
                        let hex = json.get(i + 1..i + 5)?;
                        let code = u32::from_str_radix(hex, 16).ok()?;
                        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        i += 4;
                    }
                    other => out.push(*other as char),
                }
                i += 1;
            }
            _ => {
                // Multi-byte characters are copied whole.
                let rest = &json[i..];
                let c = rest.chars().next()?;
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_single_field() {
        let json = r#"{"tag_name": "v1.2.3", "draft": false}"#;
        assert_eq!(string_field(json, "tag_name").as_deref(), Some("v1.2.3"));
        assert_eq!(string_field(json, "missing"), None);
    }

    #[test]
    fn reads_repeated_fields_in_order() {
        let json = r#"{"assets":[{"browser_download_url":"https://a/one.AppImage"},
                                 {"browser_download_url":"https://a/two.AppImage"}]}"#;
        assert_eq!(
            string_fields(json, "browser_download_url"),
            vec!["https://a/one.AppImage".to_string(), "https://a/two.AppImage".to_string()]
        );
    }

    #[test]
    fn handles_escapes_and_unicode() {
        let json = r#"{"name": "a \"quoted\" \\ pathA", "other": 1}"#;
        assert_eq!(string_field(json, "name").as_deref(), Some("a \"quoted\" \\ pathA"));
    }

    #[test]
    fn ignores_non_string_values() {
        let json = r#"{"size": 12345, "name": "real"}"#;
        assert_eq!(string_field(json, "size"), None);
        assert_eq!(string_field(json, "name").as_deref(), Some("real"));
    }

    #[test]
    fn reads_booleans() {
        let json = r#"{"draft" : false, "prerelease":true, "name": "true"}"#;
        assert_eq!(bool_field(json, "draft"), Some(false));
        assert_eq!(bool_field(json, "prerelease"), Some(true));
        assert_eq!(bool_field(json, "name"), None);
        assert_eq!(bool_field(json, "missing"), None);
    }

    #[test]
    fn splits_an_array_into_its_objects() {
        let json = r#" [ {"a": {"b": [1, {"c": 2}]}, "s": "} ] { \" ["},
                        {"a": 2}, {} ] "#;
        assert_eq!(
            array_objects(json),
            vec![r#"{"a": {"b": [1, {"c": 2}]}, "s": "} ] { \" ["}"#, r#"{"a": 2}"#, "{}"]
        );
        assert!(array_objects(r#"{"not": "an array"}"#).is_empty());
        assert!(array_objects("[]").is_empty());
    }

    #[test]
    fn splits_the_array_a_field_holds_into_its_objects() {
        // `assets_url` comes first in a release and is no `assets`.
        let json = r#"{"assets_url": "https://a/assets", "tag_name": "v1",
                       "assets" : [{"name": "one", "digest": null},
                                   {"name": "two", "digest": "sha256:ab"}],
                       "body": "\"assets\": [{}]"}"#;
        let assets = array_field_objects(json, "assets");
        assert_eq!(assets.len(), 2);
        assert_eq!(string_field(assets[0], "digest"), None);
        assert_eq!(string_field(assets[1], "digest").as_deref(), Some("sha256:ab"));

        assert!(array_field_objects(r#"{"assets": "none"}"#, "assets").is_empty());
        assert!(array_field_objects(r#"{"other": []}"#, "assets").is_empty());
    }

    #[test]
    fn reads_numbers() {
        let json = r#"{"name": "version 3", "version" : 12, "other": 1.5, "last": 7}"#;
        assert_eq!(number_field(json, "version"), Some(12));
        assert_eq!(number_field(json, "other"), None);
        assert_eq!(number_field(json, "last"), Some(7));
        assert_eq!(number_field(r#"{"v": "1"}"#, "v"), None);
        assert_eq!(number_field(r#"{"v": -1}"#, "v"), None);
        assert_eq!(number_field(r#"{"v": 1e3}"#, "v"), None);
        assert_eq!(number_field(r#"{}"#, "v"), None);
    }

    #[test]
    fn reads_arrays_of_strings() {
        let json = r#"{"a": ["x", "y \"z\"", ""], "b": [], "c": ["x", 1], "d": "no", "e": ["x""#;
        assert_eq!(
            string_array_field(json, "a"),
            Some(vec!["x".to_string(), "y \"z\"".to_string(), String::new()])
        );
        assert_eq!(string_array_field(json, "b"), Some(Vec::new()));
        assert_eq!(string_array_field(json, "c"), None);
        assert_eq!(string_array_field(json, "d"), None);
        assert_eq!(string_array_field(json, "e"), None);
        assert_eq!(string_array_field(json, "missing"), None);
    }

    #[test]
    fn escaping_round_trips() {
        let value = "line\nwith \"quotes\" and \\ backslash";
        let json = format!("{{\"v\": \"{}\"}}", escape(value));
        assert_eq!(string_field(&json, "v").as_deref(), Some(value));
    }
}
