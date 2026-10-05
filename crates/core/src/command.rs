//! An application as a command: a symbolic link in `~/.local/bin` to the
//! installed AppImage, so a shell runs it by that name. The desktop entry
//! records the name, [`KEY_COMMAND`], and that is what makes the link
//! appimg's: it removes it with the application, and `doctor` reports it
//! when it is gone or points elsewhere.
//!
//! The link points at `<slug>.AppImage` itself. An update swaps a new file
//! in under that path and a rollback puts the old one back there, so the
//! link runs whatever version is installed and never has to change.
//!
//! `~/.local/bin` is shared with everything else on the machine. Nothing
//! there that appimg did not create for the application is ever written
//! over or removed: a name that is taken is refused.

use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use crate::desktop_entry::{DesktopEntry, KEY_COMMAND};
use crate::error::{Error, Result};
use crate::list::{self, InstalledApp};
use crate::paths::Paths;

/// What is at the place of a recorded command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// The link, to the application's AppImage.
    Linked,
    /// Nothing at all.
    Missing,
    /// A link to a file that is not there.
    Broken { target: PathBuf },
    /// A link to another file.
    Elsewhere { target: PathBuf },
    /// No link at all, a file or a directory.
    NotALink,
    /// The entry records a name no command can have, by hand say.
    InvalidName,
}

/// What [`set`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// The application had that command, link and all, already.
    Unchanged,
    /// The link is there now, and recorded. `replaced` is the command the
    /// application had before, and whether its link went: one that no
    /// longer ran the application stays where it is.
    Linked { replaced: Option<(String, bool)> },
}

/// Refuses a name a shell could not run as a command of its own: it has to
/// be one file name, which a desktop entry can hold.
pub fn check_name(name: &str) -> Result<()> {
    let invalid = |reason| Err(Error::InvalidCommand { name: name.to_string(), reason });
    if name.is_empty() {
        return invalid("it is empty");
    }
    if name == "." || name == ".." {
        return invalid("it names a directory");
    }
    if name.contains('/') {
        return invalid("a command is one file name, without a '/'");
    }
    if name.starts_with('-') {
        return invalid("it would read as an option");
    }
    if name.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return invalid("it has whitespace or control characters in it");
    }
    if name.len() > 255 {
        return invalid("it is longer than a file name can be");
    }
    Ok(())
}

/// Where the command `name` goes.
pub fn link_path(paths: &Paths, name: &str) -> PathBuf {
    paths.bin_dir.join(name)
}

/// What is at the place of the command `name` of `slug`.
pub fn state(paths: &Paths, slug: &str, name: &str) -> State {
    if check_name(name).is_err() {
        return State::InvalidName;
    }
    let link = link_path(paths, name);
    let metadata = match fs::symlink_metadata(&link) {
        Ok(metadata) => metadata,
        Err(_) => return State::Missing,
    };
    if !metadata.file_type().is_symlink() {
        return State::NotALink;
    }
    let target = resolve(&link);
    if !target.exists() {
        return State::Broken { target };
    }
    if runs(&link, &paths.appimage_path(slug)) {
        State::Linked
    } else {
        State::Elsewhere { target }
    }
}

/// Whether `link` is a link that runs `appimage`: it resolves to that file,
/// or names it while it is not there.
fn runs(link: &Path, appimage: &Path) -> bool {
    if !fs::symlink_metadata(link).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return false;
    }
    match (fs::canonicalize(link), fs::canonicalize(appimage)) {
        (Ok(file), Ok(installed)) => file == installed,
        _ => resolve(link) == appimage,
    }
}

/// Where a link points, a relative target taken from the link's directory.
fn resolve(link: &Path) -> PathBuf {
    let target = fs::read_link(link).unwrap_or_default();
    match link.parent() {
        Some(dir) if target.is_relative() => dir.join(target),
        _ => target,
    }
}

/// Whether `name` can be the command of `slug`. Returns whether its link is
/// there already. Refuses whatever else is there, and a name another
/// application records, whether its link is there or not.
pub fn check_free(paths: &Paths, slug: &str, name: &str) -> Result<bool> {
    check_name(name)?;
    for app in list::list(paths)? {
        if app.slug != slug && app.command.as_deref() == Some(name) {
            return Err(Error::CommandOfAnother { name: name.to_string(), slug: app.slug });
        }
    }
    let link = link_path(paths, name);
    match fs::symlink_metadata(&link) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::io(&link, e)),
        Ok(_) if runs(&link, &paths.appimage_path(slug)) => Ok(true),
        Ok(_) => Err(Error::CommandTaken { name: name.to_string(), path: link }),
    }
}

