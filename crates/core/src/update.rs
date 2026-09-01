use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::desktop_entry::{self, DesktopEntry};
use crate::download::{self, ProgressFn};
use crate::error::{Error, Result};
use crate::fs_util::{self, human_size, MODE_EXEC};
use crate::list::InstalledApp;
use crate::metadata;
use crate::paths::Paths;
use crate::{caches, date, icon, json, version, zsync};

const GITHUB_API: &str = "https://api.github.com";

/// How an application can be updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateSource {
    /// `X-AppImg-UpdateInfo` pointing at a zsync file. The check reads that
    /// file's header directly, applying the delta needs `appimageupdatetool`.
    Zsync {
        update_info: String,
    },
    /// A GitHub release, queried through the API. `tag` is set only for a
    /// tag that keeps moving, see [`tag_to_follow`]; without one the latest
    /// release is asked for.
    GitHubRelease {
        owner: String,
        repo: String,
        tag: Option<String>,
        asset: Option<String>,
    },
    /// Plain re-download of the stored URL.
    DirectUrl {
        url: String,
    },
    /// Re-copy from the local file it was installed from.
    LocalFile {
        path: PathBuf,
    },
    None,
}

impl UpdateSource {
    pub fn describe(&self) -> String {
        match self {
            UpdateSource::Zsync { .. } => "zsync".to_string(),
            UpdateSource::GitHubRelease { owner, repo, .. } => format!("github:{owner}/{repo}"),
            UpdateSource::DirectUrl { .. } => "url".to_string(),
            UpdateSource::LocalFile { .. } => "file".to_string(),
            UpdateSource::None => "none".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct UpdateStatus {
    pub slug: String,
    pub name: String,
    pub current_version: Option<String>,
    pub latest_version: Option<String>,
    pub available: bool,
    pub source: UpdateSource,
    /// Why nothing can be said, when that is the case.
    pub note: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UpdateOutcome {
    pub slug: String,
    pub from_version: Option<String>,
    pub to_version: Option<String>,
    pub appimage_path: PathBuf,
    pub backup_path: Option<PathBuf>,
    pub icons: Vec<PathBuf>,
    pub source: UpdateSource,
}

/// Works out how an application would be updated, without changing anything.
pub fn source_for(app: &InstalledApp) -> UpdateSource {
    if let Some(info) = app.update_info.as_deref() {
        if let Some(source) = source_from_update_info(info) {
            return source;
        }
    }
    match app.origin.as_deref() {
        Some(origin) if download::is_url(origin) => github_source_from_url(origin)
            .unwrap_or_else(|| UpdateSource::DirectUrl { url: origin.to_string() }),
        Some(origin) => {
            let path = PathBuf::from(origin);
            if path.is_file() {
                UpdateSource::LocalFile { path }
            } else {
                UpdateSource::None
            }
        }
        None => UpdateSource::None,
    }
}

/// Reports whether an update is available. Never writes anything.
pub fn check(app: &InstalledApp) -> Result<UpdateStatus> {
    let source = source_for(app);
    let current = app.version.clone();

    let mut status = UpdateStatus {
        slug: app.slug.clone(),
        name: app.name.clone(),
        current_version: current.clone(),
        latest_version: None,
        available: false,
        source: source.clone(),
        note: None,
    };

    match &source {
        UpdateSource::None => {
            status.note = Some("no update source recorded".to_string());
        }
        UpdateSource::Zsync { update_info } => {
            let url =
                zsync_url(update_info).ok_or_else(|| Error::NoUpdateInfo(app.slug.clone()))?;
            let header = zsync::fetch_header(&url)?;
            status.latest_version = offered_by_zsync(&header).or_else(|| current.clone());
            let (available, note) = zsync_compare(&header, &app.appimage_path)?;
            status.available = available;
            status.note = note;
        }
        UpdateSource::GitHubRelease { owner, repo, tag, asset } => {
            let release = fetch_release(owner, repo, tag.as_deref())?;
            status.latest_version = release.version();
            let (installed, available, note) = compare_release(current.as_deref(), &release);
            status.current_version = installed;
            status.available = available;
            status.note = note;
            if release.asset_url(asset.as_deref()).is_none() {
                status.available = false;
                status.note = Some("the latest release has no matching AppImage asset".to_string());
            }
        }
        UpdateSource::DirectUrl { .. } => {
            status.note =
                Some("the source URL carries no version, updating re-downloads it".to_string());
        }
        UpdateSource::LocalFile { path } => {
            let latest = version::extract(&path.file_name().unwrap_or_default().to_string_lossy());
            status.latest_version = latest.clone();
            status.available = match (&current, &latest) {
                (Some(current), Some(latest)) => version::is_newer(latest, current),
                _ => false,
            };
        }
    }
    Ok(status)
}

/// Every name an update can leave next to `<slug>.AppImage`, whoever wrote
/// it. `.new` is appimg's own staging file and `.bak` its backup of the
/// previous version; `.zs-old` and `.part` come from the zsync client inside
/// `appimageupdatetool`, which hard-links the previous version out of the
/// way before the swap and downloads into a partial file. All four are named
/// after the AppImage, so a file that carries one of these suffixes and a
/// managed slug is provably ours.
pub const LEFTOVER_SUFFIXES: &[&str] =
    &["AppImage.bak", "AppImage.new", "AppImage.zs-old", "AppImage.part"];

/// What `appimageupdatetool` calls the copy of the version it replaced. It
/// never deletes it, so a delta update otherwise leaves a second full
/// AppImage behind.
const ZSYNC_BACKUP: &str = "AppImage.zs-old";

/// The leftovers of `slug` that exist right now, in the order of
/// [`LEFTOVER_SUFFIXES`].
pub fn leftovers(paths: &Paths, slug: &str) -> Vec<PathBuf> {
    LEFTOVER_SUFFIXES
        .iter()
        .map(|suffix| paths.appimage_dir.join(format!("{slug}.{suffix}")))
        .filter(|path| path.is_file())
        .collect()
}

/// Updates one application in place. The previous binary stays as `.bak`
/// until [`confirm`] removes it, so a broken update can be rolled back.
pub fn update(
    paths: &Paths,
    app: &InstalledApp,
    progress: Option<ProgressFn<'_>>,
) -> Result<UpdateOutcome> {
    let source = source_for(app);
    let target = paths.appimage_path(&app.slug);

    match &source {
        UpdateSource::None => Err(Error::NoUpdateSource(app.slug.clone())),
        UpdateSource::Zsync { .. } => {
            zsync_update(&target)?;
            let backup = claim_zsync_backup(paths, &app.slug);
            finish(paths, app, &target, backup, source, None)
        }
        UpdateSource::GitHubRelease { owner, repo, tag, asset } => {
            let release = fetch_release(owner, repo, tag.as_deref())?;
            let url = release
                .asset_url(asset.as_deref())
                .ok_or_else(|| Error::NoUpdateInfo(app.slug.clone()))?;
            let recorded = release.recorded_version();
            let staged = download_staged(paths, &app.slug, &url, progress)?;
            let backup = swap_in(&staged, &target)?;
            finish(paths, app, &target, Some(backup), source, recorded)
        }
        UpdateSource::DirectUrl { url } => {
            let staged = download_staged(paths, &app.slug, url, progress)?;
            let backup = swap_in(&staged, &target)?;
            finish(paths, app, &target, Some(backup), source, None)
        }
        UpdateSource::LocalFile { path } => {
            let staged = paths.appimage_dir.join(format!("{}.AppImage.new", app.slug));
            fs_util::copy_atomic(path, &staged, MODE_EXEC)?;
            let backup = swap_in(&staged, &target)?;
            finish(paths, app, &target, Some(backup), source, None)
        }
    }
}

/// Drops the backup of a successful update, and with it anything else the
/// update left next to the AppImage. Once the new binary is confirmed, none
/// of it is worth the disk it sits on.
pub fn confirm(paths: &Paths, slug: &str) -> Result<()> {
    for file in leftovers(paths, slug) {
        match fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(&file, e)),
        }
    }
    Ok(())
}

/// Puts the previous version back.
pub fn rollback(paths: &Paths, slug: &str) -> Result<()> {
    let backup = backup_path(paths, slug);
    if !backup.is_file() {
        return Err(Error::NotFound(backup));
    }
    let target = paths.appimage_path(slug);
    fs::rename(&backup, &target).map_err(|e| Error::io(&target, e))?;
    fs_util::set_mode(&target, MODE_EXEC)
}

pub fn backup_path(paths: &Paths, slug: &str) -> PathBuf {
    paths.appimage_dir.join(format!("{slug}.AppImage.bak"))
}

/// Renames the copy `appimageupdatetool` left behind to the `.bak` every
/// other source uses, so one rollback and one cleanup cover them all.
/// Returns `None` when the tool wrote no backup, which is what an update
/// that had nothing to apply looks like.
fn claim_zsync_backup(paths: &Paths, slug: &str) -> Option<PathBuf> {
    let left_behind = paths.appimage_dir.join(format!("{slug}.{ZSYNC_BACKUP}"));
    if !left_behind.is_file() {
        return None;
    }
    let backup = backup_path(paths, slug);
    // A rename inside one directory either works or leaves both files where
    // they were, and doctor reports whatever stays.
    fs::rename(&left_behind, &backup).ok()?;
    Some(backup)
}

/// Re-reads metadata and icons from the new binary and refreshes only the
/// technical keys. Name, categories and launch arguments stay as they are,
/// they may well have been edited by hand.
///
/// `from_release` is the version the source knows and the file does not: a
/// rolling build declares a build number and a commit, while the release it
/// came out of knows the day it was published.
fn finish(
    paths: &Paths,
    app: &InstalledApp,
    target: &Path,
    backup: Option<PathBuf>,
    source: UpdateSource,
    from_release: Option<String>,
) -> Result<UpdateOutcome> {
    let info = metadata::inspect(target, None).ok();

    let icons = match info.as_ref().and_then(|info| info.extract_root().map(Path::to_path_buf)) {
        Some(root) => {
            for stale in fs_util::find_files_with_stem(&paths.icons_root, &app.slug)? {
                let _ = fs::remove_file(stale);
            }
            let icon_name = info.as_ref().and_then(|info| info.icon_name.clone());
            icon::install_icons(&root, icon_name.as_deref(), &app.slug, &paths.icons_root)
        }
        None => Vec::new(),
    };

    let new_version = match info.as_ref().and_then(|info| info.version.clone()) {
        // A build id says nothing about age. Recording the date of the
        // release it was downloaded from is what lets the next check tell
        // whether this file has fallen behind.
        Some(declared) if version::is_rolling(&declared) => from_release.or(Some(declared)),
        Some(declared) => Some(declared),
        None => from_release.or_else(|| {
            version::extract(&target.file_name().unwrap_or_default().to_string_lossy())
        }),
    };

    let mut entry = DesktopEntry::read(&app.desktop_entry_path)?;
    entry.set_optional(desktop_entry::KEY_VERSION, new_version.clone());
    if let Some(update_info) = info.as_ref().and_then(|info| info.update_info.clone()) {
        entry.set(desktop_entry::KEY_UPDATE_INFO, update_info);
    }
    if !icons.is_empty() {
        entry.set("Icon", app.slug.clone());
    }
    entry.write(&app.desktop_entry_path)?;

    caches::refresh(paths);

    Ok(UpdateOutcome {
        slug: app.slug.clone(),
        from_version: app.version.clone(),
        to_version: new_version,
        appimage_path: target.to_path_buf(),
        backup_path: backup,
        icons,
        source,
    })
}

fn download_staged(
    paths: &Paths,
    slug: &str,
    url: &str,
    progress: Option<ProgressFn<'_>>,
) -> Result<PathBuf> {
    let staged = paths.appimage_dir.join(format!("{slug}.AppImage.new"));
    download::to_file(url, &staged, progress)?;

    if fs_util::file_size(&staged).unwrap_or(0) == 0 {
        let _ = fs::remove_file(&staged);
        return Err(Error::Download(format!("{url}: the downloaded file is empty")));
    }
    fs_util::set_mode(&staged, MODE_EXEC)?;
    Ok(staged)
}

/// Moves the new binary into place and keeps the old one as `.bak`. A failure
/// while swapping restores the previous state.
fn swap_in(staged: &Path, target: &Path) -> Result<PathBuf> {
    let backup = target.with_extension("AppImage.bak");

    if target.exists() {
        fs::rename(target, &backup).map_err(|e| Error::io(target, e))?;
    }
    if let Err(e) = fs::rename(staged, target) {
        if backup.exists() {
            let _ = fs::rename(&backup, target);
        }
        return Err(Error::io(target, e));
    }
    fs_util::set_mode(target, MODE_EXEC)?;
    Ok(backup)
}

fn source_from_update_info(info: &str) -> Option<UpdateSource> {
    let parts: Vec<&str> = info.split('|').collect();
    match parts.first().copied() {
        // gh-releases-zsync|owner|repo|tag|pattern
        Some("gh-releases-zsync") if parts.len() >= 5 => Some(UpdateSource::GitHubRelease {
            owner: parts[1].to_string(),
            repo: parts[2].to_string(),
            tag: tag_to_follow(parts[3]),
            asset: Some(parts[4].to_string()),
        }),
        Some("zsync") if parts.len() >= 2 => {
            Some(UpdateSource::Zsync { update_info: info.to_string() })
        }
        _ => None,
    }
}

fn github_source_from_url(url: &str) -> Option<UpdateSource> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let mut parts = rest.split('/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    if parts.next()? != "releases" {
        return None;
    }
    // .../releases/download/<tag>/<asset>
    let tag = match parts.next() {
        Some("download") => parts.next().and_then(tag_to_follow),
        _ => None,
    };
    let asset = rest.rsplit('/').next().map(str::to_string);
    Some(UpdateSource::GitHubRelease { owner, repo, tag, asset })
}

/// Which tag an update should follow, out of the one the application was
/// installed from. A moving tag like `continuous` is the whole point of the
/// channel and is followed as it is written, because the newest build only
/// ever appears under it. A tag that names a version was a snapshot, and
/// following that would pin the application to the version it was installed
/// at, so the latest release is asked for instead. `latest` is GitHub's own
/// word for exactly that, and no tag of that name has to exist.
fn tag_to_follow(tag: &str) -> Option<String> {
    let tag = tag.trim();
    (version::is_rolling(tag) && !tag.eq_ignore_ascii_case("latest")).then(|| tag.to_string())
}

struct Release {
    tag: Option<String>,
    assets: Vec<String>,
    /// The day it was published, `2025-10-18`.
    published: Option<String>,
    /// The commit it was built from, abbreviated.
    commit: Option<String>,
}

impl Release {
    /// Whether this release is a moving one rather than a cut version.
    fn is_rolling(&self) -> bool {
        self.tag.as_deref().is_some_and(version::is_rolling)
    }

