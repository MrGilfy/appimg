use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::fs_util::{self, MODE_FILE};

/// Where an icon file belongs inside a hicolor theme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconPlacement {
    /// A raster icon, placed in `<size>x<size>/apps`.
    Sized { width: u32, height: u32, extension: &'static str },
    /// A vector icon, placed in `scalable/apps`.
    Scalable,
}

impl IconPlacement {
    pub fn directory(&self) -> String {
        match self {
            IconPlacement::Sized { width, height, .. } => format!("{width}x{height}"),
            IconPlacement::Scalable => "scalable".to_string(),
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            IconPlacement::Sized { extension, .. } => extension,
            IconPlacement::Scalable => "svg",
        }
    }

    pub fn relative_path(&self, slug: &str) -> PathBuf {
        Path::new(&self.directory()).join("apps").join(format!("{slug}.{}", self.extension()))
    }
}

/// Decides where an icon file goes: SVGs are scalable, raster images are
/// measured and land in the directory for their pixel size.
pub fn placement_for(path: &Path) -> Result<IconPlacement> {
    match fs_util::sniff_image_extension(path) {
        Some("svg") => return Ok(IconPlacement::Scalable),
        Some("png") | None => {}
        Some(_) => {}
    }

    let size = imagesize::size(path).map_err(|_| Error::UnreadableImage(path.to_path_buf()))?;
    let width =
        u32::try_from(size.width).map_err(|_| Error::UnreadableImage(path.to_path_buf()))?;
    let height =
        u32::try_from(size.height).map_err(|_| Error::UnreadableImage(path.to_path_buf()))?;
    if width == 0 || height == 0 {
        return Err(Error::UnreadableImage(path.to_path_buf()));
    }

    Ok(IconPlacement::Sized { width, height, extension: "png" })
}

/// Copies one icon into the hicolor tree and returns where it landed.
pub fn install_icon(source: &Path, slug: &str, icons_root: &Path) -> Result<PathBuf> {
    let placement = placement_for(source)?;
    let dest = icons_root.join(placement.relative_path(slug));
    fs_util::copy_atomic(source, &dest, MODE_FILE)?;
    Ok(dest)
}

/// Collects the icons an extracted AppImage offers, in the order the spec
/// prescribes: every hicolor size for the declared icon name, then
/// `.DirIcon`, then the first image at the top level.
pub fn find_icons(extract_root: &Path, icon_name: Option<&str>) -> Vec<PathBuf> {
    if let Some(name) = icon_name {
        let hicolor = find_hicolor_icons(extract_root, name);
        if !hicolor.is_empty() {
            return hicolor;
        }
        for extension in ["png", "svg"] {
            let candidate = extract_root.join(format!("{name}.{extension}"));
            if candidate.is_file() {
                return vec![candidate];
            }
        }
    }

    let dir_icon = extract_root.join(".DirIcon");
    if dir_icon.is_file() {
        return vec![dir_icon];
    }

    find_top_level_icon(extract_root).into_iter().collect()
}

/// Installs everything `find_icons` turned up. Icons that cannot be measured
/// are skipped rather than failing the whole installation.
pub fn install_icons(
    extract_root: &Path,
    icon_name: Option<&str>,
    slug: &str,
    icons_root: &Path,
) -> Vec<PathBuf> {
    find_icons(extract_root, icon_name)
        .into_iter()
        .filter_map(|source| install_icon(&source, slug, icons_root).ok())
        .collect()
}

fn find_hicolor_icons(extract_root: &Path, icon_name: &str) -> Vec<PathBuf> {
    let hicolor = extract_root.join("usr/share/icons/hicolor");
    let Ok(entries) = fs::read_dir(&hicolor) else {
        return Vec::new();
    };

    let mut found = Vec::new();
    for entry in entries.flatten() {
        let apps_dir = entry.path().join("apps");
        for extension in ["png", "svg"] {
            let candidate = apps_dir.join(format!("{icon_name}.{extension}"));
            if candidate.is_file() {
                found.push(candidate);
            }
        }
    }
    found.sort();
    found
}

