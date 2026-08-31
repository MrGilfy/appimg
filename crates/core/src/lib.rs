//! Core logic for `appimg`: installing, updating and removing AppImages.
//!
//! Nothing in here writes to a terminal or asks a question. Every decision a
//! user could make arrives as data, every path comes from [`Paths`].

pub mod caches;
pub mod desktop_entry;
pub mod doctor;
pub mod download;
pub mod elf;
pub mod error;
pub mod fs_util;
pub mod icon;
pub mod install;
pub mod json;
pub mod list;
pub mod metadata;
pub mod paths;
pub mod remove;
pub mod slug;
pub mod update;
pub mod version;

pub use desktop_entry::{DesktopEntry, MAIN_CATEGORIES};
pub use error::{Error, Result};
pub use install::{IconChoice, InstallOutcome, InstallPlan, InstallRequest};
pub use list::{Health, InstalledApp};
pub use metadata::AppImageInfo;
pub use paths::Paths;
pub use update::{UpdateOutcome, UpdateSource, UpdateStatus};

/// The locale to prefer when reading localized desktop entry keys, e.g.
/// `de_DE`. Reads the usual environment variables, in the order POSIX
/// prescribes.
pub fn current_locale() -> Option<String> {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(value) = std::env::var_os(key) {
            let value = value.to_string_lossy().to_string();
            let trimmed = value.trim();
            if !trimmed.is_empty() && trimmed != "C" && trimmed != "POSIX" {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}
