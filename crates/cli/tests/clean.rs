//! `appimg clean`: it lists and removes the backups and update leftovers of
//! the applications appimg manages, and nothing else. Every test takes a
//! snapshot of the whole temporary home before and after, so a file it
//! should not have touched cannot go missing or change unnoticed. No test
//! in here talks to any host or touches the real `$HOME`.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};

struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

/// Everything below a directory: each file with its contents and mode, each
/// link with where it points, each directory.
type Snapshot = BTreeMap<PathBuf, String>;

impl Home {
    fn new() -> Self {
        let dir = tempfile::Builder::new().prefix("appimg-test-").tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in [
            "home/Applications",
            "home/.local/bin",
            "data/appimages",
            "data/applications",
            "config",
            "state",
            "tmp",
        ] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        Self { _dir: dir, root }
    }

    fn appimages(&self) -> PathBuf {
        self.root.join("data/appimages")
    }

    /// Runs `appimg` on a pipe, with nobody to answer a question.
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_appimg"))
            .arg("--no-color")
            .args(args)
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("TMPDIR", self.root.join("tmp"))
            .env_remove("APPIMG_DIR")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// Runs `appimg` and insists on this exit code. Returns what it wrote.
    fn exits(&self, code: i32, args: &[&str]) -> String {
        let output = self.run(args);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.status.code(), Some(code), "{args:?}\n{text}");
        text
    }

    /// An application appimg manages, the way an install leaves it.
    fn managed(&self, slug: &str, name: &str) {
        self.entry(slug, name, true);
        write(&self.appimages().join(format!("{slug}.AppImage")), 700, Age::Old);
    }

    fn entry(&self, slug: &str, name: &str, managed: bool) {
        let file = slug.rsplit('/').next().unwrap();
        fs::write(
            self.root.join(format!("data/applications/{file}.desktop")),
            format!(
                "[Desktop Entry]\nType=Application\nName={name}\nExec={}/{slug}.AppImage\n\
                 X-AppImg-Managed={managed}\nX-AppImg-Slug={slug}\n",
                self.appimages().display()
            ),
        )
        .unwrap();
    }

    /// Two managed applications with every kind of leftover between them,
    /// 9700 bytes in all, and around them files that are none of appimg's
    /// business. Returns the leftovers.
    fn furnish(&self) -> Vec<PathBuf> {
        self.managed("fake-app", "Fake App");
        self.managed("other-app", "Other App");
        let leftovers: Vec<PathBuf> = [
            ("fake-app.AppImage.bak", 3000),
            ("fake-app.AppImage.new", 200),
            ("fake-app.AppImage.part", 100),
            ("fake-app.AppImage.archive", 400),
            ("fake-app.AppImage.zs-old", 5000),
            ("other-app.AppImage.bak", 1000),
        ]
        .into_iter()
        .map(|(name, size)| {
            let path = self.appimages().join(name);
            write(&path, size, Age::Old);
            path
        })
        .collect();
        self.strangers();
        leftovers
    }

    /// Files appimg does not own, which nothing may touch.
    fn strangers(&self) {
        let dir = self.appimages();
        // AppImages appimg does not manage, which `adopt --scan` lists.
        write(&dir.join("osu.AppImage"), 900, Age::Old);
        write(&self.root.join("home/Applications/Tool.AppImage"), 800, Age::Old);
        write(&self.root.join("home/.local/bin/thing.AppImage"), 600, Age::Old);
        // Named like leftovers, but of no application appimg manages: one
        // has an entry that does not say it is managed.
        write(&dir.join("osu.AppImage.bak"), 500, Age::Old);
        write(&dir.join("osu.AppImage.new"), 50, Age::Old);
        self.entry("osu", "osu!", false);
        // Named after a managed slug, but not the way appimg names a file,
        // or not where it keeps them.
        write(&dir.join("fake-app.appimage.bak"), 40, Age::Old);
        write(&dir.join("fake-app.AppImage.bak.1"), 40, Age::Old);
        write(&self.root.join("home/Applications/fake-app.AppImage.bak"), 40, Age::Old);
        // A temporary file of whoever, appimg cannot tell.
        write(&dir.join(".tmpAbC123"), 30, Age::Old);
        // A managed slug whose leftover is a link: appimg writes none, and
        // removing it is not its call.
        self.managed("linked", "Linked");
        symlink(self.root.join("home/Applications/Tool.AppImage"), dir.join("linked.AppImage.bak"))
            .unwrap();
        // A hand-edited entry whose slug leads out of the directory.
        self.entry("../escape", "Escape", true);
        write(&self.root.join("data/escape.AppImage.bak"), 20, Age::Old);
    }

    fn snapshot(&self) -> Snapshot {
        let mut snapshot = Snapshot::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let metadata = fs::symlink_metadata(&path).unwrap();
                let what = if metadata.is_symlink() {
                    format!("link to {}", fs::read_link(&path).unwrap().display())
                } else if metadata.is_dir() {
                    stack.push(path.clone());
                    "directory".to_string()
                } else {
                    format!(
                        "file {:o} {:?}",
                        metadata.permissions().mode(),
                        fs::read(&path).unwrap()
                    )
                };
                snapshot.insert(path, what);
            }
        }
        snapshot
    }
}

enum Age {
    /// Last written two hours ago.
    Old,
    /// Being written now.
    Fresh,
}

