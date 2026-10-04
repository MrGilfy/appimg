//! Update notifications: a systemd user timer that runs `appimg notify
//! check` once a day, and the desktop notification that check shows when
//! updates are available.
//!
//! Everything stays in the user's home. The units live in
//! `$XDG_CONFIG_HOME/systemd/user` and `systemctl --user` manages them;
//! nothing here needs root or touches the system manager.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::error::{Error, Result};
use crate::fs_util::{self, MODE_FILE};
use crate::list::InstalledApp;
use crate::paths::Paths;
use crate::update::UpdateStatus;

pub const SERVICE: &str = "appimg-notify.service";
pub const TIMER: &str = "appimg-notify.timer";

/// What the timer runs, after the binary.
pub const CHECK_ARGS: &[&str] = &["notify", "check"];

/// The icon both notification tools ask the notification server for.
const ICON: &str = "system-software-update";

const HEADER: &str = "# Written by `appimg notify enable`, removed by `appimg notify disable`.\n";

/// Where the two units go.
#[derive(Debug, Clone)]
pub struct Units {
    pub service: PathBuf,
    pub timer: PathBuf,
}

impl Units {
    pub fn new(paths: &Paths) -> Self {
        let dir = paths.config_home.join("systemd").join("user");
        Self { service: dir.join(SERVICE), timer: dir.join(TIMER) }
    }

    /// Whether either unit file is there.
    pub fn exist(&self) -> bool {
        self.service.exists() || self.timer.exists()
    }
}

/// The service unit: it runs `binary notify check` against the same
/// directories this run of appimg uses, whatever the environment of the
/// user manager says about them.
pub fn service_unit(binary: &Path, paths: &Paths) -> Result<String> {
    let mut unit = String::from(HEADER);
    unit.push_str("[Unit]\nDescription=Check the AppImages appimg manages for updates\n\n");
    unit.push_str("[Service]\nType=oneshot\n");
    let pinned = [
        ("XDG_DATA_HOME", &paths.data_home),
        ("APPIMG_DIR", &paths.appimage_dir),
        // Where the record of announced updates lives, see [`Announced`].
        ("XDG_STATE_HOME", &paths.state_home),
    ];
    for (key, value) in pinned {
        let value = unit_text(value)?;
        unit.push_str(&format!("Environment={}\n", quote(&format!("{key}={value}"))));
    }
    let binary = unit_text(binary)?;
    // systemd takes these in an argument, but refuses the program itself.
    if binary.contains(['"', '\'', '\\']) {
        return Err(Error::NotForUnit {
            path: binary.into(),
            reason: "systemd runs no program whose path holds a quote, an apostrophe or a \
                     backslash",
        });
    }
    unit.push_str(&format!("ExecStart={} {}\n", quote(binary), CHECK_ARGS.join(" ")));
    Ok(unit)
}

/// The timer unit: once a day, a missed day caught up after the next boot,
/// and up to an hour later than that at random, so that not every machine
/// asks GitHub in the same minute.
pub fn timer_unit() -> String {
    format!(
        "{HEADER}[Unit]\nDescription=Check the AppImages appimg manages for updates once a \
         day\n\n[Timer]\nOnCalendar=daily\nPersistent=true\nRandomizedDelaySec=1h\n\n\
         [Install]\nWantedBy=timers.target\n"
    )
}

/// Writes both units, replacing whatever was there.
pub fn write_units(paths: &Paths, binary: &Path) -> Result<Units> {
    let units = Units::new(paths);
    let service = service_unit(binary, paths)?;
    fs_util::write_atomic(&units.service, service.as_bytes(), MODE_FILE)?;
    fs_util::write_atomic(&units.timer, timer_unit().as_bytes(), MODE_FILE)?;
    Ok(units)
}

/// Removes both unit files. Returns whether there was anything to remove.
pub fn remove_units(paths: &Paths) -> Result<bool> {
    let units = Units::new(paths);
    let mut removed = false;
    for path in [&units.timer, &units.service] {
        match fs::remove_file(path) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io(path, error)),
        }
    }
    Ok(removed)
}

/// What the service unit on disk runs, read back out of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub binary: PathBuf,
    pub environment: Vec<(String, String)>,
}

