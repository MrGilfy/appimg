//! The SHA-1 of an installed AppImage, kept in its desktop entry together
//! with the size and modification time the file had when it was hashed.
//!
//! A zsync check compares the installed file with the one a zsync file
//! describes by this checksum. For an AppImage of a few hundred megabytes,
//! reading the whole file is most of what such a check costs, and the page
//! cache it fills counts against whoever runs it. So appimg hashes a file
//! when it writes it, and a check takes the stored checksum for as long as
//! the size and the time still match. A file that changed, or an entry
//! written before there was a checksum in it, is hashed once and the result
//! stored.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::desktop_entry::{DesktopEntry, KEY_SHA1};
use crate::error::{Error, Result};
use crate::list::InstalledApp;
use crate::zsync;

/// A checksum and the file it was taken of, as [`KEY_SHA1`] holds it:
/// `<sha1> <size> <seconds>.<nanoseconds>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    /// Lowercase hex.
    pub sha1: String,
    size: u64,
    mtime: (i64, i64),
}

impl Stamp {
    /// Hashes the file. Its size and time are read first, so a file that
    /// changes while it is read does not match the stamp afterwards.
    pub fn of(path: &Path) -> Result<Self> {
        let meta = fs::metadata(path).map_err(|e| Error::io(path, e))?;
        let sha1 = zsync::sha1_file(path)?;
        Ok(Self::from_metadata(sha1, &meta))
    }

    /// The stamp of a file whose checksum is known already, such as one a
    /// zsync update assembled and checked against it.
    pub fn known(path: &Path, sha1: &str) -> Result<Self> {
        let meta = fs::metadata(path).map_err(|e| Error::io(path, e))?;
        Ok(Self::from_metadata(sha1.to_ascii_lowercase(), &meta))
    }

    fn from_metadata(sha1: String, meta: &fs::Metadata) -> Self {
        Self { sha1, size: meta.len(), mtime: (meta.mtime(), meta.mtime_nsec()) }
    }

    pub fn parse(value: &str) -> Option<Self> {
        let mut fields = value.split_whitespace();
        let sha1 = fields.next()?;
        let size = fields.next()?.parse().ok()?;
        let (seconds, nanoseconds) = fields.next()?.split_once('.')?;
        if fields.next().is_some()
            || sha1.len() != 40
            || !sha1.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        Some(Self {
            sha1: sha1.to_string(),
            size,
            mtime: (seconds.parse().ok()?, nanoseconds.parse().ok()?),
        })
    }

    /// Whether the file at `path` still has the size and time it had when
    /// it was hashed.
    pub fn matches(&self, path: &Path) -> bool {
        fs::metadata(path)
            .is_ok_and(|meta| Self::from_metadata(String::new(), &meta).same_file_as(self))
    }

    fn same_file_as(&self, other: &Stamp) -> bool {
        self.size == other.size && self.mtime == other.mtime
    }
}

impl std::fmt::Display for Stamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} {}.{:09}", self.sha1, self.size, self.mtime.0, self.mtime.1)
    }
}

/// The SHA-1 of the installed file of `app`: the stored one while the file
/// still matches it, otherwise a fresh one, which is then stored.
pub fn sha1(app: &InstalledApp) -> Result<String> {
    let entry = DesktopEntry::read(&app.desktop_entry_path)?;
    if let Some(stamp) = entry.get(KEY_SHA1).and_then(Stamp::parse) {
        if stamp.matches(&app.appimage_path) {
            return Ok(stamp.sha1);
        }
    }
    let stamp = Stamp::of(&app.appimage_path)?;
    // Storing it is what spares the next check the reading. A check that
    // cannot store it still has its answer.
    let mut entry = entry;
    entry.set(KEY_SHA1, stamp.to_string());
    let _ = entry.write(&app.desktop_entry_path);
    Ok(stamp.sha1)
}

/// Stamps `entry` with the file at `path`, or takes any stamp out of it
/// when the file cannot be read, so that no stale one stays behind.
pub fn record(entry: &mut DesktopEntry, path: &Path, known_sha1: Option<&str>) {
    let stamp = match known_sha1 {
        Some(sha1) => Stamp::known(path, sha1),
        None => Stamp::of(path),
    };
    match stamp {
        Ok(stamp) => entry.set(KEY_SHA1, stamp.to_string()),
        Err(_) => entry.remove(KEY_SHA1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_reads_back_what_it_wrote() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), b"abc").unwrap();
        let stamp = Stamp::of(file.path()).unwrap();
        assert_eq!(stamp.sha1, "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(Stamp::parse(&stamp.to_string()), Some(stamp.clone()));
        assert!(stamp.to_string().starts_with("a9993e364706816aba3e25717850c26c9cd0d89d 3 "));
        assert!(stamp.matches(file.path()));

        let known = Stamp::known(file.path(), "A9993E364706816ABA3E25717850C26C9CD0D89D").unwrap();
        assert_eq!(known, stamp);
    }

    #[test]
    fn a_stamp_that_is_not_one_is_ignored() {
        let sha1 = "a9993e364706816aba3e25717850c26c9cd0d89d";
        for value in [
            "",
            sha1,
            &format!("{sha1} 3"),
            &format!("{sha1} 3 17"),
            &format!("{sha1} x 17.5"),
            &format!("{sha1} 3 17.5 more"),
            "a9993e 3 17.5",
            &format!("{} 3 17.5", sha1.to_uppercase()),
        ] {
            assert_eq!(Stamp::parse(value), None, "{value:?}");
        }
        assert!(Stamp::parse(&format!("{sha1} 3 17.000000005")).is_some());
    }

    #[test]
    fn another_size_or_time_is_another_file() {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), b"abc").unwrap();
        let stamp = Stamp::of(file.path()).unwrap();
        let modified = fs::metadata(file.path()).unwrap().modified().unwrap();

        // Other bytes of the same length, at the same time, are not noticed:
        // that is the point of the stamp.
        fs::write(file.path(), b"xyz").unwrap();
        fs::File::options().write(true).open(file.path()).unwrap().set_modified(modified).unwrap();
        assert!(stamp.matches(file.path()));

        let later = modified + std::time::Duration::from_nanos(1);
        fs::File::options().write(true).open(file.path()).unwrap().set_modified(later).unwrap();
        assert!(!stamp.matches(file.path()));

        fs::write(file.path(), b"abcd").unwrap();
        fs::File::options().write(true).open(file.path()).unwrap().set_modified(modified).unwrap();
        assert!(!stamp.matches(file.path()));

        fs::remove_file(file.path()).unwrap();
        assert!(!stamp.matches(file.path()));
    }
}
