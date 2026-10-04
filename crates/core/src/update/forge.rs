//! The forges an update can follow the releases of: GitHub, GitLab, and
//! Forgejo, which Codeberg runs and which answers the way Gitea does. What
//! they have in common, picking the file out of a release, following a tag,
//! comparing versions, is the business of the rest of [`super`]. What is
//! here is what differs: how a source names a repository, where its API is,
//! and how its answers read.
//!
//! The update sources, as the desktop entry stores them, `@tag` and
//! `#pattern` optional behind each:
//!
//! - `github:owner/repo`, exactly as 0.4.x wrote it;
//! - `gitlab:group/project` on gitlab.com, with as many subgroups as the
//!   project has, and `gitlab:https://host/group/project` on any other host;
//! - `codeberg:owner/repo`, and `forgejo:https://host/owner/repo` on any
//!   other host, which `gitea:` names as well.
//!
//! A self-hosted forge given with the address of gitlab.com or codeberg.org
//! is stored the short way.

use crate::download::{self, Api};
use crate::{date, json, version};

use super::{valid_pattern, Asset, Release};

const GITLAB_COM: &str = "https://gitlab.com";
const CODEBERG: &str = "https://codeberg.org";

/// How many releases the one request for a listing asks for.
pub(super) const RELEASES_PER_PAGE: usize = 30;

/// Which forge, and for one that runs anywhere, where: the scheme and the
/// host, with the port when it has one, and nothing behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Forge {
    GitHub,
    GitLab { base: String },
    Forgejo { base: String },
}

/// A repository on a forge. `path` is `owner/repo`, or on GitLab the
/// project with every group above it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub forge: Forge,
    pub path: String,
}

impl Repo {
    pub fn github(owner: &str, repo: &str) -> Self {
        Self { forge: Forge::GitHub, path: format!("{owner}/{repo}") }
    }

    /// An update source that follows releases, as a user writes it or the
    /// desktop entry stores it, taken apart: the repository, the tag to
    /// follow, and the pattern that picks the file. `None` for anything
    /// else. A `github:` source reads exactly as it always did.
    pub fn parse_source(value: &str) -> Option<(Repo, Option<String>, Option<String>)> {
        let (prefix, spec) = value.trim().split_once(':')?;
        if prefix == "github" {
            let (owner, repo, tag, pattern) = super::github_source_spec(spec)?;
            return Some((Repo::github(&owner, &repo), tag, pattern));
        }
        let (spec, pattern) = match spec.split_once('#') {
            Some((spec, pattern)) => (spec, Some(pattern)),
            None => (spec, None),
        };
        if pattern.is_some_and(|pattern| !valid_pattern(pattern)) {
            return None;
        }
        let (repository, tag) = match spec.split_once('@') {
            Some((repository, tag)) => (repository, Some(tag)),
            None => (spec, None),
        };
        if !tag.is_none_or(valid_tag) {
            return None;
        }
        let (base, path) = match prefix {
            "gitlab" | "codeberg" if !repository.contains("://") => {
                let base = if prefix == "gitlab" { GITLAB_COM } else { CODEBERG };
                (base.to_string(), repository)
            }
            "gitlab" | "forgejo" | "gitea" => split_address(repository)?,
            _ => return None,
        };
        let path = path.trim_end_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        let forge = match prefix {
            "gitlab" => Forge::GitLab { base },
            _ => Forge::Forgejo { base },
        };
        let segments: Vec<&str> = path.split('/').collect();
        let fits = match forge {
            Forge::GitLab { .. } => segments.len() >= 2,
            _ => segments.len() == 2,
        };
        if !fits || !segments.iter().all(|segment| valid_segment(segment)) {
            return None;
        }
        let repo = Repo { forge, path: path.to_string() };
        Some((repo, tag.map(str::to_string), pattern.map(str::to_string)))
    }

    /// The update source that follows this repository, as the desktop
    /// entry stores it.
    pub fn setting(&self, tag: Option<&str>, pattern: Option<&str>) -> String {
        let mut setting = match &self.forge {
            Forge::GitHub => format!("github:{}", self.path),
            Forge::GitLab { base } if base == GITLAB_COM => format!("gitlab:{}", self.path),
            Forge::GitLab { base } => format!("gitlab:{base}/{}", self.path),
            Forge::Forgejo { base } if base == CODEBERG => format!("codeberg:{}", self.path),
            Forge::Forgejo { base } => format!("forgejo:{base}/{}", self.path),
        };
        if let Some(tag) = tag {
            setting.push('@');
            setting.push_str(tag);
        }
        if let Some(pattern) = pattern {
            setting.push('#');
            setting.push_str(pattern);
        }
        setting
    }

