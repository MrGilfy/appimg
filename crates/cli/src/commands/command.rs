use anyhow::Result;
use appimg_core::command::{self, Change, State};
use appimg_core::list::InstalledApp;
use appimg_core::{list, Paths};

use crate::cli::CommandArgs;
use crate::ui::Ui;
use crate::Outcome;

/// Shows the command of an application, gives it one, or takes it away.
/// Asking for what it has already changes nothing, says so, and is exit
/// code 3, the way `hold` does.
pub fn run(paths: &Paths, ui: &Ui, args: &CommandArgs) -> Result<Outcome> {
    let app = list::find(paths, &args.name)?;
    let name = ui.bold(&app.name);

    if args.remove {
        let Some((command, removed)) = command::unset(&app, paths)? else {
            ui.info(&format!("{name} has no command."));
            return Ok(Outcome::NothingToDo);
        };
        let link = command::link_path(paths, &command).display().to_string();
        ui.info(&format!("{name} no longer runs as `{command}`."));
        if removed {
            ui.info(&format!("  removed {link}"));
        } else if std::fs::symlink_metadata(&link).is_ok() {
            ui.info(&format!("  {link} no longer ran it, so it stays where it is"));
        }
        return Ok(Outcome::Done);
    }

    let Some(command) = &args.command else {
        return Ok(show(paths, ui, &app));
    };
    match command::set(paths, &app, command)? {
        Change::Unchanged => {
            ui.info(&format!("{name} runs as `{command}` already."));
            Ok(Outcome::NothingToDo)
        }
        Change::Linked { replaced } => {
            ui.info(&format!("{name} runs as `{command}` now."));
            ui.info(&format!("  link {}", link_line(paths, &app, command)));
            match replaced {
                Some((previous, true)) => ui.info(&format!("  `{previous}` is gone")),
                Some((previous, false)) => ui.info(&format!(
                    "  `{previous}` is no longer its command, and {} stays where it is, it no \
                     longer ran it",
                    command::link_path(paths, &previous).display()
                )),
                None => {}
            }
            warn_off_path(paths, ui, command);
            Ok(Outcome::Done)
        }
    }
}

fn show(paths: &Paths, ui: &Ui, app: &InstalledApp) -> Outcome {
    let name = ui.bold(&app.name);
    let Some(command) = &app.command else {
        ui.info(&format!(
            "{name} has no command. Add one with: appimg command {} <name>",
            app.slug
        ));
        return Outcome::NothingToDo;
    };
    ui.info(&format!("{name} runs as `{command}`."));
    match command::state(paths, &app.slug, command) {
        State::Linked => ui.info(&format!("  link {}", link_line(paths, app, command))),
        state => ui.info(&format!("  {}", describe(paths, &app.slug, command, &state))),
    }
    warn_off_path(paths, ui, command);
    Outcome::Done
}

fn link_line(paths: &Paths, app: &InstalledApp, command: &str) -> String {
    format!(
        "{} -> {}",
        command::link_path(paths, command).display(),
        paths.appimage_path(&app.slug).display()
    )
}

/// What is wrong with the link of a recorded command, and how to mend it.
pub(crate) fn describe(paths: &Paths, slug: &str, command: &str, state: &State) -> String {
    let link = command::link_path(paths, command).display().to_string();
    let again = format!("create it again with `appimg command {slug} {command}`");
    match state {
        State::Linked => format!("{link} runs it"),
        State::Missing => format!("{link} is missing, {again}"),
        State::Broken { target } => {
            format!("{link} points at {}, which is not there", target.display())
        }
        State::Elsewhere { target } => format!("{link} runs {} instead", target.display()),
        State::NotALink => format!("{link} is no link appimg created, it stays where it is"),
        State::InvalidName => format!("the entry records {command:?}, which no command can be"),
    }
}

/// A command in a directory the shell does not look in runs by its path
/// only.
pub(crate) fn warn_off_path(paths: &Paths, ui: &Ui, command: &str) {
    if !command::bin_dir_on_path(paths) {
        ui.warn(&format!(
            "{} is not on PATH, so a shell does not find `{command}` by its name: add it to PATH",
            paths.bin_dir.display()
        ));
    }
}
