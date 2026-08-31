use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

use crate::desktop_entry::{self, DesktopEntry};
use crate::error::{Error, Result};
use crate::fs_util;
use crate::{elf, slug, version};

const SQUASHFS_MAGIC: &[u8; 4] = b"hsqs";

/// A temporary directory holding an extracted AppImage. Dropping it removes
/// the extracted tree.
#[derive(Debug)]
pub struct Extraction {
    _dir: TempDir,
    root: PathBuf,
}

impl Extraction {
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Everything worth knowing about an AppImage before installing it. All of it
/// is optional: AppImages that refuse to extract still install fine, the user
/// just has to supply name and icon.
#[derive(Debug, Default)]
pub struct AppImageInfo {
    pub name: Option<String>,
    pub comment: Option<String>,
    pub categories: Vec<String>,
    pub icon_name: Option<String>,
    pub startup_wm_class: Option<String>,
    pub mime_type: Option<String>,
    pub field_code: Option<String>,
    pub terminal: bool,
    pub version: Option<String>,
    pub update_info: Option<String>,
    pub extraction: Option<Extraction>,
}

impl AppImageInfo {
    pub fn extract_root(&self) -> Option<&Path> {
        self.extraction.as_ref().map(Extraction::root)
    }
}

/// Reads an AppImage: extracts it, parses the embedded desktop entry for the
/// given locale and picks up the update information from the ELF section.
pub fn inspect(appimage: &Path, locale: Option<&str>) -> Result<AppImageInfo> {
    if !appimage.exists() {
        return Err(Error::NotFound(appimage.to_path_buf()));
    }
    if !appimage.is_file() {
        return Err(Error::NotAFile(appimage.to_path_buf()));
    }

    let mut info = AppImageInfo {
        update_info: read_update_info(appimage),
        version: version::extract(&file_name(appimage)),
        ..Default::default()
    };

    let extraction = extract(appimage);
    if let Some(extraction) = extraction {
        if let Some(entry) = read_embedded_entry(extraction.root()) {
            apply_entry(&mut info, &entry, locale);
        }
        info.extraction = Some(extraction);
    }

    if info.name.is_none() {
        info.name = Some(slug::name_from_filename(&file_name(appimage)));
    }
    Ok(info)
}

/// Extracts an AppImage, first through its own runtime, then through
/// `unsquashfs`. Returns `None` when neither works.
pub fn extract(appimage: &Path) -> Option<Extraction> {
    extract_with_runtime(appimage).or_else(|| extract_with_unsquashfs(appimage))
}

/// Reads the `.upd_info` ELF section, which holds the zsync update string.
pub fn read_update_info(appimage: &Path) -> Option<String> {
    let bytes = elf::read_section(appimage, ".upd_info")?;
    let text = String::from_utf8_lossy(&bytes);
    let trimmed = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Whether the file carries the AppImage magic bytes, and which type it is.
pub fn appimage_type(appimage: &Path) -> Option<u8> {
    let mut head = [0u8; 11];
    let mut file = File::open(appimage).ok()?;
    file.read_exact(&mut head).ok()?;
    if &head[0..4] != b"\x7fELF" || &head[8..10] != b"AI" {
        return None;
    }
    Some(head[10])
}

/// A file counts as an AppImage when it carries the magic bytes, extracts
/// itself, or at least claims the extension. The last case exists because
/// plenty of AppImages in the wild have a stripped or unusual runtime.
pub fn looks_like_appimage(path: &Path) -> bool {
    appimage_type(path).is_some()
        || path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("appimage"))
}

fn apply_entry(info: &mut AppImageInfo, entry: &DesktopEntry, locale: Option<&str>) {
    info.name = entry.get_localized("Name", locale).map(str::to_string);
    info.comment = entry.get_localized("Comment", locale).map(str::to_string);
    info.categories = entry
        .categories()
        .into_iter()
        .filter(|c| desktop_entry::MAIN_CATEGORIES.contains(&c.as_str()))
        .collect();
    info.icon_name = entry.get("Icon").map(str::to_string);
    info.startup_wm_class = entry.get("StartupWMClass").map(str::to_string);
    info.mime_type = entry.get("MimeType").map(str::to_string);
    info.field_code = entry.get("Exec").and_then(desktop_entry::field_code_of).map(str::to_string);
    info.terminal = entry.terminal();
    if let Some(embedded_version) = entry.get("X-AppImage-Version").map(str::to_string) {
        info.version = Some(embedded_version);
    }
}

fn extract_with_runtime(appimage: &Path) -> Option<Extraction> {
    if !fs_util::is_executable(appimage) {
        return None;
    }
    let dir = tempfile::Builder::new().prefix("appimg-extract-").tempdir().ok()?;

    let status = Command::new(appimage)
        .arg("--appimage-extract")
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;

    let root = dir.path().join("squashfs-root");
    (status.success() && root.is_dir()).then_some(Extraction { _dir: dir, root })
}

fn extract_with_unsquashfs(appimage: &Path) -> Option<Extraction> {
    let unsquashfs = fs_util::which("unsquashfs")?;
    let offset = find_squashfs_offset(appimage)?;
    let dir = tempfile::Builder::new().prefix("appimg-extract-").tempdir().ok()?;
    let root = dir.path().join("squashfs-root");

    let status = Command::new(unsquashfs)
        .arg("-no-progress")
        .arg("-o")
        .arg(offset.to_string())
        .arg("-d")
        .arg(&root)
        .arg(appimage)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;

    (status.success() && root.is_dir()).then_some(Extraction { _dir: dir, root })
}

/// Finds where the squashfs image starts inside an AppImage by scanning for
/// its magic bytes, which is what `unsquashfs` needs as an offset.
fn find_squashfs_offset(appimage: &Path) -> Option<u64> {
    const CHUNK: usize = 64 * 1024;
    let mut file = File::open(appimage).ok()?;
    let mut buffer = vec![0u8; CHUNK];
    let mut carry: Vec<u8> = Vec::new();
    let mut consumed: u64 = 0;

    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }
        let mut haystack = Vec::with_capacity(carry.len() + read);
        haystack.extend_from_slice(&carry);
        haystack.extend_from_slice(&buffer[..read]);