/// Reads the service unit `appimg notify enable` wrote. `None` when there is
/// none, or when it holds no command line this module could have written.
pub fn installed(paths: &Paths) -> Option<Installed> {
    let text = fs::read_to_string(Units::new(paths).service).ok()?;
    let mut binary = None;
    let mut environment = Vec::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("ExecStart=") {
            binary = unquote(value).map(PathBuf::from);
        } else if let Some(value) = line.strip_prefix("Environment=") {
            if let Some((key, value)) = unquote(value).as_deref().and_then(|v| v.split_once('=')) {
                environment.push((key.to_string(), value.to_string()));
            }
        }
    }
    Some(Installed { binary: binary?, environment })
}

impl Installed {
    /// What the service sets `key` to.
    pub fn var(&self, key: &str) -> Option<&str> {
        self.environment.iter().find(|(k, _)| k == key).map(|(_, value)| value.as_str())
    }
}

/// Why a binary at `path` may not stay there, for the warning that a timer
/// running it can break: `None` when it looks like a place to keep one.
pub fn looks_temporary(path: &Path) -> Option<&'static str> {
    let names: Vec<&std::ffi::OsStr> = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    let in_target = names.iter().position(|name| *name == "target").is_some_and(|at| {
        names[at + 1..].iter().any(|name| *name == "debug" || *name == "release")
    });
    let custom_target = std::env::var_os("CARGO_TARGET_DIR")
        .filter(|dir| !dir.is_empty())
        .is_some_and(|dir| path.starts_with(dir));
    if in_target || custom_target {
        return Some("a cargo target directory");
    }

    let mut temporary = vec![
        PathBuf::from("/tmp"),
        PathBuf::from("/var/tmp"),
        PathBuf::from("/dev/shm"),
        std::env::temp_dir(),
    ];
    // Emptied when the last session of the user ends.
    temporary
        .extend(std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()).map(PathBuf::from));
    if temporary.iter().any(|dir| path.starts_with(dir)) {
        return Some("a temporary directory");
    }
    None
}

/// Runs `systemctl --user` with these arguments and returns what it printed.
pub fn systemctl(args: &[&str]) -> Result<String> {
    let tool = fs_util::which("systemctl").ok_or(Error::NoSystemctl)?;
    let output = run(Command::new(&tool).arg("--user").args(args), &tool)?;
    if !output.status.success() {
        return Err(Error::Systemctl { command: args.join(" "), message: failure(&output) });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether `systemctl` is there to run at all.
pub fn has_systemctl() -> bool {
    fs_util::which("systemctl").is_some()
}

/// The properties `systemctl --user show` reports for `unit`, by name. A
/// property without a value, such as a timestamp of something that never
/// happened, is an empty string.
pub fn show(unit: &str, properties: &[&str]) -> Result<HashMap<String, String>> {
    let output = systemctl(&["show", unit, &format!("--property={}", properties.join(","))])?;
    Ok(output
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect())
}

/// A desktop notification: a summary and a body of plain text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub summary: String,
    pub body: String,
}

impl Notification {
    /// The notification for the applications an update is available for,
    /// one line each with the versions where they are known.
    pub fn for_updates(available: &[&UpdateStatus]) -> Self {
        let summary = match available.len() {
            1 => "An AppImage update is available".to_string(),
            count => format!("{count} AppImage updates are available"),
        };
        let mut lines: Vec<String> = available
            .iter()
            .map(|status| match (&status.current_version, &status.latest_version) {
                (Some(current), Some(latest)) => format!("{} {current} → {latest}", status.name),
                (None, Some(latest)) => format!("{} → {latest}", status.name),
                _ => status.name.clone(),
            })
            .collect();
        lines.push("appimg update --all installs them".to_string());
        Self { summary, body: lines.join("\n") }
    }

    /// The notification `appimg notify test` shows.
    pub fn sample() -> Self {
        Self {
            summary: "appimg update notifications work".to_string(),
            body: "When updates are available, a notification like this one names the \
                   applications."
                .to_string(),
        }
    }
}

/// The updates notifications named already, by slug, so that a check names
/// each new version once instead of every day it stays pending. Kept in
/// `$XDG_STATE_HOME/appimg/announced`, one `<slug>\t<version>` per line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Announced {
    versions: BTreeMap<String, String>,
}

impl Announced {
    pub fn path(paths: &Paths) -> PathBuf {
        Self::path_in(&paths.state_home)
    }

    fn path_in(state_home: &Path) -> PathBuf {
        state_home.join("appimg").join("announced")
    }

