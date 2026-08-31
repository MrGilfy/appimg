use crate::error::{Error, Result};

/// Turns a display name into a filesystem-safe slug: lowercase ASCII
/// alphanumerics plus `.`, `_` and `-`. Everything else collapses into a
/// single `-`.
pub fn slugify(name: &str) -> Result<String> {
    let lower = name.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut pending_separator = false;

    for c in lower.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            if pending_separator {
                out.push('-');
                pending_separator = false;
            }
            out.push(c);
        } else if !out.is_empty() {
            pending_separator = true;
        }
    }

    let trimmed = out.trim_matches(|c: char| matches!(c, '-' | '.'));
    if trimmed.is_empty() {
        return Err(Error::InvalidName(name.to_string()));
    }
    Ok(trimmed.to_string())
}

/// Strips a trailing `.AppImage` and a trailing version suffix from a file
/// name so it can serve as a default application name.
pub fn name_from_filename(file_name: &str) -> String {
    let base = file_name
        .strip_suffix(".AppImage")
        .or_else(|| file_name.strip_suffix(".appimage"))
        .unwrap_or(file_name);

    match base.find(['-', '_']) {
        Some(idx) if base[idx + 1..].starts_with(|c: char| c.is_ascii_digit()) => {
            base[..idx].to_string()
        }
        _ => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercases_and_replaces_spaces() {
        assert_eq!(slugify("Some App").unwrap(), "some-app");
    }

    #[test]
    fn collapses_runs_of_invalid_characters() {
        assert_eq!(slugify("Some   //  App!!").unwrap(), "some-app");
    }

    #[test]
    fn keeps_dots_underscores_and_dashes() {
        assert_eq!(slugify("my_app-2.0").unwrap(), "my_app-2.0");
    }

    #[test]
    fn strips_slashes_so_no_path_traversal_is_possible() {
        let slug = slugify("../../etc/passwd").unwrap();
        assert_eq!(slug, "etc-passwd");
        assert!(!slug.contains('/'));
    }

    #[test]
    fn trims_leading_and_trailing_separators() {
        assert_eq!(slugify("...App...").unwrap(), "app");
        assert_eq!(slugify("--app--").unwrap(), "app");
    }

    #[test]
    fn transliterates_nothing_but_survives_unicode() {
        assert_eq!(slugify("Grüße App").unwrap(), "gr-e-app");
    }

    #[test]
    fn rejects_names_without_usable_characters() {
        assert!(slugify("").is_err());
        assert!(slugify("   ").is_err());
        assert!(slugify("日本語").is_err());
    }

    #[test]
    fn default_name_drops_extension_and_version() {
        assert_eq!(name_from_filename("Nextcloud-3.13.0-x86_64.AppImage"), "Nextcloud");
        assert_eq!(name_from_filename("obsidian_1.5.3.AppImage"), "obsidian");
        assert_eq!(name_from_filename("Cursor.AppImage"), "Cursor");
        assert_eq!(name_from_filename("some-app.AppImage"), "some-app");
    }
}
