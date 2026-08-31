use anyhow::Result;
use clap::CommandFactory;

use crate::cli::{Cli, CompletionsArgs};
use crate::ui::Ui;
use crate::Outcome;

pub fn run(ui: &Ui, args: &CompletionsArgs) -> Result<Outcome> {
    let mut command = Cli::command();
    let mut script = Vec::new();
    clap_complete::generate(
        clap_complete::Shell::from(args.shell),
        &mut command,
        "appimg",
        &mut script,
    );
    ui.raw(&script);
    Ok(Outcome::Done)
}
