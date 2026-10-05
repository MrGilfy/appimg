//! Helpers shared by the integration tests. Everything happens inside a
//! temporary directory, no test may touch the real `$HOME`.

#![allow(dead_code)]

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use appimg_core::desktop_entry::{DesktopEntry, KEY_SHA1};
use appimg_core::paths::Paths;
use appimg_core::stamp::Stamp;
use appimg_core::zsync;
use tempfile::TempDir;

/// Executing a file while any process still holds it open for writing fails
/// with `ETXTBSY`. The writing thread closes its handle before the file gets
/// its final name, but a `fork` in another test thread inherits that handle
/// for the moment between fork and exec, which is enough. The tests run one
/// at a time, so no fork can ever happen while a fixture is being written.
pub fn serial() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let lock = LOCK.get_or_init(|| Mutex::new(()));
    // A failing test must not take the rest of the suite down with it.
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Asserts that the entry at `entry` holds the checksum of `file`, stamped
/// with the size and time the file has now.
pub fn assert_stamped(entry: &Path, file: &Path) {
    let entry = DesktopEntry::read(entry).unwrap();
    let value = entry.get(KEY_SHA1).expect("the entry holds no checksum");
    let stamp = Stamp::parse(value).unwrap_or_else(|| panic!("not a stamp: {value}"));
    assert_eq!(stamp.sha1, zsync::sha1_file(file).unwrap(), "{value}");
    assert!(stamp.matches(file), "{value}");
}

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
            bin_dir: root.join("bin"),
            config_home: root.join("config"),
            state_home: root.join("state"),
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
    /// Whether the fixture gets the executable bit. A file that arrived
    /// through a browser does not have it.
    pub executable: bool,
    /// What the fake runtime does instead of extracting, if anything.
    pub failure: Option<(i32, String)>,
    /// Extra bytes appended to the runtime, so two builds differ.
    pub marker: String,
    /// Whether the file starts with an ELF header instead of a shebang.
    pub elf: bool,
    /// AppStream metainfo to put into `usr/share/metainfo`.
    pub metainfo: Option<String>,
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
            executable: true,
            failure: None,
            marker: String::new(),
            elf: false,
            metainfo: None,
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

    /// Builds a fixture without the executable bit, the way a download from
    /// a browser arrives.
    pub fn not_executable(mut self) -> Self {
        self.executable = false;
        self
    }

    /// Makes `--appimage-extract` fail with this exit code and message.
    pub fn failing(mut self, code: i32, message: &str) -> Self {
        self.failure = Some((code, message.to_string()));
        self
    }

    /// Ships AppStream metainfo with these `<url>` elements.
    pub fn metainfo(mut self, urls: &str) -> Self {
        self.metainfo = Some(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <component type=\"desktop-application\">\n  <id>org.example.FakeApp</id>\n  \
             {urls}\n</component>\n"
        ));
        self
    }

    pub fn marker(mut self, marker: &str) -> Self {
        self.marker = marker.to_string();
        self
    }

    /// Builds a file that starts with the ELF magic, as a real AppImage
    /// does and as an update insists on. It has no runtime that could run,
    /// so it only extracts through `unsquashfs`, and only through a stand-in
    /// for it that copies the payload the file names on its `payload=` line.
    pub fn elf(mut self) -> Self {
        self.elf = true;
        self
    }

    /// Writes a shell script that behaves like an AppImage runtime: called
    /// with `--appimage-extract` it drops a `squashfs-root` next to itself.
    /// With [`Self::elf`], writes a file that only starts like one instead.
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
        if let Some(metainfo) = &self.metainfo {
            let dir = payload.join("usr/share/metainfo");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("org.example.FakeApp.metainfo.xml"), metainfo).unwrap();
        }

        let extract = match &self.failure {
            Some((code, message)) => format!("echo '{message}' >&2\nexit {code}\n"),
            None => {
                format!("mkdir -p squashfs-root\ncp -R '{}/.' squashfs-root/\n", payload.display())
            }
        };
        let script = if self.elf {
            // The squashfs magic at the end is what tells appimg where the
            // payload starts, once the ELF header leads nowhere.
            format!(
                "\x7fELF\n\
                 payload={payload}\n\
                 # fake AppImage runtime {marker}\n\
                 hsqs\n",
                payload = payload.display(),
                marker = self.marker,
            )
        } else {
            format!(
                "#!/bin/sh\n\
                 # fake AppImage runtime {marker}\n\
                 if [ \"$1\" != \"--appimage-extract\" ]; then\n\
                 \techo 'fake appimage'\n\
                 \texit 0\n\
                 fi\n\
                 {extract}",
                marker = self.marker,
            )
        };

        // The script only appears under its final name once it is complete
        // and closed: no reader, and no `exec`, ever sees a half-written file.
        let path = dir.join(file_name);
        let partial = dir.join(format!(".{file_name}.partial"));
        let mut file = File::create(&partial).unwrap();
        file.write_all(script.as_bytes()).unwrap();
        file.flush().unwrap();
        drop(file);
        let mode = if self.executable { 0o755 } else { 0o644 };
        fs::set_permissions(&partial, fs::Permissions::from_mode(mode)).unwrap();
        fs::rename(&partial, &path).unwrap();
        path
    }
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

