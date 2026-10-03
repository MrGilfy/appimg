use std::fs;
use std::path::{Path, PathBuf};

use crate::desktop_entry::{self, DesktopEntry};
use crate::digest::{self, Published, Verified};
use crate::download::{self, ProgressFn};
use crate::error::{Error, Result};
use crate::fs_util::{self, human_size, MODE_EXEC};
use crate::list::InstalledApp;
use crate::metadata;
use crate::paths::Paths;
use crate::{caches, date, icon, json, version, zsync};

const GITHUB_API: &str = "https://api.github.com";

/// Where the GitHub API is asked. `APPIMG_GITHUB_API` exists for the tests,
/// which serve release JSON from a local server; a token never goes there,
/// see [`download::to_string`].
fn github_api() -> String {
    std::env::var("APPIMG_GITHUB_API")
        .ok()
        .filter(|base| !base.is_empty())
        .unwrap_or_else(|| GITHUB_API.to_string())
}

/// The update source of an application that has none, as the desktop entry
/// stores it and as a status shows it.
pub const MANUAL: &str = "manual";

/// How an application can be updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateSource {
    /// `X-AppImg-UpdateInfo` pointing at a zsync file. The check reads that
    /// file's header directly, and appimg applies the delta itself; a full
    /// download of the file it describes is the fallback when that fails.
    Zsync { update_info: String },
    /// A GitHub release, queried through the API. `tag` is the tag to
    /// follow: one that keeps moving out of a download URL, see
    /// [`tag_to_follow`], or the one an update source names; without one, the
    /// newest release that has the installed AppImage in it, see
    /// [`release_to_follow`]. `asset` is the name of the file that was
    /// installed, which picks the matching file out of the release.
    GitHubRelease { owner: String, repo: String, tag: Option<String>, asset: Option<String> },
    /// `gh-releases-zsync`: a GitHub release whose named asset is a zsync
    /// file. The release says which assets exist, the zsync file inside it
    /// says what the update is, and from there this is a zsync source like
    /// any other. `asset` is the pattern the update information names.
    GitHubZsync { owner: String, repo: String, tag: Option<String>, asset: String },
    /// Plain re-download of the stored URL.
    DirectUrl { url: String },
    /// Nothing to update from: no update information, no update source.
    Manual,
}

impl UpdateSource {
    pub fn describe(&self) -> String {
        match self {
            UpdateSource::Zsync { .. } => "zsync".to_string(),
            // The release is where the zsync file is found, not what the
            // update is: a delta is a delta.
            UpdateSource::GitHubZsync { .. } => "zsync".to_string(),
            UpdateSource::GitHubRelease { owner, repo, .. } => format!("github:{owner}/{repo}"),
            UpdateSource::DirectUrl { .. } => "url".to_string(),
            UpdateSource::Manual => MANUAL.to_string(),
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

/// How an update was carried out. Every update reports one of these, so it
/// is always clear which path ran and what it cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdatePath {
    /// appimg applied a zsync delta itself.
    Delta {
        /// Blocks the new version has.
        blocks: usize,
        /// Blocks that were already on disk.
        reused: usize,
        /// Bytes that came over the wire.
        fetched: u64,
        requests: usize,
    },
    /// A zsync source whose server ignored the range requests and sent the
    /// whole file, so there was no delta to apply after all.
    ZsyncWithoutRanges { bytes: u64 },
    /// Applying the delta failed, so the whole file the zsync file names
    /// was downloaded instead, and held to the same checksum.
    DeltaFailed {
        /// Why the delta gave up.
        reason: String,
        bytes: u64,
    },
    /// The whole file was downloaded, because the source offers no delta.
    FullDownload { bytes: u64 },
}

impl UpdatePath {
    /// One line saying what happened, for a caller that reports to a user.
    pub fn describe(&self) -> String {
        match self {
            UpdatePath::Delta { blocks, reused, fetched, requests } => format!(
                "reused {reused} of {blocks} blocks, fetched {} in {requests} {}",
                human_size(*fetched),
                if *requests == 1 { "request" } else { "requests" }
            ),
            UpdatePath::ZsyncWithoutRanges { bytes } => format!(
                "the server ignored the range requests, downloaded the whole file, {}",
                human_size(*bytes)
            ),
            UpdatePath::DeltaFailed { reason, bytes } => format!(
                "the delta failed, downloaded the whole file instead, {}: {reason}",
                human_size(*bytes)
            ),
            UpdatePath::FullDownload { bytes } => {
                format!("no delta for this source, downloaded {}", human_size(*bytes))
            }
        }
    }
}

impl From<zsync::Applied> for UpdatePath {
    fn from(applied: zsync::Applied) -> Self {
        if applied.whole_file {
            return UpdatePath::ZsyncWithoutRanges { bytes: applied.fetched };
        }
        UpdatePath::Delta {
            blocks: applied.blocks,
            reused: applied.reused,
            fetched: applied.fetched,
            requests: applied.requests,
        }
    }
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
    /// Which path the update took, and what it cost.
    pub path: UpdatePath,
    /// What checking the new file against the digest its GitHub release
    /// publishes found. `None` for an update that came out of no release.
    pub digest: Option<Verified>,
}

/// Works out how an application would be updated, without changing anything.
/// The update information embedded in the AppImage comes first, then the
/// update source in the desktop entry.
pub fn source_for(app: &InstalledApp) -> UpdateSource {
    if let Some(info) = app.update_info.as_deref() {
        if let Some(source) = source_from_update_info(info) {
            return source;
        }
    }
    match app.update_source.as_deref() {
        Some(value) => source_from_setting(value, app.origin.as_deref()),
        // An entry written by 0.2.x has no update source of its own: what it
        // was installed from was the update source then. A URL still is, a
        // local file is history, whether it is still there or not.
        None => match app.origin.as_deref() {
            Some(origin) if download::is_url(origin) => source_from_url(origin),
            _ => UpdateSource::Manual,
        },
    }
}

/// Checks an update source as a user gives it, and returns it the way the
/// desktop entry stores it. Accepted are an http(s) URL and
/// `github:owner/repo`, optionally followed by `@tag`, a tag that is then
/// followed exactly as written. A link to a repository or to its releases
/// on github.com is stored as `github:owner/repo`, and one to a release
/// follows its tag the way a download URL does, see [`tag_to_follow`].
pub fn parse_update_source(value: &str) -> Result<String> {
    let trimmed = value.trim();
    let invalid = || Error::InvalidUpdateSource(value.to_string());

    if let Some(spec) = trimmed.strip_prefix("github:") {
        let (owner, repo, tag) = github_spec(spec).ok_or_else(invalid)?;
        return Ok(github_setting(&owner, &repo, tag.as_deref()));
    }
    if !download::is_url(trimmed) {
        return Err(invalid());
    }
    let host = trimmed.split_once("://").map_or("", |(_, rest)| rest);
    let host = host.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() || trimmed.contains(char::is_whitespace) {
        return Err(invalid());
    }
    match github_page(trimmed) {
        Some(GitHubPage::Repository { owner, repo, tag }) => {
            Ok(github_setting(&owner, &repo, tag.as_deref()))
        }
        Some(GitHubPage::Download) => Ok(trimmed.to_string()),
        Some(GitHubPage::Other) => Err(invalid()),
        None => Ok(trimmed.to_string()),
    }
}

/// The `owner/repo` an update source follows the releases of, if it is a
/// GitHub one, whether written as `github:` or as a download URL.
pub fn github_repository(value: &str) -> Option<String> {
    match source_from_setting(value, None) {
        UpdateSource::GitHubRelease { owner, repo, .. } => Some(format!("{owner}/{repo}")),
        _ => None,
    }
}

/// Sets the update source of an installed application, or makes it manual
/// with `None`, and writes nothing else. Returns whether anything changed.
pub fn set_update_source(app: &InstalledApp, value: Option<&str>) -> Result<bool> {
    let value = match value {
        Some(value) => parse_update_source(value)?,
        None => MANUAL.to_string(),
    };
    let mut entry = DesktopEntry::read(&app.desktop_entry_path)?;
    if entry.get(desktop_entry::KEY_UPDATE_SOURCE) == Some(value.as_str()) {
        return Ok(false);
    }
    entry.set(desktop_entry::KEY_UPDATE_SOURCE, value);
    entry.write(&app.desktop_entry_path)?;
    Ok(true)
}

/// The source an `X-AppImg-UpdateSource` value stands for. `manual`, and
/// anything that is not an update source, is no source at all.
fn source_from_setting(value: &str, origin: Option<&str>) -> UpdateSource {
    let Ok(value) = parse_update_source(value) else {
        return UpdateSource::Manual;
    };
    match value.strip_prefix("github:").and_then(github_spec) {
        // The file that was installed is what picks the file out of each
        // release, whether it came from that release or from anywhere else.
        Some((owner, repo, tag)) => UpdateSource::GitHubRelease {
            owner,
            repo,
            tag,
            asset: origin
                .and_then(|origin| origin.rsplit('/').next())
                .filter(|name| !name.is_empty())
                .map(str::to_string),
        },
        None => source_from_url(&value),
    }
}

fn source_from_url(url: &str) -> UpdateSource {
    github_source_from_url(url).unwrap_or_else(|| UpdateSource::DirectUrl { url: url.to_string() })
}

/// `owner/repo` or `owner/repo@tag`, as a `github:` update source names a
/// repository.
fn github_spec(spec: &str) -> Option<(String, String, Option<String>)> {
    let (repository, tag) = match spec.split_once('@') {
        Some((repository, tag)) => (repository, Some(tag)),
        None => (spec, None),
    };
    let (owner, repo) = repository.split_once('/')?;
    let owner_ok =
        !owner.is_empty() && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    let repo_ok = !repo.is_empty()
        && repo != "."
        && repo != ".."
        && repo.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    // A tag goes into a URL as it is, so nothing that would end that URL.
    let tag_ok = tag.is_none_or(|tag| {
        !tag.is_empty()
            && !tag.chars().any(|c| c.is_whitespace() || c.is_control() || "?#".contains(c))
    });
    (owner_ok && repo_ok && tag_ok)
        .then(|| (owner.to_string(), repo.to_string(), tag.map(str::to_string)))
}

fn github_setting(owner: &str, repo: &str, tag: Option<&str>) -> String {
    match tag {
        Some(tag) => format!("github:{owner}/{repo}@{tag}"),
        None => format!("github:{owner}/{repo}"),
    }
}

/// What a link to github.com points at.
enum GitHubPage {
    /// The repository, its releases, or one release.
    Repository { owner: String, repo: String, tag: Option<String> },
    /// A file out of a release.
    Download,
    /// Anything else, which is nothing to update from.
    Other,
}

fn github_page(url: &str) -> Option<GitHubPage> {
    let rest = url.split_once("://")?.1;
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let path = rest.strip_prefix("github.com")?;
    if !path.is_empty() && !path.starts_with('/') {
        return None;
    }
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let [owner, repo, rest @ ..] = parts.as_slice() else {
        return Some(GitHubPage::Other);
    };
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    let Some((owner, repo, _)) = github_spec(&format!("{owner}/{repo}")) else {
        return Some(GitHubPage::Other);
    };
    let tag = match rest {
        [] | ["releases"] | ["releases", "latest"] => None,
        ["releases", "tag", tag] => tag_to_follow(tag),
        ["releases", "download", ..] => return Some(GitHubPage::Download),
        _ => return Some(GitHubPage::Other),
    };
    Some(GitHubPage::Repository { owner, repo, tag })
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
        UpdateSource::Manual => {
            status.note = Some(manual_note(app));
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
        UpdateSource::GitHubZsync { owner, repo, tag, asset } => {
            let release =
                release_to_follow(owner, repo, tag.as_deref(), |r| has_zsync_for(r, asset))?;

            match zsync_asset_url(&release, asset) {
                // From here on this is a zsync source: the zsync file says
                // what the new version is and whether the file on disk is
                // still it, which is more than a release tag can say.
                Some(url) => {
                    let header = zsync::fetch_header(&url)?;
                    status.latest_version = offered_by_zsync(&header)
                        .or_else(|| release.version())
                        .or_else(|| current.clone());
                    let (available, note) = zsync_compare(&header, &app.appimage_path)?;
                    status.available = available;
                    status.note = note;
                }
                None => {
                    status.latest_version = release.version();
                    let recorded = recorded_tag(app, owner, repo);
                    let (installed, available, note) =
                        compare_release(current.as_deref(), recorded.as_deref(), &release);
                    status.current_version = installed;
                    status.available = available;
                    status.note = note.or_else(|| {
                        Some(
                            "the release has no zsync file, an update would be a full download"
                                .to_string(),
                        )
                    });
                }
            }
        }
        UpdateSource::GitHubRelease { owner, repo, tag, asset } => {
            let release = release_to_follow(owner, repo, tag.as_deref(), |r| {
                r.has_appimage(asset.as_deref())
            })?;
            status.latest_version = release.version();
            let recorded = recorded_tag(app, owner, repo);
            let (installed, available, note) =
                compare_release(current.as_deref(), recorded.as_deref(), &release);
            status.current_version = installed;
            status.available = available;
            status.note = note;
            if let Err(reason) = release.appimage(asset.as_deref(), Arch::current()) {
                status.available = false;
                status.note = Some(reason);
            }
        }
        UpdateSource::DirectUrl { .. } => {
            status.note =
                Some("the source URL carries no version, updating re-downloads it".to_string());
        }
    }
    Ok(status)
}

/// What to say about an application that has nothing to update from.
fn manual_note(app: &InstalledApp) -> String {
    match app.update_source.as_deref() {
        Some(value) if value != MANUAL && parse_update_source(value).is_err() => format!(
            "{} holds {value:?}, which is no update source, set one with: appimg update-source {} \
             <URL|github:owner/repo>",
            desktop_entry::KEY_UPDATE_SOURCE,
            app.slug
        ),
        _ => format!(
            "no update source, set one with: appimg update-source {} <URL|github:owner/repo>",
            app.slug
        ),
    }
}

/// Every name an update can leave next to `<slug>.AppImage`, whoever wrote
/// it. `.new` is appimg's own staging file and `.bak` its backup of the
/// previous version. `.zs-old` and `.part` come from the zsync client inside
/// `appimageupdatetool`, which appimg 0.3 and older fell back to: it
/// hard-links the previous version out of the way before the swap and
/// downloads into a partial file, and never cleans up either. appimg no
/// longer runs it, but those files can still be on disk. All four are named
/// after the AppImage, so a file that carries one of these suffixes and a
/// managed slug is provably ours.
pub const LEFTOVER_SUFFIXES: &[&str] =
    &["AppImage.bak", "AppImage.new", "AppImage.zs-old", "AppImage.part"];

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
        UpdateSource::Manual => Err(Error::NoUpdateSource(app.slug.clone())),
        UpdateSource::Zsync { update_info } => {
            let url =
                zsync_url(update_info).ok_or_else(|| Error::NoUpdateInfo(app.slug.clone()))?;
            apply_zsync(paths, app, &target, &url, source, Provenance::default(), progress)
        }
        UpdateSource::GitHubZsync { owner, repo, tag, asset } => {
            let release =
                release_to_follow(owner, repo, tag.as_deref(), |r| has_zsync_for(r, asset))?;
            let from = Provenance::of(owner, repo, &release);

            match zsync_asset_url(&release, asset) {
                Some(url) => apply_zsync(paths, app, &target, &url, source, from, progress),
                // A release that ships no zsync file leaves nothing to apply
                // a delta from, so the whole file it is.
                None => {
                    let hint = appimage_named_by(asset);
                    let url = pick_appimage(&release, owner, repo, Some(&hint))?;
                    full_download(paths, app, &target, &url, source, from, progress)
                }
            }
        }
        UpdateSource::GitHubRelease { owner, repo, tag, asset } => {
            let release = release_to_follow(owner, repo, tag.as_deref(), |r| {
                r.has_appimage(asset.as_deref())
            })?;
            let url = pick_appimage(&release, owner, repo, asset.as_deref())?;
            let from = Provenance::of(owner, repo, &release);
            full_download(paths, app, &target, &url, source, from, progress)
        }
        UpdateSource::DirectUrl { url } => {
            let url = url.clone();
            full_download(paths, app, &target, &url, source, Provenance::default(), progress)
        }
    }
}

