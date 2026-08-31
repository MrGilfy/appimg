use anyhow::Result;
use appimg_core::{list, remove, Paths};

use crate::cli::RemoveArgs;
use crate::ui::Ui;
use crate::Outcome;

pub fn run(paths: &Paths, ui: &Ui, args: &RemoveArgs) -> Result<Outcome> {
    let app = list::find(paths, &args.name)?;
    let plan = remove::plan(paths, &app.slug)?;

    ui.info(&format!("Removing {} deletes:", ui.bold(&app.name)));
    for file in plan.files() {
        ui.info(&format!("  {}", file.display()));
    }

    if !ui.confirm("Delete these files?", false)? {
        ui.info("Nothing was changed.");
        return Ok(Outcome::NothingToDo);
    }

    let removed = remove::remove(paths, &app.slug)?;
    ui.info(&format!("Removed {} ({} files).", ui.bold(&app.name), removed.files().len()));
    Ok(Outcome::Done)
}
