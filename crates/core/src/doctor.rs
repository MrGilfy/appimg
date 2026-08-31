use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::desktop_entry::DesktopEntry;
use crate::error::Result;
use crate::fs_util;
use crate::list::{self, Health};
use crate::paths::Paths;

const LIBRARY_DIRS: &[&str] =
    &["/usr/lib", "/usr/lib64", "/usr/lib/x86_64-linux-gnu", "/lib", "/lib64", "/usr/local/lib"];

const REQUIRED_TOOLS: &[(&str, &str)] = &[
    ("update-desktop-database", "desktop entries are picked up later than they could be"),
    ("gtk-update-icon-cache", "icons may not show up until the session restarts"),
    ("desktop-file-validate", "generated entries cannot be validated"),
];

const OPTIONAL_TOOLS: &[(&str, &str)] = &[
    (
        "appimageupdatetool",
        "delta updates via zsync are unavailable, updates re-download the whole file",
    ),
    ("unsquashfs", "AppImages with a broken runtime cannot be inspected"),
];

#[derive(Debug, Clone)]
pub struct ToolStatus {
    pub name: String,
    pub found: bool,
    pub consequence: String,
}

#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub libfuse2: bool,
    pub xdg_data_home_in_search_path: bool,
    pub applications_dir_writable: bool,
    pub required_tools: Vec<ToolStatus>,
    pub optional_tools: Vec<ToolStatus>,
    /// Icons of a slug appimg manages whose entry no longer refers to them.
    pub orphaned_icons: Vec<PathBuf>,
    /// `<slug>.AppImage.bak` and `.new` files an interrupted update left
    /// behind, for slugs appimg manages.
    pub leftover_files: Vec<PathBuf>,
    /// Managed entries whose AppImage or slug is missing.
    pub broken_entries: Vec<(String, PathBuf)>,
}

impl DoctorReport {
    /// True when nothing needs attention.
    pub fn is_clean(&self) -> bool {
        self.libfuse2
            && self.applications_dir_writable
            && self.required_tools.iter().all(|t| t.found)
            && self.orphaned_icons.is_empty()
            && self.leftover_files.is_empty()
            && self.broken_entries.is_empty()
    }
}

/// Looks the environment over and hunts for leftovers of appimg's own
/// installations.
///
/// Only files that belong to a slug with `X-AppImg-Managed=true` are ever
/// considered. The icon theme and the AppImage directory are shared with
/// everything else on the machine, and a file appimg did not put there is
/// none of its business: it is never reported and never offered for
/// deletion.
pub fn run(paths: &Paths) -> Result<DoctorReport> {
    let apps = list::list(paths)?;
    let managed = managed_icon_names(&apps);

    Ok(DoctorReport {
        libfuse2: has_libfuse2(),
        xdg_data_home_in_search_path: data_home_is_searched(paths),
        applications_dir_writable: is_writable(&paths.applications_dir),
        required_tools: check_tools(REQUIRED_TOOLS),
        optional_tools: check_tools(OPTIONAL_TOOLS),
        leftover_files: collect_leftovers(paths, &managed),
        broken_entries: apps
            .iter()
            .filter(|app| app.health != Health::Ok)
            .map(|app| (app.slug.clone(), app.desktop_entry_path.clone()))
            .collect(),
        orphaned_icons: collect_orphaned_icons(&paths.icons_root, &managed),
    })
}

/// The slugs appimg manages, each with the icon name its entry uses. An
/// entry that fell back to a generic icon no longer refers to the icons that
/// were installed under its slug.
fn managed_icon_names(apps: &[list::InstalledApp]) -> HashMap<String, String> {
    apps.iter()
        .filter(|app| !app.slug.is_empty())
        .map(|app| {
            let icon = DesktopEntry::read(&app.desktop_entry_path)
                .ok()
                .and_then(|entry| entry.get("Icon").map(str::to_string))
                .unwrap_or_default();
            (app.slug.clone(), icon)
        })
        .collect()
}

fn check_tools(tools: &[(&str, &str)]) -> Vec<ToolStatus> {
    tools
        .iter()
        .map(|(name, consequence)| ToolStatus {
            name: (*name).to_string(),
            found: fs_util::which(name).is_some(),
            consequence: (*consequence).to_string(),
        })
        .collect()
}

fn has_libfuse2() -> bool {
    LIBRARY_DIRS.iter().any(|dir| {
        fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .any(|entry| entry.file_name().to_string_lossy().starts_with("libfuse.so.2"))
            })
            .unwrap_or(false)
    })
}

/// Desktop environments always read `$XDG_DATA_HOME/applications`, but a
/// `XDG_DATA_DIRS` that points somewhere else entirely is worth reporting.
fn data_home_is_searched(paths: &Paths) -> bool {
    let Some(dirs) = env::var_os("XDG_DATA_DIRS") else {
        return true;
    };
    let dirs = dirs.to_string_lossy();
    if dirs.trim().is_empty() {
        return false;
    }
    dirs.split(':').any(|dir| Path::new(dir).is_dir()) || paths.data_home.is_dir()
}

fn is_writable(dir: &Path) -> bool {
    if !dir.exists() {
        return dir.parent().map(is_writable).unwrap_or(false);
    }
    tempfile::NamedTempFile::new_in(dir).is_ok()
}

/// Icons named after a slug appimg manages, in the `<size>/apps` layout it
/// writes, that the slug's entry does not use any more. Icons of other
/// applications share the same tree and are skipped, whatever they are
/// called.
fn collect_orphaned_icons(icons_root: &Path, managed: &HashMap<String, String>) -> Vec<PathBuf> {
    let mut orphans = Vec::new();
    let mut stack = vec![icons_root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if !in_apps_directory(&path) {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            match managed.get(stem) {
                // The entry still points at these icons, they are in use.
                Some(icon) if icon == stem => {}
                Some(_) => orphans.push(path),
                // Not one of ours, so not ours to report.
                None => {}
            }
        }
    }
    orphans.sort();
    orphans
}

fn in_apps_directory(icon: &Path) -> bool {
    icon.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) == Some("apps")
}

/// The staging and backup files an interrupted update leaves behind. Both
/// are named after a managed slug, so they are provably appimg's own.
fn collect_leftovers(paths: &Paths, managed: &HashMap<String, String>) -> Vec<PathBuf> {
    let mut leftovers: Vec<PathBuf> = managed
        .keys()
        .flat_map(|slug| {
            ["AppImage.bak", "AppImage.new"]
                .iter()
                .map(|suffix| paths.appimage_dir.join(format!("{slug}.{suffix}")))
                .collect::<Vec<_>>()
        })
        .filter(|path| path.is_file())
        .collect();

    leftovers.sort();
    leftovers
}
