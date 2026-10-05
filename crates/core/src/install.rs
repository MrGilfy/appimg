use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::desktop_entry::{self, DesktopEntry};
use crate::digest::{self, Verified};
use crate::error::{Error, Result};
use crate::fs_util::{self, MODE_EXEC};
use crate::hold::{self, Hold};
use crate::icon;
use crate::metadata::AppImageInfo;
use crate::paths::Paths;
use crate::remote::{self, Remote};
use crate::{archive, caches, command, download, elf, launcher, slug, stamp, update};

pub const FALLBACK_ICON: &str = "application-x-executable";
const DEFAULT_CATEGORY: &str = "Utility";

/// Which icon an installation should end up with. Choosing a file or giving
/// up is a decision for the caller, `appimg-core` never asks.
#[derive(Debug, Clone, Default)]
pub enum IconChoice {
    /// Take whatever the AppImage ships.
    #[default]
    Embedded,
    /// Use this image file.
    File(PathBuf),
    /// Use the generic executable icon.
    Fallback,
}

/// A fully decided installation. Everything interactive has already happened
/// by the time this reaches the core.
#[derive(Debug, Clone)]
pub struct InstallRequest {
    /// The local AppImage file to install, already downloaded if it came from a URL.
    pub source: PathBuf,
    /// What to record as the origin: the original path or the URL.
    pub origin: String,
    pub name: String,
    pub comment: Option<String>,
    pub categories: Vec<String>,
    pub extra_args: Vec<String>,
    pub terminal: bool,
    pub startup_wm_class: Option<String>,
    pub mime_type: Option<String>,
    pub field_code: Option<String>,
    pub icon: IconChoice,
    pub icon_name: Option<String>,
    pub extract_root: Option<PathBuf>,
    pub version: Option<String>,
    pub update_info: Option<String>,
    /// What an update follows when the AppImage embeds no update
    /// information, as [`update::parse_update_source`] returns it. `None`
    /// updates manually.
    pub update_source: Option<String>,
    /// The GitHub release the file came out of, as `github:owner/repo@tag`,
    /// when it was downloaded from one.
    pub release: Option<String>,
    /// What the server said about the file, when it was downloaded from a
    /// URL, for the first check of that URL to compare with, see
    /// [`crate::remote`].
    pub remote: Option<Remote>,
    /// The slug to install under instead of the one the name gives: an
    /// import keeps the one the application had, whatever it was renamed to.
    pub slug: Option<String>,
    /// Replace an existing installation with the same slug.
    pub overwrite: bool,
    /// The command to run it by, a link in `~/.local/bin`, see
    /// [`crate::command`]. `None` keeps the one of the application it
    /// replaces, if any.
    pub command: Option<String>,
    /// Keep it out of the application launcher, with no icons, see
    /// [`crate::launcher`]. One that replaces an application out of the
    /// launcher stays out either way.
    pub hidden: bool,
}

impl InstallRequest {
    /// Builds a request from what the AppImage itself declares. Callers
    /// override the fields the user edited. An AppImage downloaded from a
    /// URL updates from that URL; one installed from a local file updates
    /// manually, and what its AppStream metadata suggests is for the caller
    /// to ask about, see [`suggested_update_source`].
    pub fn from_info(source: &Path, origin: &str, info: &AppImageInfo) -> Self {
        Self {
            source: source.to_path_buf(),
            origin: origin.to_string(),
            name: info.name.clone().unwrap_or_default(),
            comment: info.comment.clone(),
            categories: info.categories.clone(),
            extra_args: Vec::new(),
            terminal: info.terminal,
            startup_wm_class: info.startup_wm_class.clone(),
            mime_type: info.mime_type.clone(),
            field_code: info.field_code.clone(),
            icon: IconChoice::Embedded,
            icon_name: info.icon_name.clone(),
            extract_root: info.extract_root().map(Path::to_path_buf),
            version: info.version.clone(),
            update_info: info.update_info.clone(),
            update_source: download::is_url(origin)
                .then(|| update::parse_update_source(origin).ok())
                .flatten(),
            release: update::release_of_download(origin),
            remote: None,
            slug: None,
            overwrite: false,
            command: None,
            hidden: false,
        }
    }
}

