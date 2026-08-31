use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::fs_util;
use crate::paths::Paths;

const SYSTEM_INDEX_THEMES: &[&str] =
    &["/usr/share/icons/hicolor/index.theme", "/usr/local/share/icons/hicolor/index.theme"];

/// Refreshes the desktop database and the icon cache. Both tools are
/// optional, a missing one only means the desktop picks the change up a
/// little later.
pub fn refresh(paths: &Paths) {
    if let Some(tool) = fs_util::which("update-desktop-database") {
        run(&tool, &[paths.applications_dir.as_os_str()]);
    }

    if let Some(tool) = fs_util::which("gtk-update-icon-cache") {
        ensure_index_theme(&paths.icons_root);
        run(&tool, &["-f".as_ref(), "-t".as_ref(), paths.icons_root.as_os_str()]);
    }
}

/// Runs `desktop-file-validate` and returns whatever it complains about.
pub fn validate_desktop_entry(path: &Path) -> Vec<String> {
    let Some(tool) = fs_util::which("desktop-file-validate") else {
        return Vec::new();
    };
    let Ok(output) = Command::new(tool).arg(path).stdin(Stdio::null()).output() else {
        return Vec::new();
    };

    let mut messages: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&output.stderr).lines())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    messages.dedup();
    messages
}

/// Without an `index.theme` in the user's hicolor tree the icon cache stays
/// empty, so seed it from the system theme.
pub fn ensure_index_theme(icons_root: &Path) {
    let target = icons_root.join("index.theme");
    if target.exists() {
        return;
    }
    if fs::create_dir_all(icons_root).is_err() {
        return;
    }
    for source in SYSTEM_INDEX_THEMES {
        if fs::copy(source, &target).is_ok() {
            return;
        }
    }
}

fn run(tool: &Path, args: &[&std::ffi::OsStr]) {
    let _ = Command::new(tool)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