/// An update that downloads the whole new file, checks it against the
/// digest its release publishes when it came out of one, and swaps it in.
fn full_download(
    paths: &Paths,
    app: &InstalledApp,
    target: &Path,
    url: &str,
    source: UpdateSource,
    from: Provenance,
    progress: Option<ProgressFn<'_>>,
) -> Result<UpdateOutcome> {
    let (staged, bytes) = download_staged(paths, &app.slug, url, progress)?;
    let verified = verify_staged(&staged, url, &from)?;
    let backup = swap_in(&staged, target)?;
    let path = UpdatePath::FullDownload { bytes };
    let outcome = finish(paths, app, target, Some(backup), source, from, path)?;
    Ok(UpdateOutcome { digest: verified, ..outcome })
}

/// The version to record for a file: the one it declares, unless that is a
/// build id or there is none, then what the release it came out of knows,
/// then what its file name says.
fn version_to_record(
    declared: Option<String>,
    from_release: Option<String>,
    file_name: &str,
) -> Option<String> {
    match declared {
        // A build id says nothing about age. Recording the date of the
        // release it was downloaded from is what lets the next check tell
        // whether this file has fallen behind.
        Some(declared) if version::is_rolling(&declared) => from_release.or(Some(declared)),
        Some(declared) => Some(declared),
        None => from_release.or_else(|| version::extract(file_name)),
    }
}

/// The AppImage a `github:` update source offers now, picked the way an
/// update picks it, with what its release publishes about it.
#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    pub url: String,
    /// `github:owner/repo@tag`, for [`desktop_entry::KEY_RELEASE`].
    pub release: Option<String>,
    /// What the release publishes for the file, to check it against.
    pub published: Published,
    /// The version the release knows, for a file that declares none worth
    /// keeping.
    from_release: Option<String>,
}

impl ReleaseAsset {
    /// The version to record for the file, which declares `declared`, the
    /// way an update records it.
    pub fn version_for(&self, declared: Option<String>) -> Option<String> {
        version_to_record(declared, self.from_release.clone(), last_segment(&self.url))
    }
}

/// The newest AppImage the `github:owner/repo[@tag]` update source
/// `source` offers: out of the newest release that has one matching
/// `asset_hint`, the file name of an earlier download, the way an update
/// finds it. One request for the releases.
pub fn newest_asset(source: &str, asset_hint: Option<&str>) -> Result<ReleaseAsset> {
    let invalid = || Error::InvalidUpdateSource(source.to_string());
    let (owner, repo, tag) =
        source.strip_prefix("github:").and_then(github_spec).ok_or_else(invalid)?;
    let release = release_to_follow(&owner, &repo, tag.as_deref(), |r| r.has_appimage(asset_hint))?;
    let url = pick_appimage(&release, &owner, &repo, asset_hint)?;
    let from = Provenance::of(&owner, &repo, &release);
    Ok(ReleaseAsset {
        published: release.published_for(&url),
        release: from.release,
        from_release: from.version,
        url,
    })
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

/// Re-reads metadata and icons from the new binary and refreshes only the
/// technical keys. Name, categories and launch arguments stay as they are,
/// they may well have been edited by hand.
///
/// `from` is what the GitHub release the file came out of knows and the file
/// does not, see [`Provenance`]. An update that came out of no release
/// leaves no release recorded either.
fn finish(
    paths: &Paths,
    app: &InstalledApp,
    target: &Path,
    backup: Option<PathBuf>,
    source: UpdateSource,
    from: Provenance,
    path: UpdatePath,
) -> Result<UpdateOutcome> {
    let from_release = from.version;
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

    let new_version = version_to_record(
        info.as_ref().and_then(|info| info.version.clone()),
        from_release,
        &target.file_name().unwrap_or_default().to_string_lossy(),
    );

    let mut entry = DesktopEntry::read(&app.desktop_entry_path)?;
    entry.set_optional(desktop_entry::KEY_VERSION, new_version.clone());
    if let Some(update_info) = info.as_ref().and_then(|info| info.update_info.clone()) {
        entry.set(desktop_entry::KEY_UPDATE_INFO, update_info);
    }
    if !icons.is_empty() {
        entry.set("Icon", app.slug.clone());
    }
    entry.set_optional(desktop_entry::KEY_RELEASE, from.release);
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
        path,
        digest: None,
    })
}

fn download_staged(
    paths: &Paths,
    slug: &str,
    url: &str,
    progress: Option<ProgressFn<'_>>,
) -> Result<(PathBuf, u64)> {
    let staged = paths.appimage_dir.join(format!("{slug}.AppImage.new"));
    let bytes = download::appimage_to_file(url, &staged, progress)?;
    fs_util::set_mode(&staged, MODE_EXEC)?;
    Ok((staged, bytes))
}

/// Moves the new binary into place and keeps the old one as `.bak`. A failure
/// while swapping restores the previous state.
///
/// Every update that installs a file appimg wrote ends here, whether it was
/// downloaded whole or assembled from a delta. The new file reaches
/// the disk before it gets the installed name: a power cut right after a
/// rename of a file that is not on disk yet can leave an empty or partial
/// AppImage under that name.
fn swap_in(staged: &Path, target: &Path) -> Result<PathBuf> {
    let backup = target.with_extension("AppImage.bak");

    fs_util::sync(staged)?;
    if target.exists() {
        fs::rename(target, &backup).map_err(|e| Error::io(target, e))?;
    }
    if let Err(e) = fs::rename(staged, target) {
        if backup.exists() {
            let _ = fs::rename(&backup, target);
        }
        return Err(Error::io(target, e));
    }
    // This makes the renames themselves last. Its failure does not fail the
    // update: the new version is in place and on disk by now, and without
    // the sync a power cut can only undo renames, which a power cut just
    // before it could do anyway. Every file such a cut leaves behind is a
    // complete one. Some filesystems refuse to sync a directory at all.
    if let Some(dir) = target.parent() {
        let _ = fs_util::sync(dir);
    }
    fs_util::set_mode(target, MODE_EXEC)?;
    Ok(backup)
}

