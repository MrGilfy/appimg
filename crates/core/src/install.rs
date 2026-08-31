use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::desktop_entry::{self, DesktopEntry};
use crate::error::{Error, Result};
use crate::fs_util::{self, MODE_EXEC};
use crate::icon;
use crate::metadata::AppImageInfo;
use crate::paths::Paths;
use crate::{caches, slug};

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
    /// Replace an existing installation with the same slug.
    pub overwrite: bool,
}

impl InstallRequest {
    /// Builds a request from what the AppImage itself declares. Callers
    /// override the fields the user edited.
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
            overwrite: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct InstallOutcome {
    pub slug: String,
    pub appimage_path: PathBuf,
    pub desktop_entry_path: PathBuf,
    pub icons: Vec<PathBuf>,
    pub validation_warnings: Vec<String>,
    pub replaced: bool,
}

/// Everything an installation would write, without writing it.
#[derive(Debug, Clone)]
pub struct InstallPlan {
    pub slug: String,
    pub appimage_path: PathBuf,
    pub desktop_entry_path: PathBuf,
    pub desktop_entry: DesktopEntry,
    pub already_installed: bool,
}

pub fn plan(paths: &Paths, request: &InstallRequest) -> Result<InstallPlan> {
    let slug = slug::slugify(&request.name)?;
    let appimage_path = paths.appimage_path(&slug);
    let desktop_entry_path = paths.desktop_entry_path(&slug);
    let categories = effective_categories(request)?;
    let icon_field = planned_icon_field(request, &slug);

    Ok(InstallPlan {
        desktop_entry: build_entry(request, &slug, &appimage_path, &categories, &icon_field),
        already_installed: desktop_entry_path.exists() || appimage_path.exists(),
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

    let icons = install_icons(paths, request, &plan.slug);
    let icon_field = if icons.is_empty() { FALLBACK_ICON.to_string() } else { plan.slug.clone() };

    let mut entry = plan.desktop_entry;
    entry.set("Icon", icon_field);
    entry.write(&plan.desktop_entry_path)?;

    let validation_warnings = caches::validate_desktop_entry(&plan.desktop_entry_path);
    caches::refresh(paths);

    Ok(InstallOutcome {
        slug: plan.slug,
        appimage_path: plan.appimage_path,
        desktop_entry_path: plan.desktop_entry_path,
        icons,
        validation_warnings,
        replaced: plan.already_installed,
    })
}

fn install_icons(paths: &Paths, request: &InstallRequest, slug: &str) -> Vec<PathBuf> {
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
    entry.set(desktop_entry::KEY_SOURCE, request.origin.clone());
    entry.set_optional(desktop_entry::KEY_VERSION, request.version.clone());
    entry.set_optional(desktop_entry::KEY_UPDATE_INFO, request.update_info.clone());
    entry.set(desktop_entry::KEY_INSTALLED_AT, timestamp());
    entry
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
            overwrite: false,
        }
    }

    fn paths_in(dir: &Path) -> Paths {
        Paths {
            data_home: dir.to_path_buf(),
            config_home: dir.join("config"),
            appimage_dir: dir.join("appimages"),
            applications_dir: dir.join("applications"),
            icons_root: dir.join("icons/hicolor"),
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
}