/// The checks a file on disk gets before anything reads its metadata, which
/// can run it, and before it is installed or adopted: the same ones a download
/// gets, see [`elf::check_whole`].
pub fn check_file(path: &Path) -> Result<()> {
    elf::check_whole(path).map_err(|unfit| Error::Unfit { path: path.to_path_buf(), unfit })
}

/// Takes the one AppImage out of an archive on disk and writes it into
/// `dir`, see [`archive::extract_appimage`], then gives it the checks a
/// download gets. It is named after the archive, without the ending that
/// makes it one, so the name and version a file name gives come from the
/// file the user picked, never from anything inside it. Returns where it is
/// and what the archive called it. A file that fails the checks is gone
/// again before the error, which names the archive, comes back.
pub fn unpack_archive(archive: &Path, dir: &Path) -> Result<(PathBuf, archive::Extracted)> {
    let name = archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = archive::strip_archive_suffix(&name).unwrap_or(&name);
    // Room for the ending within the 255 bytes a file name has.
    let stem = if stem.is_empty() || stem.len() > 200 { "unpacked" } else { stem };
    let dest = dir.join(format!("{stem}.AppImage"));
    let extracted = archive::extract_appimage(archive, &dest)?;
    if let Err(unfit) = elf::check_whole(&dest) {
        let _ = std::fs::remove_file(&dest);
        return Err(Error::Archive {
            archive: archive.display().to_string(),
            reason: format!("{}: {unfit}", extracted.entry),
        });
    }
    Ok((dest, extracted))
}

/// Checks an AppImage downloaded from `url` against the digest its GitHub
/// release publishes, when it came out of one: one request for that
/// release. This has to happen right after the download, before anything
/// reads the metadata out of the file, which can run it. A file that does not
/// match is removed before the error comes back. `None` for a URL that is
/// no GitHub release download, which has nothing to check against.
pub fn verify_download(file: &Path, url: &str) -> Result<Option<Verified>> {
    let Some(published) = update::published_for_download(url) else {
        return Ok(None);
    };
    match digest::verify(file, url, &published) {
        Ok(verified) => Ok(Some(verified)),
        Err(error) => {
            let _ = std::fs::remove_file(file);
            Err(error)
        }
    }
}

/// The update source the AppStream metadata inside the AppImage suggests,
/// when it is worth asking about: the AppImage embeds no update
/// information, which would come first, and the request does not follow
/// that repository already. Never applied by itself.
pub fn suggested_update_source(request: &InstallRequest, info: &AppImageInfo) -> Option<String> {
    let suggested = info.suggested_update_source.clone()?;
    if request.update_info.is_some() {
        return None;
    }
    let following = request.update_source.as_deref().and_then(update::release_repository);
    let offered = update::release_repository(&suggested);
    let same = following.zip(offered).is_some_and(|(a, b)| a.eq_ignore_ascii_case(&b));
    (!same).then_some(suggested)
}

#[derive(Debug, Clone)]
pub struct InstallOutcome {
    pub slug: String,
    pub appimage_path: PathBuf,
    pub desktop_entry_path: PathBuf,
    pub icons: Vec<PathBuf>,
    pub validation_warnings: Vec<String>,
    pub replaced: bool,
    /// Whether it replaced a held application, whose hold it kept.
    pub held: bool,
    /// The update source it kept from the application it replaced.
    pub kept_update_source: Option<String>,
    /// The command it has, asked for or kept from the application it
    /// replaced.
    pub command: Option<String>,
    /// Why the link of the command it asked for could not be created. The
    /// installation is complete, without that command.
    pub command_failed: Option<String>,
    /// Whether the launcher passes it over.
    pub hidden: bool,
}

