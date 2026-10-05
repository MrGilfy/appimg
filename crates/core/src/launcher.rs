//! Whether the application launcher lists an application. A command-line
//! tool needs no place in the menu, but appimg knows the applications it
//! manages by their desktop entries and nothing else, so the entry stays and
//! says `NoDisplay=true`: what the freedesktop specification calls an
//! application that exists but is not shown in menus. Launchers pass it
//! over, a file manager can still open files with it, and every command of
//! appimg works on it as on any other. (`Hidden=true` would say it was
//! deleted.)
//!
//! An application installed that way gets no icons, nothing shows them.
//! Listing it again installs the ones its AppImage ships. Taking an
//! application out of the launcher leaves the icons it has where they are,
//! one picked by hand among them, so listing it again is the way it was.

use std::path::PathBuf;

use crate::caches;
use crate::desktop_entry::DesktopEntry;
use crate::error::Result;
use crate::icon;
use crate::install::FALLBACK_ICON;
use crate::list::InstalledApp;
use crate::metadata::{self, Reading};
use crate::paths::Paths;

pub const KEY_NO_DISPLAY: &str = "NoDisplay";

/// Whether the entry keeps the application out of the launcher.
pub fn is_hidden(entry: &DesktopEntry) -> bool {
    entry.get(KEY_NO_DISPLAY) == Some("true")
}

/// Keeps the application of `entry` out of the launcher.
pub fn hide_in(entry: &mut DesktopEntry) {
    entry.set(KEY_NO_DISPLAY, "true");
}

/// Whether an entry of `slug` goes without icons of its own: it is out of
/// the launcher and never had any, so an update installs none either.
pub fn without_icons(entry: &DesktopEntry, slug: &str) -> bool {
    is_hidden(entry) && entry.get("Icon") != Some(slug)
}

/// Takes `app` out of the launcher, or lists it again. `None` when it is
/// that way already, nothing changed. Listing it again installs the icons
/// its AppImage ships when the entry uses none of its own, and returns
/// them; reading them can run the AppImage when `unsquashfs` cannot.
pub fn set(paths: &Paths, app: &InstalledApp, hidden: bool) -> Result<Option<Vec<PathBuf>>> {
    let mut entry = DesktopEntry::read(&app.desktop_entry_path)?;
    if is_hidden(&entry) == hidden {
        return Ok(None);
    }

    let mut icons = Vec::new();
    if hidden {
        hide_in(&mut entry);
    } else {
        entry.remove(KEY_NO_DISPLAY);
        if entry.get("Icon").is_none_or(|icon| icon == FALLBACK_ICON) {
            icons = icons_of(paths, &app.slug);
            if !icons.is_empty() {
                entry.set("Icon", app.slug.clone());
            }
        }
    }
    entry.write(&app.desktop_entry_path)?;
    caches::refresh(paths);
    Ok(Some(icons))
}

/// The icons the installed AppImage of `slug` ships, installed under the
/// slug. None when it ships none or cannot be read.
fn icons_of(paths: &Paths, slug: &str) -> Vec<PathBuf> {
    // Asked for, of an application the user installed, the way an update
    // reads it.
    let Ok(info) = metadata::inspect(&paths.appimage_path(slug), None, Reading::MayRun) else {
        return Vec::new();
    };
    match info.extract_root() {
        Some(root) => icon::install_icons(root, info.icon_name.as_deref(), slug, &paths.icons_root),
        None => Vec::new(),
    }
}
