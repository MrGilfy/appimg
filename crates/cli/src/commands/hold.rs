use anyhow::Result;
use appimg_core::{hold, list, Paths};

use crate::cli::HoldArgs;
use crate::ui::Ui;
use crate::Outcome;

/// `hold` with `held`, `unhold` without. Asking for the state an application
/// is in already changes nothing, says so, and is exit code 3.
pub fn run(paths: &Paths, ui: &Ui, args: &HoldArgs, held: bool) -> Result<Outcome> {
    let app = list::find(paths, &args.name)?;
    let name = ui.bold(&app.name);
    if !hold::set(&app, held)? {
        ui.info(&if held {
            format!("{name} is held already.")
        } else {
            format!("{name} is not held.")
        });
        return Ok(Outcome::NothingToDo);
    }
    if held {
        let at = app.version.as_deref().map(|version| format!(" at {version}")).unwrap_or_default();
        ui.info(&format!("{name} is held{at}."));
        ui.info(&format!(
            "update --all, the terminal interface and update notifications pass it over, and \
             appimg update {} asks first. Release it with: appimg unhold {}",
            app.slug, app.slug
        ));
    } else {
        ui.info(&format!("{name} is no longer held, it updates like any other."));
    }
    Ok(Outcome::Done)
}