/// Everything an installation would write, without writing it.
#[derive(Debug, Clone)]
pub struct InstallPlan {
    pub slug: String,
    pub appimage_path: PathBuf,
    pub desktop_entry_path: PathBuf,
    pub desktop_entry: DesktopEntry,
    pub already_installed: bool,
    /// Whether the application it replaces is held. The hold stays: holding
    /// an application and installing an older version over it is what a
    /// hold is for.
    pub keeps_hold: bool,
    /// The update source of the application it replaces, which it keeps
    /// because it would update manually otherwise, see
    /// [`update_source_to_keep`].
    pub kept_update_source: Option<String>,
    /// What [`InstallRequest::remote`] says, recorded with the checksum of
    /// the installed file once it is in place.
    pub remote: Option<Remote>,
    /// The command it gets: the one asked for, which is free, see
    /// [`command::check_free`], or the one of the application it replaces.
    pub command: Option<String>,
    /// The command of the application it replaces.
    pub replaced_command: Option<String>,
    /// It stays out of the launcher, with no icons: asked for, or the way
    /// the application it replaces was.
    pub hidden: bool,
    /// The application it replaces was out of the launcher, and so it stays.
    pub keeps_hidden: bool,
}

pub fn plan(paths: &Paths, request: &InstallRequest) -> Result<InstallPlan> {
    let slug = match &request.slug {
        Some(slug) => {
            slug::check(slug)?;
            slug.clone()
        }
        None => slug::slugify(&request.name)?,
    };
    let appimage_path = paths.appimage_path(&slug);
    let desktop_entry_path = paths.desktop_entry_path(&slug);
    let categories = effective_categories(request)?;
    let replaced = DesktopEntry::read(&desktop_entry_path).ok().filter(DesktopEntry::is_managed);
    let keeps_hidden = replaced.as_ref().is_some_and(launcher::is_hidden);
    let hidden = request.hidden || keeps_hidden;
    let icon_field =
        if hidden { FALLBACK_ICON.to_string() } else { planned_icon_field(request, &slug) };
    let mut desktop_entry = build_entry(request, &slug, &appimage_path, &categories, &icon_field);
    if hidden {
        launcher::hide_in(&mut desktop_entry);
    }
    let keeps_hold = replaced.as_ref().is_some_and(|replaced| Hold::of(replaced).is_some());
    if keeps_hold {
        hold::keep_in(&mut desktop_entry);
    }
    let kept_update_source =
        replaced.as_ref().and_then(|replaced| update_source_to_keep(request, replaced));
    if let Some(source) = &kept_update_source {
        desktop_entry.set(desktop_entry::KEY_UPDATE_SOURCE, source.clone());
    }
    let replaced_command = replaced
        .as_ref()
        .and_then(|replaced| replaced.get(desktop_entry::KEY_COMMAND))
        .map(str::to_string);
    if let Some(name) = &request.command {
        command::check_free(paths, &slug, name)?;
    }
    let command = request.command.clone().or_else(|| replaced_command.clone());
    if let Some(name) = &command {
        desktop_entry.set(desktop_entry::KEY_COMMAND, name.clone());
    }

    Ok(InstallPlan {
        desktop_entry,
        keeps_hold,
        kept_update_source,
        already_installed: desktop_entry_path.exists() || appimage_path.exists(),
        remote: request.remote.clone(),
        command,
        replaced_command,
        hidden,
        keeps_hidden,
        slug,
        appimage_path,
        desktop_entry_path,
    })
}

/// Installs the AppImage: binary, icons and desktop entry, then refreshes the
/// caches.
pub fn install(paths: &Paths, request: &InstallRequest) -> Result<InstallOutcome> {
    let plan = plan(paths, request)?;

    if plan.already_installed && !request.overwrite {
        return Err(Error::AlreadyInstalled {
            name: request.name.clone(),
            slug: plan.slug.clone(),
        });
    }
    if !request.source.exists() {
        return Err(Error::NotFound(request.source.clone()));
    }
    if !request.source.is_file() {
        return Err(Error::NotAFile(request.source.clone()));
    }

    paths.ensure_dirs()?;

    if plan.already_installed {
        // Stale icons of the previous version must not survive the replacement.
        for icon_path in fs_util::find_files_with_stem(&paths.icons_root, &plan.slug)? {
            let _ = std::fs::remove_file(icon_path);
        }
    }

    fs_util::copy_atomic(&request.source, &plan.appimage_path, MODE_EXEC)?;

    // Nothing shows the icons of an application out of the launcher.
    let icons = if plan.hidden { Vec::new() } else { install_icons(paths, request, &plan.slug) };
    write_entry(&plan, &icons)?;
    let (command, command_failed) = link_command(paths, request, &plan)?;

    let validation_warnings = caches::validate_desktop_entry(&plan.desktop_entry_path);
    caches::refresh(paths);

    Ok(InstallOutcome {
        slug: plan.slug,
        appimage_path: plan.appimage_path,
        desktop_entry_path: plan.desktop_entry_path,
        icons,
        validation_warnings,
        replaced: plan.already_installed,
        held: plan.keeps_hold,
        kept_update_source: plan.kept_update_source,
        command,
        command_failed,
        hidden: plan.hidden,
    })
}