    /// Deletes the record below the state directory `state_home`, and the
    /// directory appimg keeps it in once that is empty. Returns the path of
    /// the record when there was one.
    pub fn delete(state_home: &Path) -> Result<Option<PathBuf>> {
        let path = Self::path_in(state_home);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::io(&path, error)),
        }
        if let Some(dir) = path.parent() {
            let _ = fs::remove_dir(dir);
        }
        Ok(Some(path))
    }

    /// What was announced so far. No record yet is an empty one, and a
    /// line that is no record is passed over.
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = Self::path(paths);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(Error::io(&path, error)),
        };
        let versions = text
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(slug, version)| (slug.to_string(), version.to_string()))
            .collect();
        Ok(Self { versions })
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let text: String =
            self.versions.iter().map(|(slug, version)| format!("{slug}\t{version}\n")).collect();
        fs_util::write_atomic(&Self::path(paths), text.as_bytes(), MODE_FILE)
    }

    /// The updates among `available` that no notification named yet: a
    /// version announced before and still pending is not news.
    pub fn unannounced<'a>(&self, available: &[&'a UpdateStatus]) -> Vec<&'a UpdateStatus> {
        available
            .iter()
            .copied()
            .filter(|status| self.versions.get(&status.slug) != Some(&offered(status)))
            .collect()
    }

    /// Takes in a check of `installed` that found `statuses`, once its
    /// notification went out. Every update available counts as announced.
    /// One that is not available any more is forgotten, so is an application
    /// that is no longer installed. One whose check failed keeps what it had,
    /// so a day without a network announces nothing twice. Returns whether
    /// anything changed.
    pub fn record(&mut self, statuses: &[UpdateStatus], installed: &[InstalledApp]) -> bool {
        let before = self.versions.clone();
        self.versions.retain(|slug, _| installed.iter().any(|app| &app.slug == slug));
        for status in statuses {
            if status.available {
                self.versions.insert(status.slug.clone(), offered(status));
            } else {
                self.versions.remove(&status.slug);
            }
        }
        self.versions != before
    }
}

/// The version an update offers, as the record keeps it: on one line, and
/// empty when the source names none.
fn offered(status: &UpdateStatus) -> String {
    status.latest_version.as_deref().unwrap_or_default().replace(['\t', '\n', '\r'], " ")
}

/// The tool that shows a notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notifier {
    NotifySend(PathBuf),
    /// `gdbus`, calling `org.freedesktop.Notifications` on the session bus.
    Gdbus(PathBuf),
}

impl Notifier {
    /// `notify-send` where it is installed, `gdbus` where only that is.
    pub fn find() -> Result<Self> {
        if let Some(tool) = fs_util::which("notify-send") {
            return Ok(Notifier::NotifySend(tool));
        }
        fs_util::which("gdbus").map(Notifier::Gdbus).ok_or(Error::NoNotifier)
    }

    pub fn name(&self) -> &'static str {
        match self {
            Notifier::NotifySend(_) => "notify-send",
            Notifier::Gdbus(_) => "gdbus",
        }
    }

    pub fn send(&self, notification: &Notification) -> Result<()> {
        // Most notification servers read the body as markup.
        let body = escape_markup(&notification.body);
        let (tool, mut command) = match self {
            Notifier::NotifySend(tool) => {
                let mut command = Command::new(tool);
                command
                    .arg("--app-name=appimg")
                    .arg(format!("--icon={ICON}"))
                    .arg(&notification.summary)
                    .arg(&body);
                (tool, command)
            }
            Notifier::Gdbus(tool) => {
                let mut command = Command::new(tool);
                // Notify(app_name, replaces_id, app_icon, summary, body,
                // actions, hints, expire_timeout), each argument written
                // out as a typed GVariant.
                command
                    .args(["call", "--session", "--dest", "org.freedesktop.Notifications"])
                    .args(["--object-path", "/org/freedesktop/Notifications"])
                    .args(["--method", "org.freedesktop.Notifications.Notify"])
                    .arg(gvariant_string("appimg"))
                    .arg("uint32 0")
                    .arg(gvariant_string(ICON))
                    .arg(gvariant_string(&notification.summary))
                    .arg(gvariant_string(&body))
                    .arg("@as []")
                    .arg("@a{sv} {}")
                    .arg("int32 -1");
                (tool, command)
            }
        };
        let output = run(&mut command, tool)?;
        if !output.status.success() {
            return Err(Error::Notification { tool: self.name(), message: failure(&output) });
        }
        Ok(())
    }
}

