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
use crate::{caches, icon, json, version, zsync};

const GITHUB_API: &str = "https://api.github.com";

/// How an application can be updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateSource {
    /// `X-AppImg-UpdateInfo` pointing at a zsync file. The check reads that
    /// file's header directly, applying the delta needs `appimageupdatetool`.
    Zsync {
        update_info: String,
    },
    /// A GitHub release, queried through the API.
    GitHubRelease {
        owner: String,
        repo: String,
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
            status.latest_version =
                header.filename.as_deref().and_then(version::extract).or_else(|| current.clone());
            let (available, note) = zsync_compare(&header, &app.appimage_path)?;
            status.available = available;
            status.note = note;
        }
        UpdateSource::GitHubRelease { owner, repo, asset } => {
            let release = latest_release(owner, repo)?;
            status.latest_version = release.version();
            status.available = match (&current, &status.latest_version) {
                (Some(current), Some(latest)) => version::is_newer(latest, current),
                (None, Some(_)) => true,
                _ => false,
            };
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
            finish(paths, app, &target, backup, source)
        }
        UpdateSource::GitHubRelease { owner, repo, asset } => {
            let release = latest_release(owner, repo)?;
            let url = release
                .asset_url(asset.as_deref())
                .ok_or_else(|| Error::NoUpdateInfo(app.slug.clone()))?;
            let staged = download_staged(paths, &app.slug, &url, progress)?;
            let backup = swap_in(&staged, &target)?;
            finish(paths, app, &target, Some(backup), source)
        }
        UpdateSource::DirectUrl { url } => {
            let staged = download_staged(paths, &app.slug, url, progress)?;
            let backup = swap_in(&staged, &target)?;
            finish(paths, app, &target, Some(backup), source)
        }
        UpdateSource::LocalFile { path } => {
            let staged = paths.appimage_dir.join(format!("{}.AppImage.new", app.slug));
            fs_util::copy_atomic(path, &staged, MODE_EXEC)?;
            let backup = swap_in(&staged, &target)?;
            finish(paths, app, &target, Some(backup), source)
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
fn finish(
    paths: &Paths,
    app: &InstalledApp,
    target: &Path,
    backup: Option<PathBuf>,
    source: UpdateSource,
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

    let new_version = info
        .as_ref()
        .and_then(|info| info.version.clone())
        .or_else(|| version::extract(&target.file_name().unwrap_or_default().to_string_lossy()));

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
    let asset = rest.rsplit('/').next().map(str::to_string);
    Some(UpdateSource::GitHubRelease { owner, repo, asset })
}

struct Release {
    tag: Option<String>,
    assets: Vec<String>,
}

impl Release {
    fn version(&self) -> Option<String> {
        self.tag
            .as_deref()
            .and_then(version::extract)
            .or_else(|| self.assets.first().and_then(|url| version::extract(url)))
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

fn latest_release(owner: &str, repo: &str) -> Result<Release> {
    let url = format!("{GITHUB_API}/repos/{owner}/{repo}/releases/latest");
    let body = download::to_string(&url)?;
    Ok(Release {
        tag: json::string_field(&body, "tag_name"),
        assets: json::string_fields(&body, "browser_download_url"),
    })
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
                asset: Some("App-*x86_64.AppImage.zsync".to_string()),
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
                asset: Some("App.AppImage".to_string()),
            })
        );
        assert_eq!(github_source_from_url("https://example.com/App.AppImage"), None);
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
        };
        assert_eq!(release.asset_url(None), None);
    }
}
