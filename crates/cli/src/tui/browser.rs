//! A small file browser: directories and AppImages, nothing else.

use std::path::{Path, PathBuf};

/// One row in the browser.
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub label: String,
    pub is_dir: bool,
}

#[derive(Debug)]
pub struct Browser {
    pub directory: PathBuf,
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub error: Option<String>,
}

impl Browser {
    /// Starts in `~/Downloads` when that exists, otherwise in the home
    /// directory, otherwise wherever the process happens to be.
    pub fn new() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let start = home
            .as_ref()
            .map(|home| home.join("Downloads"))
            .filter(|downloads| downloads.is_dir())
            .or(home)
            .unwrap_or_else(|| PathBuf::from("."));

        let mut browser = Self { directory: start, entries: Vec::new(), selected: 0, error: None };
        browser.reload();
        browser
    }

    pub fn reload(&mut self) {
        self.selected = 0;
        self.entries.clear();
        self.error = None;

        if self.directory.parent().is_some() {
            self.entries.push(Entry {
                path: self.directory.join(".."),
                label: "..".to_string(),
                is_dir: true,
            });
        }

        let read = match std::fs::read_dir(&self.directory) {
            Ok(read) => read,
            Err(error) => {
                self.error = Some(format!("{}: {error}", self.directory.display()));
                return;
            }
        };

        let mut directories = Vec::new();
        let mut files = Vec::new();
        for entry in read.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                directories.push(Entry { path, label: format!("{name}/"), is_dir: true });
            } else if is_appimage(&path) {
                files.push(Entry { path, label: name, is_dir: false });
            }
        }

        directories.sort_by_key(|entry| entry.label.to_lowercase());
        files.sort_by_key(|entry| entry.label.to_lowercase());
        self.entries.extend(directories);
        self.entries.extend(files);
    }

    pub fn selected_entry(&self) -> Option<&Entry> {
        self.entries.get(self.selected)
    }

    pub fn move_by(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() - 1;
        self.selected = match delta {
            delta if delta < 0 => self.selected.saturating_sub(delta.unsigned_abs()),
            delta => (self.selected + delta as usize).min(last),
        };
    }

    pub fn go_to(&mut self, index: usize) {
        self.selected = index.min(self.entries.len().saturating_sub(1));
    }

    /// Enters a directory. Returns the file to install when one was picked.
    pub fn activate(&mut self) -> Option<PathBuf> {
        let entry = self.selected_entry()?.clone();
        if !entry.is_dir {
            return Some(entry.path);
        }

        let target = if entry.label == ".." {
            self.directory.parent().map(Path::to_path_buf)
        } else {
            Some(entry.path)
        };
        if let Some(target) = target {
            self.directory = target;
            self.reload();
        }
        None
    }
}

fn is_appimage(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("appimage"))
}
