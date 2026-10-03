use anyhow::Result;
use appimg_core::{export, fs_util, Paths};

use crate::cli::ExportArgs;
use crate::ui::Ui;
use crate::Outcome;

/// Writes every managed application to the file, or to standard output
/// without one, where nothing else is written.
pub fn run(paths: &Paths, ui: &Ui, args: &ExportArgs) -> Result<Outcome> {
    let apps = export::collect(paths)?;
    let text = export::to_json(&apps);
    match &args.file {
        None => ui.raw(text.as_bytes()),
        Some(file) => {
            fs_util::write_atomic(file, text.as_bytes(), 0o644)?;
            let count = apps.len();
            ui.info(&format!(
                "Exported {count} {} to {}",
                if count == 1 { "app" } else { "apps" },
                file.display()
            ));
        }
    }
    Ok(Outcome::Done)
}
