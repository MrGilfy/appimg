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
    /// Take over an AppImage that is already on disk, or list the ones that
    /// could be.
    Adopt(AdoptArgs),
    /// Show the installed AppImages.
    List(ListArgs),
    /// Update installed AppImages.
    Update(UpdateArgs),
    /// Show, set or clear where an installed AppImage updates from.
    UpdateSource(UpdateSourceArgs),
    /// Remove an installed AppImage.
    Remove(RemoveArgs),
    /// Change the desktop entry of an installed AppImage in $EDITOR.
    Edit(EditArgs),
    /// Write the installed AppImages to a file another machine can import.
    Export(ExportArgs),
    /// Install the AppImages an export lists, downloading each again.
    Import(ImportArgs),
    /// Show a notification when updates are available, checked once a day by
    /// a systemd user timer.
    Notify(NotifyArgs),
    /// Check the environment and look for leftovers.
    Doctor,
    /// Print a shell completion script.
    Completions(CompletionsArgs),
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Path to an AppImage file, or a URL to download it from.
    pub source: String,

    #[command(flatten)]
    pub entry: EntryArgs,

    /// Show what would happen, write nothing and never run the AppImage.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
#[command(group = clap::ArgGroup::new("what").args(["path", "scan"]).required(true))]
pub struct AdoptArgs {
    /// The AppImage file to take over. It moves into $XDG_DATA_HOME/appimages;
    /// one in ~/.local/bin leaves a symbolic link to it behind.
    pub path: Option<PathBuf>,

    /// List the AppImages that could be adopted, with the command for each,
    /// and change nothing: files appimg does not manage in its appimages
    /// directory, ~/Applications and ~/.local/bin.
    #[arg(
        long,
        conflicts_with_all = [
            "copy", "keep_entries", "dry_run", "name", "comment", "categories", "args",
            "terminal", "icon", "update_source", "asset",
        ]
    )]
    pub scan: bool,

    /// Copy the file and leave the original where it is, instead of moving it.
    #[arg(long)]
    pub copy: bool,

    /// Leave desktop entries from elsewhere that launch the file alone,
    /// without asking.
    #[arg(long)]
    pub keep_entries: bool,

    #[command(flatten)]
    pub entry: EntryArgs,

    /// Show what would happen, write nothing and never run the AppImage.
    #[arg(long)]
    pub dry_run: bool,
}

/// What goes into the desktop entry, for an install and an adoption alike.
#[derive(Debug, Args)]
pub struct EntryArgs {
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

    /// Where updates come from when the AppImage carries no update
    /// information of its own: a URL to download, github:owner/repo to
    /// follow the newest of its releases that has the AppImage, or
    /// github:owner/repo@tag to follow that tag.
    #[arg(long, value_name = "URL|github:owner/repo")]
    pub update_source: Option<String>,

    /// Which file of each GitHub release updates come from: a file name in
    /// which * stands for whatever changes between releases, such as
    /// 'SoH-*-Linux.zip', matched without regard to case. It is kept with
    /// the update source and picks the file instead of the name of the
    /// installed one. Needs a github: update source.
    #[arg(long, value_name = "PATTERN")]
    pub asset: Option<String>,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Machine-readable output. The field set may change between versions
    /// without notice.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
#[command(group = clap::ArgGroup::new("target").args(["name", "all"]).required(true))]
pub struct UpdateArgs {
    /// Name or slug of the application to update.
    pub name: Option<String>,

    /// Update every installed application.
    #[arg(long)]
    pub all: bool,

    /// Only report what is available, change nothing.
    #[arg(long)]
    pub check: bool,

    /// Machine-readable output, with --check. The field set may change
    /// between versions without notice.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct UpdateSourceArgs {
    /// Name or slug of the application.
    pub name: String,

    /// The update source to set: a URL to download, github:owner/repo to
    /// follow the newest of its releases that has the AppImage, or
    /// github:owner/repo@tag to follow that tag. Without it and without
    /// --clear, the current one is shown.
    #[arg(value_name = "URL|github:owner/repo")]
    pub source: Option<String>,

    /// Update manually from now on, from no source at all.
    #[arg(long, conflicts_with = "source")]
    pub clear: bool,

    /// Pick the file out of each GitHub release with this pattern, a file
    /// name in which * stands for whatever changes between releases, such
    /// as 'SoH-*-Linux.zip'. Without a source it is added to the current
    /// one. Setting a source without it drops the pattern.
    #[arg(long, value_name = "PATTERN", conflicts_with = "clear")]
    pub asset: Option<String>,
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
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// The file to write. Without one, the export goes to standard output.
    pub file: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    /// A file `appimg export` wrote.
    pub file: PathBuf,

    /// Show what would be installed and from where, and change nothing.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct NotifyArgs {
    #[command(subcommand)]
    pub action: NotifyAction,
}

#[derive(Debug, Subcommand)]
pub enum NotifyAction {
    /// Write a systemd user service and timer to
    /// $XDG_CONFIG_HOME/systemd/user and start the timer. It runs this appimg
    /// once a day, against the applications it manages now.
    Enable,
    /// Stop the timer and remove both units.
    Disable,
    /// Show whether the timer is on, when it runs next, how the last check
    /// went, and whether the appimg it runs is still there.
    Status,
    /// Show a sample notification right away.
    Test,
    /// Check every application and show one notification naming those with
    /// an update, if any. This is what the timer runs.
    #[command(hide = true)]
    Check,
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