    /// What to show as the version of this release. A tag that names a
    /// version is that version, as it always was. A rolling tag names none,
    /// and reading one out of an asset called `x86_64` would be a guess, so
    /// the day the release was published stands in for it, and the commit
    /// when there is not even a date.
    fn version(&self) -> Option<String> {
        if self.is_rolling() {
            return self.published.clone().or_else(|| self.commit.clone());
        }
        self.tag
            .as_deref()
            .and_then(version::extract)
            .or_else(|| self.assets.first().and_then(|url| version::extract(url)))
    }

    /// The version to record for a file downloaded out of this release,
    /// when the file itself will not carry one.
    fn recorded_version(&self) -> Option<String> {
        self.is_rolling().then(|| self.published.clone()).flatten()
    }

    /// Picks the asset that looks most like the one that was installed.
    fn asset_url(&self, hint: Option<&str>) -> Option<String> {
        let appimages: Vec<&String> =
            self.assets.iter().filter(|url| url.to_lowercase().ends_with(".appimage")).collect();

        if appimages.is_empty() {
            return None;
        }
        if let Some(hint) = hint {
            let signature = asset_signature(hint);
            if let Some(best) = appimages.iter().find(|url| asset_signature(url) == signature) {
                return Some((*best).clone());
            }
        }
        // Prefer the architecture we are running on.
        let arch = std::env::consts::ARCH;
        appimages
            .iter()
            .find(|url| url.contains(arch))
            .or_else(|| appimages.first())
            .map(|url| (*url).clone())
    }
}

/// An asset name with all digits stripped, so `App-1.2.3-x86_64.AppImage` and
/// `App-1.3.0-x86_64.AppImage` match while an `arm64` build does not.
fn asset_signature(name: &str) -> String {
    let file = name.rsplit('/').next().unwrap_or(name).to_lowercase();
    let mut out = String::with_capacity(file.len());
    let mut in_separator = false;

    for c in file.chars() {
        if c.is_ascii_digit() {
            continue;
        }
        if c.is_alphanumeric() {
            out.push(c);
            in_separator = false;
        } else if !in_separator {
            out.push('-');
            in_separator = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Reads a release: the one behind a moving tag, or the latest one when no
/// tag is followed.
fn fetch_release(owner: &str, repo: &str, tag: Option<&str>) -> Result<Release> {
    let url = match tag {
        Some(tag) => format!("{GITHUB_API}/repos/{owner}/{repo}/releases/tags/{tag}"),
        None => format!("{GITHUB_API}/repos/{owner}/{repo}/releases/latest"),
    };
    let body = download::to_string(&url)?;
    Ok(Release {
        tag: json::string_field(&body, "tag_name"),
        assets: json::string_fields(&body, "browser_download_url"),
        published: json::string_field(&body, "published_at")
            .as_deref()
            .and_then(date::from_timestamp),
        // A continuous release points its tag at the commit it was built
        // from, which is the same commit the builds themselves name.
        commit: json::string_field(&body, "target_commitish")
            .as_deref()
            .and_then(version::short_commit),
    })
}

/// Whether `release` is newer than what is installed, and what to show as
/// the installed version while saying so.
///
/// A rolling release names no version, and neither does a build out of one.
/// What both carry is the commit: on a channel that only ever moves forward
/// the same commit is the same build, and a different one supersedes it. An
/// installed file that already carries a date is compared as a date, which
/// orders, so it says whether the file is the older one and not merely a
/// different one. Anything that names a version is compared exactly as it
/// was before.
fn compare_release(
    current: Option<&str>,
    release: &Release,
) -> (Option<String>, bool, Option<String>) {
    let latest = release.version();

    if release.is_rolling() {
        let installed = current.and_then(version::short_commit);
        if let (Some(installed), Some(offered)) = (installed, release.commit.as_deref()) {
            if installed != offered {
                return (Some(installed), true, None);
            }
            // Same commit, same build: the installed file is that release,
            // so it carries the day that release was published.
            return (release.published.clone().or(Some(installed)), false, None);
        }
    }

    match (current, latest.as_deref()) {
        (Some(current), Some(latest)) if version::comparable(current, latest) => {
            (Some(current.to_string()), version::is_newer(latest, current), None)
        }
        (Some(current), Some(_)) => (
            Some(current.to_string()),
            false,
            Some(
                "the installed build carries no version to compare with the offered one"
                    .to_string(),
            ),
        ),
        (None, Some(_)) => (None, true, None),
        (current, None) => (current.map(str::to_string), false, None),
    }
}

/// What the header of a zsync file says the offered version is. The name of
/// the complete file carries one when the project ships versions at all; a
/// continuous build does not, and reading a version out of `x86_64` would
/// be a guess, so the day the file was built stands in for it.
fn offered_by_zsync(header: &zsync::Header) -> Option<String> {
    header
        .filename
        .as_deref()
        .filter(|name| version::names_a_version(name))
        .and_then(version::extract)
        .or_else(|| header.mtime.as_deref().and_then(date::from_http_date))
}

/// The zsync URL out of an `X-AppImg-UpdateInfo` of the form
/// `zsync|<url>`.
fn zsync_url(update_info: &str) -> Option<String> {
    let url = update_info.split('|').nth(1)?.trim();
    download::is_url(url).then(|| url.to_string())
}

/// Whether the local file still is the one a zsync header describes, and what
/// to say about it. The length settles most cases on its own; the checksum
/// only has to be computed when the two files are the same size.
fn zsync_compare(header: &zsync::Header, appimage: &Path) -> Result<(bool, Option<String>)> {
    let local = fs_util::file_size(appimage).ok_or_else(|| Error::NotFound(appimage.into()))?;

    if local != header.length {
        return Ok((
            true,
            Some(format!(
                "the offered file is {}, the installed one {}",
                human_size(header.length),
                human_size(local)
            )),
        ));
    }
    let Some(remote) = header.sha1.as_deref() else {
        return Ok((
            false,
            Some("the zsync file has no checksum, only the sizes match".to_string()),
        ));
    };
    if zsync::sha1_file(appimage)? == remote {
        Ok((false, None))
    } else {
        Ok((true, Some("same size as the installed file, different checksum".to_string())))
    }
}

/// Applies the delta. This is the one step that still needs the external
/// tool, and saying so is more use than silently doing nothing.
fn zsync_update(appimage: &Path) -> Result<()> {
    let Some(tool) = fs_util::which("appimageupdatetool") else {
        return Err(Error::MissingTool {
            tool: "appimageupdatetool".to_string(),
            purpose: "it applies the zsync delta an update of this application needs".to_string(),
        });
    };
    let status = Command::new(tool)
        .arg("--overwrite")
        .arg(appimage)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| Error::io(appimage, e))?;

    if !status.success() {
        return Err(Error::Download(format!(
            "appimageupdatetool could not update {}",
            appimage.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_github_coordinates_from_update_info() {
        let source = source_from_update_info(
            "gh-releases-zsync|owner|repo|latest|App-*x86_64.AppImage.zsync",
        );
        assert_eq!(
            source,
            Some(UpdateSource::GitHubRelease {
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                tag: None,
                asset: Some("App-*x86_64.AppImage.zsync".to_string()),
            })
        );
    }

    #[test]
    fn only_a_moving_tag_is_followed() {
        // The continuous build only ever appears under its own tag, and
        // the latest release is not it.
        assert_eq!(tag_to_follow("continuous").as_deref(), Some("continuous"));
        assert_eq!(tag_to_follow("nightly").as_deref(), Some("nightly"));
        // Following a version tag would pin the application to the version
        // it was installed at, forever.
        assert_eq!(tag_to_follow("v1.2.3"), None);
        assert_eq!(tag_to_follow("2.0.0-alpha-1-20251018"), None);
        // GitHub's own word for the endpoint, not a tag that has to exist.
        assert_eq!(tag_to_follow("latest"), None);

        let source = source_from_update_info(
            "gh-releases-zsync|AppImage|AppImageUpdate|continuous|AppImageUpdate-*x86_64.AppImage.zsync",
        );
        assert_eq!(
            source,
            Some(UpdateSource::GitHubRelease {
                owner: "AppImage".to_string(),
                repo: "AppImageUpdate".to_string(),
                tag: Some("continuous".to_string()),
                asset: Some("AppImageUpdate-*x86_64.AppImage.zsync".to_string()),
            })
        );
    }

    #[test]
    fn plain_zsync_update_info_carries_the_url_of_the_zsync_file() {
        let info = "zsync|https://example.com/App.AppImage.zsync";
        let source = source_from_update_info(info);
        assert!(matches!(source, Some(UpdateSource::Zsync { .. })));
        assert_eq!(zsync_url(info).as_deref(), Some("https://example.com/App.AppImage.zsync"));
        assert_eq!(source_from_update_info("nonsense"), None);
        // Nothing that could be fetched, so nothing to check against.
        assert_eq!(zsync_url("zsync|App.AppImage.zsync"), None);
        assert_eq!(zsync_url("zsync"), None);
    }

    #[test]
    fn a_different_length_is_an_update_and_names_both_sizes() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), vec![0u8; 2048]).unwrap();
        let header = zsync::Header {
            filename: Some("App-2.0.0.AppImage".to_string()),
            length: 4096,
            sha1: Some("0".repeat(40)),
            url: None,
            mtime: None,
        };

        let (available, note) = zsync_compare(&header, file.path()).unwrap();
        assert!(available);
        let note = note.unwrap();
        assert!(note.contains("4.0 KB"), "{note}");
        assert!(note.contains("2.0 KB"), "{note}");
    }

    #[test]
    fn the_same_length_is_decided_by_the_checksum() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"abc").unwrap();
        let mut header = zsync::Header {
            filename: None,
            length: 3,
            sha1: Some("a9993e364706816aba3e25717850c26c9cd0d89d".to_string()),
            url: None,
            mtime: None,
        };

        assert_eq!(zsync_compare(&header, file.path()).unwrap(), (false, None));

        header.sha1 = Some("0".repeat(40));
        let (available, note) = zsync_compare(&header, file.path()).unwrap();
        assert!(available);
        assert!(note.unwrap().contains("checksum"));

        // Without a checksum the sizes are all there is, and the check says so.
        header.sha1 = None;
        let (available, note) = zsync_compare(&header, file.path()).unwrap();
        assert!(!available);
        assert!(note.is_some());
    }

    /// A `Paths` whose AppImage directory is a temporary one, for the two
    /// tests that only look at files next to the AppImage.
    fn sandbox_paths(dir: &Path) -> Paths {
        Paths {
            appimage_dir: dir.to_path_buf(),
            applications_dir: dir.join("applications"),
            icons_root: dir.join("icons"),
            config_home: dir.join("config"),
            data_home: dir.to_path_buf(),
        }
    }

    #[test]
    fn the_copy_appimageupdatetool_leaves_becomes_the_backup() {
        let dir = tempfile::tempdir().unwrap();
        let paths = sandbox_paths(dir.path());
        let left_behind = dir.path().join("krita.AppImage.zs-old");
        fs::write(&left_behind, "the previous 371 MB").unwrap();

        let backup = claim_zsync_backup(&paths, "krita").unwrap();
        assert_eq!(backup, backup_path(&paths, "krita"));
        assert!(!left_behind.exists());
        assert_eq!(fs::read_to_string(&backup).unwrap(), "the previous 371 MB");

        // Confirming the update takes it away, so nothing of the previous
        // version is left on disk.
        confirm(&paths, "krita").unwrap();
        assert!(leftovers(&paths, "krita").is_empty());

        // An update that had nothing to apply writes no backup.
        assert_eq!(claim_zsync_backup(&paths, "krita"), None);
    }

    #[test]
    fn leftovers_cover_every_name_an_update_can_leave() {
        let dir = tempfile::tempdir().unwrap();
        let paths = sandbox_paths(dir.path());
        for suffix in LEFTOVER_SUFFIXES {
            fs::write(dir.path().join(format!("krita.{suffix}")), "x").unwrap();
        }
        // Neither the AppImage itself nor another application's leftovers.
        fs::write(dir.path().join("krita.AppImage"), "x").unwrap();
        fs::write(dir.path().join("osu.AppImage.zs-old"), "x").unwrap();

        let found = leftovers(&paths, "krita");
        assert_eq!(found.len(), LEFTOVER_SUFFIXES.len());
        assert!(found.iter().all(|path| path.file_name().unwrap() != "krita.AppImage"));
    }

    #[test]
    fn reads_github_coordinates_from_a_release_url() {
        let source = github_source_from_url(
            "https://github.com/owner/repo/releases/download/v1.0/App.AppImage",
        );
        assert_eq!(
            source,
            Some(UpdateSource::GitHubRelease {
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                tag: None,
                asset: Some("App.AppImage".to_string()),
            })
        );
        assert_eq!(github_source_from_url("https://example.com/App.AppImage"), None);

        // A download out of a continuous release keeps following it.
        let source = github_source_from_url(
            "https://github.com/AppImage/AppImageUpdate/releases/download/continuous/appimageupdatetool-x86_64.AppImage",
        );
        assert_eq!(
            source,
            Some(UpdateSource::GitHubRelease {
                owner: "AppImage".to_string(),
                repo: "AppImageUpdate".to_string(),
                tag: Some("continuous".to_string()),
                asset: Some("appimageupdatetool-x86_64.AppImage".to_string()),
            })
        );
    }

    #[test]
    fn picks_the_asset_that_matches_the_installed_one() {
        let release = Release {
            tag: Some("v2.0.0".to_string()),
            assets: vec![
                "https://example.com/App-2.0.0-arm64.AppImage".to_string(),
                "https://example.com/App-2.0.0-x86_64.AppImage".to_string(),
                "https://example.com/App-2.0.0.tar.gz".to_string(),
            ],
            published: Some("2025-10-18".to_string()),
            commit: None,
        };

        assert_eq!(release.version().as_deref(), Some("2.0.0"));
        assert_eq!(
            release.asset_url(Some("App-1.0.0-x86_64.AppImage")).as_deref(),
            Some("https://example.com/App-2.0.0-x86_64.AppImage")
        );
    }

    #[test]
    fn a_release_without_appimages_offers_nothing() {
        let release = Release {
            tag: Some("v1".to_string()),
            assets: vec!["https://x/App.tar.gz".to_string()],
            published: None,
            commit: None,
        };
        assert_eq!(release.asset_url(None), None);
    }

    /// The AppImageUpdate continuous release as the API returns it, and the
    /// two builds out of it that started this.
    fn continuous_release() -> Release {
        Release {
            tag: Some("continuous".to_string()),
            assets: vec!["https://x/AppImageUpdate-x86_64.AppImage".to_string()],
            published: Some("2025-10-18".to_string()),
            commit: Some("a211784".to_string()),
        }
    }

    #[test]
    fn a_continuous_release_is_dated_instead_of_guessed_at() {
        let release = continuous_release();
        // Not the `64` of `x86_64`, which is what reading a version out of
        // the asset name yields.
        assert_eq!(release.version().as_deref(), Some("2025-10-18"));
        assert_eq!(release.recorded_version().as_deref(), Some("2025-10-18"));

        // Without a date the commit is what is left.
        let undated = Release { published: None, ..continuous_release() };
        assert_eq!(undated.version().as_deref(), Some("a211784"));

        // A release that names a version keeps naming it.
        let tagged = Release { tag: Some("v2.0.0".to_string()), ..continuous_release() };
        assert_eq!(tagged.version().as_deref(), Some("2.0.0"));
        assert_eq!(tagged.recorded_version(), None);
    }

    #[test]
    fn the_same_commit_is_the_same_build() {
        // The two AppImages of the release differ in their build number and
        // in nothing else, so both are up to date and both are shown as the
        // day the release was published.
        for installed in ["255-a211784", "254-a211784", "a211784"] {
            let (current, available, note) =
                compare_release(Some(installed), &continuous_release());
            assert_eq!(current.as_deref(), Some("2025-10-18"), "{installed}");
            assert!(!available, "{installed}");
            assert_eq!(note, None);
        }
    }

    #[test]
    fn another_commit_on_the_channel_is_an_update() {
        let (current, available, _) = compare_release(Some("255-b0b0b0b"), &continuous_release());
        assert_eq!(current.as_deref(), Some("b0b0b0b"));
        assert!(available);
    }

    #[test]
    fn two_dates_say_which_build_is_older() {
        // What the file was recorded as after its last update, against what
        // the channel offers now.
        let (current, available, note) = compare_release(Some("2025-09-01"), &continuous_release());
        assert_eq!(current.as_deref(), Some("2025-09-01"));
        assert!(available, "an older date is an update");
        assert_eq!(note, None);

        let newer = Release { published: Some("2025-09-01".to_string()), ..continuous_release() };
        let (_, available, _) = compare_release(Some("2025-10-18"), &newer);
        assert!(!available, "a newer date is not an update");

        let (_, available, _) = compare_release(Some("2025-10-18"), &continuous_release());
        assert!(!available, "the same date is not an update");
    }

    #[test]
    fn a_version_release_is_still_compared_as_a_version() {
        let release = Release {
            tag: Some("v2.0.0".to_string()),
            assets: vec!["https://x/App-2.0.0-x86_64.AppImage".to_string()],
            published: Some("2025-10-18".to_string()),
            commit: Some("a211784".to_string()),
        };
        let (current, available, note) = compare_release(Some("1.9.0"), &release);
        assert_eq!(current.as_deref(), Some("1.9.0"));
        assert!(available);
        assert_eq!(note, None);

        let (_, available, _) = compare_release(Some("2.0.0"), &release);
        assert!(!available);

        // Nothing installed is an update, as it always was.
        assert!(compare_release(None, &release).1);
    }

    #[test]
    fn a_build_id_against_a_version_is_not_guessed_at() {
        // The build was installed off the continuous channel and the source
        // now offers a numbered release: neither is older than the other,
        // and saying so beats ordering `a211784` against `2.0.0`.
        let release = Release {
            tag: Some("v2.0.0".to_string()),
            assets: vec!["https://x/App-2.0.0-x86_64.AppImage".to_string()],
            published: Some("2025-10-18".to_string()),
            commit: None,
        };
        let (current, available, note) = compare_release(Some("a211784"), &release);
        assert_eq!(current.as_deref(), Some("a211784"));
        assert!(!available);
        assert!(note.unwrap().contains("no version"));
    }

    #[test]
    fn a_zsync_header_without_a_version_falls_back_to_its_date() {
        let mut header = zsync::Header {
            filename: Some("appimageupdatetool-x86_64.AppImage".to_string()),
            length: 4096,
            sha1: None,
            url: None,
            mtime: Some("Sat, 18 Oct 2025 19:39:31 +0000".to_string()),
        };
        // Not the `64` of `x86_64`.
        assert_eq!(offered_by_zsync(&header).as_deref(), Some("2025-10-18"));

        // A name that carries a version still names it.
        header.filename = Some("App-2.0.0-x86_64.AppImage".to_string());
        assert_eq!(offered_by_zsync(&header).as_deref(), Some("2.0.0"));

        // Neither a version nor a date: nothing is made up.
        header.filename = Some("appimageupdatetool-x86_64.AppImage".to_string());
        header.mtime = None;
        assert_eq!(offered_by_zsync(&header), None);
    }
}
