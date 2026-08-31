use std::fmt;
use std::path::Path;

use crate::error::{Error, Result};

pub const KEY_MANAGED: &str = "X-AppImg-Managed";
pub const KEY_SLUG: &str = "X-AppImg-Slug";
pub const KEY_SOURCE: &str = "X-AppImg-Source";
pub const KEY_VERSION: &str = "X-AppImg-Version";
pub const KEY_UPDATE_INFO: &str = "X-AppImg-UpdateInfo";
pub const KEY_INSTALLED_AT: &str = "X-AppImg-InstalledAt";

/// The freedesktop main categories. A desktop entry needs at least one of
/// them, everything else is an additional category we do not offer.
pub const MAIN_CATEGORIES: &[&str] = &[
    "AudioVideo",
    "Audio",
    "Video",
    "Development",
    "Education",
    "Game",
    "Graphics",
    "Network",
    "Office",
    "Science",
    "Settings",
    "System",
    "Utility",
];

pub fn validate_categories(categories: &[String]) -> Result<()> {
    for category in categories {
        if !MAIN_CATEGORIES.contains(&category.as_str()) {
            return Err(Error::InvalidCategory(category.clone()));
        }
    }
    Ok(())
}

/// The `[Desktop Entry]` group of a desktop file. Key order is preserved so a
/// rewritten entry keeps the shape it had.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesktopEntry {
    fields: Vec<(String, String)>,
}

impl DesktopEntry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses a desktop file and keeps the `[Desktop Entry]` group. Other
    /// groups such as `[Desktop Action ...]` are ignored, never an error.
    pub fn parse(text: &str) -> Self {
        let mut fields: Vec<(String, String)> = Vec::new();
        let mut in_entry_group = false;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') {
                in_entry_group = line == "[Desktop Entry]";
                continue;
            }
            if !in_entry_group {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            if let Some(existing) = fields.iter_mut().find(|(k, _)| *k == key) {
                existing.1 = value;
            } else {
                fields.push((key, value));
            }
        }

        Self { fields }
    }

    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        Ok(Self::parse(&text))
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        crate::fs_util::write_atomic(path, self.to_string().as_bytes(), 0o644)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Looks up `key` for the given locale, e.g. `Name[de_DE]`, then `Name[de]`,
    /// then plain `Name`.
    pub fn get_localized(&self, key: &str, locale: Option<&str>) -> Option<&str> {
        if let Some(locale) = locale {
            let locale = locale.split('.').next().unwrap_or(locale);
            if let Some(value) = self.get(&format!("{key}[{locale}]")) {
                return Some(value);
            }
            let language = locale.split(['_', '@']).next().unwrap_or(locale);
            if language != locale {
                if let Some(value) = self.get(&format!("{key}[{language}]")) {
                    return Some(value);
                }
            }
        }
        self.get(key)
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        let value = value.into();
        match self.fields.iter_mut().find(|(k, _)| *k == key) {
            Some(existing) => existing.1 = value,
            None => self.fields.push((key, value)),
        }
    }

    pub fn set_optional(&mut self, key: &str, value: Option<impl Into<String>>) {
        match value {
            Some(value) => self.set(key, value),
            None => self.remove(key),
        }
    }

    pub fn remove(&mut self, key: &str) {
        self.fields.retain(|(k, _)| k != key);
    }

    pub fn categories(&self) -> Vec<String> {
        self.get("Categories").map(split_list).unwrap_or_default()
    }

    pub fn set_categories(&mut self, categories: &[String]) {
        if categories.is_empty() {
            self.remove("Categories");
        } else {
            self.set("Categories", join_list(categories));
        }
    }

    pub fn terminal(&self) -> bool {
        self.get("Terminal") == Some("true")
    }

    pub fn is_managed(&self) -> bool {
        self.get(KEY_MANAGED) == Some("true")
    }

    pub fn slug(&self) -> Option<&str> {
        self.get(KEY_SLUG)
    }
}

impl fmt::Display for DesktopEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "[Desktop Entry]")?;
        for (key, value) in &self.fields {
            writeln!(f, "{key}={value}")?;
        }
        Ok(())
    }
}