    /// Whether this is the same repository as `other`, however either
    /// spells the case of its path.
    pub fn same_as(&self, other: &Repo) -> bool {
        self.forge == other.forge && self.path.eq_ignore_ascii_case(&other.path)
    }

    pub(super) fn api(&self) -> Api {
        match self.forge {
            Forge::GitHub => Api::GitHub,
            Forge::GitLab { .. } => Api::GitLab,
            Forge::Forgejo { .. } => Api::Forgejo,
        }
    }

    /// The first page of the releases, newest first.
    pub(super) fn listing_url(&self) -> String {
        match &self.forge {
            Forge::GitHub => format!(
                "{}/repos/{}/releases?per_page={RELEASES_PER_PAGE}",
                super::github_api(),
                self.path
            ),
            Forge::GitLab { base } => format!(
                "{}/releases?per_page={RELEASES_PER_PAGE}",
                gitlab_project(&api_base(base, "APPIMG_GITLAB_URL", GITLAB_COM), &self.path)
            ),
            Forge::Forgejo { base } => format!(
                "{}/api/v1/repos/{}/releases?limit={RELEASES_PER_PAGE}",
                api_base(base, "APPIMG_CODEBERG_URL", CODEBERG),
                self.path
            ),
        }
    }

    /// The release behind a tag. A GitHub tag goes into the URL as it is,
    /// as it always did; the others are encoded, a GitLab tag may hold a
    /// slash.
    pub(super) fn tag_url(&self, tag: &str) -> String {
        match &self.forge {
            Forge::GitHub => {
                format!("{}/repos/{}/releases/tags/{tag}", super::github_api(), self.path)
            }
            Forge::GitLab { base } => format!(
                "{}/releases/{}",
                gitlab_project(&api_base(base, "APPIMG_GITLAB_URL", GITLAB_COM), &self.path),
                encode(tag)
            ),
            Forge::Forgejo { base } => format!(
                "{}/api/v1/repos/{}/releases/tags/{}",
                api_base(base, "APPIMG_CODEBERG_URL", CODEBERG),
                self.path,
                encode(tag)
            ),
        }
    }

    /// The releases of a listing that an update follows without a tag,
    /// newest first: no drafts and no pre-releases. GitLab marks neither,
    /// so there a release is passed over when it lies in the future, or
    /// when its tag names a pre-release or a moving build; following one of
    /// those takes `@tag`.
    pub(super) fn published_releases(&self, listing: &str) -> Vec<Release> {
        json::array_objects(listing)
            .into_iter()
            .filter(|release| match self.forge {
                Forge::GitHub | Forge::Forgejo { .. } => {
                    json::bool_field(release, "draft") != Some(true)
                        && json::bool_field(release, "prerelease") != Some(true)
                }
                Forge::GitLab { .. } => {
                    json::bool_field(release, "upcoming_release") != Some(true)
                        && json::string_field(release, "tag_name").is_none_or(|tag| {
                            !version::is_prerelease(&tag) && !version::is_rolling(&tag)
                        })
                }
            })
            .map(|release| self.parse_release(release))
            .collect()
    }

    /// One release, as the forge's API answers for it.
    pub(super) fn parse_release(&self, body: &str) -> Release {
        match self.forge {
            Forge::GitHub => super::parse_release(body),
            Forge::Forgejo { .. } => Release {
                tag: json::string_field(body, "tag_name"),
                assets: json::array_field_objects(body, "assets")
                    .into_iter()
                    .filter_map(|asset| {
                        Some(Asset {
                            name: json::string_field(asset, "name")?,
                            url: json::string_field(asset, "browser_download_url")?,
                            sha256: None,
                        })
                    })
                    .collect(),
                published: json::string_field(body, "published_at")
                    .as_deref()
                    .and_then(date::from_timestamp),
                commit: json::string_field(body, "target_commitish")
                    .as_deref()
                    .and_then(version::short_commit),
            },
            Forge::GitLab { .. } => Release {
                tag: json::string_field(body, "tag_name"),
                assets: json::array_field_objects(body, "links")
                    .into_iter()
                    .filter_map(|link| {
                        let url = json::string_field(link, "url")?;
                        let name = gitlab_asset_name(json::string_field(link, "name"), &url);
                        Some(Asset { name, url, sha256: None })
                    })
                    .collect(),
                published: json::string_field(body, "released_at")
                    .as_deref()
                    .and_then(date::from_timestamp),
                commit: json::string_field(body, "short_id")
                    .as_deref()
                    .and_then(version::short_commit),
            },
        }
    }

