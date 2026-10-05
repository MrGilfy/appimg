use std::io;
use std::path::{Path, PathBuf};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot determine {0}: neither HOME nor the matching XDG variable is set")]
    HomeUnset(&'static str),

    #[error("the name {0:?} contains no usable characters")]
    InvalidName(String),

    #[error("{0:?} is not a slug: lowercase letters, digits, '.', '_' and '-' only")]
    InvalidSlug(String),

    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{0} does not exist")]
    NotFound(PathBuf),

    #[error("{0} is not a regular file")]
    NotAFile(PathBuf),

    #[error("{0} does not look like an AppImage")]
    NotAnAppImage(PathBuf),

    #[error("{path}: {unfit}")]
    Unfit { path: PathBuf, unfit: crate::elf::Unfit },

    #[error("{path} is a symbolic link to {target}, adopt the file it points to instead")]
    SymbolicLink { path: PathBuf, target: PathBuf },

    #[error("{path} is already installed as {slug:?}")]
    AlreadyManaged { path: PathBuf, slug: String },

    #[error("{taken_by} is already there, so {slug:?} is taken: pass --name to adopt it under another name")]
    SlugTaken { slug: String, taken_by: PathBuf },

    #[error("{name:?} cannot be a command: {reason}")]
    InvalidCommand { name: String, reason: &'static str },

    #[error(
        "{} is already there and appimg did not create it, so {name:?} cannot be a command: \
         nothing was changed, pick another name or move that file away first",
        path.display()
    )]
    CommandTaken { name: String, path: PathBuf },

    #[error(
        "{name:?} is the command of {slug:?} already: nothing was changed, drop it there first \
         with `appimg command {slug} --remove`"
    )]
    CommandOfAnother { name: String, slug: String },

    #[error("{0:?} is not a valid freedesktop main category")]
    InvalidCategory(String),

    #[error("cannot read image dimensions of {0}")]
    UnreadableImage(PathBuf),

    #[error("{name:?} is already installed as {slug:?}")]
    AlreadyInstalled { name: String, slug: String },

    #[error("no installed application matches {0:?}")]
    NotInstalled(String),

    #[error("{0:?} matches several installed applications: {1}")]
    Ambiguous(String, String),

    #[error("network request failed: {0}")]
    Network(String),

    #[error("download failed: {0}")]
    Download(String),

    /// Which API: `GitHub`, or the host of another forge.
    #[error("the {0} API rate limit is exhausted, try again later")]
    RateLimited(String),

    #[error("no update information stored for {0:?}")]
    NoUpdateInfo(String),

    #[error(
        "{0} is updated manually: it carries no update information and has no update source. \
         Set one with `appimg update-source {0} <URL|github:owner/repo>`, or install a newer \
         version over it"
    )]
    NoUpdateSource(String),

    #[error(
        "{0:?} is not an update source: expected an http(s) URL or github:owner/repo, optionally \
         followed by @tag"
    )]
    InvalidUpdateSource(String),

    #[error("{release}: {reason}")]
    NoMatchingAsset { release: String, reason: String },

    #[error(
        "{0:?} is no asset pattern: give a file name, with * for whatever changes between \
         releases, and no spaces, slashes or #"
    )]
    InvalidAssetPattern(String),

    #[error(
        "--asset picks a file out of a release, and {0:?} follows none: give a \
         github:owner/repo, gitlab:group/project or codeberg:owner/repo update source with it"
    )]
    AssetNeedsRelease(String),

    #[error("{url}: {reason}")]
    Zsync { url: String, reason: String },

    #[error(
        "{url}: the new file is checksummed sha256:{found}, the GitHub release publishes \
         sha256:{expected}: what arrived is not the file the release holds"
    )]
    DigestMismatch { url: String, found: String, expected: String },

    #[error("not an appimg export: {0}")]
    NotAnExport(String),

    #[error(
        "the export has format version {found}, which this appimg does not know: it reads \
         version {supported}"
    )]
    UnknownExportVersion { found: u64, supported: u64 },

    #[error(
        "neither notify-send nor gdbus is installed, so there is no way to show a notification: \
         install notify-send (libnotify) or gdbus (GLib)"
    )]
    NoNotifier,

    #[error("{tool} could not show the notification: {message}")]
    Notification { tool: &'static str, message: String },

    #[error("systemctl is not installed: update notifications need a systemd user session")]
    NoSystemctl,

    #[error("`systemctl --user {command}` failed: {message}")]
    Systemctl { command: String, message: String },

    #[error("{archive}: {reason}")]
    Archive { archive: String, reason: String },

    #[error("{path:?} cannot go into a systemd unit: {reason}")]
    NotForUnit { path: PathBuf, reason: &'static str },
}

impl Error {
    pub fn io(path: impl AsRef<Path>, source: io::Error) -> Self {
        Error::Io { path: path.as_ref().to_path_buf(), source }
    }
}
