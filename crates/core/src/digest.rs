//! The SHA-256 digests GitHub publishes for release assets, and checking a
//! file against one.
//!
//! The release JSON carries `"digest": "sha256:..."` for every asset
//! uploaded since GitHub began computing them. Older assets carry `null`,
//! which is no reason to refuse them, only nothing to check them against.
//! The hashing is ring's, which `ureq` already builds for TLS: a digest that
//! decides whether a file is installed is no place for a home-made one.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use ring::digest::{Context, SHA256};

use crate::error::{Error, Result};

/// What a GitHub release publishes for one of its assets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Published {
    /// The SHA-256 of the asset, lowercase hex.
    Sha256(String),
    /// Nothing to check the file against, and why.
    Nothing(String),
}

/// What checking a file against what was published for it found. A file
/// that does not match never gets this far, it is an error instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verified {
    /// The file is the asset, its SHA-256 is the published one.
    Matches(String),
    /// Nothing was published to check it against, and why.
    Unchecked(String),
}

impl Verified {
    /// One line saying what the check found, for a caller that reports to a
    /// user.
    pub fn describe(&self) -> String {
        match self {
            Verified::Matches(_) => "sha256 matches the digest GitHub publishes".to_string(),
            Verified::Unchecked(reason) => format!("not checked, {reason}"),
        }
    }
}

/// What to say about an asset whose `digest` is missing or `null`.
pub const NONE_PUBLISHED: &str = "GitHub publishes no digest for this file";

/// The SHA-256 out of a `digest` field, lowercase hex. Anything but
/// `sha256:` and 64 hex digits is no digest this can check.
pub fn parse(value: &str) -> Option<String> {
    let (algorithm, hex) = value.trim().split_once(':')?;
    (algorithm.eq_ignore_ascii_case("sha256")
        && hex.len() == 64
        && hex.bytes().all(|b| b.is_ascii_hexdigit()))
    .then(|| hex.to_ascii_lowercase())
}

/// The SHA-256 of a file, lowercase hex. Read in chunks, an AppImage is
/// large.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;
    let mut context = Context::new(&SHA256);
    let mut buffer = vec![0u8; 64 * 1024];

    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => context.update(&buffer[..read]),
            Err(e) => return Err(Error::io(path, e)),
        }
    }
    Ok(context.finish().as_ref().iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Checks `file`, which came from `url`, against what the release publishes
/// for that asset. The file stays where it is either way: what to do with
/// one that does not match is up to the caller, which knows whether it is a
/// staged download or the installed AppImage.
pub fn verify(file: &Path, url: &str, published: &Published) -> Result<Verified> {
    let expected = match published {
        Published::Sha256(expected) => expected,
        Published::Nothing(reason) => return Ok(Verified::Unchecked(reason.clone())),
    };
    let found = sha256_file(file)?;
    if &found != expected {
        return Err(Error::DigestMismatch {
            url: url.to_string(),
            found,
            expected: expected.clone(),
        });
    }
    Ok(Verified::Matches(found))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn a_digest_field_is_read_as_lowercase_hex() {
        let upper = format!("sha256:{}", ABC.to_uppercase());
        assert_eq!(parse(&format!("sha256:{ABC}")).as_deref(), Some(ABC));
        assert_eq!(parse(&upper).as_deref(), Some(ABC));
        assert_eq!(parse(&format!("SHA256:{ABC}")).as_deref(), Some(ABC));
    }

    #[test]
    fn anything_but_a_sha256_is_no_digest() {
        assert_eq!(parse(ABC), None);
        assert_eq!(parse(&format!("sha512:{ABC}")), None);
        assert_eq!(parse(&format!("sha256:{}", &ABC[1..])), None);
        assert_eq!(parse(&format!("sha256:{}g", &ABC[1..])), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn a_file_is_hashed_as_sha256() {
        // FIPS 180-2, appendix B.1.
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"abc").unwrap();
        assert_eq!(sha256_file(file.path()).unwrap(), ABC);
    }

    #[test]
    fn a_file_matches_or_is_an_error_that_names_both_digests() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"abc").unwrap();

        let matched = verify(file.path(), "u", &Published::Sha256(ABC.to_string())).unwrap();
        assert_eq!(matched, Verified::Matches(ABC.to_string()));

        let other = "0".repeat(64);
        let error = verify(file.path(), "u", &Published::Sha256(other.clone())).unwrap_err();
        let message = error.to_string();
        assert!(message.contains(ABC) && message.contains(&other), "{message}");
        // What happens to the file is the caller's business.
        assert!(file.path().exists());
    }

    #[test]
    fn nothing_published_is_no_check_and_no_error() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let unchecked =
            verify(file.path(), "u", &Published::Nothing(NONE_PUBLISHED.to_string())).unwrap();
        assert_eq!(unchecked.describe(), format!("not checked, {NONE_PUBLISHED}"));
        assert!(file.path().exists());
    }
}