    /// The SHA-256 the package registry of a GitLab project knows for a
    /// file of a generic package, by its download URL,
    /// `<base>/api/v4/projects/<id>/packages/generic/<name>/<version>/<file>`:
    /// two requests, one for the package and one for its files. `None` for
    /// another forge, another host, or a URL of any other kind.
    pub(super) fn package_checksum(&self, url: &str) -> Option<String> {
        let Forge::GitLab { base } = &self.forge else {
            return None;
        };
        let base = api_base(base, "APPIMG_GITLAB_URL", GITLAB_COM);
        let rest = url.strip_prefix(&format!("{base}/api/v4/projects/"))?;
        let rest = rest.split(['?', '#']).next().unwrap_or(rest);
        let [project, "packages", "generic", name, version, file] =
            rest.split('/').collect::<Vec<_>>()[..]
        else {
            return None;
        };
        let packages = download::api_text(
            &format!(
                "{base}/api/v4/projects/{project}/packages?package_type=generic&package_name={name}\
                 &package_version={version}"
            ),
            Api::GitLab,
        )
        .ok()?;
        let package = json::array_objects(&packages)
            .into_iter()
            .find_map(|package| json::number_field(package, "id"))?;
        let files = download::api_text(
            &format!(
                "{base}/api/v4/projects/{project}/packages/{package}/package_files?per_page=100"
            ),
            Api::GitLab,
        )
        .ok()?;
        let file = download::percent_decode(file);
        // A file uploaded again under the same name is listed again, and the
        // download serves the newest.
        json::array_objects(&files)
            .into_iter()
            .rev()
            .filter(|entry| json::string_field(entry, "file_name").as_deref() == Some(&file))
            .filter_map(|entry| json::string_field(entry, "file_sha256"))
            .find(|sha256| sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(|sha256| sha256.to_ascii_lowercase())
    }
}

/// The name a GitLab release link stands for. A link's name is whatever its
/// author typed: the file name, the file name with a version in it, or
/// "AppImage Daemon". One that reads like a file name is taken, otherwise
/// the end of the URL, unless that is no file name either, as a package
/// file's `.../download` is not.
fn gitlab_asset_name(name: Option<String>, url: &str) -> String {
    let looks_like_a_file = |text: &str| {
        !text.is_empty()
            && !text.contains(char::is_whitespace)
            && text.rsplit_once('.').is_some_and(|(stem, extension)| {
                !stem.is_empty() && extension.chars().any(|c| c.is_ascii_alphabetic())
            })
    };
    if let Some(name) = name.as_deref().filter(|name| looks_like_a_file(name)) {
        return name.to_string();
    }
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let segment = download::percent_decode(path.rsplit('/').next().unwrap_or(""));
    if looks_like_a_file(&segment) {
        return segment;
    }
    name.unwrap_or(segment)
}

/// `scheme://host[:port]` and the path behind it, out of the address of a
/// self-hosted repository. The scheme is http or https, and the host has no
/// user name in front: nothing a token could be sent along with by mistake.
fn split_address(address: &str) -> Option<(String, &str)> {
    let (scheme, rest) = address.split_once("://")?;
    if !matches!(scheme, "https" | "http") {
        return None;
    }
    let (authority, path) = rest.split_once('/')?;
    let host_ok = !authority.is_empty()
        && authority.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'));
    host_ok.then(|| (format!("{scheme}://{}", authority.to_ascii_lowercase()), path))
}

/// One segment of a repository path.
fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A tag, the way [`super::github_spec`] takes one.
fn valid_tag(tag: &str) -> bool {
    !tag.is_empty() && !tag.chars().any(|c| c.is_whitespace() || c.is_control() || "?#".contains(c))
}

/// Where the API of gitlab.com or codeberg.org is asked: the address the
/// source names, unless `variable` holds another. That exists for the
/// tests, which serve releases from a local server; a token is only ever
/// sent over https, see [`download::api_text`].
fn api_base(base: &str, variable: &str, public: &str) -> String {
    if base == public {
        if let Some(local) = std::env::var(variable).ok().filter(|value| !value.is_empty()) {
            return local.trim_end_matches('/').to_string();
        }
    }
    base.to_string()
}

/// The API address of a GitLab project, its path encoded the way GitLab
/// takes it in place of the project's number.
fn gitlab_project(base: &str, path: &str) -> String {
    format!("{base}/api/v4/projects/{}", path.replace('/', "%2F"))
}

/// Percent-encodes what goes into one segment of a URL path.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(value: &str) -> Option<(String, Option<String>, Option<String>)> {
        Repo::parse_source(value)
            .map(|(repo, tag, pattern)| (repo.setting(None, None), tag, pattern))
    }

    #[test]
    fn every_forge_reads_back_the_way_it_is_stored() {
        for setting in [
            "github:o/r",
            "github:o/r@continuous#App-*.AppImage",
            "gitlab:es-de/emulationstation-de",
            "gitlab:librewolf-community/browser/appimage@v151.0.3-1",
            "gitlab:https://invent.kde.org/multimedia/kdenlive#kdenlive-*.AppImage",
            "gitlab:http://127.0.0.1:8080/group/sub/project",
            "codeberg:tenacityteam/tenacity",
            "codeberg:naev/naev@nightly",
            "forgejo:https://git.example.org/o/r",
        ] {
            let (repo, tag, pattern) = Repo::parse_source(setting).unwrap();
            assert_eq!(repo.setting(tag.as_deref(), pattern.as_deref()), setting);
        }
    }

    #[test]
    fn the_public_hosts_are_stored_the_short_way() {
        assert_eq!(parse("gitlab:https://gitlab.com/o/r/").unwrap().0, "gitlab:o/r");
        assert_eq!(parse("forgejo:https://codeberg.org/o/r.git").unwrap().0, "codeberg:o/r");
        assert_eq!(
            parse("gitea:https://Git.Example.org/o/r").unwrap().0,
            "forgejo:https://git.example.org/o/r"
        );
    }

    #[test]
    fn what_names_no_repository_is_refused() {
        for value in [
            "gitlab:o",
            "gitlab:https://gitlab.com/o",
            "codeberg:o/r/extra",
            "codeberg:o",
            "forgejo:o/r",
            "forgejo:ftp://host/o/r",
            "forgejo:https://user@host/o/r",
            "gitlab:https:///o/r",
            "gitlab:o/../r",
            "gitlab:o/r@",
            "gitlab:o/r#",
            "gitlab:o/r#a/b",
            "codeberg:o/r@a b",
            "bitbucket:o/r",
            "o/r",
        ] {
            assert!(Repo::parse_source(value).is_none(), "{value:?}");
        }
    }

    #[test]
    fn each_forge_has_its_own_api() {
        let repo = |value: &str| Repo::parse_source(value).unwrap().0;
        assert_eq!(
            repo("gitlab:librewolf-community/browser/appimage").listing_url(),
            "https://gitlab.com/api/v4/projects/librewolf-community%2Fbrowser%2Fappimage/releases\
             ?per_page=30"
        );
        assert_eq!(
            repo("gitlab:https://invent.kde.org/g/p").tag_url("release/1.0"),
            "https://invent.kde.org/api/v4/projects/g%2Fp/releases/release%2F1.0"
        );
        assert_eq!(
            repo("codeberg:naev/naev").listing_url(),
            "https://codeberg.org/api/v1/repos/naev/naev/releases?limit=30"
        );
        assert_eq!(
            repo("forgejo:https://git.example.org/o/r").tag_url("v1.0"),
            "https://git.example.org/api/v1/repos/o/r/releases/tags/v1.0"
        );
    }

    /// What the releases of gitlab.com looked like, trimmed: the name of a
    /// link is what its author typed.
    #[test]
    fn a_gitlab_link_stands_for_the_file_it_names() {
        let name = |name: &str, url: &str| gitlab_asset_name(Some(name.to_string()), url);
        assert_eq!(
            name(
                "ES-DE_x64.AppImage",
                "https://gitlab.com/es-de/emulationstation-de/-/package_files/357718352/download"
            ),
            "ES-DE_x64.AppImage"
        );
        assert_eq!(
            name(
                "AppImage Daemon",
                "https://gitlab.com/api/v4/projects/30707566/packages/generic/coolercontrol/5.0.1/\
                 CoolerControlD-x86_64.AppImage"
            ),
            "CoolerControlD-x86_64.AppImage"
        );
        assert_eq!(
            name(
                "LibreWolf-151.0.3-1.x86_64.AppImage",
                "https://gitlab.com/api/v4/projects/24386000/packages/generic/librewolf/151.0.3-1/\
                 LibreWolf.x86_64.AppImage"
            ),
            "LibreWolf-151.0.3-1.x86_64.AppImage"
        );
        assert_eq!(name("Release notes", "https://example.com/notes"), "Release notes");
        assert_eq!(gitlab_asset_name(None, "https://e.com/My%20App.AppImage"), "My App.AppImage");
    }

    #[test]
    fn gitlab_passes_over_what_no_flag_marks() {
        let repo = Repo::parse_source("gitlab:o/r").unwrap().0;
        let listing = r#"[
            {"tag_name":"v3.0","upcoming_release":true,"released_at":"2027-01-01T00:00:00Z","assets":{"links":[]}},
            {"tag_name":"v2.1-rc1","upcoming_release":false,"assets":{"links":[]}},
            {"tag_name":"nightly","upcoming_release":false,"assets":{"links":[]}},
            {"tag_name":"v2.0","upcoming_release":false,"released_at":"2026-09-30T09:21:15.488Z",
             "commit":{"id":"bb03eaa1234567890","short_id":"bb03eaa1"},
             "assets":{"count":1,"sources":[{"format":"zip","url":"https://gitlab.com/o/r/-/archive/v2.0/r-v2.0.zip"}],
                       "links":[{"id":1,"name":"App-2.0-x86_64.AppImage","url":"https://gitlab.com/o/r/-/package_files/1/download","link_type":"other"}]},
             "_links":{"self":"https://gitlab.com/o/r/-/releases/v2.0"}}
        ]"#;
        let releases = repo.published_releases(listing);
        assert_eq!(releases.len(), 1);
        let release = &releases[0];
        assert_eq!(release.tag.as_deref(), Some("v2.0"));
        assert_eq!(release.published.as_deref(), Some("2026-09-30"));
        assert_eq!(release.commit.as_deref(), Some("bb03eaa"));
        let names: Vec<&str> = release.assets.iter().map(|asset| asset.name.as_str()).collect();
        // The source archives GitLab makes of every tag are no asset.
        assert_eq!(names, ["App-2.0-x86_64.AppImage"]);
    }

    #[test]
    fn forgejo_reads_like_github_without_digests() {
        let repo = Repo::parse_source("codeberg:tenacityteam/tenacity").unwrap().0;
        let listing = r#"[
            {"tag_name":"v1.4-alpha1","draft":false,"prerelease":true,"assets":[]},
            {"tag_name":"v1.3.5","draft":false,"prerelease":false,"published_at":"2026-07-06T00:11:00+02:00",
             "target_commitish":"1.3","author":{"login":"x","full_name":"X"},
             "assets":[{"id":1,"name":"tenacity-linux-1.3.5-x86_64.AppImage","size":53041656,
                        "browser_download_url":"https://codeberg.org/tenacityteam/tenacity/releases/download/v1.3.5/tenacity-linux-1.3.5-x86_64.AppImage","type":"attachment"},
                       {"id":2,"name":"tenacity-macos-1.3.5-Apple Silicon.dmg","size":1,
                        "browser_download_url":"https://codeberg.org/tenacityteam/tenacity/releases/download/v1.3.5/tenacity-macos-1.3.5-Apple%20Silicon.dmg","type":"attachment"}]}
        ]"#;
        let releases = repo.published_releases(listing);
        assert_eq!(releases.len(), 1);
        let release = &releases[0];
        assert_eq!(release.tag.as_deref(), Some("v1.3.5"));
        assert_eq!(release.published.as_deref(), Some("2026-07-06"));
        assert_eq!(release.commit, None);
        assert_eq!(release.assets[1].name, "tenacity-macos-1.3.5-Apple Silicon.dmg");
        assert!(release.assets.iter().all(|asset| asset.sha256.is_none()));
    }
}