        if let Some(position) =
            haystack.windows(SQUASHFS_MAGIC.len()).position(|w| w == SQUASHFS_MAGIC)
        {
            return Some(consumed - carry.len() as u64 + position as u64);
        }

        let keep = read.min(SQUASHFS_MAGIC.len() - 1);
        carry = buffer[read - keep..read].to_vec();
        consumed += read as u64;
    }
}

fn read_embedded_entry(root: &Path) -> Option<DesktopEntry> {
    let mut candidates: Vec<PathBuf> = fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("desktop"))
        .collect();
    candidates.sort();

    let path = candidates.into_iter().next()?;
    let text = fs::read_to_string(path).ok()?;
    Some(DesktopEntry::parse(&text))
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_util::{write_atomic, MODE_EXEC, MODE_FILE};

    #[test]
    fn finds_the_squashfs_offset_across_chunk_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake.AppImage");

        let offset = 64 * 1024 - 2;
        let mut bytes = vec![0u8; offset];
        bytes.extend_from_slice(SQUASHFS_MAGIC);
        bytes.extend_from_slice(&[1u8; 128]);
        write_atomic(&path, &bytes, MODE_FILE).unwrap();

        assert_eq!(find_squashfs_offset(&path), Some(offset as u64));
    }

    #[test]
    fn no_magic_means_no_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain");
        write_atomic(&path, &vec![7u8; 4096], MODE_FILE).unwrap();
        assert_eq!(find_squashfs_offset(&path), None);
    }

    #[test]
    fn recognises_the_appimage_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("magic.AppImage");
        let mut bytes = b"\x7fELF\x02\x01\x01\x00AI\x02".to_vec();
        bytes.extend_from_slice(&[0u8; 32]);
        write_atomic(&path, &bytes, MODE_FILE).unwrap();

        assert_eq!(appimage_type(&path), Some(2));
        assert!(looks_like_appimage(&path));
    }

    #[test]
    fn extension_alone_is_enough_to_try() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.AppImage");
        write_atomic(&path, b"#!/bin/sh\n", MODE_EXEC).unwrap();
        assert_eq!(appimage_type(&path), None);
        assert!(looks_like_appimage(&path));

        let other = dir.path().join("notes.txt");
        write_atomic(&other, b"hello", MODE_FILE).unwrap();
        assert!(!looks_like_appimage(&other));
    }

    #[test]
    fn inspecting_a_missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(inspect(&dir.path().join("nope.AppImage"), None).is_err());
    }
}
