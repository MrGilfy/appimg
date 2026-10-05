use anyhow::Result;
use appimg_core::{launcher, list, Paths};

use crate::cli::HoldArgs;
use crate::ui::Ui;
use crate::Outcome;

/// `hide` with `hidden`, `unhide` without. Asking for the state an
/// application is in already changes nothing, says so, and is exit code 3,
/// the way `hold` does.
pub fn run(paths: &Paths, ui: &Ui, args: &HoldArgs, hidden: bool) -> Result<Outcome> {
    let app = list::find(paths, &args.name)?;
    let name = ui.bold(&app.name);
    let Some(icons) = launcher::set(paths, &app, hidden)? else {
        ui.info(&if hidden {
            format!("{name} is out of the launcher already.")
        } else {
            format!("{name} is in the launcher already.")
        });
        return Ok(Outcome::NothingToDo);
    };

    if !hidden {
        ui.info(&format!("{name} is in the launcher again."));
        if !icons.is_empty() {
            ui.info(&format!("  icons   {} installed into the hicolor theme", icons.len()));
        }
        return Ok(Outcome::Done);
    }
    ui.info(&format!(
        "{name} is out of the launcher. appimg manages it as before, list it again with: \
         appimg unhide {}",
        app.slug
    ));
    match &app.command {
        Some(command) => ui.info(&format!("  runs as the command {command}")),
        None => warn_without_command(ui, &app.slug),
    }
    Ok(Outcome::Done)
}

/// An application out of the launcher with no command runs by its path
/// only.
pub(crate) fn warn_without_command(ui: &Ui, slug: &str) {
    ui.warn(&format!(
        "nothing lists it now and it has no command, so it runs by its path only: give it one \
         with appimg command {slug} <name>"
    ));
}
