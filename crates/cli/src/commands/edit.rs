use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use appimg_core::desktop_entry::{self, DesktopEntry};
use appimg_core::{caches, list, Paths};

use crate::cli::EditArgs;
use crate::ui::Ui;
use crate::Outcome;

/// Keys appimg owns. A user may edit anything else, but losing these would
/// turn the entry into something appimg no longer manages.
const MANAGED_KEYS: &[&str] = &[
    desktop_entry::KEY_MANAGED,
    desktop_entry::KEY_SLUG,
    desktop_entry::KEY_SOURCE,
    desktop_entry::KEY_VERSION,
    desktop_entry::KEY_UPDATE_INFO,
    desktop_entry::KEY_INSTALLED_AT,
];

/// What came out of an editing session.
pub struct Edited {
    pub changed: bool,
    /// Whatever `desktop-file-validate` said about the saved entry.
    pub warnings: Vec<String>,
}

pub fn run(paths: &Paths, ui: &Ui, args: &EditArgs) -> Result<Outcome> {
    if !ui.is_interactive() {
        bail!("editing needs a terminal");
    }

    let app = list::find(paths, &args.name)?;
    let edited = edit_entry(paths, &app.desktop_entry_path)?;
    if !edited.changed {
        ui.info("Nothing changed.");
        return Ok(Outcome::NothingToDo);
    }

    for warning in &edited.warnings {
        ui.warn(warning);
    }
    ui.info(&format!("Updated {}.", app.desktop_entry_path.display()));
    Ok(Outcome::Done)
}

/// Opens a desktop entry in `$EDITOR` and writes it back. The keys appimg
/// owns survive even when the user deletes them. A saved entry is validated
/// again and the desktop database and the icon cache are refreshed, because
/// the name, the categories or the icon may have changed.
pub fn edit_entry(paths: &Paths, entry_path: &Path) -> Result<Edited> {
    let original = DesktopEntry::read(entry_path)?;
    let mut edited = edit_in_editor(&original)?;
    if edited == original {
        return Ok(Edited { changed: false, warnings: Vec::new() });
    }

    for key in MANAGED_KEYS {
        if edited.get(key).is_none() {
            if let Some(value) = original.get(key) {
                edited.set(*key, value);
            }
        }
    }
    desktop_entry::validate_categories(&edited.categories())?;

    edited.write(entry_path)?;
    let warnings = caches::validate_desktop_entry(entry_path);
    caches::refresh(paths);
    Ok(Edited { changed: true, warnings })
}

fn edit_in_editor(entry: &DesktopEntry) -> Result<DesktopEntry> {
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());

    let dir = tempfile::Builder::new().prefix("appimg-edit-").tempdir()?;
    let file = dir.path().join("entry.desktop");
    std::fs::write(&file, entry.to_string())?;

    let status = Command::new(&editor)
        .arg(&file)
        .status()
        .with_context(|| format!("cannot start the editor {editor:?}"))?;
    if !status.success() {
        bail!("the editor {editor:?} exited with {status}");
    }

    let text = std::fs::read_to_string(&file)?;
    Ok(DesktopEntry::parse(&text))
}