fn run(command: &mut Command, tool: &Path) -> Result<Output> {
    command.stdin(Stdio::null()).output().map_err(|e| Error::io(tool, e))
}

/// What a tool that failed said about it, or its exit status when it said
/// nothing.
fn failure(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        output.status.to_string()
    } else {
        stderr
    }
}

/// A path as text a unit file can hold: UTF-8, without control characters,
/// which no quoting carries through.
fn unit_text(path: &Path) -> Result<&str> {
    path.to_str().filter(|text| !text.chars().any(char::is_control)).ok_or_else(|| {
        Error::NotForUnit {
            path: path.to_path_buf(),
            reason: "it is not UTF-8 or holds a control character",
        }
    })
}

/// `value` as one quoted word of a unit file setting: backslash and quote
/// escaped, `%` doubled so no specifier expands. A `$` stays as it is,
/// which is what systemd reads in a program path and in `Environment=`;
/// only arguments to the program would expand it.
fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            '%' => out.push_str("%%"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The first word of a setting [`quote`] wrote, as it was before quoting.
fn unquote(setting: &str) -> Option<String> {
    let mut chars = setting.strip_prefix('"')?.chars();
    let mut out = String::new();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => out.push(chars.next()?),
            '%' => {
                chars.next().filter(|c| *c == '%')?;
                out.push('%');
            }
            c => out.push(c),
        }
    }
}

