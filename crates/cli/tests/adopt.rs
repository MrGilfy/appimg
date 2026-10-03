//! `appimg adopt` as a user runs it, against a temporary home. No test in
//! here talks to any host or touches the real `$HOME`, and the ones that
//! change something compare the whole temporary home before and after.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
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

struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::Builder::new().prefix("appimg-test-").tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in
            ["home/Applications", "home/.local/bin", "data/applications", "config", "tmp", "tools"]
        {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        // The fixtures only start like an AppImage, so they extract through
        // a stand-in for `unsquashfs`, called as `unsquashfs -no-progress -o
        // OFFSET -d ROOT FILE`: it copies the payload the file names.
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

    fn command(&self) -> Command {
        let path = format!(
            "{}:{}",
            self.root.join("tools").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_appimg"));
        command
            .arg("--no-color")
            .env("PATH", path)
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("TMPDIR", self.root.join("tmp"))
            .env_remove("APPIMG_DIR")
            .stdin(Stdio::null());
        command
    }

    /// Runs `appimg` with these arguments, on a pipe, with nobody to answer
    /// a question.
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn data(&self, relative: &str) -> PathBuf {
        self.root.join("data").join(relative)
    }

    /// A file that starts the way an AppImage does and names a payload with
    /// a desktop entry, an icon and the AppStream metainfo given, which the
    /// stand-in for `unsquashfs` extracts.
    fn appimage(&self, dir: &str, file_name: &str, metainfo_urls: Option<&str>) -> PathBuf {
        let dir = self.root.join(dir);
        let payload = self.root.join(format!("payloads/{file_name}"));
        fs::create_dir_all(payload.join("usr/share/icons/hicolor/48x48/apps")).unwrap();
        fs::write(
            payload.join("fakeapp.desktop"),
            "[Desktop Entry]\nType=Application\nName=Fake App\nExec=AppRun %U\nIcon=fakeapp\n\
             Categories=Utility;\n",
        )
        .unwrap();
        fs::write(payload.join("usr/share/icons/hicolor/48x48/apps/fakeapp.png"), png(48)).unwrap();
        if let Some(urls) = metainfo_urls {
            let metainfo = payload.join("usr/share/metainfo");
            fs::create_dir_all(&metainfo).unwrap();
            fs::write(
                metainfo.join("org.example.FakeApp.metainfo.xml"),
                format!("<?xml version=\"1.0\"?>\n<component>\n  {urls}\n</component>\n"),
            )
            .unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(file_name);
        let partial = dir.join(format!(".{file_name}.partial"));
        let mut file = File::create(&partial).unwrap();
        write!(file, "\x7fELF\npayload={}\n# fake AppImage runtime\nhsqs\n", payload.display())
            .unwrap();
        drop(file);
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&partial, &path).unwrap();
        path
    }

    /// A desktop entry the way AppImageLauncher writes one, and its icon.
    fn launcher_entry(&self, name: &str, exec: &str, icon: &str) -> (PathBuf, PathBuf) {
        let entry = self.data(&format!("applications/{name}"));
        fs::write(
            &entry,
            format!("[Desktop Entry]\nType=Application\nName=Fake App\nExec={exec}\nIcon={icon}\n"),
        )
        .unwrap();
        let icon_path = self.data(&format!("icons/hicolor/48x48/apps/{icon}.png"));
        fs::create_dir_all(icon_path.parent().unwrap()).unwrap();
        fs::write(&icon_path, png(48)).unwrap();
        (entry, icon_path)
    }

    /// Every file and link in the temporary home, with what it holds or
    /// where it points, caches aside.
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
                let relative = path.strip_prefix(&self.root).unwrap().to_path_buf();
                if meta.file_type().is_symlink() {
                    found.insert(
                        relative,
                        format!("-> {}", fs::read_link(&path).unwrap().display()),
                    );
                } else if meta.is_dir() {
                    stack.push(path);
                } else {
                    let sha256 = appimg_core::digest::sha256_file(&path).unwrap();
                    let mode = meta.permissions().mode() & 0o7777;
                    found.insert(relative, format!("{mode:o} {sha256}"));
                }
            }
        }
        found
    }
}