/// Makes `name` the command of `app`: the link, then the entry. A command
/// it had before goes, its link with it as long as that still runs the
/// application. Nothing is changed when the name is not free, see
/// [`check_free`].
pub fn set(paths: &Paths, app: &InstalledApp, name: &str) -> Result<Change> {
    let there = check_free(paths, &app.slug, name)?;
    let previous = app.command.as_deref();
    if there && previous == Some(name) {
        return Ok(Change::Unchanged);
    }
    if !there {
        create(paths, &app.slug, name)?;
    }
    let replaced = match previous.filter(|previous| *previous != name) {
        Some(previous) => Some((previous.to_string(), remove_link(paths, &app.slug, previous)?)),
        None => None,
    };
    record(&app.desktop_entry_path, Some(name))?;
    Ok(Change::Linked { replaced })
}

/// Creates the link of the command `name` to the AppImage of `slug`, and
/// the bin directory when it is missing. Refuses to write over anything.
pub(crate) fn create(paths: &Paths, slug: &str, name: &str) -> Result<()> {
    fs::create_dir_all(&paths.bin_dir).map_err(|e| Error::io(&paths.bin_dir, e))?;
    let link = link_path(paths, name);
    symlink(paths.appimage_path(slug), &link).map_err(|e| match e.kind() {
        io::ErrorKind::AlreadyExists => {
            Error::CommandTaken { name: name.to_string(), path: link.clone() }
        }
        _ => Error::io(&link, e),
    })
}

/// Drops the command of `app`. Returns the name it had and whether its link
/// went, `None` when it had none. A link that no longer runs the
/// application, or whatever took its place, stays where it is.
pub fn unset(app: &InstalledApp, paths: &Paths) -> Result<Option<(String, bool)>> {
    let Some(name) = app.command.clone() else {
        return Ok(None);
    };
    let removed = remove_link(paths, &app.slug, &name)?;
    record(&app.desktop_entry_path, None)?;
    Ok(Some((name, removed)))
}

/// The link of the command `name`, when it runs the application `slug` and
/// is therefore appimg's to remove.
pub fn own_link(paths: &Paths, slug: &str, name: &str) -> Option<PathBuf> {
    check_name(name).ok()?;
    let link = link_path(paths, name);
    runs(&link, &paths.appimage_path(slug)).then_some(link)
}

/// Removes the link of the command `name` when it runs `slug`. Returns
/// whether it did.
pub(crate) fn remove_link(paths: &Paths, slug: &str, name: &str) -> Result<bool> {
    let Some(link) = own_link(paths, slug, name) else {
        return Ok(false);
    };
    match fs::remove_file(&link) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::io(&link, e)),
    }
}

/// Writes the command into the entry, or takes it out.
pub(crate) fn record(entry_path: &Path, name: Option<&str>) -> Result<()> {
    let mut entry = DesktopEntry::read(entry_path)?;
    match name {
        Some(name) => entry.set(KEY_COMMAND, name),
        None => entry.remove(KEY_COMMAND),
    }
    entry.write(entry_path)
}

/// Whether a shell looks for commands in the bin directory: it is on
/// `PATH`.
pub fn bin_dir_on_path(paths: &Paths) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    let bin = fs::canonicalize(&paths.bin_dir).unwrap_or_else(|_| paths.bin_dir.clone());
    env::split_paths(&path)
        .any(|dir| dir == paths.bin_dir || fs::canonicalize(&dir).is_ok_and(|dir| dir == bin))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_is_one_file_name_a_shell_can_run() {
        for good in ["krita", "nvim-0.10", "my_app", "x", "app.AppImage", "c++"] {
            assert!(check_name(good).is_ok(), "{good}");
        }
        for bad in ["", ".", "..", "a/b", "../x", "-x", "a b", "a\nb", "a\tb"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_name(&"a".repeat(256)).is_err());
    }
}
