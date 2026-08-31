use std::fs;
use std::path::PathBuf;

use crate::caches;
use crate::error::{Error, Result};
use crate::fs_util;
use crate::paths::Paths;

#[derive(Debug, Clone)]
pub struct RemovalPlan {
    pub slug: String,
    pub desktop_entry: PathBuf,
    pub appimage: Option<PathBuf>,
    pub icons: Vec<PathBuf>,
    pub leftovers: Vec<PathBuf>,
}

impl RemovalPlan {
    /// Every file the removal will delete, in the order it will happen.
    pub fn files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        files.extend(self.appimage.clone());
        files.extend(self.icons.iter().cloned());
        files.extend(self.leftovers.iter().cloned());
        files.push(self.desktop_entry.clone());
        files
    }
}

/// Lists what removing `slug` would delete, without touching anything.
pub fn plan(paths: &Paths, slug: &str) -> Result<RemovalPlan> {
    let desktop_entry = paths.desktop_entry_path(slug);
    let appimage = paths.appimage_path(slug);
    if !desktop_entry.exists() && !appimage.exists() {
        return Err(Error::NotInstalled(slug.to_string()));
    }

    // A failed update can leave these behind, they belong to the slug too.
    let leftovers = ["AppImage.new", "AppImage.bak"]
        .iter()
        .map(|suffix| paths.appimage_dir.join(format!("{slug}.{suffix}")))
        .filter(|path| path.exists())
        .collect();

    Ok(RemovalPlan {
        slug: slug.to_string(),
        icons: fs_util::find_files_with_stem(&paths.icons_root, slug)?,
        appimage: appimage.exists().then_some(appimage),
        desktop_entry,
        leftovers,
    })
}

/// Deletes the AppImage, its icons and its desktop entry, then refreshes the
/// caches.
pub fn remove(paths: &Paths, slug: &str) -> Result<RemovalPlan> {
    let plan = plan(paths, slug)?;

    for file in plan.files() {
        match fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(&file, e)),
        }
    }

    caches::refresh(paths);
    Ok(plan)
}
