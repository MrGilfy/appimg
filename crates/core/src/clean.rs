//! What appimg keeps on disk that is no installed AppImage: the backup an
//! update keeps of the previous version, and what an interrupted update, or
//! an older appimg, left next to the AppImage. `doctor` reports these and
//! `appimg clean` removes them.
//!
//! Only a regular file named after a slug appimg manages, with one of the
//! suffixes in [`update::LEFTOVER_SUFFIXES`], is ever one of them. An
//! AppImage appimg does not manage is none, and neither is anything else in
//! the directory, which `adopt --scan` passes over the leftovers of an
//! update for. An installed AppImage ends in `.AppImage` and no leftover
//! does.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::error::{Error, Result};
use crate::list::{self, InstalledApp};
use crate::paths::Paths;
use crate::slug;
use crate::update;

/// How long a staging file stays out of reach after it was last written.
/// An update that is still running writes to it, and is not done with it.
pub const IN_USE_WINDOW: Duration = Duration::from_secs(15 * 60);

/// What a leftover is, going by the suffix that names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `.bak`: the previous version, which a rollback puts back.
    Backup,
    /// `.new`: the new version, until it is swapped in.
    Staged,
    /// `.archive`: an archive an update downloaded, until the AppImage in
    /// it is out.
    Archive,
    /// `.part`: a download, until it is complete.
    Partial,
    /// `.zs-old`: the copy of the previous version `appimageupdatetool`
    /// made, back when an older appimg fell back to it.
    ZsyncOld,
}

impl Kind {
    pub fn of(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "bak" => Some(Self::Backup),
            "new" => Some(Self::Staged),
            "archive" => Some(Self::Archive),
            "part" => Some(Self::Partial),
            "zs-old" => Some(Self::ZsyncOld),
            _ => None,
        }
    }

    /// What it is, in a few words. `appimg update` drops all of them once
    /// the new binary has run, so anything still here comes from a run that
    /// did not get that far, or from an older appimg.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Backup => "backup of the previous version",
            Self::Staged => "half-finished download",
            Self::Archive => "archive downloaded by an update that did not get to unpack it",
            Self::ZsyncOld => {
                "copy of the previous version, left by appimageupdatetool under an older appimg"
            }
            Self::Partial => "partial download",
        }
    }

    /// Whether an update writes to it while it runs.
    fn is_staging(self) -> bool {
        matches!(self, Self::Staged | Self::Archive | Self::Partial)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    pub slug: String,
    pub path: PathBuf,
    pub kind: Kind,
    pub size: u64,
    /// A staging file written within [`IN_USE_WINDOW`]: an update may still
    /// be writing it, so it is not removed.
    pub recent: bool,
}

/// Every leftover of every application appimg manages, sorted by path.
pub fn find(paths: &Paths) -> Result<Vec<Leftover>> {
    Ok(of_apps(paths, &list::list(paths)?))
}

/// The leftovers of these applications, sorted by path. A slug that
/// [`slug::slugify`] could not have made names no file of appimg's: joined to
/// the directory, it could point anywhere.
pub fn of_apps(paths: &Paths, apps: &[InstalledApp]) -> Vec<Leftover> {
    let slugs: BTreeSet<&str> =
        apps.iter().map(|app| app.slug.as_str()).filter(|slug| slug::check(slug).is_ok()).collect();
    let now = SystemTime::now();

    let mut found: Vec<Leftover> = slugs
        .into_iter()
        .flat_map(|slug| {
            update::LEFTOVER_SUFFIXES.iter().filter_map(move |suffix| {
                inspect(paths.appimage_dir.join(format!("{slug}.{suffix}")), slug, now)
            })
        })
        .collect();
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

fn inspect(path: PathBuf, slug: &str, now: SystemTime) -> Option<Leftover> {
    // Not followed: appimg writes no links, so a link is not its own,
    // whatever it is called.
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    let kind = Kind::of(&path)?;
    let recent = kind.is_staging() && metadata.modified().is_ok_and(|at| near(now, at));
    Some(Leftover { slug: slug.to_string(), size: metadata.len(), path, kind, recent })
}

/// Whether `at` lies within [`IN_USE_WINDOW`] of `now`, on either side: a
/// file written after `now` was taken is as recent as it gets.
fn near(now: SystemTime, at: SystemTime) -> bool {
    let apart = now.duration_since(at).unwrap_or_else(|later| later.duration());
    apart < IN_USE_WINDOW
}

/// Removes the leftovers in `chosen` that still are leftovers and are not
/// recent. They are looked at again first, since the question before can
/// take any time and an update may have started meanwhile. Returns those it
/// removed.
pub fn remove(paths: &Paths, chosen: &[Leftover]) -> Result<Vec<Leftover>> {
    let wanted: HashSet<&Path> = chosen.iter().map(|leftover| leftover.path.as_path()).collect();
    let mut removed = Vec::new();

    for leftover in find(paths)? {
        if leftover.recent || !wanted.contains(leftover.path.as_path()) {
            continue;
        }
        match fs::remove_file(&leftover.path) {
            Ok(()) => removed.push(leftover),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(&leftover.path, e)),
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_suffix_has_a_kind() {
        for suffix in update::LEFTOVER_SUFFIXES {
            assert!(Kind::of(Path::new(&format!("app.{suffix}"))).is_some(), "{suffix}");
        }
        assert_eq!(Kind::of(Path::new("app.AppImage")), None);
    }

    #[test]
    fn recent_means_close_to_now_either_way() {
        let now = SystemTime::now();
        let minute = Duration::from_secs(60);
        assert!(near(now, now - minute));
        assert!(near(now, now + minute));
        assert!(!near(now, now - IN_USE_WINDOW - minute));
    }
}
