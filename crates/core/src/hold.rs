//! Holding an application at the version it has. A held application is
//! passed over by `update --all`, the terminal interface's update of
//! everything and the notification timer, and updated by name only when
//! the user says so. The hold is a key in the desktop entry,
//! [`KEY_HOLD`], and nothing else.
//!
//! Holding never hides that an update exists. Every check of a held
//! application records what it found in [`KEY_HOLD_CHECK`], so that a
//! listing, which asks no server, can say whether the hold keeps one back
//! and since when it knows.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::date;
use crate::desktop_entry::{DesktopEntry, KEY_HOLD, KEY_HOLD_CHECK};
use crate::error::Result;
use crate::list::InstalledApp;
use crate::update::UpdateStatus;

/// What [`KEY_HOLD`] holds while an application is held.
const HELD: &str = "true";

/// A held application, and what the last check of it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    /// `None` until a check of the held application went through.
    pub checked: Option<Checked>,
}

/// What a check of a held application found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    /// When, in seconds since the epoch.
    pub at: i64,
    pub found: Found,
    /// The version the source offers, when it names one.
    pub latest: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// An update is available, and the hold keeps it back.
    Available,
    /// The installed version is current.
    Current,
    /// The check could not tell, see its note.
    Unknown,
}

impl Found {
    /// How [`KEY_HOLD_CHECK`] and `list --json` spell it.
    pub fn word(self) -> &'static str {
        match self {
            Found::Available => "available",
            Found::Current => "current",
            Found::Unknown => "unknown",
        }
    }
}

impl Hold {
    /// The hold an entry records, `None` for an application that is not held.
    pub fn of(entry: &DesktopEntry) -> Option<Hold> {
        (entry.get(KEY_HOLD) == Some(HELD))
            .then(|| Hold { checked: entry.get(KEY_HOLD_CHECK).and_then(Checked::parse) })
    }

    /// Whether the last check found an update the hold keeps back.
    pub fn holds_back_an_update(&self) -> bool {
        self.checked.as_ref().is_some_and(|checked| checked.found == Found::Available)
    }

    /// What to show for it: `held, 1.3.0 available (checked 2026-10-05)`.
    pub fn describe(&self) -> String {
        let Some(checked) = &self.checked else {
            return "held, not checked yet".to_string();
        };
        let day = date::from_seconds(checked.at).unwrap_or_else(|| "?".to_string());
        let found = match (checked.found, checked.latest.as_deref()) {
            (Found::Available, Some(latest)) => format!("{latest} available"),
            (Found::Available, None) => "update available".to_string(),
            (Found::Current, _) => "up to date".to_string(),
            (Found::Unknown, _) => "update unknown".to_string(),
        };
        format!("held, {found} (checked {day})")
    }
}

impl Checked {
    /// What `status`, a check that just went through, found.
    pub fn from_status(status: &UpdateStatus) -> Self {
        let found = if status.available {
            Found::Available
        } else if status.nothing_to_do() {
            Found::Current
        } else {
            Found::Unknown
        };
        Self { at: now(), found, latest: status.latest_version.clone() }
    }

    fn parse(value: &str) -> Option<Self> {
        let mut fields = value.splitn(3, ' ');
        let at = fields.next()?.parse().ok()?;
        let found = match fields.next()? {
            "available" => Found::Available,
            "current" => Found::Current,
            "unknown" => Found::Unknown,
            _ => return None,
        };
        let latest = match fields.next()?.trim() {
            "" => return None,
            "-" => None,
            latest => Some(latest.to_string()),
        };
        Some(Self { at, found, latest })
    }
}

impl std::fmt::Display for Checked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let latest = self.latest.as_deref().map(str::trim).filter(|latest| !latest.is_empty());
        write!(f, "{} {} {}", self.at, self.found.word(), latest.unwrap_or("-"))
    }
}

/// Holds an application, or releases it with `held` false. Returns whether
/// anything changed: holding one that is held already changes nothing. A
/// record of an earlier check goes either way, it belongs to the hold it
/// was taken under.
pub fn set(app: &InstalledApp, held: bool) -> Result<bool> {
    let mut entry = DesktopEntry::read(&app.desktop_entry_path)?;
    if Hold::of(&entry).is_some() == held {
        return Ok(false);
    }
    if held {
        entry.set(KEY_HOLD, HELD);
    } else {
        entry.remove(KEY_HOLD);
    }
    entry.remove(KEY_HOLD_CHECK);
    entry.write(&app.desktop_entry_path)?;
    Ok(true)
}

/// Holds the application `entry` describes. For an entry written over a
/// held one: the hold is the user's and stays, what a check found under it
/// was about the file that is gone.
pub(crate) fn keep_in(entry: &mut DesktopEntry) {
    entry.set(KEY_HOLD, HELD);
    entry.remove(KEY_HOLD_CHECK);
}

/// Records what a check of a held application found. A check of one that
/// is not held records nothing, and a check that could not be recorded
/// still has its answer.
pub(crate) fn record(app: &InstalledApp, status: &UpdateStatus) {
    if app.hold.is_none() {
        return;
    }
    let Ok(mut entry) = DesktopEntry::read(&app.desktop_entry_path) else {
        return;
    };
    if Hold::of(&entry).is_none() {
        return;
    }
    entry.set(KEY_HOLD_CHECK, Checked::from_status(status).to_string());
    let _ = entry.write(&app.desktop_entry_path);
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_check_reads_back_what_it_wrote() {
        for checked in [
            Checked { at: 1_791_158_400, found: Found::Available, latest: Some("1.3.0".into()) },
            Checked { at: 1_791_158_400, found: Found::Current, latest: None },
            Checked { at: 0, found: Found::Unknown, latest: Some("2.0 beta".into()) },
        ] {
            assert_eq!(Checked::parse(&checked.to_string()), Some(checked));
        }
        for value in ["", "17", "17 maybe 1.0", "x available 1.0", "17 available", "17 available "]
        {
            assert_eq!(Checked::parse(value), None, "{value:?}");
        }
    }

    #[test]
    fn a_hold_is_the_key_and_nothing_else() {
        let mut entry = DesktopEntry::new();
        assert_eq!(Hold::of(&entry), None);
        entry.set(KEY_HOLD_CHECK, "1791158400 available 1.3.0");
        assert_eq!(Hold::of(&entry), None);
        entry.set(KEY_HOLD, "false");
        assert_eq!(Hold::of(&entry), None);

        entry.set(KEY_HOLD, HELD);
        let hold = Hold::of(&entry).unwrap();
        assert!(hold.holds_back_an_update());
        assert_eq!(hold.describe(), "held, 1.3.0 available (checked 2026-10-05)");
    }

    #[test]
    fn a_hold_says_what_the_last_check_found() {
        let hold = |found: Found, latest: Option<&str>| Hold {
            checked: Some(Checked { at: 1_791_158_400, found, latest: latest.map(Into::into) }),
        };
        assert_eq!(
            hold(Found::Available, None).describe(),
            "held, update available (checked 2026-10-05)"
        );
        assert_eq!(
            hold(Found::Current, Some("1.2.0")).describe(),
            "held, up to date (checked 2026-10-05)"
        );
        assert_eq!(
            hold(Found::Unknown, None).describe(),
            "held, update unknown (checked 2026-10-05)"
        );
        assert!(!hold(Found::Current, None).holds_back_an_update());
        assert_eq!(Hold { checked: None }.describe(), "held, not checked yet");
    }
}
