//! The installed applications as a file another machine can import: what
//! each desktop entry holds that a user decided, and where the application
//! came from. The AppImages themselves are not in it, an import downloads
//! them again, see [`ExportedApp::fetch`].
//!
//! ```json
//! {
//!   "version": 1,
//!   "apps": [
//!     {
//!       "slug": "krita",
//!       "name": "Krita",
//!       "comment": "Digital painting",
//!       "categories": ["Graphics"],
//!       "arguments": ["--nosplash"],
//!       "terminal": false,
//!       "update_source": "github:KDE/krita",
//!       "origin": "/home/u/Downloads/krita-5.2.6-x86_64.AppImage",
//!       "installed_version": "5.2.6"
//!     }
//!   ]
//! }
//! ```

use crate::desktop_entry::{self, DesktopEntry};
use crate::error::{Error, Result};
use crate::list;
use crate::paths::Paths;
use crate::{download, json, slug, update};

/// The format version this appimg writes, and the only one it reads.
pub const FORMAT_VERSION: u64 = 1;

/// One application of an export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedApp {
    pub slug: String,
    pub name: String,
    pub comment: Option<String>,
    pub categories: Vec<String>,
    /// The extra arguments the launcher passes it.
    pub arguments: Vec<String>,
    pub terminal: bool,
    /// `X-AppImg-UpdateSource` as the entry holds it, `manual` included.
    /// `None` for an entry written by 0.2.x, which has none.
    pub update_source: Option<String>,
    /// Where it was installed from: a URL, or a path on the machine that
    /// exported it.
    pub origin: Option<String>,
    /// For information only: an import installs whatever is current.
    pub installed_version: Option<String>,
}

/// Where an import gets an application's AppImage from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetch {
    /// The newest AppImage of the releases a `github:` update source
    /// follows, picked by the file name of the one it was installed from,
    /// the way an update picks it.
    Release { source: String, asset_hint: Option<String> },
    /// The URL its update source names, which is where its updates come
    /// from.
    UpdateSource(String),
    /// The URL it was installed from, brought up to date right away when
    /// it has something to update from, so it ends up current.
    Origin(String),
    /// Nothing to download: it updates manually, and was installed from a
    /// file on the machine that exported it.
    Nothing,
}

impl ExportedApp {
    /// Where an import downloads it from, in this order: a `github:` update
    /// source, an update source that is a URL, the URL it was installed
    /// from. A path it was installed from is a file on another machine,
    /// nothing to download.
    pub fn fetch(&self) -> Fetch {
        let source = self.update_source.as_deref();
        match source.and_then(|source| update::parse_update_source(source).ok()) {
            Some(source) if source.starts_with("github:") => Fetch::Release {
                source,
                asset_hint: self
                    .origin
                    .as_deref()
                    .and_then(|origin| origin.rsplit('/').next())
                    .filter(|name| !name.is_empty())
                    .map(str::to_string),
            },
            Some(url) => Fetch::UpdateSource(url),
            None => match self.origin.as_deref() {
                Some(origin) if download::is_url(origin) => Fetch::Origin(origin.to_string()),
                _ => Fetch::Nothing,
            },
        }
    }
}

/// Every application appimg manages, as an export holds it, sorted by name.
pub fn collect(paths: &Paths) -> Result<Vec<ExportedApp>> {
    let mut apps = Vec::new();
    for app in list::list(paths)? {
        let entry = DesktopEntry::read(&app.desktop_entry_path)?;
        apps.push(ExportedApp {
            arguments: entry.get("Exec").map(desktop_entry::exec_arguments).unwrap_or_default(),
            terminal: entry.terminal(),
            slug: app.slug,
            name: app.name,
            comment: app.comment,
            categories: app.categories,
            update_source: app.update_source,
            origin: app.origin,
            installed_version: app.version,
        });
    }
    Ok(apps)
}

/// The export file for these applications.
pub fn to_json(apps: &[ExportedApp]) -> String {
    let mut out = format!("{{\n  \"version\": {FORMAT_VERSION},\n  \"apps\": [");
    for (index, app) in apps.iter().enumerate() {
        out.push_str(if index == 0 { "\n" } else { ",\n" });
        let fields = [
            ("slug", string(&app.slug)),
            ("name", string(&app.name)),
            ("comment", optional(app.comment.as_deref())),
            ("categories", strings(&app.categories)),
            ("arguments", strings(&app.arguments)),
            ("terminal", app.terminal.to_string()),
            ("update_source", optional(app.update_source.as_deref())),
            ("origin", optional(app.origin.as_deref())),
            ("installed_version", optional(app.installed_version.as_deref())),
        ];
        let fields: Vec<String> =
            fields.iter().map(|(key, value)| format!("      \"{key}\": {value}")).collect();
        out.push_str(&format!("    {{\n{}\n    }}", fields.join(",\n")));
    }
    out.push_str(if apps.is_empty() { "]\n}\n" } else { "\n  ]\n}\n" });
    out
}

/// Reads an export file. One of a format version this appimg does not
/// know is refused as a whole, before anything else in it is read.
pub fn from_json(text: &str) -> Result<Vec<ExportedApp>> {
    if !text.trim_start().starts_with('{') {
        return Err(Error::NotAnExport("it is no JSON object".to_string()));
    }
    let version = json::number_field(text, "version")
        .ok_or_else(|| Error::NotAnExport("it names no format version".to_string()))?;
    if version != FORMAT_VERSION {
        return Err(Error::UnknownExportVersion { found: version, supported: FORMAT_VERSION });
    }
    if !holds_array(text, "apps") {
        return Err(Error::NotAnExport("it has no list of apps".to_string()));
    }

    json::array_field_objects(text, "apps")
        .into_iter()
        .enumerate()
        .map(|(index, object)| read_app(object, index))
        .collect()
}

