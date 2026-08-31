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

pub fn run(paths: &Paths, ui: &Ui, args: &EditArgs) -> Result<Outcome> {
    if !ui.is_interactive() {
        bail!("editing needs a terminal");
    }

    let app = list::find(paths, &args.name)?;
    let original = DesktopEntry::read(&app.desktop_entry_path)?;

    let edited = edit_in_editor(&original)?;
    if edited == original {
        ui.info("Nothing changed.");
        return Ok(Outcome::NothingToDo);
    }

    let mut edited = edited;
    for key in MANAGED_KEYS {
        if edited.get(key).is_none() {
            if let Some(value) = original.get(key) {
                edited.set(*key, value);
            }
        }
    }
    desktop_entry::validate_categories(&edited.categories())?;

    edited.write(&app.desktop_entry_path)?;
    for warning in caches::validate_desktop_entry(&app.desktop_entry_path) {
        ui.warn(&warning);
    }
    caches::refresh(paths);

    ui.info(&format!("Updated {}.", app.desktop_entry_path.display()));
    Ok(Outcome::Done)
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