fn write(path: &Path, size: usize, age: Age) {
    fs::write(path, vec![b'x'; size]).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    if let Age::Old = age {
        let then = SystemTime::now() - Duration::from_secs(2 * 3600);
        File::options().write(true).open(path).unwrap().set_modified(then).unwrap();
    }
}

/// `before` without these paths.
fn without(before: &Snapshot, gone: &[PathBuf]) -> Snapshot {
    let mut expected = before.clone();
    for path in gone {
        assert!(expected.remove(path).is_some(), "{} was never there", path.display());
    }
    expected
}

#[test]
fn every_kind_is_listed_and_removed_and_nothing_else() {
    let home = Home::new();
    let leftovers = home.furnish();
    let scan = home.exits(3, &["adopt", "--scan"]);
    for stranger in ["osu.AppImage", "Tool.AppImage", "thing.AppImage"] {
        assert!(scan.contains(stranger), "{stranger} is not in\n{scan}");
    }
    let before = home.snapshot();

    let out = home.exits(0, &["--yes", "clean"]);

    for leftover in &leftovers {
        assert!(out.contains(&leftover.display().to_string()), "{}\n{out}", leftover.display());
    }
    for (kind, size) in [
        ("Fake App: backup of the previous version", "2.9 KB"),
        ("Fake App: half-finished download", "200 B"),
        ("Fake App: partial download", "100 B"),
        ("Fake App: archive downloaded by an update", "400 B"),
        ("Fake App: copy of the previous version", "4.9 KB"),
        ("Other App: backup of the previous version", "1000 B"),
    ] {
        let line = out.lines().find(|line| line.contains(kind)).unwrap_or_else(|| panic!("{out}"));
        assert!(line.ends_with(size), "{line}");
    }
    assert!(out.contains("removing it ends the rollback of Fake App"), "{out}");
    assert!(out.contains("removing it ends the rollback of Other App"), "{out}");
    assert!(out.contains("6 files, 9.5 KB in all."), "{out}");
    assert!(out.contains("Removed 6 files, 9.5 KB in all."), "{out}");
    for stranger in ["osu", "Tool", "thing", "linked", "escape", ".tmp", ".bak.1"] {
        assert!(!out.contains(stranger), "{stranger} is mentioned in\n{out}");
    }

    assert_eq!(home.snapshot(), without(&before, &leftovers));
    assert_eq!(home.exits(3, &["adopt", "--scan"]), scan);
    // What is left is nothing to clean.
    assert!(home.exits(3, &["clean"]).contains("Nothing to clean."));
}

#[test]
fn a_dry_run_shows_everything_and_changes_nothing() {
    let home = Home::new();
    let leftovers = home.furnish();
    let before = home.snapshot();

    for args in [&["clean", "--dry-run"][..], &["--yes", "clean", "--dry-run"]] {
        let out = home.exits(0, args);
        for leftover in &leftovers {
            assert!(out.contains(&leftover.display().to_string()), "{out}");
        }
        assert!(out.contains("6 files, 9.5 KB in all."), "{out}");
        assert!(out.contains("Nothing was changed (dry run)."), "{out}");
        assert!(!out.contains("Removed"), "{out}");
        assert_eq!(home.snapshot(), before, "{args:?}");
    }
}

#[test]
fn a_pipe_without_yes_refuses_and_changes_nothing() {
    let home = Home::new();
    home.furnish();
    let before = home.snapshot();

    let out = home.exits(1, &["clean"]);
    assert!(out.contains("pass --yes"), "{out}");
    assert_eq!(home.snapshot(), before);
}

#[test]
fn nothing_to_clean_exits_with_3() {
    let home = Home::new();
    home.managed("fake-app", "Fake App");
    home.strangers();
    let before = home.snapshot();

    for args in [&["clean"][..], &["--yes", "clean"], &["clean", "--dry-run"]] {
        assert!(home.exits(3, args).contains("Nothing to clean."), "{args:?}");
    }
    assert_eq!(home.snapshot(), before);
}

#[test]
fn a_download_an_update_may_still_be_writing_is_left_alone() {
    let home = Home::new();
    home.managed("fake-app", "Fake App");
    let part = home.appimages().join("fake-app.AppImage.part");
    let backup = home.appimages().join("fake-app.AppImage.bak");
    write(&part, 100, Age::Fresh);
    write(&backup, 3000, Age::Old);
    let before = home.snapshot();

    let out = home.exits(0, &["--yes", "clean"]);
    assert!(out.contains("Left alone, written in the last 15 minutes"), "{out}");
    assert!(out.contains("Removed 1 file, 2.9 KB in all."), "{out}");
    assert_eq!(home.snapshot(), without(&before, &[backup]));

    // A recent download alone is nothing to clean.
    let out = home.exits(3, &["--yes", "clean"]);
    assert!(out.contains(&part.display().to_string()), "{out}");
    assert!(out.contains("Nothing to clean."), "{out}");
    assert!(part.is_file());
}

#[test]
fn doctor_points_at_clean_when_there_is_something_to_clean() {
    let home = Home::new();
    home.managed("fake-app", "Fake App");
    home.strangers();
    assert!(!home.run(&["doctor"]).stdout.windows(12).any(|w| w == b"appimg clean"));

    write(&home.appimages().join("fake-app.AppImage.new"), 200, Age::Old);
    let out = String::from_utf8_lossy(&home.run(&["doctor"]).stdout).into_owned();
    assert!(out.contains("half-finished download"), "{out}");
    assert!(out.contains("`appimg clean`"), "{out}");
}
