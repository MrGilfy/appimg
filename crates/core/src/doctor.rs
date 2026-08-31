use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

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
    /// Icons whose slug has no desktop entry any more.
    pub orphaned_icons: Vec<PathBuf>,
    /// AppImages in the managed directory that no entry refers to.
    pub orphaned_appimages: Vec<PathBuf>,
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
            && self.orphaned_appimages.is_empty()
            && self.broken_entries.is_empty()
    }
}

pub fn run(paths: &Paths) -> Result<DoctorReport> {
    let apps = list::list(paths)?;
    let slugs: HashSet<String> = apps.iter().map(|app| app.slug.clone()).collect();

    Ok(DoctorReport {
        libfuse2: has_libfuse2(),
        xdg_data_home_in_search_path: data_home_is_searched(paths),
        applications_dir_writable: is_writable(&paths.applications_dir),
        required_tools: check_tools(REQUIRED_TOOLS),
        optional_tools: check_tools(OPTIONAL_TOOLS),
        orphaned_appimages: collect_orphaned_appimages(&paths.appimage_dir, &slugs),
        broken_entries: apps
            .iter()
            .filter(|app| app.health != Health::Ok)
            .map(|app| (app.slug.clone(), app.desktop_entry_path.clone()))
            .collect(),
        orphaned_icons: collect_orphaned_icons(&paths.icons_root, &slugs),
    })
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

fn collect_orphaned_icons(icons_root: &Path, slugs: &HashSet<String>) -> Vec<PathBuf> {
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
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // Only icons that look like ours are our business.
            if !slugs.contains(stem) && was_installed_by_appimg(&path) {
                orphans.push(path);
            }
        }
    }
    orphans.sort();
    orphans
}

/// An icon counts as ours when it sits in `<size>/apps` and no other
/// application in the tree claims it. Anything outside that layout belongs to
/// the distribution and is left alone.
fn was_installed_by_appimg(icon: &Path) -> bool {
    icon.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) == Some("apps")
}

fn collect_orphaned_appimages(appimage_dir: &Path, slugs: &HashSet<String>) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(appimage_dir) else {
        return Vec::new();
    };

    let mut orphans: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("AppImage"))
        .filter(|path| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(|stem| !slugs.contains(stem))
                .unwrap_or(false)
        })
        .collect();

    orphans.sort();
    orphans
}
