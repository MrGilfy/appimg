use std::fs;
use std::path::PathBuf;

use crate::desktop_entry::{self, DesktopEntry};
use crate::error::{Error, Result};
use crate::fs_util;
use crate::paths::Paths;

/// What is left of an installation on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ok,
    /// The desktop entry is there, the AppImage is gone.
    MissingBinary,
    /// The entry claims to be managed but has no slug to work with.
    Incomplete,
}

#[derive(Debug, Clone)]
pub struct InstalledApp {
    pub slug: String,
    pub name: String,
    pub comment: Option<String>,
    pub categories: Vec<String>,
    pub version: Option<String>,
    pub origin: Option<String>,
    pub update_info: Option<String>,
    pub installed_at: Option<String>,
    pub appimage_path: PathBuf,
    pub desktop_entry_path: PathBuf,
    pub size_bytes: Option<u64>,
    pub health: Health,
}

impl InstalledApp {
    pub fn is_broken(&self) -> bool {
        self.health != Health::Ok
    }
}

/// Every managed desktop entry, sorted by name. The desktop entries are the
/// only source of truth, there is no state file.
pub fn list(paths: &Paths) -> Result<Vec<InstalledApp>> {
    let entries = match fs::read_dir(&paths.applications_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io(&paths.applications_dir, e)),
    };

    let mut apps = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
            continue;
        }
        let Ok(desktop_entry) = DesktopEntry::read(&path) else {
            continue;
        };
        if !desktop_entry.is_managed() {
            continue;
        }
        apps.push(build_app(paths, &desktop_entry, path));
    }

    apps.sort_by_key(|a| a.name.to_lowercase());
    Ok(apps)
}

/// Resolves a user-supplied name to exactly one installed application. Slug,
/// exact name and a unique case-insensitive prefix all work.
pub fn find(paths: &Paths, query: &str) -> Result<InstalledApp> {
    let apps = list(paths)?;
    let needle = query.trim().to_lowercase();

    if let Some(app) = apps.iter().find(|a| a.slug == needle || a.name.to_lowercase() == needle) {
        return Ok(app.clone());
    }

    let matches: Vec<&InstalledApp> = apps
        .iter()
        .filter(|a| a.slug.starts_with(&needle) || a.name.to_lowercase().starts_with(&needle))
        .collect();

    match matches.as_slice() {
        [single] => Ok((*single).clone()),
        [] => Err(Error::NotInstalled(query.to_string())),
        several => {
            let names = several.iter().map(|a| a.slug.as_str()).collect::<Vec<_>>().join(", ");
            Err(Error::Ambiguous(query.to_string(), names))
        }
    }
}

fn build_app(paths: &Paths, entry: &DesktopEntry, desktop_entry_path: PathBuf) -> InstalledApp {
    let slug = entry
        .slug()
        .map(str::to_string)
        .or_else(|| desktop_entry_path.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_default();

    let appimage_path = paths.appimage_path(&slug);
    let health = if entry.slug().is_none() {
        Health::Incomplete
    } else if !appimage_path.is_file() {
        Health::MissingBinary
    } else {
        Health::Ok
    };

    InstalledApp {
        name: entry.get("Name").unwrap_or(&slug).to_string(),
        comment: entry.get("Comment").map(str::to_string),
        categories: entry.categories(),
        version: entry.get(desktop_entry::KEY_VERSION).map(str::to_string),
        origin: entry.get(desktop_entry::KEY_SOURCE).map(str::to_string),
        update_info: entry.get(desktop_entry::KEY_UPDATE_INFO).map(str::to_string),
        installed_at: entry.get(desktop_entry::KEY_INSTALLED_AT).map(str::to_string),
        size_bytes: fs_util::file_size(&appimage_path),
        appimage_path,
        desktop_entry_path,
        health,
        slug,
    }
}
