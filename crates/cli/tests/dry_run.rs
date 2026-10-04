//! Nothing before a confirmed install runs the AppImage: not `install
//! --dry-run`, not `adopt --dry-run`, whether `unsquashfs` is there to read
//! the metadata without running it or not. The fixture leaves a mark when
//! anything runs it, and arrives without the executable bit the way a
//! browser saves it, which running it would set first. No test in here
//! talks to any host or touches the real `$HOME`.

mod marker;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};

/// Executing a file while any process still holds it open for writing fails
/// with `ETXTBSY`, and a `fork` in another test thread can hold one for a
/// moment. The tests run one at a time.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What is on `PATH` for a run.
#[derive(Clone, Copy, Debug)]
enum Tools {
    /// A stand-in for `unsquashfs` ahead of the real `PATH`.
    Unsquashfs,
    /// Nothing at all, so no `unsquashfs` either, whatever the machine has.
    Nothing,
}

struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::Builder::new().prefix("appimg-test-").tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in ["home/Downloads", "data", "config", "tmp", "tools", "no-tools"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        // Called as `unsquashfs -no-progress -o OFFSET -d ROOT FILE`: it
        // copies the payload the file names.
        let unsquashfs = root.join("tools/unsquashfs");
        fs::write(
            &unsquashfs,
            "#!/bin/sh\npayload=$(sed -n 's/^payload=//p' \"$6\")\nmkdir -p \"$5\"\n\
             cp -R \"$payload/.\" \"$5/\"\n",
        )
        .unwrap();
        fs::set_permissions(&unsquashfs, fs::Permissions::from_mode(0o755)).unwrap();
        Self { _dir: dir, root }
    }

    /// Runs `appimg` with these arguments, on a pipe, with nobody to answer
    /// a question.
    fn run(&self, tools: Tools, args: &[&str]) -> Output {
        let path = match tools {
            Tools::Unsquashfs => format!(
                "{}:{}",
                self.root.join("tools").display(),
                std::env::var("PATH").unwrap_or_default()
            ),
            Tools::Nothing => self.root.join("no-tools").display().to_string(),
        };
        Command::new(env!("CARGO_BIN_EXE_appimg"))
            .arg("--no-color")
            .args(args)
            .env("PATH", path)
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("TMPDIR", self.root.join("tmp"))
            .env_remove("APPIMG_DIR")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// The marker AppImage, not executable, with a payload that holds a
    /// desktop entry naming it Fake App.
    fn marker(&self, dir: &str, file_name: &str) -> PathBuf {
        let payload = self.root.join(format!("payloads/{file_name}"));
        fs::create_dir_all(&payload).unwrap();
        fs::write(
            payload.join("fakeapp.desktop"),
            "[Desktop Entry]\nType=Application\nName=Fake App\nExec=AppRun %U\nIcon=fakeapp\n\
             Categories=Utility;\n",
        )
        .unwrap();
        let path = self.root.join(dir).join(file_name);
        marker::write(&path, &self.mark(), &payload, 0o644);
        path
    }

    /// The file the marker AppImage creates when it runs.
    fn mark(&self) -> PathBuf {
        self.root.join("it-ran")
    }

    fn data(&self, relative: &str) -> PathBuf {
        self.root.join("data").join(relative)
    }

    /// Every file in the temporary home with its mode and what it holds,
    /// caches aside.
    fn snapshot(&self) -> BTreeMap<PathBuf, String> {
        let mut found = BTreeMap::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                let meta = fs::symlink_metadata(&path).unwrap();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                if matches!(name.as_str(), "mimeinfo.cache" | "icon-theme.cache" | "index.theme") {
                    continue;
                }
                if meta.is_dir() {
                    stack.push(path);
                    continue;
                }
                let sha256 = appimg_core::digest::sha256_file(&path).unwrap();
                let mode = meta.permissions().mode() & 0o7777;
                let relative = path.strip_prefix(&self.root).unwrap().to_path_buf();
                found.insert(relative, format!("{mode:o} {sha256}"));
            }
        }
        found
    }
}