fn png(size: u32) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    out.extend_from_slice(&13u32.to_be_bytes());
    out.extend_from_slice(b"IHDR");
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    out
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// What changed between two snapshots: gone, new, different.
fn changes(
    before: &BTreeMap<PathBuf, String>,
    after: &BTreeMap<PathBuf, String>,
) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
    let gone = before.keys().filter(|p| !after.contains_key(*p)).cloned().collect();
    let new = after.keys().filter(|p| !before.contains_key(*p)).cloned().collect();
    let changed = before
        .iter()
        .filter(|(p, v)| after.get(*p).is_some_and(|w| w != *v))
        .map(|(p, _)| p.clone())
        .collect();
    (gone, new, changed)
}

/// The whole of it, as a user runs it with `--yes`: the file moves, the
/// entry AppImageLauncher wrote for it goes with its icon, and of everything
/// else in the home nothing changes, entries that only look alike included.
#[test]
fn adopting_with_yes_removes_what_it_showed_and_touches_nothing_else() {
    let _serial = serial();
    let home = Home::new();
    let file = home.appimage("home/Applications", "Fake App.AppImage", None);
    let quoted = format!("\"{}\"", file.display());
    let (entry, icon) = home.launcher_entry(
        "appimagekit_1a2b-Fake_App.desktop",
        &format!("{quoted} %U"),
        "appimagekit_1a2b_fakeapp",
    );
    // A copy of the same file elsewhere, launched by an entry of its own,
    // and an entry that runs the file through `env`.
    let copy = home.root.join("home/elsewhere/Fake App.AppImage");
    fs::create_dir_all(copy.parent().unwrap()).unwrap();
    fs::copy(&file, &copy).unwrap();
    home.launcher_entry(
        "appimagekit_3c4d-Fake_App.desktop",
        &format!("\"{}\"", copy.display()),
        "appimagekit_3c4d_fakeapp",
    );
    home.launcher_entry(
        "through-env.desktop",
        &format!("env A=1 {quoted}"),
        "appimagekit_5e6f_env",
    );
    let before = home.snapshot();

    let output = home.run(&["--yes", "adopt", file.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);

    // It showed what launches the file, and what would go with it.
    assert!(
        out.contains(&format!(
            "A desktop entry from elsewhere already launches {}:",
            file.display()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!("  {}\n    with its icon {}\n", entry.display(), icon.display())),
        "{out}"
    );
    assert!(out.contains("Adopted Fake App as fake-app"), "{out}");
    assert!(out.contains(&format!("moved from {}", file.display())), "{out}");
    assert!(out.contains(&format!("  removed {}", entry.display())), "{out}");
    assert!(out.contains(&format!("  removed {}", icon.display())), "{out}");

    let relative = |path: &Path| path.strip_prefix(&home.root).unwrap().to_path_buf();
    let (gone, new, changed) = changes(&before, &home.snapshot());
    let mut expected_gone = vec![relative(&file), relative(&entry), relative(&icon)];
    expected_gone.sort();
    assert_eq!(gone, expected_gone);
    assert_eq!(
        new,
        vec![
            PathBuf::from("data/appimages/fake-app.AppImage"),
            PathBuf::from("data/applications/fake-app.desktop"),
            PathBuf::from("data/icons/hicolor/48x48/apps/fake-app.png"),
        ]
    );
    assert_eq!(changed, Vec::<PathBuf>::new());
    assert_eq!(
        fs::read(home.data("appimages/fake-app.AppImage")).unwrap(),
        fs::read(&copy).unwrap()
    );
}

/// With nobody to ask, the foreign entries stay, and the output says which
/// flag decides. `--keep-entries` keeps them without the question.
#[test]
fn on_a_pipe_or_with_keep_entries_the_foreign_entries_stay() {
    let _serial = serial();
    for args in [&["adopt"][..], &["--yes", "adopt", "--keep-entries"][..]] {
        let home = Home::new();
        let file = home.appimage("home/Applications", "Fake.AppImage", None);
        let (entry, icon) = home.launcher_entry(
            "appimagekit_1-Fake.desktop",
            file.to_str().unwrap(),
            "appimagekit_1_fake",
        );

        let mut args = args.to_vec();
        args.push(file.to_str().unwrap());
        let output = home.run(&args);
        assert!(output.status.success(), "{}", stderr(&output));
        let out = stdout(&output);

        assert!(entry.is_file() && icon.is_file(), "{out}");
        assert!(!out.contains("removed"), "{out}");
        if args.contains(&"--keep-entries") {
            assert!(out.contains("They stay, as --keep-entries says."), "{out}");
        } else {
            assert!(out.contains("Pass --yes to remove them, or --keep-entries"), "{out}");
        }
        assert!(home.data("appimages/fake-app.AppImage").is_file());
    }
}

/// As it turned up for real: an AppImage already in the appimages directory
/// under its slug, and an entry another installer wrote at `<slug>.desktop`
/// that launches it, with its icon under the slug in two sizes. A dry run
/// lists that entry and its icons like any other from elsewhere, and with
/// `--yes` the adopted entry and icons take their place, no old size left.
#[test]
fn a_foreign_entry_under_the_slug_is_replaced_when_accepted() {
    let _serial = serial();
    let home = Home::new();
    let file = home.appimage("data/appimages", "fake-app.AppImage", None);
    let (entry, icon) =
        home.launcher_entry("fake-app.desktop", &format!("\"{}\" %U", file.display()), "fake-app");
    // A size the AppImage does not ship.
    let small_icon = home.data("icons/hicolor/16x16/apps/fake-app.png");
    fs::create_dir_all(small_icon.parent().unwrap()).unwrap();
    fs::write(&small_icon, png(16)).unwrap();
    let before = home.snapshot();

    let output = home.run(&["adopt", "--dry-run", file.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("Would adopt as fake-app"), "{out}");
    assert!(out.contains("written over the one from elsewhere that is there, if that one goes"));
    assert!(
        out.contains(&format!(
            "  {}\n    is where the adopted entry goes: replaced if they go, its icons below too; \
             while it stays \"fake-app\" is taken\n    with its icon {}\n    with its icon {}\n",
            entry.display(),
            small_icon.display(),
            icon.display()
        )),
        "{out}"
    );
    assert_eq!(home.snapshot(), before);

    let output = home.run(&["--yes", "adopt", file.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("Adopted Fake App as fake-app"), "{out}");
    assert!(out.contains("written over the one from elsewhere that was there"), "{out}");
    // The size the AppImage does not ship is gone; the one it ships was
    // written again at the same path.
    assert!(out.contains(&format!("  removed {}\n", small_icon.display())), "{out}");
    assert!(out.contains(&format!("  replaced {}\n", icon.display())), "{out}");
    assert!(!out.contains(&format!("removed {}", icon.display())), "{out}");
    assert!(icon.is_file());
    let written = fs::read_to_string(&entry).unwrap();
    assert!(written.contains("\nX-AppImg-Managed=true\n"), "{written}");

    let (gone, new, changed) = changes(&before, &home.snapshot());
    assert_eq!(gone, vec![PathBuf::from("data/icons/hicolor/16x16/apps/fake-app.png")]);
    assert_eq!(new, Vec::<PathBuf>::new());
    assert_eq!(changed, vec![PathBuf::from("data/applications/fake-app.desktop")]);
}

/// While that entry stays, on a pipe with nobody to ask or with
/// `--keep-entries`, the slug is taken and nothing changes. A dry run with
/// `--keep-entries` already says so.
#[test]
fn a_foreign_entry_under_the_slug_that_stays_keeps_the_slug_taken() {
    let _serial = serial();
    let cases = [
        &["adopt"][..],
        &["--yes", "adopt", "--keep-entries"],
        &["adopt", "--dry-run", "--keep-entries"],
    ];
    for args in cases {
        let home = Home::new();
        let file = home.appimage("data/appimages", "fake-app.AppImage", None);
        let (entry, _) = home.launcher_entry(
            "fake-app.desktop",
            &format!("\"{}\" %U", file.display()),
            "fake-app",
        );
        let before = home.snapshot();

        let mut args = args.to_vec();
        args.push(file.to_str().unwrap());
        let output = home.run(&args);
        assert_eq!(output.status.code(), Some(1), "{args:?}\n{}", stdout(&output));
        assert!(
            stderr(&output).contains(&format!(
                "{} is already there, so \"fake-app\" is taken: pass --name",
                entry.display()
            )),
            "{args:?}\n{}",
            stderr(&output)
        );
        assert_eq!(home.snapshot(), before, "{args:?}");
    }
}

#[test]
fn a_file_in_local_bin_leaves_a_link_behind_and_says_so() {
    let _serial = serial();
    let home = Home::new();
    let file = home.appimage("home/.local/bin", "fakeapp.AppImage", None);
    let bytes = fs::read(&file).unwrap();

    let output = home.run(&["--yes", "adopt", file.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);

    let target = home.data("appimages/fake-app.AppImage");
    assert!(
        out.contains(&format!(
            "  link    {} -> {}, so the command keeps working",
            file.display(),
            target.display()
        )),
        "{out}"
    );
    assert_eq!(fs::read_link(&file).unwrap(), target);
    assert_eq!(fs::read(&file).unwrap(), bytes);

    // Elsewhere, nothing is left behind.
    let home = Home::new();
    let file = home.appimage("home/Applications", "fakeapp.AppImage", None);
    let output = home.run(&["--yes", "adopt", file.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stdout(&output).contains("link"), "{}", stdout(&output));
    assert!(fs::symlink_metadata(&file).is_err());
}

#[test]
fn a_copy_leaves_the_original_and_says_so() {
    let _serial = serial();
    let home = Home::new();
    let file = home.appimage("home/.local/bin", "fakeapp.AppImage", None);
    let before = fs::read(&file).unwrap();

    let output = home.run(&["--yes", "adopt", "--copy", file.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(
        out.contains(&format!("copied from {}, which stays where it is", file.display())),
        "{out}"
    );
    assert!(!out.contains("link"), "{out}");
    assert!(fs::symlink_metadata(&file).unwrap().is_file());
    assert_eq!(fs::read(&file).unwrap(), before);
}

/// The update source is set up exactly the way an install sets it up: the
/// AppStream suggestion is taken with `--yes`, left out on a pipe with the
/// flag named, and an update source on the command line is not second-
/// guessed. The origin is where the file was.
#[test]
fn the_update_source_is_set_up_the_way_an_install_does_it() {
    let _serial = serial();
    let urls = "<url type=\"vcs-browser\">https://github.com/fake/app</url>";

    let home = Home::new();
    let file = home.appimage("home/Applications", "Fake.AppImage", Some(urls));
    let output = home.run(&["--yes", "adopt", file.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    let entry = fs::read_to_string(home.data("applications/fake-app.desktop")).unwrap();
    assert!(entry.contains("\nX-AppImg-UpdateSource=github:fake/app\n"), "{entry}");
    assert!(entry.contains(&format!("\nX-AppImg-Source={}\n", file.display())), "{entry}");
    assert!(stdout(&output).contains("updates from github:fake/app"), "{}", stdout(&output));

    let home = Home::new();
    let file = home.appimage("home/Applications", "Fake.AppImage", Some(urls));
    let output = home.run(&["adopt", file.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("Pass --update-source github:fake/app"),
        "{}",
        stdout(&output)
    );
    let entry = fs::read_to_string(home.data("applications/fake-app.desktop")).unwrap();
    assert!(entry.contains("\nX-AppImg-UpdateSource=manual\n"), "{entry}");

    let home = Home::new();
    let file = home.appimage("home/Applications", "Fake.AppImage", Some(urls));
    let source = "https://example.com/App.AppImage";
    let output = home.run(&["--yes", "adopt", file.to_str().unwrap(), "--update-source", source]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stdout(&output).contains("AppStream"), "{}", stdout(&output));
    let entry = fs::read_to_string(home.data("applications/fake-app.desktop")).unwrap();
    assert!(entry.contains(&format!("UpdateSource={source}\n")), "{entry}");
}

/// The scan lists what could be adopted with the command that does it, and
/// that command, run as printed, does it. The scan itself changes nothing,
/// and runs nothing: reading the metadata of an AppImage that is not
/// executable would make it executable first.
#[test]
fn the_scan_prints_the_command_for_each_candidate_and_changes_nothing() {
    let _serial = serial();
    let home = Home::new();
    let stray = home.appimage("data/appimages", "Stray.AppImage", None);
    fs::set_permissions(&stray, fs::Permissions::from_mode(0o644)).unwrap();
    let spaced = home.appimage("home/Applications", "It's Fake.AppImage", None);
    let (entry, _) = home.launcher_entry(
        "appimagekit_9-Fake.desktop",
        &format!("\"{}\"", spaced.display()),
        "appimagekit_9_fake",
    );
    let broken = home.root.join("home/.local/bin/broken.AppImage");
    fs::write(&broken, "#!/bin/sh\necho not an AppImage\n").unwrap();
    let before = home.snapshot();

    let output = home.run(&["adopt", "--scan"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert_eq!(home.snapshot(), before);

    assert!(out.contains("2 of 3 AppImages in"), "{out}");
    assert!(out.contains(&format!("appimg adopt {}\n", stray.display())), "{out}");
    let command = format!("appimg adopt '{}'", spaced.display().to_string().replace('\'', "'\\''"));
    assert!(out.contains(&format!("  launched by {}\n  {command}\n", entry.display())), "{out}");
    assert!(
        out.contains(
            "cannot be adopted: it is not an AppImage, it does not start with an ELF header"
        ),
        "{out}"
    );
    assert!(!out.contains(&format!("appimg adopt {}", broken.display())), "{out}");

    // The printed command, through a shell, as a user would paste it.
    let line = out.lines().map(str::trim).find(|line| line.contains("It'\\''s")).unwrap();
    let pasted = line.replacen("appimg", "\"$APPIMG\" --yes", 1);
    let shell = Command::new("sh")
        .arg("-c")
        .arg(&pasted)
        .env(
            "PATH",
            format!(
                "{}:{}",
                home.root.join("tools").display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("HOME", home.root.join("home"))
        .env("XDG_DATA_HOME", home.root.join("data"))
        .env("XDG_CONFIG_HOME", home.root.join("config"))
        .env("TMPDIR", home.root.join("tmp"))
        .env_remove("APPIMG_DIR")
        .env("APPIMG", env!("CARGO_BIN_EXE_appimg"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(shell.status.success(), "{pasted}\n{}", stderr(&shell));
    assert!(home.data("appimages/fake-app.AppImage").is_file());
    assert!(!spaced.exists());

    // Nothing left to adopt in a home that has nothing: exit code 3.
    let empty = Home::new();
    let output = empty.run(&["adopt", "--scan"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(stdout(&output).contains("Nothing to adopt in"), "{}", stdout(&output));
}

#[test]
fn what_cannot_be_adopted_is_refused_and_nothing_changes() {
    let _serial = serial();
    let home = Home::new();
    let real = home.appimage("home/Applications", "Real.AppImage", None);
    let link = home.root.join("home/.local/bin/real");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let script = home.root.join("home/Applications/Script.AppImage");
    fs::write(&script, "#!/bin/sh\n").unwrap();
    let before = home.snapshot();

    let output = home.run(&["--yes", "adopt", link.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains(&format!("is a symbolic link to {}", real.display())),
        "{}",
        stderr(&output)
    );

    let output = home.run(&["--yes", "adopt", script.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("does not start with an ELF header"), "{}", stderr(&output));

    let output = home.run(&["--yes", "adopt", "--dry-run", real.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("Would adopt as fake-app"), "{}", stdout(&output));
    assert_eq!(home.snapshot(), before);

    // A slug that is taken is refused, with the way out named.
    let other = home.appimage("home/Applications", "Other.AppImage", None);
    let output = home.run(&["--yes", "adopt", real.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    let taken = home.snapshot();
    let output = home.run(&["--yes", "adopt", other.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("\"fake-app\" is taken: pass --name"), "{}", stderr(&output));
    assert_eq!(home.snapshot(), taken);
    let output = home.run(&["--yes", "adopt", other.to_str().unwrap(), "--name", "Other App"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(home.data("appimages/other-app.AppImage").is_file());
}

/// The change that came with adopting: a local install gets the checks a
/// download gets, before anything runs the file.
#[test]
fn a_local_install_gets_the_checks_a_download_gets() {
    let _serial = serial();
    let home = Home::new();
    let script = home.root.join("home/Script.AppImage");
    fs::write(&script, "#!/bin/sh\ntouch \"$HOME/it-ran\"\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let before = home.snapshot();

    let output = home.run(&["--yes", "install", script.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains(&format!(
            "{}: it is not an AppImage, it does not start with an ELF header",
            script.display()
        )),
        "{}",
        stderr(&output)
    );
    assert_eq!(home.snapshot(), before);
    assert!(!home.root.join("home/it-ran").exists());
}
