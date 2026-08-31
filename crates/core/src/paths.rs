use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Every filesystem location `appimg` touches. Built once from the environment
/// so that tests can point the whole program at a temporary directory.
#[derive(Debug, Clone)]
pub struct Paths {
    pub data_home: PathBuf,
    pub config_home: PathBuf,
    pub appimage_dir: PathBuf,
    pub applications_dir: PathBuf,
    pub icons_root: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self> {
        let home = non_empty_var("HOME").map(PathBuf::from);
        let data_home =
            xdg_dir("XDG_DATA_HOME", home.as_deref(), ".local/share", "the data directory")?;
        let config_home =
            xdg_dir("XDG_CONFIG_HOME", home.as_deref(), ".config", "the config directory")?;
        let appimage_dir = non_empty_var("APPIMG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_home.join("appimages"));

        Ok(Self {
            applications_dir: data_home.join("applications"),
            icons_root: data_home.join("icons").join("hicolor"),
            appimage_dir,
            config_home,
            data_home,
        })
    }

    pub fn appimage_path(&self, slug: &str) -> PathBuf {
        self.appimage_dir.join(format!("{slug}.AppImage"))
    }

    pub fn desktop_entry_path(&self, slug: &str) -> PathBuf {
        self.applications_dir.join(format!("{slug}.desktop"))
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [&self.appimage_dir, &self.applications_dir, &self.icons_root] {
            fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        }
        Ok(())
    }
}

fn non_empty_var(key: &str) -> Option<OsString> {
    env::var_os(key).filter(|v| !v.is_empty())
}

fn xdg_dir(key: &str, home: Option<&Path>, suffix: &str, what: &'static str) -> Result<PathBuf> {
    if let Some(value) = non_empty_var(key) {
        return Ok(PathBuf::from(value));
    }
    home.map(|h| h.join(suffix)).ok_or(Error::HomeUnset(what))
}