fn arg(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// With `unsquashfs` there, the dry run shows the plan with what the
/// desktop entry inside says, and runs nothing.
#[test]
fn an_install_dry_run_reads_the_metadata_through_unsquashfs_and_runs_nothing() {
    let _serial = serial();
    let home = Home::new();
    let file = home.marker("home/Downloads", "Marker.AppImage");
    let before = home.snapshot();

    let output = home.run(Tools::Unsquashfs, &["install", "--dry-run", arg(&file)]);
    assert!(!home.mark().exists(), "the dry run ran the AppImage");
    assert_eq!(home.snapshot(), before);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("Would install as fake-app"), "{}", stdout(&output));
    assert!(stdout(&output).contains("Name=Fake App"), "{}", stdout(&output));
    assert!(!stderr(&output).contains("was not read"), "{}", stderr(&output));
}

/// Without `unsquashfs`, the dry run reads no metadata, says so and why,
/// and shows the plan with the name the file name gives. It still runs
/// nothing.
#[test]
fn an_install_dry_run_without_unsquashfs_says_why_it_read_nothing_and_runs_nothing() {
    let _serial = serial();
    let home = Home::new();
    let file = home.marker("home/Downloads", "Marker.AppImage");
    let before = home.snapshot();

    let output = home.run(Tools::Nothing, &["install", "--dry-run", arg(&file)]);
    assert!(!home.mark().exists(), "the dry run ran the AppImage");
    assert_eq!(home.snapshot(), before);

    assert!(output.status.success(), "{}", stderr(&output));
    let err = stderr(&output);
    assert!(err.contains("the metadata inside the AppImage was not read"), "{err}");
    assert!(err.contains("unsquashfs is not installed (package squashfs-tools)"), "{err}");
    assert!(err.contains("the AppImage was not run to read it instead"), "{err}");
    let out = stdout(&output);
    assert!(out.contains("The plan uses the name \"Marker\", from the file name"), "{out}");
    assert!(out.contains("Would install as marker"), "{out}");
    assert!(out.contains("Name=Marker"), "{out}");
}

/// An adoption is an install too: its dry run runs nothing either, with
/// `unsquashfs` and without.
#[test]
fn an_adopt_dry_run_runs_nothing_with_or_without_unsquashfs() {
    let _serial = serial();
    let home = Home::new();
    let file = home.marker("home/Downloads", "Marker.AppImage");
    let before = home.snapshot();

    for (tools, slug) in [(Tools::Unsquashfs, "fake-app"), (Tools::Nothing, "marker")] {
        let output = home.run(tools, &["adopt", "--dry-run", arg(&file)]);
        assert!(!home.mark().exists(), "the dry run with {tools:?} ran the AppImage");
        assert_eq!(home.snapshot(), before, "{tools:?}");

        assert!(output.status.success(), "{tools:?}: {}", stderr(&output));
        let out = stdout(&output);
        assert!(out.contains(&format!("Would adopt as {slug}")), "{tools:?}: {out}");
    }
}

/// What makes the tests above worth something: the marker does leave its
/// mark when appimg runs it. An install the user asked for may run the
/// AppImage to read the metadata, but only when `unsquashfs` cannot.
#[test]
fn a_confirmed_install_runs_the_appimage_only_when_unsquashfs_cannot_read_it() {
    let _serial = serial();

    let home = Home::new();
    let file = home.marker("home/Downloads", "Marker.AppImage");
    let output = home.run(Tools::Unsquashfs, &["--yes", "install", arg(&file)]);
    assert!(!home.mark().exists(), "the install ran the AppImage, unsquashfs could read it");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(home.data("appimages/fake-app.AppImage").is_file());

    let home = Home::new();
    let file = home.marker("home/Downloads", "Marker.AppImage");
    let output = home.run(Tools::Nothing, &["--yes", "install", arg(&file)]);
    assert!(home.mark().exists(), "the install did not run the AppImage");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(home.data("appimages/marker.AppImage").is_file());
}
