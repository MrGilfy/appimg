mod cli;
mod commands;
mod tui;
mod ui;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, Command};
use crate::ui::Ui;

/// What a command achieved, which is what the exit code reports.
pub enum Outcome {
    Done,
    /// Nothing needed doing: exit code 3.
    NothingToDo,
}

fn main() -> ExitCode {
    let args = match Cli::try_parse() {
        Ok(args) => args,
        Err(error) => {
            let _ = error.print();
            // Help and version are a success, everything else is a usage error.
            return if error.use_stderr() { ExitCode::from(2) } else { ExitCode::SUCCESS };
        }
    };

    let ui = Ui::new(args.no_color, args.yes);
    match run(&args, &ui) {
        Ok(Outcome::Done) => ExitCode::SUCCESS,
        Ok(Outcome::NothingToDo) => ExitCode::from(3),
        Err(error) => {
            eprintln!("{} {error:#}", ui.bold("error:"));
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Cli, ui: &Ui) -> anyhow::Result<Outcome> {
    let paths = appimg_core::Paths::from_env()?;

    match &args.command {
        Some(Command::Install(install_args)) => commands::install::run(&paths, ui, install_args),
        Some(Command::List(list_args)) => commands::list::run(&paths, ui, list_args),
        Some(Command::Update(update_args)) => commands::update::run(&paths, ui, update_args),
        Some(Command::Remove(remove_args)) => commands::remove::run(&paths, ui, remove_args),
        Some(Command::Edit(edit_args)) => commands::edit::run(&paths, ui, edit_args),
        Some(Command::Doctor) => commands::doctor::run(&paths, ui),
        Some(Command::Completions(completion_args)) => {
            commands::completions::run(ui, completion_args)
        }
        None => tui::run(&paths, ui),
    }
}