/// Creates the link of the command the request asks for, once the AppImage
/// and its entry are in place, and removes the link of the one the replaced
/// application had instead. A kept command keeps its link, the AppImage
/// is where it was. When the link cannot be created, the entry goes back
/// to the command it had before, and why comes back with it.
fn link_command(
    paths: &Paths,
    request: &InstallRequest,
    plan: &InstallPlan,
) -> Result<(Option<String>, Option<String>)> {
    let Some(name) = &request.command else {
        return Ok((plan.command.clone(), None));
    };
    if command::own_link(paths, &plan.slug, name).is_none() {
        if let Err(error) = command::create(paths, &plan.slug, name) {
            command::record(&plan.desktop_entry_path, plan.replaced_command.as_deref())?;
            return Ok((plan.replaced_command.clone(), Some(error.to_string())));
        }
    }
    if let Some(previous) = plan.replaced_command.as_deref().filter(|previous| previous != name) {
        command::remove_link(paths, &plan.slug, previous)?;
    }
    Ok((Some(name.clone()), None))
}

/// Writes the planned desktop entry, with the icon the installed icons
/// make it, or the generic one without any, the checksum of the AppImage
/// that is in place by now, and what the server said about it.
pub(crate) fn write_entry(plan: &InstallPlan, icons: &[PathBuf]) -> Result<()> {
    let icon_field = if icons.is_empty() { FALLBACK_ICON.to_string() } else { plan.slug.clone() };
    let mut entry = plan.desktop_entry.clone();
    entry.set("Icon", icon_field);
    stamp::record(&mut entry, &plan.appimage_path, None);
    remote::record_in(&mut entry, plan.remote.as_ref());
    entry.write(&plan.desktop_entry_path)
}

pub(crate) fn install_icons(paths: &Paths, request: &InstallRequest, slug: &str) -> Vec<PathBuf> {
    match &request.icon {
        IconChoice::Fallback => Vec::new(),
        IconChoice::File(file) => icon::install_icon(file, slug, &paths.icons_root)
            .map(|path| vec![path])
            .unwrap_or_default(),
        IconChoice::Embedded => match &request.extract_root {
            Some(root) => {
                icon::install_icons(root, request.icon_name.as_deref(), slug, &paths.icons_root)
            }
            None => Vec::new(),
        },
    }
}

fn planned_icon_field(request: &InstallRequest, slug: &str) -> String {
    match &request.icon {
        IconChoice::Fallback => FALLBACK_ICON.to_string(),
        IconChoice::File(_) => slug.to_string(),
        IconChoice::Embedded => match &request.extract_root {
            Some(root) if !icon::find_icons(root, request.icon_name.as_deref()).is_empty() => {
                slug.to_string()
            }
            _ => FALLBACK_ICON.to_string(),
        },
    }
}

fn effective_categories(request: &InstallRequest) -> Result<Vec<String>> {
    if request.categories.is_empty() {
        return Ok(vec![DEFAULT_CATEGORY.to_string()]);
    }
    desktop_entry::validate_categories(&request.categories)?;
    Ok(request.categories.clone())
}

