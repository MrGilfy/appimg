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

    #[error("no update source could be determined for {0:?}")]
    NoUpdateSource(String),
}

impl Error {
    pub fn io(path: impl AsRef<Path>, source: io::Error) -> Self {
        Error::Io { path: path.as_ref().to_path_buf(), source }
    }
}