fn find_top_level_icon(extract_root: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = fs::read_dir(extract_root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            matches!(
                path.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase())
                    .as_deref(),
                Some("png") | Some("svg")
            )
        })
        .collect();

    candidates.sort();
    candidates.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_util::write_atomic;

    /// Smallest possible valid PNG header for the requested dimensions:
    /// signature plus an IHDR chunk, which is all `imagesize` reads.
    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        out.extend_from_slice(&13u32.to_be_bytes());
        out.extend_from_slice(b"IHDR");
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&[8, 6, 0, 0, 0]);
        out.extend_from_slice(&[0, 0, 0, 0]);
        out
    }

    fn write_png(path: &Path, width: u32, height: u32) {
        write_atomic(path, &png_bytes(width, height), MODE_FILE).unwrap();
    }

    #[test]
    fn raster_icons_land_in_their_pixel_size_directory() {
        let dir = tempfile::tempdir().unwrap();
        let icon = dir.path().join("icon.png");
        write_png(&icon, 128, 128);

        let placement = placement_for(&icon).unwrap();
        assert_eq!(placement.directory(), "128x128");
        assert_eq!(placement.relative_path("app"), PathBuf::from("128x128/apps/app.png"));
    }

    #[test]
    fn non_square_icons_keep_both_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let icon = dir.path().join("icon.png");
        write_png(&icon, 64, 32);
        assert_eq!(placement_for(&icon).unwrap().directory(), "64x32");
    }

    #[test]
    fn svg_icons_are_scalable() {
        let dir = tempfile::tempdir().unwrap();
        let icon = dir.path().join("icon.svg");
        write_atomic(&icon, b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>", MODE_FILE)
            .unwrap();

        let placement = placement_for(&icon).unwrap();
        assert_eq!(placement, IconPlacement::Scalable);
        assert_eq!(placement.relative_path("app"), PathBuf::from("scalable/apps/app.svg"));
    }

    #[test]
    fn unreadable_images_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let icon = dir.path().join("icon.png");
        write_atomic(&icon, b"not an image", MODE_FILE).unwrap();
        assert!(placement_for(&icon).is_err());
    }

    #[test]
    fn hicolor_icons_win_over_diricon() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        write_png(&root.join("usr/share/icons/hicolor/64x64/apps/sample.png"), 64, 64);
        write_png(&root.join("usr/share/icons/hicolor/256x256/apps/sample.png"), 256, 256);
        write_png(&root.join(".DirIcon"), 48, 48);

        let icons = find_icons(root, Some("sample"));
        assert_eq!(icons.len(), 2);
        assert!(icons.iter().all(|p| p.to_string_lossy().contains("hicolor")));
    }

    #[test]
    fn falls_back_to_diricon_then_to_a_top_level_image() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        write_png(&root.join(".DirIcon"), 48, 48);
        assert_eq!(find_icons(root, Some("missing")), vec![root.join(".DirIcon")]);

        let other = tempfile::tempdir().unwrap();
        let other = other.path();
        write_png(&other.join("logo.png"), 96, 96);
        assert_eq!(find_icons(other, None), vec![other.join("logo.png")]);
    }

    #[test]
    fn nothing_found_is_an_empty_list_not_an_error() {
        let root = tempfile::tempdir().unwrap();
        assert!(find_icons(root.path(), Some("sample")).is_empty());
    }

    #[test]
    fn installs_every_size_under_the_slug() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        write_png(&root.join("usr/share/icons/hicolor/64x64/apps/sample.png"), 64, 64);
        write_png(&root.join("usr/share/icons/hicolor/256x256/apps/sample.png"), 256, 256);

        let icons_root = tempfile::tempdir().unwrap();
        let installed = install_icons(root, Some("sample"), "my-app", icons_root.path());

        assert_eq!(installed.len(), 2);
        assert!(icons_root.path().join("64x64/apps/my-app.png").is_file());
        assert!(icons_root.path().join("256x256/apps/my-app.png").is_file());
    }
}