/// One application of an export. A slug and a name it has to have, the
/// slug one that names no file outside the directories it goes into.
fn read_app(object: &str, index: usize) -> Result<ExportedApp> {
    let invalid = |what: &str| Error::NotAnExport(format!("app {} has {what}", index + 1));
    let slug = json::string_field(object, "slug").ok_or_else(|| invalid("no slug"))?;
    slug::check(&slug)?;
    let name = json::string_field(object, "name")
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| invalid("no name"))?;
    let list = |key: &str| json::string_array_field(object, key).unwrap_or_default();
    Ok(ExportedApp {
        comment: json::string_field(object, "comment"),
        categories: list("categories"),
        arguments: list("arguments"),
        terminal: json::bool_field(object, "terminal").unwrap_or(false),
        update_source: json::string_field(object, "update_source"),
        origin: json::string_field(object, "origin"),
        installed_version: json::string_field(object, "installed_version"),
        slug,
        name,
    })
}

/// Whether some `"key"` in the document holds an array.
fn holds_array(text: &str, key: &str) -> bool {
    let needle = format!("\"{key}\"");
    text.match_indices(&needle).any(|(at, _)| {
        text[at + needle.len()..]
            .trim_start()
            .strip_prefix(':')
            .is_some_and(|value| value.trim_start().starts_with('['))
    })
}

fn string(value: &str) -> String {
    format!("\"{}\"", json::escape(value))
}

fn optional(value: Option<&str>) -> String {
    value.map_or_else(|| "null".to_string(), string)
}

fn strings(values: &[String]) -> String {
    let values: Vec<String> = values.iter().map(|value| string(value)).collect();
    format!("[{}]", values.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(slug: &str) -> ExportedApp {
        ExportedApp {
            slug: slug.to_string(),
            name: "Fake \"App\"".to_string(),
            comment: Some("line\nbreak, \\ and ünïcode".to_string()),
            categories: vec!["Graphics".to_string(), "Development".to_string()],
            arguments: vec!["--flag".to_string(), "two words".to_string(), String::new()],
            terminal: true,
            update_source: Some("github:o/r".to_string()),
            origin: Some("/home/u/Fake_App-1.0.AppImage".to_string()),
            installed_version: Some("1.0".to_string()),
        }
    }

    #[test]
    fn an_export_reads_back_as_it_was_written() {
        let bare = ExportedApp {
            comment: None,
            categories: Vec::new(),
            arguments: Vec::new(),
            terminal: false,
            update_source: None,
            origin: None,
            installed_version: None,
            ..app("bare")
        };
        let apps = vec![app("fake-app"), bare];
        assert_eq!(from_json(&to_json(&apps)).unwrap(), apps);
        assert_eq!(from_json(&to_json(&[])).unwrap(), Vec::new());
    }

    #[test]
    fn a_format_version_it_does_not_know_is_refused() {
        let text = to_json(&[app("fake-app")]).replace("\"version\": 1", "\"version\": 2");
        assert!(matches!(
            from_json(&text),
            Err(Error::UnknownExportVersion { found: 2, supported: FORMAT_VERSION })
        ));
        let not_exports =
            ["", "[]", "{\"apps\": []}", "{\"version\": \"1\", \"apps\": []}", "{\"version\": 1}"];
        for text in not_exports {
            assert!(matches!(from_json(text), Err(Error::NotAnExport(_))), "{text:?}");
        }
    }

    #[test]
    fn an_app_without_a_slug_it_could_install_under_is_refused() {
        for slug in ["../escape", "Upper", ""] {
            let text = to_json(&[app("fake-app")]).replace("\"fake-app\"", &format!("{slug:?}"));
            assert!(from_json(&text).is_err(), "{slug:?}");
        }
    }

    #[test]
    fn a_github_source_comes_first_then_a_url_to_update_from_then_one_it_came_from() {
        let (source, origin) =
            ("https://example.com/latest/Fake.AppImage", "https://example.com/dl/Fake-1.AppImage");
        let with = |update_source: Option<&str>, origin: Option<&str>| {
            ExportedApp {
                update_source: update_source.map(str::to_string),
                origin: origin.map(str::to_string),
                ..app("a")
            }
            .fetch()
        };
        let release = |hint: &str| Fetch::Release {
            source: "github:o/r".to_string(),
            asset_hint: Some(hint.to_string()),
        };

        assert_eq!(with(Some("github:o/r"), Some(origin)), release("Fake-1.AppImage"));
        assert_eq!(app("a").fetch(), release("Fake_App-1.0.AppImage"));
        let local = "/home/u/Fake_App-1.0.AppImage";
        assert_eq!(with(Some(source), Some(origin)), Fetch::UpdateSource(source.to_string()));
        assert_eq!(with(Some(source), Some(local)), Fetch::UpdateSource(source.to_string()));
        assert_eq!(with(Some(source), None), Fetch::UpdateSource(source.to_string()));
        for update_source in [None, Some("manual"), Some("github:"), Some("ftp://x/y")] {
            assert_eq!(with(update_source, Some(origin)), Fetch::Origin(origin.to_string()));
            assert_eq!(with(update_source, Some(local)), Fetch::Nothing);
            assert_eq!(with(update_source, None), Fetch::Nothing);
        }
    }
}
