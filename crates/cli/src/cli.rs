use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Install, update and remove AppImages as proper desktop applications.
#[derive(Debug, Parser)]
#[command(
    name = "appimg",
    version,
    about,
    long_about = "Installs AppImages into the user's home: the binary goes to \
                  $XDG_DATA_HOME/appimages, icons into the hicolor theme and a desktop \
                  entry into $XDG_DATA_HOME/applications. Without a subcommand the \
                  terminal interface starts.\n\n\
                  Exit codes: 0 success, 1 error, 2 usage error, 3 nothing to do.",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Never use colors or spinners.
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Never ask, assume yes.
    #[arg(short = 'y', long, global = true)]
    pub yes: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Install an AppImage from a file or a URL.
    Install(InstallArgs),
    /// Show the installed AppImages.
    List(ListArgs),
    /// Update installed AppImages.
    Update(UpdateArgs),
    /// Remove an installed AppImage.
    Remove(RemoveArgs),
    /// Change the desktop entry of an installed AppImage.
    Edit(EditArgs),
    /// Check the environment and look for leftovers.
    Doctor,
    /// Print a shell completion script.
    Completions(CompletionsArgs),
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Path to an AppImage file, or a URL to download it from.
    pub source: String,

    /// Application name, defaults to what the AppImage declares.
    #[arg(long)]
    pub name: Option<String>,

    /// Comment shown in the launcher.
    #[arg(long)]
    pub comment: Option<String>,

    /// Freedesktop main categories, comma separated.
    #[arg(long, value_delimiter = ',')]
    pub categories: Vec<String>,

    /// Extra arguments the launcher passes to the AppImage.
    #[arg(long = "args", allow_hyphen_values = true)]
    pub args: Option<String>,

    /// Run the application in a terminal.
    #[arg(long)]
    pub terminal: bool,

    /// Icon file to use instead of the embedded one.
    #[arg(long)]
    pub icon: Option<PathBuf>,

    /// Show what would happen and write nothing.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Name or slug of the application to update.
    pub name: Option<String>,

    /// Update every installed application.
    #[arg(long, conflicts_with = "name")]
    pub all: bool,

    /// Only report what is available, change nothing.
    #[arg(long)]
    pub check: bool,

    /// Machine-readable output, with --check.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct RemoveArgs {
    /// Name or slug of the application to remove.
    pub name: String,
}

#[derive(Debug, Args)]
pub struct EditArgs {
    /// Name or slug of the application to edit.
    pub name: String,

    /// Open the desktop entry in $EDITOR instead of the form.
    #[arg(long)]
    pub editor: bool,
}

#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// Shell to generate the completion script for.
    pub shell: Shell,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Shell {
    Fish,
    Bash,
    Zsh,
    Elvish,
}

impl From<Shell> for clap_complete::Shell {
    fn from(shell: Shell) -> Self {
        match shell {
            Shell::Fish => clap_complete::Shell::Fish,
            Shell::Bash => clap_complete::Shell::Bash,
            Shell::Zsh => clap_complete::Shell::Zsh,
            Shell::Elvish => clap_complete::Shell::Elvish,
        }
    }
}
