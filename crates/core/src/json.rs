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
    fn escaping_round_trips() {
        let value = "line\nwith \"quotes\" and \\ backslash";
        let json = format!("{{\"v\": \"{}\"}}", escape(value));
        assert_eq!(string_field(&json, "v").as_deref(), Some(value));
    }
}