pub fn split_list(value: &str) -> Vec<String> {
    value.split(';').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

pub fn join_list(values: &[String]) -> String {
    let mut out = values.join(";");
    if !out.is_empty() {
        out.push(';');
    }
    out
}

/// Quotes a single `Exec` token following the desktop entry specification.
pub fn quote_exec_token(token: &str, always_quote: bool) -> String {
    let needs_quoting = always_quote
        || token.is_empty()
        || token.chars().any(|c| c.is_whitespace() || RESERVED_EXEC_CHARS.contains(c));

    if !needs_quoting {
        return token.to_string();
    }

    let mut out = String::with_capacity(token.len() + 2);
    out.push('"');
    for c in token.chars() {
        if matches!(c, '"' | '\\' | '`' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

const RESERVED_EXEC_CHARS: &str = "\"'\\><~|&;$*?#()`";

/// Builds an `Exec` line: always-quoted absolute binary path, then the extra
/// arguments, then the field code the AppImage declared for itself.
pub fn build_exec_line(binary: &Path, extra_args: &[String], field_code: Option<&str>) -> String {
    let mut tokens = vec![quote_exec_token(&binary.to_string_lossy(), true)];
    tokens.extend(extra_args.iter().map(|arg| quote_exec_token(arg, false)));
    if let Some(code) = field_code {
        tokens.push(code.to_string());
    }
    tokens.join(" ")
}

/// Returns the field code an embedded `Exec` line declared, if any. Only the
/// codes that accept files or URLs are carried over.
pub fn field_code_of(exec: &str) -> Option<&'static str> {
    ["%U", "%F", "%u", "%f"].into_iter().find(|code| exec.contains(code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const SAMPLE: &str = "\
# a comment

[Desktop Entry]
Type=Application
Name=Sample App
Name[de]=Beispiel
Name[de_AT]=Beispiel AT
Comment=Does things
Exec=AppRun %U
Icon=sample
Categories=Utility;Development;
Terminal=false

[Desktop Action New]
Name=New Window
Exec=AppRun --new-window
";

    #[test]
    fn parses_only_the_desktop_entry_group() {
        let entry = DesktopEntry::parse(SAMPLE);
        assert_eq!(entry.get("Name"), Some("Sample App"));
        assert_eq!(entry.get("Exec"), Some("AppRun %U"));
        // The action group must not leak into the main group.
        assert_ne!(entry.get("Exec"), Some("AppRun --new-window"));
    }

    #[test]
    fn parses_categories_and_terminal() {
        let entry = DesktopEntry::parse(SAMPLE);
        assert_eq!(entry.categories(), vec!["Utility".to_string(), "Development".to_string()]);
        assert!(!entry.terminal());
    }

    #[test]
    fn prefers_the_matching_locale() {
        let entry = DesktopEntry::parse(SAMPLE);
        assert_eq!(entry.get_localized("Name", Some("de_AT")), Some("Beispiel AT"));
        assert_eq!(entry.get_localized("Name", Some("de_DE")), Some("Beispiel"));
        assert_eq!(entry.get_localized("Name", Some("de_DE.UTF-8")), Some("Beispiel"));
        assert_eq!(entry.get_localized("Name", Some("fr")), Some("Sample App"));
        assert_eq!(entry.get_localized("Name", None), Some("Sample App"));
    }

    #[test]
    fn writes_the_group_header_and_keeps_key_order() {
        let mut entry = DesktopEntry::new();
        entry.set("Type", "Application");
        entry.set("Name", "Sample");
        entry.set_categories(&["Utility".to_string()]);
        assert_eq!(
            entry.to_string(),
            "[Desktop Entry]\nType=Application\nName=Sample\nCategories=Utility;\n"
        );
    }

    #[test]
    fn round_trips_through_parse_and_display() {
        let entry = DesktopEntry::parse(SAMPLE);
        let reparsed = DesktopEntry::parse(&entry.to_string());
        assert_eq!(entry, reparsed);
    }

    #[test]
    fn setting_a_key_twice_replaces_it() {
        let mut entry = DesktopEntry::new();
        entry.set("Name", "One");
        entry.set("Name", "Two");
        assert_eq!(entry.to_string(), "[Desktop Entry]\nName=Two\n");
    }

    #[test]
    fn quotes_the_binary_path_and_escapes_specials() {
        let exec = build_exec_line(
            &PathBuf::from("/home/u/.local/share/appimages/a b.AppImage"),
            &[],
            Some("%U"),
        );
        assert_eq!(exec, "\"/home/u/.local/share/appimages/a b.AppImage\" %U");

        let exec = build_exec_line(&PathBuf::from("/tmp/we\"ird$.AppImage"), &[], None);
        assert_eq!(exec, "\"/tmp/we\\\"ird\\$.AppImage\"");
    }

    #[test]
    fn extra_arguments_are_quoted_only_when_needed() {
        let exec = build_exec_line(
            &PathBuf::from("/a.AppImage"),
            &["--flag".into(), "two words".into()],
            None,
        );
        assert_eq!(exec, "\"/a.AppImage\" --flag \"two words\"");
    }

    #[test]
    fn field_codes_are_detected() {
        assert_eq!(field_code_of("AppRun %U"), Some("%U"));
        assert_eq!(field_code_of("AppRun %F"), Some("%F"));
        assert_eq!(field_code_of("AppRun"), None);
    }

    #[test]
    fn categories_are_validated_against_the_main_list() {
        assert!(validate_categories(&["Utility".to_string(), "Network".to_string()]).is_ok());
        assert!(validate_categories(&["Nonsense".to_string()]).is_err());
    }
}
