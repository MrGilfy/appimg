//! Just enough AppStream for one job: finding the GitHub repository the
//! metainfo inside an AppImage links, to suggest updating from its releases.
//! A full XML parser would be a lot of dependency for a handful of `<url>`
//! elements.

use std::fs;
use std::path::{Path, PathBuf};

use crate::update;

/// Where AppStream metainfo lives inside an AppImage, the current place
/// first, then the one older builds still use.
const METAINFO_DIRS: &[&str] = &["usr/share/metainfo", "usr/share/appdata"];

/// The `<url>` types that name the project's own repository first, then
/// those that may point into it, such as its issue tracker.
const URL_TYPES: &[&str] = &["vcs-browser", "homepage", "bugtracker", "help", "contribute", "faq"];

/// Paths on github.com that are GitHub's own pages, not a user's.
const NOT_AN_OWNER: &[&str] = &[
    "about",
    "apps",
    "collections",
    "enterprise",
    "explore",
    "features",
    "issues",
    "login",
    "marketplace",
    "notifications",
    "orgs",
    "pricing",
    "pulls",
    "search",
    "settings",
    "site",
    "sponsors",
    "topics",
    "trending",
    "users",
];

/// `github:owner/repo` for the repository the metainfo of an extracted
/// AppImage links, preferring the types of link in the order of
/// [`URL_TYPES`].
pub fn github_repository(root: &Path) -> Option<String> {
    metainfo_files(root).iter().find_map(|path| {
        let text = fs::read_to_string(path).ok()?;
        let urls = urls(&text);
        URL_TYPES.iter().find_map(|wanted| {
            urls.iter().filter(|(kind, _)| kind == wanted).find_map(|(_, url)| repository_of(url))
        })
    })
}

fn metainfo_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in METAINFO_DIRS {
        let Ok(entries) = fs::read_dir(root.join(dir)) else {
            continue;
        };
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("xml"))
            .collect();
        found.sort();
        files.extend(found);
    }
    files
}

/// Every `<url type="...">...</url>` in a document, as its type and the URL.
fn urls(text: &str) -> Vec<(String, String)> {
    let text = without_comments(text);
    let mut found = Vec::new();
    let mut rest = text.as_str();

    while let Some(start) = rest.find("<url") {
        rest = &rest[start + "<url".len()..];
        // `<urls>`, or anything else that only starts the same way.
        if !rest.starts_with(|c: char| c.is_whitespace() || c == '>') {
            continue;
        }
        let Some(close) = rest.find('>') else {
            break;
        };
        let attributes = &rest[..close];
        rest = &rest[close + 1..];
        if attributes.ends_with('/') {
            continue;
        }
        let Some(end) = rest.find("</url>") else {
            break;
        };
        let url = decode(rest[..end].trim());
        rest = &rest[end..];
        if let Some(kind) = attribute(attributes, "type") {
            found.push((kind, url));
        }
    }
    found
}

/// The value of one attribute, quoted either way.
fn attribute(attributes: &str, name: &str) -> Option<String> {
    let mut rest = attributes;
    while let Some(at) = rest.find(name) {
        let starts_a_name = rest[..at].ends_with(char::is_whitespace);
        rest = &rest[at + name.len()..];
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        if !starts_a_name {
            continue;
        }
        let value = value.trim_start();
        let quote = value.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let value = &value[1..];
        return Some(decode(&value[..value.find(quote)?]));
    }
    None
}

/// A link commented out is no link.
fn without_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        rest = rest[start..].find("-->").map_or("", |end| &rest[start + end + 3..]);
    }
    out.push_str(rest);
    out
}

fn decode(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// `github:owner/repo` for a link into a repository on github.com: the
/// repository itself or anything below it, such as its issues. A link to a
/// user, to one of GitHub's own pages or to a GitHub Pages site is none.
fn repository_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let path = rest.strip_prefix("github.com/")?;
    let mut parts = path.split(['/', '?', '#']);
    let owner = parts.next()?;
    let repo = parts.next()?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    if NOT_AN_OWNER.contains(&owner.to_ascii_lowercase().as_str()) {
        return None;
    }
    update::parse_update_source(&format!("github:{owner}/{repo}")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metainfo(urls: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<component type=\"desktop-application\">\n  \
             <id>org.example.App</id>\n  {urls}\n  <releases>\n    <release version=\"1.0\">\n      \
             <url type=\"details\">https://github.com/elsewhere/notes/releases/1.0</url>\n    \
             </release>\n  </releases>\n</component>\n"
        )
    }

    fn root_with(dir: &str, file: &str, text: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(dir)).unwrap();
        fs::write(root.path().join(dir).join(file), text).unwrap();
        root
    }

    fn found(urls: &str) -> Option<String> {
        let root = root_with("usr/share/metainfo", "org.example.App.metainfo.xml", &metainfo(urls));
        github_repository(root.path())
    }

    #[test]
    fn the_repository_link_comes_before_the_homepage() {
        let urls = "<url type=\"homepage\">https://github.com/home/page</url>\n  \
                    <url type=\"vcs-browser\">https://github.com/WerWolv/ImHex</url>";
        assert_eq!(found(urls).as_deref(), Some("github:WerWolv/ImHex"));
    }

    #[test]
    fn a_link_below_a_repository_names_the_repository() {
        let urls = "<url type=\"homepage\">https://example.org</url>\n  \
                    <url type='bugtracker'>https://github.com/owner/repo/issues?q=is%3Aopen</url>";
        assert_eq!(found(urls).as_deref(), Some("github:owner/repo"));
        let urls = "<url type=\"vcs-browser\">https://www.github.com/owner/repo.git</url>";
        assert_eq!(found(urls).as_deref(), Some("github:owner/repo"));
    }

    #[test]
    fn links_that_name_no_repository_are_passed_over() {
        for url in [
            "https://github.com/owner",
            "https://github.com/sponsors/owner",
            "https://github.com/orgs/owner/repositories",
            "https://owner.github.io/repo",
            "https://gitlab.com/owner/repo",
            "https://github.com.evil.example/owner/repo",
        ] {
            let urls = format!("<url type=\"homepage\">{url}</url>");
            assert_eq!(found(&urls), None, "{url}");
        }
        // Nor does a link in a release, a commented-out one, or an empty one.
        assert_eq!(found("<!-- <url type=\"homepage\">https://github.com/a/b</url> -->"), None);
        assert_eq!(found("<url type=\"homepage\"/>"), None);
    }

    #[test]
    fn entities_are_decoded() {
        let urls = "<url type=\"homepage\">https://github.com/a-b/c_d.e?x=1&amp;y=2</url>";
        assert_eq!(found(urls).as_deref(), Some("github:a-b/c_d.e"));
    }

    #[test]
    fn the_older_appdata_directory_is_read_too() {
        let text = metainfo("<url type=\"homepage\">https://github.com/old/place</url>");
        let root = root_with("usr/share/appdata", "app.appdata.xml", &text);
        assert_eq!(github_repository(root.path()).as_deref(), Some("github:old/place"));
    }

    #[test]
    fn no_metainfo_is_no_suggestion() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(github_repository(root.path()), None);
    }
}