fn source_from_update_info(info: &str) -> Option<UpdateSource> {
    let parts: Vec<&str> = info.split('|').collect();
    match parts.first().copied() {
        // gh-releases-zsync|owner|repo|tag|pattern
        Some("gh-releases-zsync") if parts.len() >= 5 => Some(UpdateSource::GitHubZsync {
            owner: parts[1].to_string(),
            repo: parts[2].to_string(),
            tag: tag_to_follow(parts[3]),
            asset: parts[4].to_string(),
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
/// at, so no tag is followed and the newest release is taken instead, see
/// [`release_to_follow`]. `latest` is GitHub's own word for exactly that, and
/// no tag of that name has to exist.
fn tag_to_follow(tag: &str) -> Option<String> {
    let tag = tag.trim();
    (version::is_rolling(tag) && !tag.eq_ignore_ascii_case("latest")).then(|| tag.to_string())
}

#[derive(Debug, Clone)]
struct Release {
    tag: Option<String>,
    assets: Vec<String>,
    /// The download URL of every asset with the SHA-256 GitHub publishes
    /// for it, `None` for an asset older than the digests.
    digests: Vec<(String, Option<String>)>,
    /// The day it was published, `2025-10-18`.
    published: Option<String>,
    /// The commit it was built from, abbreviated.
    commit: Option<String>,
}

impl Release {
    /// What this release publishes for the asset at `url`.
    fn published_for(&self, url: &str) -> Published {
        match self.digests.iter().find(|(asset, _)| asset == url) {
            Some((_, Some(sha256))) => Published::Sha256(sha256.clone()),
            Some((_, None)) => Published::Nothing(digest::NONE_PUBLISHED.to_string()),
            None => Published::Nothing("the file is not an asset of the release".to_string()),
        }
    }

    /// The asset `name` of this release, by the last part of its download
    /// URL, as a download URL spells it.
    fn asset_named(&self, name: &str) -> Option<&String> {
        self.assets.iter().find(|url| last_segment(url) == name)
    }

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

    /// Whether an AppImage of this release fits `hint` on this machine.
    fn has_appimage(&self, hint: Option<&str>) -> bool {
        !self.fitting(hint, Arch::current()).is_empty()
    }

    fn appimages(&self) -> Vec<&String> {
        self.assets.iter().filter(|url| url.to_lowercase().ends_with(".appimage")).collect()
    }

    /// The AppImages of this release that fit `hint`, the way
    /// [`Release::appimage`] matches them.
    fn fitting(&self, hint: Option<&str>, here: Option<Arch>) -> Vec<&String> {
        let wanted = hint.map(AssetName::parse);
        let appimages: Vec<(&String, AssetName)> =
            self.appimages().into_iter().map(|url| (url, AssetName::parse(url))).collect();
        // A name without an architecture is the build for this machine,
        // unless another AppImage of the release names this machine's: then
        // it is the build for some other one, the way electron-builder leaves
        // the x86_64 AppImage unlabeled next to an `-arm64` one. Only the
        // AppImages count, Obsidian's `_amd64.deb` says nothing about them.
        let labels_here = here.is_some() && appimages.iter().any(|(_, name)| name.arch == here);
        let unlabeled = if labels_here { None } else { here };
        appimages
            .into_iter()
            .filter(|(_, candidate)| match &wanted {
                Some(wanted) => {
                    candidate.words == wanted.words
                        && arch_fits(wanted.arch, candidate.arch, here, unlabeled)
                }
                None => arch_fits(None, candidate.arch, here, unlabeled),
            })
            .map(|(url, _)| url)
            .collect()
    }

    /// Picks the AppImage out of this release that is the one installed.
    /// `hint` is the name of the installed file: whatever in it is part of a
    /// version is ignored, the rest of the name and the architecture have to
    /// match. Without a hint, the AppImage built for `here` is the one.
    /// Anything but exactly one candidate is an error that lists the
    /// AppImages there are, never a guess.
    fn appimage(
        &self,
        hint: Option<&str>,
        here: Option<Arch>,
    ) -> std::result::Result<String, String> {
        let appimages = self.appimages();
        if appimages.is_empty() {
            return Err("the release has no AppImage".to_string());
        }
        let fits = self.fitting(hint, here);

        let what = match hint {
            Some(hint) => last_segment(hint).to_string(),
            None => format!("one built for {}", here.map_or("this machine", Arch::name)),
        };
        let advice = "set the update source to the download URL of the one to follow";
        match fits.as_slice() {
            [one] => Ok((*one).clone()),
            [] => Err(format!(
                "none of its AppImages matches {what}: {}; {advice}",
                names(&appimages)
            )),
            several => Err(format!(
                "{} of its AppImages match {what}: {}; {advice}",
                several.len(),
                names(several)
            )),
        }
    }
}

/// The download URL of the AppImage to update to, out of a release.
fn pick_appimage(release: &Release, owner: &str, repo: &str, hint: Option<&str>) -> Result<String> {
    release.appimage(hint, Arch::current()).map_err(|reason| Error::NoMatchingAsset {
        release: match &release.tag {
            Some(tag) => format!("release {tag} of github:{owner}/{repo}"),
            None => format!("the release of github:{owner}/{repo} it follows"),
        },
        reason,
    })
}

/// The AppImage a `gh-releases-zsync` pattern names, for a release that
/// ships no zsync file: the pattern without `.zsync`, and without its
/// `{{...}}` placeholders, which stand for the architecture. A `*` is no
/// word of a name, so it drops out on its own.
fn appimage_named_by(pattern: &str) -> String {
    let pattern = match pattern.len().checked_sub(".zsync".len()) {
        Some(cut)
            if pattern.is_char_boundary(cut) && pattern[cut..].eq_ignore_ascii_case(".zsync") =>
        {
            &pattern[..cut]
        }
        _ => pattern,
    };
    let mut out = String::with_capacity(pattern.len());
    let mut rest = pattern;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        rest = rest[start..].find("}}").map_or("", |end| &rest[start + end + 2..]);
    }
    out.push_str(rest);
    out
}

/// The architectures AppImages are built for, which release files spell in
/// several ways.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arch {
    X86_64,
    I686,
    Aarch64,
    Armhf,
}

impl Arch {
    /// The machine this runs on.
    fn current() -> Option<Arch> {
        match std::env::consts::ARCH {
            "x86_64" => Some(Arch::X86_64),
            "x86" => Some(Arch::I686),
            "aarch64" => Some(Arch::Aarch64),
            "arm" => Some(Arch::Armhf),
            _ => None,
        }
    }

    /// The architecture one word of an asset name stands for. `x86_64` and
    /// `x86-64` arrive as two words, see [`AssetName::parse`].
    fn named(word: &str) -> Option<Arch> {
        match word {
            "amd64" | "x64" => Some(Arch::X86_64),
            "i386" | "i686" | "x86" => Some(Arch::I686),
            "aarch64" | "arm64" => Some(Arch::Aarch64),
            "armhf" | "armv7" | "armv7l" | "armv7hl" => Some(Arch::Armhf),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::I686 => "i686",
            Arch::Aarch64 => "aarch64",
            Arch::Armhf => "armhf",
        }
    }
}

/// Whether a file built for `candidate` is one for `wanted`, on a machine
/// that is `here`. An installed name that carries no architecture is the
/// build for this machine. A release file that carries none is the build
/// for `unlabeled`: this machine, which is what a project that ships one
/// build usually means, or no machine at all when the release labels
/// another file as this machine's build.
fn arch_fits(
    wanted: Option<Arch>,
    candidate: Option<Arch>,
    here: Option<Arch>,
    unlabeled: Option<Arch>,
) -> bool {
    wanted.or(here) == candidate.or(unlabeled)
}

/// An asset name as the matching sees it: the words of the name without
/// those that change from one release to the next, and the architecture.
/// `imhex-1.38.0-x86_64.AppImage` is `imhex` for x86_64, and so is
/// `imhex-1.38.1-x86_64.AppImage`.
#[derive(Debug, PartialEq, Eq)]
struct AssetName {
    words: Vec<String>,
    arch: Option<Arch>,
}

impl AssetName {
    fn parse(name: &str) -> Self {
        let file = last_segment(name).to_lowercase();
        let stem = file.strip_suffix(".appimage").unwrap_or(&file);
        let tokens: Vec<&str> =
            stem.split(|c: char| !c.is_alphanumeric()).filter(|token| !token.is_empty()).collect();

        let mut words = Vec::new();
        let mut arch = None;
        let mut index = 0;
        while index < tokens.len() {
            let token = tokens[index];
            index += 1;
            if token == "x86" && tokens.get(index) == Some(&"64") {
                arch = Some(Arch::X86_64);
                index += 1;
            } else if let Some(named) = Arch::named(token) {
                arch = Some(named);
            } else if !is_version_part(token) {
                words.push(token.to_string());
            }
        }
        Self { words, arch }
    }
}

/// Whether a word of an asset name is part of a version, which changes from
/// one release to the next: anything that starts with a digit (`1`, `38`,
/// `0rc1`), a letter followed by digits only (`v1`, `r1234`), a pre-release
/// marker (`beta`, `rc2`), or a commit (`a211784`, `g1a2b3c4`).
fn is_version_part(word: &str) -> bool {
    let mut chars = word.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let rest = chars.as_str();
    if first.is_ascii_digit() {
        return true;
    }
    if first.is_ascii_alphabetic() && !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    if matches!(
        word.trim_end_matches(|c: char| c.is_ascii_digit()),
        "alpha" | "beta" | "rc" | "pre"
    ) {
        return true;
    }
    let commit = word.strip_prefix('g').unwrap_or(word);
    commit.len() >= 7
        && commit.bytes().all(|b| b.is_ascii_hexdigit())
        && commit.bytes().any(|b| b.is_ascii_digit())
}

fn last_segment(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

fn names(urls: &[&String]) -> String {
    urls.iter().map(|url| last_segment(url)).collect::<Vec<_>>().join(", ")
}

/// The zsync file of a release, out of the pattern a `gh-releases-zsync`
/// update information names, e.g.
/// `imhex-*-{{ARCHITECTURE_FILE_NAME}}.AppImage.zsync`.
///
/// The pattern carries two kinds of hole. `*` is a wildcard, and a
/// `{{...}}` placeholder is one a build system was supposed to fill in and
/// sometimes did not: what it stands for is the architecture, so the names
/// this machine's architecture goes by are tried in its place.
fn zsync_asset_url(release: &Release, pattern: &str) -> Option<String> {
    let zsyncs: Vec<String> = release
        .assets
        .iter()
        .filter(|url| url.to_lowercase().ends_with(".zsync"))
        .cloned()
        .collect();

    if zsyncs.is_empty() {
        return None;
    }

    for arch in arch_names() {
        let wanted = fill_placeholders(pattern, arch);
        if let Some(url) = zsyncs.iter().find(|url| glob_matches(&wanted, &asset_name(url))) {
            return Some(url.clone());
        }
    }

    // The placeholder stood for something else, or the names moved on. Take
    // the zsync files the rest of the pattern still fits, and out of those
    // the one built for this machine.
    let loose = fill_placeholders(pattern, "*");
    let matching: Vec<&String> =
        zsyncs.iter().filter(|url| glob_matches(&loose, &asset_name(url))).collect();
    let candidates: Vec<&String> =
        if matching.is_empty() { zsyncs.iter().collect() } else { matching };

    for arch in arch_names() {
        if let Some(url) = candidates.iter().find(|url| asset_name(url).contains(arch)) {
            return Some((*url).clone());
        }
    }
    candidates.first().map(|url| (*url).clone())
}

/// Whether a release has a zsync file that `pattern` fits, its placeholders
/// filled in or left open. That [`zsync_asset_url`] takes any zsync file at
/// all when none fits is no reason to follow a release.
fn has_zsync_for(release: &Release, pattern: &str) -> bool {
    let loose = fill_placeholders(pattern, "*");
    release
        .assets
        .iter()
        .map(|url| asset_name(url))
        .any(|name| name.ends_with(".zsync") && glob_matches(&loose, &name))
}

/// The file name at the end of an asset URL, lowercased.
fn asset_name(url: &str) -> String {
    url.rsplit('/').next().unwrap_or(url).to_lowercase()
}

/// The names a release asset might use for the architecture this is running
/// on. `x86_64` is written the same way everywhere, but a 64 bit ARM build
/// is called `aarch64` by some projects and `arm64` by others.
fn arch_names() -> &'static [&'static str] {
    match std::env::consts::ARCH {
        "x86_64" => &["x86_64", "amd64", "x64"],
        "aarch64" => &["aarch64", "arm64"],
        "arm" => &["armhf", "armv7l", "arm"],
        "x86" => &["i686", "i386", "x86"],
        // Whatever it is, its own name is the best guess there is.
        other => std::slice::from_ref(Box::leak(Box::new(other))),
    }
}

/// Replaces every `{{...}}` in a pattern with the same text.
fn fill_placeholders(pattern: &str, with: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut rest = pattern;

    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        match rest[start..].find("}}") {
            Some(end) => {
                out.push_str(with);
                rest = &rest[start + end + 2..];
            }
            // An opening brace with no closing one is not a placeholder.
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Whether a name fits a pattern of literal text and `*` wildcards. Case is
/// ignored: the pattern comes out of an AppImage's update information, the
/// name off a server, and neither is careful about it.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let name = name.to_lowercase();
    let mut parts = pattern.split('*');

    // Everything before the first wildcard has to be where it says.
    let Some(first) = parts.next() else { return false };
    let Some(mut rest) = name.strip_prefix(first) else { return false };

    let mut parts = parts.peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            // The last piece has to sit at the end, unless the pattern
            // ended in a wildcard, in which case it is empty.
            return rest.ends_with(part);
        }
        if part.is_empty() {
            continue;
        }
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    true
}

/// Reads the release behind a tag.
fn fetch_release(owner: &str, repo: &str, tag: &str) -> Result<Release> {
    let url = format!("{}/repos/{owner}/{repo}/releases/tags/{tag}", github_api());
    let body = download::to_string(&url)?;
    Ok(parse_release(&body))
}

/// Reads the release GitHub calls the latest: the newest that is neither a
/// draft nor a pre-release, the one a `releases/latest/download` URL leads to.
fn fetch_latest_release(owner: &str, repo: &str) -> Result<Release> {
    let url = format!("{}/repos/{owner}/{repo}/releases/latest", github_api());
    let body = download::to_string(&url)?;
    Ok(parse_release(&body))
}

/// How many releases the one request for a listing asks for.
const RELEASES_PER_PAGE: usize = 30;

/// The release an update from GitHub follows. A tag is followed exactly.
/// Without one, the newest release that `holds` what the update needs: the
/// installed AppImage, or a zsync file for it. Projects publish releases for
/// some platforms only, and the latest one may hold nothing but an `.apk`.
/// One request either way, the first page of the listing. When no release on
/// it holds what is needed, the newest one is returned all the same, and
/// what comes next says why it is no use.
fn release_to_follow(
    owner: &str,
    repo: &str,
    tag: Option<&str>,
    holds: impl Fn(&Release) -> bool,
) -> Result<Release> {
    if let Some(tag) = tag {
        return fetch_release(owner, repo, tag);
    }
    let url =
        format!("{}/repos/{owner}/{repo}/releases?per_page={RELEASES_PER_PAGE}", github_api());
    let body = download::to_string(&url)?;
    newest_where(published_releases(&body), holds).ok_or_else(|| Error::NoMatchingAsset {
        release: format!("github:{owner}/{repo}"),
        reason: "it has no release that is neither a draft nor a pre-release".to_string(),
    })
}

/// The releases of a listing, newest first as GitHub lists them, without
/// drafts and pre-releases: what `releases/latest` chooses from.
fn published_releases(listing: &str) -> Vec<Release> {
    json::array_objects(listing)
        .into_iter()
        .filter(|release| {
            json::bool_field(release, "draft") != Some(true)
                && json::bool_field(release, "prerelease") != Some(true)
        })
        .map(parse_release)
        .collect()
}

/// The newest release that `holds` what is needed, or the newest release
/// when none does.
fn newest_where(releases: Vec<Release>, holds: impl Fn(&Release) -> bool) -> Option<Release> {
    let index = releases.iter().position(holds).unwrap_or(0);
    releases.into_iter().nth(index)
}

fn parse_release(body: &str) -> Release {
    Release {
        tag: json::string_field(body, "tag_name"),
        assets: json::string_fields(body, "browser_download_url"),
        digests: json::array_field_objects(body, "assets")
            .into_iter()
            .filter_map(|asset| {
                let url = json::string_field(asset, "browser_download_url")?;
                let sha256 = json::string_field(asset, "digest").as_deref().and_then(digest::parse);
                Some((url, sha256))
            })
            .collect(),
        published: json::string_field(body, "published_at")
            .as_deref()
            .and_then(date::from_timestamp),
        // A continuous release points its tag at the commit it was built
        // from, which is the same commit the builds themselves name.
        commit: json::string_field(body, "target_commitish")
            .as_deref()
            .and_then(version::short_commit),
    }
}

/// Whether `release` is newer than what is installed, and what to show as
/// the installed version while saying so.
///
/// A rolling release names no version, and neither does a build out of one.
/// What both carry is the commit: on a channel that only ever moves forward
/// the same commit is the same build, and a different one supersedes it. An
/// installed file that already carries a date is compared as a date, which
/// orders, so it says whether the file is the older one and not merely a
/// different one.
///
/// A cut release is compared by its tag when the tag the installed file came
/// out of is `recorded`: the version a file declares need not be spelled
/// the way a version is read out of its release tag: osu! calls itself
/// `2026.921.0-lazer`, and so is its tag, yet the version that tag names is
/// `2026.921.0`. Without a recorded tag, from a file or from
/// 0.2.x, versions are compared, and two that differ only in a trailing
/// label count as the same, see [`version::same_but_label`].
fn compare_release(
    current: Option<&str>,
    recorded: Option<&str>,
    release: &Release,
) -> (Option<String>, bool, Option<String>) {
    let latest = release.version();

    if let (Some(recorded), Some(offered), false) =
        (recorded, release.tag.as_deref(), release.is_rolling())
    {
        let installed = current.map(str::to_string).or_else(|| version::extract(recorded));
        if same_tag(recorded, offered) {
            return (installed, false, None);
        }
        // Another tag is another release, though not a newer one when the
        // installed file came out of a later one, a pre-release perhaps.
        let older = match (version::extract(recorded), latest.as_deref()) {
            (Some(recorded), Some(latest)) if version::comparable(&recorded, latest) => {
                !version::is_newer(latest, &recorded)
            }
            _ => false,
        };
        return (installed, !older, None);
    }

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
        (Some(current), Some(latest)) if version::comparable(current, latest) => (
            Some(current.to_string()),
            version::is_newer(latest, current) && !version::same_but_label(latest, current),
            None,
        ),
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

/// Whether two tags name the same release. `v2.0.0` and `2.0.0` do: a tag
/// recorded from a download URL and one out of the API can differ in that
/// alone.
fn same_tag(a: &str, b: &str) -> bool {
    fn without_v(tag: &str) -> &str {
        tag.strip_prefix(['v', 'V'])
            .filter(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
            .unwrap_or(tag)
    }
    without_v(a.trim()) == without_v(b.trim())
}

/// The tag `X-AppImg-Release` records for this repository, if it records
/// one. A release of another repository says nothing about this one.
fn recorded_tag(app: &InstalledApp, owner: &str, repo: &str) -> Option<String> {
    let (recorded_owner, recorded_repo, tag) =
        github_spec(app.release.as_deref()?.strip_prefix("github:")?)?;
    (recorded_owner.eq_ignore_ascii_case(owner) && recorded_repo.eq_ignore_ascii_case(repo))
        .then_some(tag)
        .flatten()
}

/// `github:owner/repo@tag` for a file downloaded out of a GitHub release,
/// by its download URL, for [`desktop_entry::KEY_RELEASE`].
pub fn release_of_download(url: &str) -> Option<String> {
    let download = ReleaseDownload::parse(url)?;
    Some(github_setting(&download.owner, &download.repo, Some(download.tag.as_deref()?)))
}

/// What GitHub publishes for the file behind a release download URL, at the
/// cost of one request for that release. `None` for a URL that is no
/// release download, which has nothing published to check against. A
/// release that cannot be read, or that lists no such asset, is no reason
/// to refuse the file either, only nothing to check it against, and the
/// reason says so.
pub fn published_for_download(url: &str) -> Option<Published> {
    let download = ReleaseDownload::parse(url)?;
    let (owner, repo) = (&download.owner, &download.repo);
    let (release, which) = match &download.tag {
        Some(tag) => (fetch_release(owner, repo, tag), format!("release {tag}")),
        None => (fetch_latest_release(owner, repo), "the latest release".to_string()),
    };
    let published = match release {
        Ok(release) => match release.asset_named(&download.asset) {
            Some(asset) => release.published_for(asset),
            None => Published::Nothing(format!("{which} lists no asset {}", download.asset)),
        },
        Err(error) => Published::Nothing(format!("{which} could not be read: {error}")),
    };
    Some(published)
}

/// A download URL out of a GitHub release taken apart:
/// `https://github.com/owner/repo/releases/download/<tag>/<asset>`, or
/// `.../releases/latest/download/<asset>`, which has no tag. Tag and asset
/// stay spelled the way the URL spells them.
struct ReleaseDownload {
    owner: String,
    repo: String,
    tag: Option<String>,
    asset: String,
}

impl ReleaseDownload {
    fn parse(url: &str) -> Option<Self> {
        let rest = url
            .strip_prefix("https://github.com/")
            .or_else(|| url.strip_prefix("http://github.com/"))?;
        let path = rest.split(['?', '#']).next().unwrap_or(rest);
        let parts: Vec<&str> = path.split('/').collect();
        let (owner, repo, tag, asset) = match parts.as_slice() {
            [owner, repo, "releases", "download", tag, asset] => (owner, repo, Some(tag), asset),
            [owner, repo, "releases", "latest", "download", asset] => (owner, repo, None, asset),
            _ => return None,
        };
        if asset.is_empty() {
            return None;
        }
        let spec = match tag {
            Some(tag) => format!("{owner}/{repo}@{tag}"),
            None => format!("{owner}/{repo}"),
        };
        let (owner, repo, tag) = github_spec(&spec)?;
        Some(Self { owner, repo, tag, asset: asset.to_string() })
    }
}

/// What an update knows from the GitHub release it came out of, and the
/// file it installs does not.
#[derive(Debug, Default)]
struct Provenance {
    /// The version to record when the file carries none worth keeping: a
    /// rolling build declares a build number and a commit, while the
    /// release knows the day it was published.
    version: Option<String>,
    /// `github:owner/repo@tag`, for [`desktop_entry::KEY_RELEASE`].
    release: Option<String>,
    /// The release itself, for the digests it publishes. `None` for an
    /// update that came out of no release, which has nothing to check a
    /// file against and nothing to say about it.
    out_of: Option<Release>,
}

impl Provenance {
    fn of(owner: &str, repo: &str, release: &Release) -> Self {
        Self {
            version: release.recorded_version(),
            release: release.tag.as_deref().map(|tag| github_setting(owner, repo, Some(tag))),
            out_of: Some(release.clone()),
        }
    }

    /// What the release publishes for the asset at `url`, `None` for an
    /// update that came out of no release.
    fn published_for(&self, url: &str) -> Option<Published> {
        self.out_of.as_ref().map(|release| release.published_for(url))
    }
}

/// Checks a file an update staged against what the release it came out of
/// publishes for the asset at `url`, before anything replaces the installed
/// AppImage. A file that does not match is removed, the installed one stays
/// as it is. `None` for an update that came out of no release.
fn verify_staged(staged: &Path, url: &str, from: &Provenance) -> Result<Option<Verified>> {
    let Some(published) = from.published_for(url) else {
        return Ok(None);
    };
    match digest::verify(staged, url, &published) {
        Ok(verified) => Ok(Some(verified)),
        Err(error) => {
            let _ = fs::remove_file(staged);
            Err(error)
        }
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

/// The whole of a zsync update. appimg applies the delta itself: reads the
/// control file, works out which blocks the installed AppImage already
/// holds, fetches the rest and assembles `<slug>.AppImage.new`, checked
/// against the checksum in the zsync header. When that fails, for any
/// reason, the complete file the zsync file names is downloaded instead and
/// held to the same checksum. Either way the new file goes on like every
/// other update: checked against the digest its release publishes when it
/// came out of one, then swapped in. The outcome says which way it went.
fn apply_zsync(
    paths: &Paths,
    app: &InstalledApp,
    target: &Path,
    zsync_url: &str,
    source: UpdateSource,
    from: Provenance,
    progress: Option<ProgressFn<'_>>,
) -> Result<UpdateOutcome> {
    if !target.is_file() {
        return Err(Error::NotFound(target.to_path_buf()));
    }

    let control = zsync::fetch_control(zsync_url)?;
    let url = payload_url(zsync_url, &control.header).ok_or_else(|| Error::Zsync {
        url: zsync_url.to_string(),
        reason: "the zsync file names no URL for the file it describes".to_string(),
    })?;

    let staged = paths.appimage_dir.join(format!("{}.AppImage.new", app.slug));
    let mut progress = progress;
    // The delta reports through this, so the caller's progress is still
    // there for the whole file if the delta fails.
    let mut report = |done, total| {
        if let Some(report) = progress.as_mut() {
            report(done, total);
        }
    };
    let path = match zsync::apply(&control, &url, target, &staged, Some(&mut report)) {
        Ok(applied) => UpdatePath::from(applied),
        Err(delta) => {
            let bytes = download_whole(paths, &app.slug, &url, &control.header, progress).map_err(
                |whole| {
                    Error::Download(format!(
                        "{delta}; downloading the whole file instead failed too: {whole}"
                    ))
                },
            )?;
            UpdatePath::DeltaFailed { reason: delta.to_string(), bytes }
        }
    };

    // The zsync checksum says the file is the one the zsync file describes,
    // the digest that it is the one the release holds.
    let verified = verify_staged(&staged, &url, &from)?;
    let backup = swap_in(&staged, target)?;
    let outcome = finish(paths, app, target, Some(backup), source, from, path)?;
    Ok(UpdateOutcome { digest: verified, ..outcome })
}

/// Downloads the complete file a zsync file describes, after applying the
/// delta failed, and holds it to what the zsync header says about it: its
/// length, and its checksum when the header has one. A header without one
/// leaves the download as unchecked as any download from a URL. A file that
/// does not match is removed before the error comes back.
fn download_whole(
    paths: &Paths,
    slug: &str,
    url: &str,
    header: &zsync::Header,
    progress: Option<ProgressFn<'_>>,
) -> Result<u64> {
    let (staged, bytes) = download_staged(paths, slug, url, progress)?;
    let mismatch = if bytes != header.length {
        Some(format!("the downloaded file is {bytes} bytes, the zsync file says {}", header.length))
    } else {
        match header.sha1.as_deref() {
            Some(expected) => {
                let found = zsync::sha1_file(&staged)?;
                (found != expected).then(|| {
                    format!("the downloaded file is checksummed {found}, the zsync file says {expected}")
                })
            }
            None => None,
        }
    };
    if let Some(mismatch) = mismatch {
        let _ = fs::remove_file(&staged);
        return Err(Error::Zsync {
            url: url.to_string(),
            reason: format!("{mismatch}: what the server sent is not the file it described"),
        });
    }
    Ok(bytes)
}

/// Where the complete file lives, as the zsync header names it. A relative
/// URL is resolved against the zsync file's own URL, which is what makes
/// `URL: App-2.0.0-x86_64.AppImage` work.
fn payload_url(zsync_url: &str, header: &zsync::Header) -> Option<String> {
    let url = header.url.as_deref()?;
    if url.is_empty() {
        return None;
    }
    if download::is_url(url) {
        return Some(url.to_string());
    }
    let without_query = zsync_url.split(['?', '#']).next().unwrap_or(zsync_url);
    let (directory, _) = without_query.rsplit_once('/')?;
    Some(format!("{directory}/{url}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_with_url(url: Option<&str>) -> zsync::Header {
        zsync::Header {
            filename: None,
            length: 4096,
            sha1: None,
            url: url.map(str::to_string),
            mtime: None,
            blocksize: Some(2048),
            hash_lengths: zsync::HashLengths::DEFAULT,
        }
    }

    #[test]
    fn the_file_a_zsync_describes_is_found_next_to_the_zsync_file() {
        let zsync = "https://example.com/releases/App.AppImage.zsync";

        // The usual case: a bare name, which means the same directory.
        assert_eq!(
            payload_url(zsync, &header_with_url(Some("App-2.0.0-x86_64.AppImage"))),
            Some("https://example.com/releases/App-2.0.0-x86_64.AppImage".to_string())
        );
        // An absolute URL is used as it stands, wherever it points.
        assert_eq!(
            payload_url(zsync, &header_with_url(Some("https://cdn.example.net/App.AppImage"))),
            Some("https://cdn.example.net/App.AppImage".to_string())
        );
        // A query on the zsync URL is not part of the directory it sits in.
        assert_eq!(
            payload_url(
                "https://example.com/d/App.zsync?token=1",
                &header_with_url(Some("App.AppImage"))
            ),
            Some("https://example.com/d/App.AppImage".to_string())
        );
        // Nothing to go on.
        assert_eq!(payload_url(zsync, &header_with_url(None)), None);
        assert_eq!(payload_url(zsync, &header_with_url(Some(""))), None);
    }

    /// A release with these assets, named as GitHub names them.
    fn release_with(tag: &str, assets: &[&str]) -> Release {
        Release {
            tag: Some(tag.to_string()),
            assets: assets
                .iter()
                .map(|name| {
                    format!("https://github.com/WerWolv/ImHex/releases/download/{tag}/{name}")
                })
                .collect(),
            digests: Vec::new(),
            published: None,
            commit: None,
        }
    }

    /// The assets of an ImHex release, which is what this was written for.
    fn imhex_release() -> Release {
        release_with(
            "v1.38.1",
            &[
                "imhex-1.38.1-arm64.AppImage",
                "imhex-1.38.1-arm64.AppImage.zsync",
                "imhex-1.38.1-x86_64.AppImage",
                "imhex-1.38.1-x86_64.AppImage.zsync",
                "imhex-1.38.1-Windows-x86_64.msi",
            ],
        )
    }

    #[test]
    fn the_zsync_file_of_a_release_is_found_through_the_architecture_placeholder() {
        // What ImHex ships: a placeholder its build system left behind.
        let chosen =
            zsync_asset_url(&imhex_release(), "imhex-*-{{ARCHITECTURE_FILE_NAME}}.AppImage.zsync")
                .unwrap();

        assert!(chosen.ends_with(".AppImage.zsync"), "{chosen}");
        assert!(
            arch_names().iter().any(|arch| chosen.contains(arch)),
            "{chosen} is not a build for {}",
            std::env::consts::ARCH
        );
        // Never the AppImage itself, which is what made this a full
        // download before.
        assert!(!chosen.ends_with(".AppImage"), "{chosen}");
    }

    #[test]
    fn a_pattern_that_names_an_architecture_is_taken_at_its_word() {
        assert_eq!(
            zsync_asset_url(&imhex_release(), "imhex-*-arm64.AppImage.zsync").as_deref(),
            Some(
                "https://github.com/WerWolv/ImHex/releases/download/v1.38.1/imhex-1.38.1-arm64.AppImage.zsync"
            )
        );
    }

    #[test]
    fn a_placeholder_that_is_not_an_architecture_still_matches() {
        // `{{VERSION}}` stands for none of the architecture names, so the
        // rest of the pattern has to carry it.
        let release = release_with("v2", &["App-1.2.3-x86_64.AppImage.zsync"]);
        assert!(zsync_asset_url(&release, "App-{{VERSION}}-x86_64.AppImage.zsync").is_some());
    }

    #[test]
    fn a_release_with_no_zsync_file_offers_none() {
        let release = release_with("v1", &["App-1.0.0-x86_64.AppImage", "App-1.0.0.tar.gz"]);
        assert_eq!(zsync_asset_url(&release, "App-*.AppImage.zsync"), None);
    }

    #[test]
    fn a_zsync_file_is_still_found_when_the_names_moved_on() {
        // The pattern was written for a name the project no longer uses.
        let release = release_with("v9", &["renamed-9.0-x86_64.AppImage.zsync"]);
        assert_eq!(
            zsync_asset_url(&release, "App-*-{{ARCHITECTURE_FILE_NAME}}.AppImage.zsync").as_deref(),
            Some("https://github.com/WerWolv/ImHex/releases/download/v9/renamed-9.0-x86_64.AppImage.zsync")
        );
    }

    #[test]
    fn a_pattern_matches_the_way_a_shell_glob_does() {
        assert!(glob_matches(
            "imhex-*-x86_64.AppImage.zsync",
            "imhex-1.38.1-x86_64.AppImage.zsync"
        ));
        assert!(!glob_matches(
            "imhex-*-x86_64.AppImage.zsync",
            "imhex-1.38.1-arm64.AppImage.zsync"
        ));
        // Wildcards at either end, and none at all.
        assert!(glob_matches("*.zsync", "app.AppImage.zsync"));
        assert!(glob_matches("app*", "app.AppImage.zsync"));
        assert!(glob_matches("app.AppImage.zsync", "app.AppImage.zsync"));
        assert!(!glob_matches("app.AppImage.zsync", "app.AppImage"));
        // Several wildcards, and one that has to match nothing.
        assert!(glob_matches("a*b*c", "abc"));
        assert!(glob_matches("a*b*c", "a-b-c"));
        assert!(!glob_matches("a*b*c", "a-c-b"));
        // Case is not what tells two assets apart.
        assert!(glob_matches("App-*.AppImage.zsync", "app-1.0-x86_64.appimage.zsync"));
    }

    #[test]
    fn placeholders_are_filled_in_wherever_they_are() {
        assert_eq!(fill_placeholders("a-{{X}}.zsync", "64"), "a-64.zsync");
        assert_eq!(fill_placeholders("{{A}}-{{B}}", "*"), "*-*");
        assert_eq!(fill_placeholders("nothing to fill", "*"), "nothing to fill");
        // A brace that opens and never closes is part of the name.
        assert_eq!(fill_placeholders("a-{{X.zsync", "*"), "a-{{X.zsync");
    }

    #[test]
    fn every_update_path_says_what_it_did() {
        let delta = UpdatePath::Delta { blocks: 1061, reused: 1060, fetched: 2048, requests: 1 };
        let described = delta.describe();
        assert!(described.contains("reused 1060 of 1061 blocks"), "{described}");
        assert!(described.contains("2.0 KB"), "{described}");
        assert!(described.contains("1 request"), "{described}");

        // A server that ignored the ranges did not apply a delta at all.
        let whole = UpdatePath::ZsyncWithoutRanges { bytes: 2_172_096 };
        assert!(whole.describe().contains("ignored the range requests"), "{}", whole.describe());

        // A delta that failed says so, says why, and says what it cost
        // instead.
        let failed =
            UpdatePath::DeltaFailed { reason: "the server hung up".to_string(), bytes: 3 << 20 };
        let described = failed.describe();
        assert!(described.contains("the delta failed"), "{described}");
        assert!(described.contains("downloaded the whole file instead"), "{described}");
        assert!(described.contains("3.0 MB"), "{described}");
        assert!(described.contains("the server hung up"), "{described}");

        // And the sources that have no delta to apply say so too.
        let full = UpdatePath::FullDownload { bytes: 190 * 1024 * 1024 };
        assert!(full.describe().contains("no delta"), "{}", full.describe());
        assert!(full.describe().contains("190.0 MB"), "{}", full.describe());
    }

    #[test]
    fn reads_github_coordinates_from_update_info() {
        let source = source_from_update_info(
            "gh-releases-zsync|owner|repo|latest|App-*x86_64.AppImage.zsync",
        );
        // The asset it names is a zsync file, so this is a zsync source
        // that happens to find its zsync file through a release.
        assert_eq!(
            source,
            Some(UpdateSource::GitHubZsync {
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                tag: None,
                asset: "App-*x86_64.AppImage.zsync".to_string(),
            })
        );
        assert_eq!(source.unwrap().describe(), "zsync");
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
            Some(UpdateSource::GitHubZsync {
                owner: "AppImage".to_string(),
                repo: "AppImageUpdate".to_string(),
                tag: Some("continuous".to_string()),
                asset: "AppImageUpdate-*x86_64.AppImage.zsync".to_string(),
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
            blocksize: None,
            hash_lengths: zsync::HashLengths::DEFAULT,
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
            blocksize: None,
            hash_lengths: zsync::HashLengths::DEFAULT,
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

    /// appimg no longer runs `appimageupdatetool`, but the copy it left
    /// when an older appimg fell back to it can still be on disk, as large
    /// as the AppImage itself. The next confirmed update takes it along.
    #[test]
    fn what_appimageupdatetool_left_goes_with_the_next_confirmed_update() {
        let dir = tempfile::tempdir().unwrap();
        let paths = sandbox_paths(dir.path());
        let zs_old = dir.path().join("krita.AppImage.zs-old");
        let part = dir.path().join("krita.AppImage.part");
        fs::write(&zs_old, "the previous 371 MB").unwrap();
        fs::write(&part, "half a delta").unwrap();
        fs::write(backup_path(&paths, "krita"), "the version just replaced").unwrap();

        confirm(&paths, "krita").unwrap();
        assert!(leftovers(&paths, "krita").is_empty());
        assert!(!zs_old.exists() && !part.exists());
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

    /// The file names of a release, at a download URL as GitHub has them.
    fn names_in(tag: &str, names: &[&str]) -> Release {
        release_with(tag, names)
    }

    fn picked(release: &Release, hint: Option<&str>, here: Arch) -> Option<String> {
        release.appimage(hint, Some(here)).ok().map(|url| last_segment(&url).to_string())
    }

    #[test]
    fn a_release_says_its_version() {
        let release = names_in("v2.0.0", &["App-2.0.0-x86_64.AppImage"]);
        assert_eq!(release.version().as_deref(), Some("2.0.0"));
    }

    #[test]
    fn the_next_release_of_imhex_is_found_for_each_architecture() {
        let next = names_in(
            "v1.38.1",
            &[
                "imhex-1.38.1-arm64.AppImage",
                "imhex-1.38.1-arm64.AppImage.zsync",
                "imhex-1.38.1-x86_64.AppImage",
                "imhex-1.38.1-x86_64.AppImage.zsync",
                "imhex-1.38.1-Windows-x86_64.msi",
            ],
        );
        assert_eq!(
            picked(&next, Some("imhex-1.38.0-x86_64.AppImage"), Arch::X86_64).as_deref(),
            Some("imhex-1.38.1-x86_64.AppImage")
        );
        assert_eq!(
            picked(&next, Some("imhex-1.38.0-arm64.AppImage"), Arch::Aarch64).as_deref(),
            Some("imhex-1.38.1-arm64.AppImage")
        );
        // The hint decides, not the machine: an arm64 build installed on
        // this one stays an arm64 build.
        assert_eq!(
            picked(&next, Some("imhex-1.38.0-arm64.AppImage"), Arch::X86_64).as_deref(),
            Some("imhex-1.38.1-arm64.AppImage")
        );
    }

    #[test]
    fn the_next_release_of_lm_studio_is_found_through_a_build_number_and_a_new_spelling() {
        let next = names_in(
            "0.4.26",
            &["LM-Studio-0.4.26-2-x64.AppImage", "LM-Studio-0.4.26-2-arm64.AppImage"],
        );
        let installed = Some("LM-Studio-0.4.25-1-x64.AppImage");
        assert_eq!(
            picked(&next, installed, Arch::X86_64).as_deref(),
            Some("LM-Studio-0.4.26-2-x64.AppImage")
        );

        // `x64` and `x86_64` are the same architecture, whichever a release
        // happens to use.
        let renamed = names_in(
            "0.5.0",
            &["LM-Studio-0.5.0-1-x86_64.AppImage", "LM-Studio-0.5.0-1-aarch64.AppImage"],
        );
        assert_eq!(
            picked(&renamed, installed, Arch::X86_64).as_deref(),
            Some("LM-Studio-0.5.0-1-x86_64.AppImage")
        );
    }

    #[test]
    fn version_parts_of_every_kind_are_ignored() {
        for (installed, next) in [
            ("App-v1.2.3-x86_64.AppImage", "App-v1.3.0-x86_64.AppImage"),
            ("App_r1234_amd64.AppImage", "App_r1301_amd64.AppImage"),
            ("App-2.0.0-beta1-x86_64.AppImage", "App-2.0.0-x86_64.AppImage"),
            ("App-2.0.0-rc2-x86-64.AppImage", "App-2.0.1-x86_64.AppImage"),
            ("Neovim-nightly-a211784-x86_64.AppImage", "Neovim-nightly-g9f0c1d2e-x86_64.AppImage"),
            ("App-20251018-x86_64.AppImage", "App-20251102-x86_64.AppImage"),
        ] {
            let release = names_in("next", &[next, "Other-1.0-x86_64.AppImage"]);
            assert_eq!(
                picked(&release, Some(installed), Arch::X86_64).as_deref(),
                Some(next),
                "{installed}"
            );
        }
    }

    #[test]
    fn a_name_without_an_architecture_is_built_for_this_machine() {
        let next = names_in("v2.0.0", &["App-2.0.0.AppImage", "App-2.0.0.dmg"]);
        for here in [Arch::X86_64, Arch::Aarch64] {
            assert_eq!(
                picked(&next, Some("App-1.0.0.AppImage"), here).as_deref(),
                Some("App-2.0.0.AppImage"),
                "{here:?}"
            );
            assert_eq!(
                picked(&next, None, here).as_deref(),
                Some("App-2.0.0.AppImage"),
                "{here:?}"
            );
        }
    }

    #[test]
    fn a_name_with_this_machines_architecture_beats_one_without() {
        // electron-builder's way, and Obsidian's: the x86_64 build without
        // an architecture in its name, the arm64 build with one.
        let next = names_in(
            "v1.13.8",
            &[
                "Obsidian-1.13.8.AppImage",
                "Obsidian-1.13.8-arm64.AppImage",
                "obsidian_1.13.8_amd64.deb",
            ],
        );
        assert_eq!(
            picked(&next, Some("Obsidian-1.13.7-arm64.AppImage"), Arch::Aarch64).as_deref(),
            Some("Obsidian-1.13.8-arm64.AppImage")
        );
        // An installed name without one is this machine's build, which the
        // release labels.
        assert_eq!(
            picked(&next, Some("Obsidian-1.13.7.AppImage"), Arch::Aarch64).as_deref(),
            Some("Obsidian-1.13.8-arm64.AppImage")
        );
        assert_eq!(
            picked(&next, None, Arch::Aarch64).as_deref(),
            Some("Obsidian-1.13.8-arm64.AppImage")
        );

        // No AppImage names x86_64, so the unlabeled one is still that build,
        // whatever the `.deb` calls itself.
        assert_eq!(
            picked(&next, Some("Obsidian-1.13.7.AppImage"), Arch::X86_64).as_deref(),
            Some("Obsidian-1.13.8.AppImage")
        );
        assert_eq!(picked(&next, None, Arch::X86_64).as_deref(), Some("Obsidian-1.13.8.AppImage"));
    }

    #[test]
    fn several_fits_are_an_error_that_lists_them() {
        let next = names_in(
            "v2.1.0",
            &[
                "App-2.0.0-x86_64.AppImage",
                "App-2.1.0-beta-x86_64.AppImage",
                "App-2.1.0-arm64.AppImage",
            ],
        );
        let error =
            next.appimage(Some("App-1.0.0-x86_64.AppImage"), Some(Arch::X86_64)).unwrap_err();
        assert!(error.contains("2 of its AppImages match App-1.0.0-x86_64.AppImage"), "{error}");
        assert!(
            error.contains("App-2.0.0-x86_64.AppImage, App-2.1.0-beta-x86_64.AppImage"),
            "{error}"
        );
        assert!(!error.contains("arm64"), "{error}");
        assert!(error.contains("download URL"), "{error}");
    }

    #[test]
    fn another_variant_is_no_match_and_says_what_there_is() {
        let next = names_in("v2.0", &["App-qt6-2.0-x86_64.AppImage", "App-qt6-2.0-arm64.AppImage"]);
        let error =
            next.appimage(Some("App-qt5-1.0-x86_64.AppImage"), Some(Arch::X86_64)).unwrap_err();
        assert!(
            error.contains("none of its AppImages matches App-qt5-1.0-x86_64.AppImage"),
            "{error}"
        );
        assert!(
            error.contains("App-qt6-2.0-x86_64.AppImage, App-qt6-2.0-arm64.AppImage"),
            "{error}"
        );
    }

    #[test]
    fn without_a_hint_the_build_for_this_machine_is_the_one() {
        let release = names_in("v2", &["App-2-x86_64.AppImage", "App-2-aarch64.AppImage"]);
        assert_eq!(picked(&release, None, Arch::X86_64).as_deref(), Some("App-2-x86_64.AppImage"));
        assert_eq!(
            picked(&release, None, Arch::Aarch64).as_deref(),
            Some("App-2-aarch64.AppImage")
        );
        let error = release.appimage(None, Some(Arch::Armhf)).unwrap_err();
        assert!(error.contains("none of its AppImages matches one built for armhf"), "{error}");

        let variants = names_in("v2", &["App-2-x86_64.AppImage", "App-Lite-2-x86_64.AppImage"]);
        let error = variants.appimage(None, Some(Arch::X86_64)).unwrap_err();
        assert!(error.contains("2 of its AppImages match one built for x86_64"), "{error}");
    }

    #[test]
    fn a_release_without_appimages_offers_nothing() {
        let release = names_in("v1", &["App.tar.gz", "App.AppImage.zsync"]);
        assert_eq!(
            release.appimage(Some("App.AppImage"), Some(Arch::X86_64)),
            Err("the release has no AppImage".to_string())
        );
    }

    #[test]
    fn a_zsync_pattern_names_the_appimage_of_a_release_without_zsync_files() {
        assert_eq!(
            appimage_named_by("imhex-*-{{ARCHITECTURE_FILE_NAME}}.AppImage.zsync"),
            "imhex-*-.AppImage"
        );
        let release =
            names_in("v1.38.1", &["imhex-1.38.1-arm64.AppImage", "imhex-1.38.1-x86_64.AppImage"]);
        let hint = appimage_named_by("imhex-*-{{ARCHITECTURE_FILE_NAME}}.AppImage.zsync");
        assert_eq!(
            picked(&release, Some(&hint), Arch::X86_64).as_deref(),
            Some("imhex-1.38.1-x86_64.AppImage")
        );
        let hint = appimage_named_by("imhex-*-arm64.AppImage.ZSYNC");
        assert_eq!(
            picked(&release, Some(&hint), Arch::X86_64).as_deref(),
            Some("imhex-1.38.1-arm64.AppImage")
        );
    }

    /// One release of a listing: its tag, whether it is a draft or a
    /// pre-release, and its file names.
    type Listed<'a> = (&'a str, bool, bool, &'a [&'a str]);

    /// A listing as `GET /repos/{owner}/{repo}/releases` returns it, newest
    /// first, with the fields around the ones that are read: an author, an
    /// uploader for every file, and release notes full of brackets.
    fn listing(releases: &[Listed]) -> String {
        let objects: Vec<String> = releases
            .iter()
            .enumerate()
            .map(|(index, (tag, draft, prerelease, files))| {
                let assets: Vec<String> = files
                    .iter()
                    .map(|name| {
                        format!(
                            "{{\"url\":\"https://api.github.com/assets/{index}\",\"name\":\"{name}\",\
                             \"uploader\":{{\"login\":\"bot\",\"site_admin\":false}},\
                             \"content_type\":\"application/octet-stream\",\"size\":1,\
                             \"created_at\":\"2025-10-0{index}T10:00:00Z\",\
                             \"browser_download_url\":\"https://github.com/obsidianmd/\
                             obsidian-releases/releases/download/{tag}/{name}\"}}"
                        )
                    })
                    .collect();
                format!(
                    "{{\"url\":\"https://api.github.com/releases/{index}\",\
                     \"author\":{{\"login\":\"someone\",\"type\":\"User\",\"site_admin\":false}},\
                     \"tag_name\":\"{tag}\",\"target_commitish\":\"master\",\"name\":\"{tag}\",\
                     \"draft\":{draft},\"prerelease\":{prerelease},\
                     \"created_at\":\"2025-10-0{index}T09:00:00Z\",\
                     \"published_at\":\"2025-10-0{index}T10:00:00Z\",\"assets\":[{}],\
                     \"body\":\"Fixed: \\\"draft\\\": true }} ] {{ [ and \\\"tag_name\\\": \\\"v0\\\"\"}}",
                    assets.join(",")
                )
            })
            .collect();
        format!("[{}]", objects.join(",\n"))
    }

    fn followed(releases: &[Listed], hint: Option<&str>) -> Option<Release> {
        newest_where(published_releases(&listing(releases)), |release| {
            !release.fitting(hint, Some(Arch::X86_64)).is_empty()
        })
    }

    #[test]
    fn the_newest_release_with_the_installed_appimage_is_followed() {
        // What obsidianmd/obsidian-releases did: the latest release ships an
        // Android build only, the one before it has the AppImage.
        let releases: &[Listed] = &[
            ("v1.13.8", false, false, &["app-release.apk"]),
            (
                "v1.13.7",
                false,
                false,
                &[
                    "Obsidian-1.13.7.AppImage",
                    "Obsidian-1.13.7-arm64.AppImage",
                    "Obsidian-1.13.7.dmg",
                ],
            ),
            ("v1.13.6", false, false, &["Obsidian-1.13.6.AppImage"]),
        ];

        let release = followed(releases, Some("Obsidian-1.13.7.AppImage")).unwrap();
        assert_eq!(release.tag.as_deref(), Some("v1.13.7"));
        assert_eq!(release.version().as_deref(), Some("1.13.7"));
        assert_eq!(release.published.as_deref(), Some("2025-10-01"));
        // So the installed 1.13.7 is up to date.
        assert_eq!(
            compare_release(Some("1.13.7"), None, &release),
            (Some("1.13.7".to_string()), false, None)
        );
        assert_eq!(
            release.appimage(Some("Obsidian-1.13.7.AppImage"), Some(Arch::X86_64)).as_deref(),
            Ok("https://github.com/obsidianmd/obsidian-releases/releases/download/v1.13.7/Obsidian-1.13.7.AppImage")
        );

        // An older installation finds the same release, and an update in it.
        let release = followed(releases, Some("Obsidian-1.13.6.AppImage")).unwrap();
        assert_eq!(release.tag.as_deref(), Some("v1.13.7"));
        assert!(compare_release(Some("1.13.6"), None, &release).1);

        // Without a hint, the newest release built for this machine.
        let release = followed(releases, None).unwrap();
        assert_eq!(release.tag.as_deref(), Some("v1.13.7"));
    }

    #[test]
    fn a_pre_release_or_draft_newer_than_the_matching_release_is_passed_over() {
        let releases: &[Listed] = &[
            ("v1.14.0", true, false, &["Obsidian-1.14.0.AppImage"]),
            ("v1.14.0-beta.2", false, true, &["Obsidian-1.14.0-beta.2.AppImage"]),
            ("v1.13.8", false, false, &["app-release.apk"]),
            ("v1.13.7", false, false, &["Obsidian-1.13.7.AppImage"]),
        ];
        let release = followed(releases, Some("Obsidian-1.13.7.AppImage")).unwrap();
        assert_eq!(release.tag.as_deref(), Some("v1.13.7"));
    }

    #[test]
    fn when_no_release_has_it_the_newest_says_why_as_before() {
        let releases: &[Listed] = &[
            ("v1.14.0-beta.2", false, true, &["Obsidian-1.14.0-beta.2.AppImage"]),
            ("v1.13.8", false, false, &["app-release.apk"]),
            ("v1.13.7", false, false, &["Obsidian-1.13.7.dmg", "Obsidian.Setup.1.13.7.exe"]),
        ];
        let release = followed(releases, Some("Obsidian-1.13.6.AppImage")).unwrap();
        assert_eq!(release.tag.as_deref(), Some("v1.13.8"));
        let error = pick_appimage(&release, "obsidianmd", "obsidian-releases", None).unwrap_err();
        assert_eq!(
            error.to_string(),
            "release v1.13.8 of github:obsidianmd/obsidian-releases: the release has no AppImage"
        );

        // AppImages that are all something else are no match either, and
        // the newest release lists them.
        let releases: &[Listed] = &[
            ("v3", false, false, &["Other-3-x86_64.AppImage"]),
            ("v2", false, false, &["Other-2-x86_64.AppImage"]),
        ];
        let release = followed(releases, Some("Obsidian-1.13.7.AppImage")).unwrap();
        assert_eq!(release.tag.as_deref(), Some("v3"));
        let error =
            release.appimage(Some("Obsidian-1.13.7.AppImage"), Some(Arch::X86_64)).unwrap_err();
        assert!(
            error.starts_with("none of its AppImages matches Obsidian-1.13.7.AppImage"),
            "{error}"
        );
    }

    #[test]
    fn the_newest_release_with_a_zsync_file_for_the_pattern_is_followed() {
        let pattern = "imhex-*-x86_64.AppImage.zsync";
        let follows = |releases: &[Listed]| {
            newest_where(published_releases(&listing(releases)), |r| has_zsync_for(r, pattern))
        };

        // The newest release ships Windows only, and the one before it a
        // zsync file of another project; drafts and pre-releases do not
        // count however much they ship.
        let releases: &[Listed] = &[
            ("v1.39.0", true, false, &["imhex-1.39.0-x86_64.AppImage.zsync"]),
            ("v1.39.0-beta.1", false, true, &["imhex-1.39.0-beta.1-x86_64.AppImage.zsync"]),
            ("v1.38.2", false, false, &["imhex-1.38.2-Windows-x86_64.msi"]),
            ("v1.38.1", false, false, &["other-1.0-x86_64.AppImage.zsync"]),
            (
                "v1.38.0",
                false,
                false,
                &[
                    "imhex-1.38.0-arm64.AppImage",
                    "imhex-1.38.0-arm64.AppImage.zsync",
                    "imhex-1.38.0-x86_64.AppImage",
                    "imhex-1.38.0-x86_64.AppImage.zsync",
                ],
            ),
        ];
        let release = follows(releases).unwrap();
        assert_eq!(release.tag.as_deref(), Some("v1.38.0"));
        assert_eq!(
            zsync_asset_url(&release, pattern).map(|url| asset_name(&url)).as_deref(),
            Some("imhex-1.38.0-x86_64.appimage.zsync")
        );

        // A placeholder fits whatever it stands for.
        let placeholder = "imhex-*-{{ARCHITECTURE_FILE_NAME}}.AppImage.zsync";
        let release =
            newest_where(published_releases(&listing(releases)), |r| has_zsync_for(r, placeholder))
                .unwrap();
        assert_eq!(release.tag.as_deref(), Some("v1.38.0"));

        // When no release has one, the newest is taken all the same, and
        // the update falls back to the whole file as it always did.
        let releases: &[Listed] = &[
            ("v1.38.2", false, false, &["imhex-1.38.2-Windows-x86_64.msi"]),
            ("v1.38.1", false, false, &["imhex-1.38.1-x86_64.AppImage"]),
        ];
        let release = follows(releases).unwrap();
        assert_eq!(release.tag.as_deref(), Some("v1.38.2"));
        assert_eq!(zsync_asset_url(&release, pattern), None);
    }

    #[test]
    fn nothing_but_drafts_and_pre_releases_is_no_release() {
        let releases: &[Listed] = &[
            ("v2", true, false, &["App-2-x86_64.AppImage"]),
            ("v2-rc1", false, true, &["App-2-rc1-x86_64.AppImage"]),
        ];
        assert!(followed(releases, Some("App-1-x86_64.AppImage")).is_none());
        assert!(followed(&[], None).is_none());
    }

    /// osu! as it is installed through `github:ppy/osu`: the file calls
    /// itself `2026.921.0-lazer`, and so is the release it came out of
    /// tagged, but the version read out of that tag is `2026.921.0`.
    fn osu(recorded: Option<&str>) -> InstalledApp {
        InstalledApp {
            version: Some("2026.921.0-lazer".to_string()),
            release: recorded.map(str::to_string),
            ..installed(None, Some("github:ppy/osu"), Some("/home/me/Downloads/osu.AppImage"))
        }
    }

    fn osu_release(tag: &str) -> Release {
        release_with(tag, &["osu.AppImage", "osu.AppImage.zsync"])
    }

    fn compared(app: &InstalledApp, release: &Release) -> (Option<String>, bool, Option<String>) {
        let recorded = recorded_tag(app, "ppy", "osu");
        compare_release(app.version.as_deref(), recorded.as_deref(), release)
    }

    #[test]
    fn osu_is_up_to_date_once_the_tag_it_came_out_of_is_recorded() {
        // What the check said right after an update: an update to the very
        // release just installed, forever.
        let release = osu_release("2026.921.0-lazer");
        assert_eq!(release.version().as_deref(), Some("2026.921.0"));
        let app = osu(Some("github:ppy/osu@2026.921.0-lazer"));
        assert_eq!(compared(&app, &release), (Some("2026.921.0-lazer".to_string()), false, None));
        // The next release is one.
        let (current, available, _) = compared(&app, &osu_release("2026.1002.0-lazer"));
        assert_eq!(current.as_deref(), Some("2026.921.0-lazer"));
        assert!(available);

        // A release recorded for another repository says nothing about this
        // one, and the versions decide.
        let elsewhere = osu(Some("github:someone/osu-fork@2026.1002.0-lazer"));
        assert_eq!(recorded_tag(&elsewhere, "ppy", "osu"), None);
        assert!(compared(&elsewhere, &osu_release("2026.1002.0-lazer")).1);
    }

    #[test]
    fn without_a_recorded_tag_a_trailing_label_is_no_difference() {
        // Installed from a file, or by 0.2.x: no tag to go on, so the
        // versions are compared, `-lazer` and all.
        // This is the check that said "update available" forever.
        let app = osu(None);
        assert_eq!(
            compared(&app, &osu_release("2026.921.0-lazer")),
            (Some("2026.921.0-lazer".to_string()), false, None)
        );
        // However the release happens to be tagged.
        assert!(!compared(&app, &osu_release("2026.921.0")).1);
        assert!(compared(&app, &osu_release("2026.1002.0-lazer")).1);
        assert!(!compared(&app, &osu_release("2026.920.0-lazer")).1);
        // A pre-release is still older than the release it leads up to.
        let (_, available, _) = compare_release(Some("1.2.0-beta"), None, &osu_release("v1.2.0"));
        assert!(available);
    }

    #[test]
    fn a_tag_that_differs_only_by_a_leading_v_is_the_same_release() {
        let tagged = |recorded: &str, offered: &str| {
            let app = InstalledApp {
                version: Some("1.2.0".to_string()),
                release: Some(format!("github:ppy/osu@{recorded}")),
                ..osu(None)
            };
            compared(&app, &osu_release(offered)).1
        };
        assert!(!tagged("v1.2.0", "1.2.0"));
        assert!(!tagged("1.2.0", "v1.2.0"));
        assert!(!tagged("V1.2.0", "v1.2.0"));
        assert!(tagged("v1.2.0", "v1.2.1"));
        // A tag that merely starts with a v is not a version with one.
        assert!(tagged("vintage", "intage"));
        // Another tag is no update when the installed file came out of a
        // later release, a pre-release for instance.
        assert!(!tagged("v2.0.0-beta.1", "v1.9.0"));
    }

    #[test]
    fn the_release_a_file_came_out_of_is_recorded_as_one_source() {
        assert_eq!(
            release_of_download(
                "https://github.com/ppy/osu/releases/download/2026.921.0-lazer/osu.AppImage"
            )
            .as_deref(),
            Some("github:ppy/osu@2026.921.0-lazer")
        );
        assert_eq!(
            release_of_download("https://github.com/o/r/releases/download/v1/App.AppImage?x=1")
                .as_deref(),
            Some("github:o/r@v1")
        );
        for url in [
            "https://example.com/o/r/releases/download/v1/App.AppImage",
            "https://github.com/o/r/releases/download/v1/",
            "https://github.com/o/r/releases/tag/v1",
            "https://github.com/o/r",
        ] {
            assert_eq!(release_of_download(url), None, "{url}");
        }

        let from = Provenance::of("ppy", "osu", &osu_release("2026.921.0-lazer"));
        assert_eq!(from.release.as_deref(), Some("github:ppy/osu@2026.921.0-lazer"));
        assert_eq!(
            Provenance::of("ppy", "osu", &Release { tag: None, ..osu_release("x") }).release,
            None
        );

        // A link to the latest release names no release to record.
        assert_eq!(
            release_of_download("https://github.com/o/r/releases/latest/download/App.AppImage"),
            None
        );
    }

    #[test]
    fn every_asset_keeps_the_digest_its_release_publishes_for_it() {
        let sha256 = "ab".repeat(32);
        let url = |name: &str| format!("https://github.com/o/r/releases/download/v2/{name}");
        let body = format!(
            "{{\"assets_url\":\"https://api.github.com/x\",\"tag_name\":\"v2\",\"assets\":[\
             {{\"name\":\"App-2.AppImage\",\"uploader\":{{\"login\":\"bot\"}},\
             \"digest\":\"sha256:{sha256}\",\"browser_download_url\":\"{}\"}},\
             {{\"name\":\"App-2.AppImage.zsync\",\"digest\":null,\"browser_download_url\":\"{}\"}}],\
             \"body\":\"\\\"digest\\\": \\\"sha256:{}\\\"\"}}",
            url("App-2.AppImage"),
            url("App-2.AppImage.zsync"),
            "cd".repeat(32),
        );
        let release = parse_release(&body);

        assert_eq!(release.published_for(&url("App-2.AppImage")), Published::Sha256(sha256));
        assert_eq!(
            release.published_for(&url("App-2.AppImage.zsync")),
            Published::Nothing(digest::NONE_PUBLISHED.to_string())
        );
        assert!(matches!(
            release.published_for(&url("Other.AppImage")),
            Published::Nothing(reason) if reason.contains("not an asset")
        ));

        // A release from before GitHub published digests has no field at
        // all, which is the same as `null`.
        let old = published_releases(&listing(&[("v1", false, false, &["App-1.AppImage"])]));
        let asset = "https://github.com/obsidianmd/obsidian-releases/releases/download/v1/\
                     App-1.AppImage";
        assert_eq!(old[0].published_for(asset), Published::Nothing(digest::NONE_PUBLISHED.into()));

        // An update out of no release has nothing to check against and
        // nothing to say about it.
        assert_eq!(Provenance::default().published_for(asset), None);
        assert!(Provenance::of("o", "r", &old[0]).published_for(asset).is_some());
    }

    #[test]
    fn only_a_release_download_url_is_looked_up_at_all() {
        // None of these is asked about, they come out of no release.
        for url in [
            "https://example.com/o/r/releases/download/v1/App.AppImage",
            "https://github.com/o/r/releases/download/v1/",
            "https://github.com/o/r/releases/tag/v1",
            "https://github.com/o/r/archive/refs/tags/v1.tar.gz",
        ] {
            assert_eq!(published_for_download(url), None, "{url}");
        }

        let tagged = ReleaseDownload::parse(
            "https://github.com/o/r/releases/download/%40app%2Fv1/App%20One.AppImage?x=1",
        )
        .unwrap();
        assert_eq!((tagged.owner.as_str(), tagged.repo.as_str()), ("o", "r"));
        assert_eq!(tagged.tag.as_deref(), Some("%40app%2Fv1"));
        assert_eq!(tagged.asset, "App%20One.AppImage");

        let latest =
            ReleaseDownload::parse("https://github.com/o/r/releases/latest/download/App.AppImage")
                .unwrap();
        assert_eq!(latest.tag, None);
        assert_eq!(latest.asset, "App.AppImage");
    }

    #[test]
    fn update_sources_are_checked_and_stored_in_one_spelling() {
        for (given, stored) in [
            ("github:WerWolv/ImHex", "github:WerWolv/ImHex"),
            ("  github:o/r.js  ", "github:o/r.js"),
            ("github:o/r@continuous", "github:o/r@continuous"),
            // A tag the user names is followed as written, version or not.
            ("github:o/r@v1.2.0", "github:o/r@v1.2.0"),
            ("https://github.com/o/r", "github:o/r"),
            ("https://github.com/o/r/", "github:o/r"),
            ("https://www.github.com/o/r.git", "github:o/r"),
            ("https://github.com/o/r/releases", "github:o/r"),
            ("https://github.com/o/r/releases/latest", "github:o/r"),
            ("https://github.com/o/r/releases/tag/continuous", "github:o/r@continuous"),
            ("https://github.com/o/r/releases/tag/v1.2.0", "github:o/r"),
            (
                "https://github.com/o/r/releases/download/v1/App-1-x86_64.AppImage",
                "https://github.com/o/r/releases/download/v1/App-1-x86_64.AppImage",
            ),
            ("https://example.com/App.AppImage", "https://example.com/App.AppImage"),
            ("http://example.com/get?app=1", "http://example.com/get?app=1"),
        ] {
            assert_eq!(parse_update_source(given).ok().as_deref(), Some(stored), "{given}");
        }

        for given in [
            "",
            "manual",
            "owner/repo",
            "github:owner",
            "github:/repo",
            "github:o/r/x",
            "github:o/r@",
            "github:o/r@a b",
            "github:o/..",
            "ftp://example.com/App.AppImage",
            "https://",
            "https://example.com/a b",
            "https://github.com/o",
            "https://github.com/o/r/issues",
        ] {
            assert!(
                matches!(parse_update_source(given), Err(Error::InvalidUpdateSource(_))),
                "{given}"
            );
        }
    }

    fn installed(
        update_info: Option<&str>,
        update_source: Option<&str>,
        origin: Option<&str>,
    ) -> InstalledApp {
        InstalledApp {
            slug: "app".to_string(),
            name: "App".to_string(),
            comment: None,
            categories: Vec::new(),
            version: None,
            origin: origin.map(str::to_string),
            update_info: update_info.map(str::to_string),
            update_source: update_source.map(str::to_string),
            release: None,
            installed_at: None,
            appimage_path: PathBuf::from("/nowhere/app.AppImage"),
            desktop_entry_path: PathBuf::from("/nowhere/app.desktop"),
            size_bytes: None,
            health: crate::list::Health::Ok,
        }
    }

    fn github(owner: &str, repo: &str, tag: Option<&str>, asset: Option<&str>) -> UpdateSource {
        UpdateSource::GitHubRelease {
            owner: owner.to_string(),
            repo: repo.to_string(),
            tag: tag.map(str::to_string),
            asset: asset.map(str::to_string),
        }
    }

    #[test]
    fn embedded_update_information_comes_before_the_update_source() {
        let app = installed(
            Some("zsync|https://example.com/App.AppImage.zsync"),
            Some("github:o/r"),
            Some("https://example.com/App.AppImage"),
        );
        assert!(matches!(source_for(&app), UpdateSource::Zsync { .. }));
    }

    #[test]
    fn the_update_source_comes_before_the_origin() {
        let origin = Some("/home/me/Downloads/App-1.0-x86_64.AppImage");
        assert_eq!(
            source_for(&installed(None, Some("github:o/r@continuous"), origin)),
            github("o", "r", Some("continuous"), Some("App-1.0-x86_64.AppImage"))
        );
        assert_eq!(
            source_for(&installed(None, Some("github:o/r"), None)),
            github("o", "r", None, None)
        );
        assert_eq!(
            source_for(&installed(None, Some("https://example.com/App.AppImage"), origin)),
            UpdateSource::DirectUrl { url: "https://example.com/App.AppImage".to_string() }
        );
        assert_eq!(
            source_for(&installed(
                None,
                Some("https://github.com/o/r/releases/download/continuous/App-x86_64.AppImage"),
                origin
            )),
            github("o", "r", Some("continuous"), Some("App-x86_64.AppImage"))
        );

        // Manual is manual, whatever the origin was, and so is a value that
        // is no update source at all.
        let url = Some("https://example.com/App.AppImage");
        assert_eq!(source_for(&installed(None, Some(MANUAL), url)), UpdateSource::Manual);
        assert_eq!(source_for(&installed(None, Some("nonsense"), url)), UpdateSource::Manual);
    }

    #[test]
    fn an_entry_written_by_0_2_follows_a_url_origin_and_nothing_else() {
        let url = "https://example.com/App.AppImage";
        assert_eq!(
            source_for(&installed(None, None, Some(url))),
            UpdateSource::DirectUrl { url: url.to_string() }
        );
        let download = "https://github.com/o/r/releases/download/v1.0/App-1.0-x86_64.AppImage";
        assert_eq!(
            source_for(&installed(None, None, Some(download))),
            github("o", "r", None, Some("App-1.0-x86_64.AppImage"))
        );

        // A local file is history, even while it is still there.
        let dir = tempfile::tempdir().unwrap();
        let still_there = dir.path().join("App.AppImage");
        fs::write(&still_there, "old build").unwrap();
        let local = still_there.to_string_lossy();
        assert_eq!(source_for(&installed(None, None, Some(&local))), UpdateSource::Manual);
        assert_eq!(source_for(&installed(None, None, None)), UpdateSource::Manual);
        assert_eq!(UpdateSource::Manual.describe(), "manual");
    }

    #[test]
    fn a_github_repository_is_recognised_however_it_is_written() {
        assert_eq!(github_repository("github:o/r@continuous").as_deref(), Some("o/r"));
        assert_eq!(github_repository("https://github.com/o/r").as_deref(), Some("o/r"));
        assert_eq!(
            github_repository("https://github.com/o/r/releases/download/v1/App.AppImage")
                .as_deref(),
            Some("o/r")
        );
        assert_eq!(github_repository("https://example.com/App.AppImage"), None);
        assert_eq!(github_repository(MANUAL), None);
    }

    /// The AppImageUpdate continuous release as the API returns it, and the
    /// two builds out of it that started this.
    fn continuous_release() -> Release {
        Release {
            tag: Some("continuous".to_string()),
            assets: vec!["https://x/AppImageUpdate-x86_64.AppImage".to_string()],
            digests: Vec::new(),
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
                compare_release(Some(installed), None, &continuous_release());
            assert_eq!(current.as_deref(), Some("2025-10-18"), "{installed}");
            assert!(!available, "{installed}");
            assert_eq!(note, None);
        }
    }

    #[test]
    fn another_commit_on_the_channel_is_an_update() {
        let (current, available, _) =
            compare_release(Some("255-b0b0b0b"), None, &continuous_release());
        assert_eq!(current.as_deref(), Some("b0b0b0b"));
        assert!(available);
    }

    #[test]
    fn two_dates_say_which_build_is_older() {
        // What the file was recorded as after its last update, against what
        // the channel offers now.
        let (current, available, note) =
            compare_release(Some("2025-09-01"), None, &continuous_release());
        assert_eq!(current.as_deref(), Some("2025-09-01"));
        assert!(available, "an older date is an update");
        assert_eq!(note, None);

        let newer = Release { published: Some("2025-09-01".to_string()), ..continuous_release() };
        let (_, available, _) = compare_release(Some("2025-10-18"), None, &newer);
        assert!(!available, "a newer date is not an update");

        let (_, available, _) = compare_release(Some("2025-10-18"), None, &continuous_release());
        assert!(!available, "the same date is not an update");
    }

    #[test]
    fn a_version_release_is_still_compared_as_a_version() {
        let release = Release {
            tag: Some("v2.0.0".to_string()),
            assets: vec!["https://x/App-2.0.0-x86_64.AppImage".to_string()],
            digests: Vec::new(),
            published: Some("2025-10-18".to_string()),
            commit: Some("a211784".to_string()),
        };
        let (current, available, note) = compare_release(Some("1.9.0"), None, &release);
        assert_eq!(current.as_deref(), Some("1.9.0"));
        assert!(available);
        assert_eq!(note, None);

        let (_, available, _) = compare_release(Some("2.0.0"), None, &release);
        assert!(!available);

        // Nothing installed is an update, as it always was.
        assert!(compare_release(None, None, &release).1);
    }

    #[test]
    fn a_build_id_against_a_version_is_not_guessed_at() {
        // The build was installed off the continuous channel and the source
        // now offers a numbered release: neither is older than the other,
        // and saying so beats ordering `a211784` against `2.0.0`.
        let release = Release {
            tag: Some("v2.0.0".to_string()),
            assets: vec!["https://x/App-2.0.0-x86_64.AppImage".to_string()],
            digests: Vec::new(),
            published: Some("2025-10-18".to_string()),
            commit: None,
        };
        let (current, available, note) = compare_release(Some("a211784"), None, &release);
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
            blocksize: None,
            hash_lengths: zsync::HashLengths::DEFAULT,
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