fn build_entry(
    request: &InstallRequest,
    slug: &str,
    appimage_path: &Path,
    categories: &[String],
    icon_field: &str,
) -> DesktopEntry {
    let mut entry = DesktopEntry::new();
    entry.set("Type", "Application");
    entry.set("Name", request.name.trim());
    if let Some(comment) = request.comment.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        entry.set("Comment", comment);
    }
    entry.set(
        "Exec",
        desktop_entry::build_exec_line(
            appimage_path,
            &request.extra_args,
            request.field_code.as_deref(),
        ),
    );
    entry.set("Icon", icon_field);
    entry.set("Terminal", if request.terminal { "true" } else { "false" });
    entry.set_categories(categories);
    entry.set_optional("StartupWMClass", request.startup_wm_class.clone());
    entry.set_optional("MimeType", request.mime_type.clone());
    entry.set("StartupNotify", "true");
    entry.set(desktop_entry::KEY_MANAGED, "true");
    entry.set(desktop_entry::KEY_SLUG, slug);
    entry.set(desktop_entry::KEY_ORIGIN, request.origin.clone());
    entry.set_optional(desktop_entry::KEY_VERSION, request.version.clone());
    entry.set_optional(desktop_entry::KEY_UPDATE_INFO, request.update_info.clone());
    // Always written, so an entry without it is one written by 0.2.x.
    entry.set(
        desktop_entry::KEY_UPDATE_SOURCE,
        request.update_source.clone().unwrap_or_else(|| update::MANUAL.to_string()),
    );
    entry.set_optional(desktop_entry::KEY_RELEASE, request.release.clone());
    entry.set(desktop_entry::KEY_INSTALLED_AT, timestamp());
    entry
}

/// The update source an install over `replaced` keeps: the one it had,
/// when the install would leave the application to update manually
/// otherwise. An update source the request names, from `--update-source`
/// or the URL it was installed from, comes first, and so does update
/// information the new AppImage embeds. Nothing to keep is no source:
/// `manual`, or a value that is no update source.
pub fn update_source_to_keep(request: &InstallRequest, replaced: &DesktopEntry) -> Option<String> {
    if request.update_source.is_some() || request.update_info.is_some() {
        return None;
    }
    // `manual` is no update source, so it is nothing to keep either.
    update::parse_update_source(replaced.get(desktop_entry::KEY_UPDATE_SOURCE)?).ok()
}

