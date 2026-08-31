use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

use crate::error::{Error, Result};

pub const MODE_FILE: u32 = 0o644;
pub const MODE_EXEC: u32 = 0o755;

/// Writes through a temporary file in the same directory and renames, so a
/// full disk or an interrupted run never leaves a half-written file behind.
pub fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let dir = parent_dir(path);
    fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;

    let mut tmp = NamedTempFile::new_in(dir).map_err(|e| Error::io(dir, e))?;
    tmp.write_all(contents).map_err(|e| Error::io(path, e))?;
    tmp.flush().map_err(|e| Error::io(path, e))?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|e| Error::io(path, e))?;
    tmp.persist(path).map_err(|e| Error::io(path, e.error))?;
    Ok(())
}

/// Copies a file into place through a temporary file in the target directory.
pub fn copy_atomic(source: &Path, dest: &Path, mode: u32) -> Result<()> {
    let dir = parent_dir(dest);
    fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;

    let mut input = File::open(source).map_err(|e| Error::io(source, e))?;
    let mut tmp = NamedTempFile::new_in(dir).map_err(|e| Error::io(dir, e))?;
    std::io::copy(&mut input, tmp.as_file_mut()).map_err(|e| Error::io(dest, e))?;
    tmp.flush().map_err(|e| Error::io(dest, e))?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|e| Error::io(dest, e))?;
    tmp.persist(dest).map_err(|e| Error::io(dest, e.error))?;
    Ok(())
}

pub fn set_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| Error::io(path, e))
}

pub fn is_executable(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

pub fn file_size(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|m| m.len())
}

/// Looks up an executable on `PATH`.
pub fn which(binary: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    env::split_paths(&path_var).find_map(|dir| {
        let candidate = dir.join(binary);
        is_executable(&candidate).then_some(candidate)
    })
}

/// All files below `root` whose file stem is exactly `stem`.
pub fn find_files_with_stem(root: &Path, stem: &str) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_stem().and_then(|s| s.to_str()) == Some(stem) {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Guesses whether a file is a PNG or an SVG from its contents, for icons that
/// carry no extension such as `.DirIcon`.
pub fn sniff_image_extension(path: &Path) -> Option<&'static str> {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        match ext.to_ascii_lowercase().as_str() {
            "png" => return Some("png"),
            "svg" | "svgz" => return Some("svg"),
            _ => {}
        }
    }

    let mut head = [0u8; 512];
    let mut file = File::open(path).ok()?;
    let read = file.read(&mut head).ok()?;
    let head = &head[..read];

    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("png");
    }
    let text = String::from_utf8_lossy(head);
    if text.contains("<svg") || text.trim_start().starts_with("<?xml") {
        return Some("svg");
    }
    None
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_atomically_with_the_requested_mode() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sub").join("file.txt");
        write_atomic(&target, b"hello", 0o600).unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "hello");
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn overwrites_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("file.txt");
        write_atomic(&target, b"one", MODE_FILE).unwrap();
        write_atomic(&target, b"two", MODE_FILE).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "two");
    }

    #[test]
    fn copies_and_marks_executable() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        write_atomic(&source, b"payload", MODE_FILE).unwrap();

        let dest = dir.path().join("nested").join("dest");
        copy_atomic(&source, &dest, MODE_EXEC).unwrap();

        assert_eq!(fs::read_to_string(&dest).unwrap(), "payload");
        assert!(is_executable(&dest));
    }

    #[test]
    fn finds_files_by_stem() {
        let dir = tempfile::tempdir().unwrap();
        write_atomic(&dir.path().join("a/b/thing.png"), b"x", MODE_FILE).unwrap();
        write_atomic(&dir.path().join("a/thing.svg"), b"x", MODE_FILE).unwrap();
        write_atomic(&dir.path().join("a/other.png"), b"x", MODE_FILE).unwrap();

        let found = find_files_with_stem(dir.path(), "thing").unwrap();
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn sniffs_png_and_svg_without_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("DirIcon");
        write_atomic(&png, b"\x89PNG\r\n\x1a\n and more", MODE_FILE).unwrap();
        assert_eq!(sniff_image_extension(&png), Some("png"));

        let svg = dir.path().join("other");
        write_atomic(&svg, b"<?xml version=\"1.0\"?><svg></svg>", MODE_FILE).unwrap();
        assert_eq!(sniff_image_extension(&svg), Some("svg"));

        let junk = dir.path().join("junk");
        write_atomic(&junk, b"\x00\x01\x02", MODE_FILE).unwrap();
        assert_eq!(sniff_image_extension(&junk), None);
    }
}
