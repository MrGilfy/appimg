use std::io;
use std::path::{Path, PathBuf};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot determine {0}: neither HOME nor the matching XDG variable is set")]
    HomeUnset(&'static str),

    #[error("the name {0:?} contains no usable characters")]
    InvalidName(String),

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

    #[error("the GitHub API rate limit is exhausted, try again later")]
    RateLimited,

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

    #[error("{url}: {reason}")]
    Zsync { url: String, reason: String },

    #[error(
        "{url}: the new file is checksummed sha256:{found}, the GitHub release publishes \
         sha256:{expected}: what arrived is not the file the release holds"
    )]
    DigestMismatch { url: String, found: String, expected: String },

    #[error("{tool} is not installed, {purpose}")]
    MissingTool { tool: String, purpose: String },
}

impl Error {
    pub fn io(path: impl AsRef<Path>, source: io::Error) -> Self {
        Error::Io { path: path.as_ref().to_path_buf(), source }
    }
}