/// Runs something with a stand-in for `unsquashfs` on `PATH`, the one way a
/// `FakeAppImage::elf` build extracts: it copies the payload the file names
/// into the directory it is asked to unpack to.
pub fn with_unsquashfs_stand_in<T>(sandbox: &Sandbox, run: impl FnOnce() -> T) -> T {
    let dir = sandbox.root.join("unsquashfs-tool");
    std::fs::create_dir_all(&dir).unwrap();
    let tool = dir.join("unsquashfs");
    // Called as `unsquashfs -no-progress -o OFFSET -d ROOT FILE`.
    std::fs::write(
        &tool,
        "#!/bin/sh\n\
         payload=$(sed -n 's/^payload=//p' \"$6\")\n\
         mkdir -p \"$5\"\n\
         cp -R \"$payload/.\" \"$5/\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&tool, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    with_tools_on_path(&dir, run)
}

/// Runs something with a directory prepended to `PATH`. The tests run one at
/// a time, which is what makes this safe.
pub fn with_tools_on_path<T>(dir: &Path, run: impl FnOnce() -> T) -> T {
    let previous = std::env::var_os("PATH");
    let mut path = dir.as_os_str().to_os_string();
    if let Some(existing) = &previous {
        path.push(":");
        path.push(existing);
    }
    std::env::set_var("PATH", path);
    let result = run();
    match previous {
        Some(path) => std::env::set_var("PATH", path),
        None => std::env::remove_var("PATH"),
    }
    result
}

/// What a file or link under a directory is, as far as telling whether it
/// was touched goes: a file's mode and the SHA-256 of what it holds, or
/// where a link points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    File { mode: u32, sha256: String },
    Link(PathBuf),
}

/// Every file and symbolic link below `root`, by its path relative to it,
/// with what it is. Directories are left out: creating one touches nothing
/// that was there.
pub fn snapshot(root: &Path) -> std::collections::BTreeMap<PathBuf, Node> {
    let mut found = std::collections::BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if meta.file_type().is_symlink() {
                found.insert(relative, Node::Link(fs::read_link(&path).unwrap()));
            } else if meta.is_dir() {
                stack.push(path);
            } else {
                let sha256 = appimg_core::digest::sha256_file(&path).unwrap();
                found.insert(
                    relative,
                    Node::File { mode: meta.permissions().mode() & 0o7777, sha256 },
                );
            }
        }
    }
    found
}

/// The files the desktop database and the icon cache tools write after an
/// install, or after anything else that changes entries or icons. Whether
/// they are there depends on the machine running the tests.
pub fn is_cache(relative: &Path) -> bool {
    let name = relative.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    matches!(name, "mimeinfo.cache" | "icon-theme.cache" | "index.theme")
}

/// What changed between two snapshots, caches aside: what is gone, what is
/// new, and what is different.
pub fn changes(
    before: &std::collections::BTreeMap<PathBuf, Node>,
    after: &std::collections::BTreeMap<PathBuf, Node>,
) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
    let gone = before.keys().filter(|p| !after.contains_key(*p) && !is_cache(p)).cloned().collect();
    let new = after.keys().filter(|p| !before.contains_key(*p) && !is_cache(p)).cloned().collect();
    let changed = before
        .iter()
        .filter(|(p, node)| !is_cache(p) && after.get(*p).is_some_and(|other| other != *node))
        .map(|(p, _)| p.clone())
        .collect();
    (gone, new, changed)
}
