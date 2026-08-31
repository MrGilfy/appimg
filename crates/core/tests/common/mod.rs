//! Helpers shared by the integration tests. Everything happens inside a
//! temporary directory, no test may touch the real `$HOME`.

#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use appimg_core::metadata::{self, AppImageInfo};
use appimg_core::paths::Paths;
use tempfile::TempDir;

/// A throwaway XDG home with the directories `appimg` writes to.
pub struct Sandbox {
    _dir: TempDir,
    pub root: PathBuf,
    pub paths: Paths,
    pub downloads: PathBuf,
}

impl Sandbox {
    pub fn new() -> Self {
        let dir = tempfile::Builder::new().prefix("appimg-test-").tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let data_home = root.join("data");
        let downloads = root.join("downloads");
        fs::create_dir_all(&downloads).unwrap();

        let paths = Paths {
            appimage_dir: data_home.join("appimages"),
            applications_dir: data_home.join("applications"),
            icons_root: data_home.join("icons").join("hicolor"),
            config_home: root.join("config"),
            data_home,
        };
        paths.ensure_dirs().unwrap();

        Self { _dir: dir, root, paths, downloads }
    }
}

/// Smallest PNG `imagesize` accepts: signature plus an IHDR chunk.
pub fn png_bytes(width: u32, height: u32) -> Vec<u8> {
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

/// What an extracted AppImage should contain.
pub struct FakeAppImage {
    pub app_name: String,
    pub icon_name: String,
    pub icon_sizes: Vec<u32>,
    pub categories: String,
    pub exec: String,
    pub extra_keys: Vec<(String, String)>,
    /// Extra bytes appended to the runtime, so two builds differ.
    pub marker: String,
}

impl FakeAppImage {
    pub fn new(app_name: &str) -> Self {
        Self {
            app_name: app_name.to_string(),
            icon_name: "fakeapp".to_string(),
            icon_sizes: vec![48, 256],
            categories: "Utility;".to_string(),
            exec: "AppRun %U".to_string(),
            extra_keys: Vec::new(),
            marker: String::new(),
        }
    }

    pub fn icon_sizes(mut self, sizes: &[u32]) -> Self {
        self.icon_sizes = sizes.to_vec();
        self
    }

    pub fn key(mut self, key: &str, value: &str) -> Self {
        self.extra_keys.push((key.to_string(), value.to_string()));
        self
    }

    pub fn marker(mut self, marker: &str) -> Self {
        self.marker = marker.to_string();
        self
    }

    /// Writes a shell script that behaves like an AppImage runtime: called
    /// with `--appimage-extract` it drops a `squashfs-root` next to itself.
    pub fn build(&self, dir: &Path, file_name: &str) -> PathBuf {
        let payload = dir.join(format!(".payload-{file_name}"));
        let apps_root = payload.join("usr/share/icons/hicolor");
        fs::create_dir_all(&payload).unwrap();

        let mut entry = String::from("[Desktop Entry]\nType=Application\n");
        entry.push_str(&format!("Name={}\n", self.app_name));
        entry.push_str(&format!("Name[de]={} (de)\n", self.app_name));
        entry.push_str("Comment=A fake application\n");
        entry.push_str(&format!("Exec={}\n", self.exec));
        entry.push_str(&format!("Icon={}\n", self.icon_name));
        entry.push_str(&format!("Categories={}\n", self.categories));
        entry.push_str("Terminal=false\n");
        for (key, value) in &self.extra_keys {
            entry.push_str(&format!("{key}={value}\n"));
        }
        fs::write(payload.join(format!("{}.desktop", self.icon_name)), entry).unwrap();

        for size in &self.icon_sizes {
            let apps = apps_root.join(format!("{size}x{size}")).join("apps");
            fs::create_dir_all(&apps).unwrap();
            fs::write(apps.join(format!("{}.png", self.icon_name)), png_bytes(*size, *size))
                .unwrap();
        }
        fs::write(payload.join(".DirIcon"), png_bytes(32, 32)).unwrap();

        let script = format!(
            "#!/bin/sh\n\
             # fake AppImage runtime {marker}\n\
             if [ \"$1\" != \"--appimage-extract\" ]; then\n\
             \techo 'fake appimage'\n\
             \texit 0\n\
             fi\n\
             mkdir -p squashfs-root\n\
             cp -R '{payload}/.' squashfs-root/\n",
            marker = self.marker,
            payload = payload.display(),
        );

        let path = dir.join(file_name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
}

/// Reads an AppImage the way the tests need it. Running a file that another
/// test thread still holds open for writing fails with `ETXTBSY`, which says
/// nothing about the code under test, so give the extraction a few tries.
pub fn inspect(path: &Path, locale: Option<&str>) -> AppImageInfo {
    for _ in 0..50 {
        let info = metadata::inspect(path, locale).unwrap();
        if info.extract_root().is_some() {
            return info;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("{} never extracted", path.display());
}

pub fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

pub fn is_executable(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// Every file below `root`, relative to it, sorted.
pub fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    collect(root, root, &mut found);
    found.sort();
    found
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else {
            out.push(path.strip_prefix(root).unwrap().to_path_buf());
        }
    }
}