/// Seconds since the epoch, the least surprising timestamp format that needs
/// no date library.
fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(name: &str) -> InstallRequest {
        InstallRequest {
            source: PathBuf::from("/tmp/source.AppImage"),
            origin: "/tmp/source.AppImage".to_string(),
            name: name.to_string(),
            comment: Some("A sample".to_string()),
            categories: vec!["Utility".to_string()],
            extra_args: Vec::new(),
            terminal: false,
            startup_wm_class: None,
            mime_type: None,
            field_code: None,
            icon: IconChoice::Fallback,
            icon_name: None,
            extract_root: None,
            version: Some("1.2.3".to_string()),
            update_info: None,
            update_source: None,
            release: None,
            remote: None,
            slug: None,
            overwrite: false,
            command: None,
            hidden: false,
        }
    }

    fn paths_in(dir: &Path) -> Paths {
        Paths {
            data_home: dir.to_path_buf(),
            config_home: dir.join("config"),
            state_home: dir.join("state"),
            appimage_dir: dir.join("appimages"),
            applications_dir: dir.join("applications"),
            icons_root: dir.join("icons/hicolor"),
            bin_dir: dir.join("bin"),
        }
    }

    #[test]
    fn the_entry_carries_the_management_keys() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(&paths_in(dir.path()), &request("Sample App")).unwrap();

        assert_eq!(plan.slug, "sample-app");
        assert_eq!(plan.desktop_entry.get("Type"), Some("Application"));
        assert_eq!(plan.desktop_entry.get(desktop_entry::KEY_MANAGED), Some("true"));
        assert_eq!(plan.desktop_entry.get(desktop_entry::KEY_SLUG), Some("sample-app"));
        assert_eq!(plan.desktop_entry.get(desktop_entry::KEY_VERSION), Some("1.2.3"));
        assert!(plan.desktop_entry.get(desktop_entry::KEY_INSTALLED_AT).is_some());
    }

    #[test]
    fn the_exec_line_points_at_the_installed_binary() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(&paths_in(dir.path()), &request("Sample App")).unwrap();
        let expected = format!("\"{}/appimages/sample-app.AppImage\"", dir.path().display());
        assert_eq!(plan.desktop_entry.get("Exec"), Some(expected.as_str()));
    }

    #[test]
    fn field_codes_only_appear_when_the_appimage_declared_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut req = request("Sample App");
        req.field_code = Some("%U".to_string());
        let plan = plan(&paths_in(dir.path()), &req).unwrap();
        assert!(plan.desktop_entry.get("Exec").unwrap().ends_with(" %U"));
    }

    #[test]
    fn without_categories_the_entry_falls_back_to_utility() {
        let dir = tempfile::tempdir().unwrap();
        let mut req = request("Sample App");
        req.categories.clear();
        let plan = plan(&paths_in(dir.path()), &req).unwrap();
        assert_eq!(plan.desktop_entry.get("Categories"), Some("Utility;"));
    }

    #[test]
    fn invalid_categories_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut req = request("Sample App");
        req.categories = vec!["Nonsense".to_string()];
        assert!(plan(&paths_in(dir.path()), &req).is_err());
    }

    #[test]
    fn names_without_usable_characters_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(plan(&paths_in(dir.path()), &request("   ")).is_err());
    }

    #[test]
    fn without_an_icon_the_generic_one_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(&paths_in(dir.path()), &request("Sample App")).unwrap();
        assert_eq!(plan.desktop_entry.get("Icon"), Some(FALLBACK_ICON));
    }

    /// Over an installed application, a request that names no update source
    /// keeps the one it had. One that names one, update information the new
    /// file embeds, and a replaced one that had none, keep nothing.
    #[test]
    fn a_replace_keeps_the_update_source_only_when_it_would_end_up_manual() {
        let replaced = |source: Option<&str>| {
            let mut entry = DesktopEntry::new();
            entry.set_optional(desktop_entry::KEY_UPDATE_SOURCE, source);
            entry
        };
        let github = replaced(Some("github:o/r@continuous#App-*.AppImage"));
        assert_eq!(
            update_source_to_keep(&request("A"), &github).as_deref(),
            Some("github:o/r@continuous#App-*.AppImage")
        );
        let url = replaced(Some("https://example.com/latest/App.AppImage"));
        assert_eq!(
            update_source_to_keep(&request("A"), &url).as_deref(),
            Some("https://example.com/latest/App.AppImage")
        );

        let named = InstallRequest { update_source: Some("github:o/other".into()), ..request("A") };
        assert_eq!(update_source_to_keep(&named, &github), None);
        let embeds = InstallRequest {
            update_info: Some("zsync|https://e.com/A.zsync".into()),
            ..request("A")
        };
        assert_eq!(update_source_to_keep(&embeds, &github), None);
        for nothing in [None, Some("manual"), Some("not a source")] {
            assert_eq!(
                update_source_to_keep(&request("A"), &replaced(nothing)),
                None,
                "{nothing:?}"
            );
        }
    }

    #[test]
    fn the_plan_writes_the_kept_source_into_the_entry_and_only_over_a_managed_one() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let entry_path = paths.desktop_entry_path("sample-app");
        let mut existing = DesktopEntry::new();
        existing.set(desktop_entry::KEY_UPDATE_SOURCE, "github:o/r");
        existing.write(&entry_path).unwrap();
        // Not one appimg manages: nothing of it is kept.
        let plan = plan(&paths, &request("Sample App")).unwrap();
        assert_eq!(plan.kept_update_source, None);
        assert_eq!(plan.desktop_entry.get(desktop_entry::KEY_UPDATE_SOURCE), Some(update::MANUAL));

        existing.set(desktop_entry::KEY_MANAGED, "true");
        existing.write(&entry_path).unwrap();
        let plan = super::plan(&paths, &request("Sample App")).unwrap();
        assert_eq!(plan.kept_update_source.as_deref(), Some("github:o/r"));
        assert_eq!(plan.desktop_entry.get(desktop_entry::KEY_UPDATE_SOURCE), Some("github:o/r"));
    }
}