/// `value` as a GVariant string literal, which `gdbus call` parses.
fn gvariant_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(root: &str) -> Paths {
        let root = PathBuf::from(root);
        Paths {
            data_home: root.join("data"),
            config_home: root.join("config"),
            state_home: root.join("state"),
            appimage_dir: root.join("apps"),
            applications_dir: root.join("data/applications"),
            icons_root: root.join("data/icons/hicolor"),
        }
    }

    #[test]
    fn quoting_survives_what_systemd_would_expand() {
        for (value, quoted) in [
            ("/usr/bin/appimg", r#""/usr/bin/appimg""#),
            (r#"A=/a b/"q"\x"#, r#""A=/a b/\"q\"\\x""#),
            ("/100%/$HOME", r#""/100%%/$HOME""#),
        ] {
            assert_eq!(quote(value), quoted);
            assert_eq!(unquote(&format!("{quoted} notify check")).unwrap(), value);
        }
        assert_eq!(unquote("/usr/bin/appimg notify check"), None);
        assert_eq!(unquote(r#""/unterminated"#), None);
    }

    #[test]
    fn the_service_unit_runs_this_binary_against_these_directories() {
        let paths = paths("/home/u/my %h");
        let unit = service_unit(Path::new("/opt/app$img/appimg"), &paths).unwrap();
        assert!(unit.contains("\nType=oneshot\n"), "{unit}");
        assert!(unit.contains("\nEnvironment=\"XDG_DATA_HOME=/home/u/my %%h/data\"\n"), "{unit}");
        assert!(unit.contains("\nEnvironment=\"APPIMG_DIR=/home/u/my %%h/apps\"\n"), "{unit}");
        assert!(unit.contains("\nEnvironment=\"XDG_STATE_HOME=/home/u/my %%h/state\"\n"), "{unit}");
        assert!(unit.contains("\nExecStart=\"/opt/app$img/appimg\" notify check\n"), "{unit}");

        let line = |prefix: &str| unit.lines().find_map(|l| l.strip_prefix(prefix)).unwrap();
        assert_eq!(unquote(line("ExecStart=")).unwrap(), "/opt/app$img/appimg");

        for refused in ["/tmp/new\nline/appimg", "/tmp/it's/appimg", "/tmp/a\\b/appimg"] {
            let refused = service_unit(Path::new(refused), &paths);
            assert!(matches!(refused, Err(Error::NotForUnit { .. })), "{refused:?}");
        }
    }

    #[test]
    fn the_timer_is_daily_persistent_and_spread_out() {
        let timer = timer_unit();
        for line in ["OnCalendar=daily", "Persistent=true", "RandomizedDelaySec=1h"] {
            assert!(timer.contains(&format!("\n{line}\n")), "{timer}");
        }
        assert!(timer.contains("\n[Install]\nWantedBy=timers.target\n"), "{timer}");
    }

    #[test]
    fn a_cargo_target_or_temporary_directory_looks_temporary() {
        for path in [
            "/home/u/src/appimg/target/debug/appimg",
            "/home/u/src/appimg/target/x86_64-unknown-linux-musl/release/appimg",
            "/tmp/appimg",
            "/var/tmp/x/appimg",
        ] {
            assert!(looks_temporary(Path::new(path)).is_some(), "{path}");
        }
        for path in ["/usr/bin/appimg", "/home/u/.cargo/bin/appimg", "/home/u/target/appimg"] {
            assert_eq!(looks_temporary(Path::new(path)), None, "{path}");
        }
    }

    #[test]
    fn the_notification_names_every_app_with_an_update() {
        let status = |name: &str, current: Option<&str>, latest: Option<&str>| UpdateStatus {
            slug: name.to_lowercase(),
            name: name.to_string(),
            current_version: current.map(str::to_string),
            latest_version: latest.map(str::to_string),
            available: true,
            source: crate::update::UpdateSource::Manual,
            note: None,
            settled: false,
        };
        let (one, two) = (status("One", Some("1.0"), Some("2.0")), status("Two", None, None));

        let single = Notification::for_updates(&[&one]);
        assert_eq!(single.summary, "An AppImage update is available");
        assert_eq!(single.body, "One 1.0 → 2.0\nappimg update --all installs them");

        let both = Notification::for_updates(&[&one, &two]);
        assert_eq!(both.summary, "2 AppImage updates are available");
        assert_eq!(both.body, "One 1.0 → 2.0\nTwo\nappimg update --all installs them");
    }

    #[test]
    fn each_version_is_announced_once_and_a_failed_check_forgets_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path().to_str().unwrap());
        let app = |slug: &str| InstalledApp {
            slug: slug.to_string(),
            name: slug.to_string(),
            comment: None,
            categories: Vec::new(),
            version: None,
            origin: None,
            update_info: None,
            update_source: None,
            release: None,
            installed_at: None,
            appimage_path: PathBuf::new(),
            desktop_entry_path: PathBuf::new(),
            size_bytes: None,
            health: crate::list::Health::Ok,
        };
        let status = |slug: &str, latest: &str, available: bool| UpdateStatus {
            slug: slug.to_string(),
            name: slug.to_string(),
            current_version: Some("1".to_string()),
            latest_version: Some(latest.to_string()),
            available,
            source: crate::update::UpdateSource::Manual,
            note: None,
            settled: false,
        };
        let names = |found: Vec<&UpdateStatus>| -> Vec<String> {
            found
                .iter()
                .map(|s| format!("{} {}", s.slug, s.latest_version.as_deref().unwrap()))
                .collect()
        };
        let installed = [app("a"), app("b")];

        let mut announced = Announced::load(&paths).unwrap();
        assert_eq!(announced, Announced::default());
        let (a2, b1) = (status("a", "2", true), status("b", "1", false));
        assert_eq!(names(announced.unannounced(&[&a2])), ["a 2"]);
        assert!(announced.record(&[a2.clone(), b1], &installed));
        announced.save(&paths).unwrap();

        // Still pending: nothing new, nothing changed.
        let mut announced = Announced::load(&paths).unwrap();
        assert!(announced.unannounced(&[&a2]).is_empty());
        assert!(!announced.record(std::slice::from_ref(&a2), &installed));

        // A newer version is news, and the pending one beside it is not.
        let (a3, b2) = (status("a", "3", true), status("b", "2", true));
        assert_eq!(names(announced.unannounced(&[&a2, &b2])), ["b 2"]);
        assert_eq!(names(announced.unannounced(&[&a3, &b2])), ["a 3", "b 2"]);
        announced.record(&[a3.clone(), b2.clone()], &installed);

        // b's check failed: what it had stays. a was updated: forgotten.
        announced.record(&[status("a", "3", false)], &installed);
        assert!(announced.unannounced(&[&b2]).is_empty());
        assert_eq!(names(announced.unannounced(&[&a3])), ["a 3"]);

        // An application that went is forgotten too.
        announced.record(&[], &installed[..1]);
        assert_eq!(names(announced.unannounced(&[&b2])), ["b 2"]);
        announced.save(&paths).unwrap();
        assert_eq!(fs::read_to_string(Announced::path(&paths)).unwrap(), "");
    }

    #[test]
    fn gvariant_strings_and_markup_are_escaped() {
        assert_eq!(gvariant_string("a \"b\"\\\nc"), r#""a \"b\"\\\nc""#);
        assert_eq!(escape_markup("Tom & <Jerry>"), "Tom &amp; &lt;Jerry&gt;");
    }
}
