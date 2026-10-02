//! Where an installed AppImage updates from, as a user sets it on the
//! command line. Everything runs against a temporary XDG home and local
//! files; no test in here talks to any host.

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
        let root = dir.path().to_path_buf();
        for sub in ["home", "data", "config", "tmp", "downloads"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        Self { _dir: dir, root }
    }

    /// Runs `appimg` with these arguments, on a pipe, with nobody to answer
    /// a question.
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_appimg"))
            .arg("--no-color")
            .args(args)
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("TMPDIR", self.root.join("tmp"))
            .env_remove("APPIMG_DIR")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn entry(&self, slug: &str) -> String {
        fs::read_to_string(self.root.join(format!("data/applications/{slug}.desktop"))).unwrap()
    }

    /// A shell script that behaves like an AppImage runtime: called with
    /// `--appimage-extract` it drops a `squashfs-root` with a desktop entry,
    /// and the AppStream metainfo given, next to itself.
    fn appimage(&self, file_name: &str, metainfo_urls: Option<&str>) -> PathBuf {
        let dir = self.root.join("downloads");
        let payload = dir.join(format!(".payload-{file_name}"));
        fs::create_dir_all(&payload).unwrap();
        fs::write(
            payload.join("fakeapp.desktop"),
            "[Desktop Entry]\nType=Application\nName=Fake App\nExec=AppRun\nIcon=fakeapp\n\
             Categories=Utility;\n",
        )
        .unwrap();
        if let Some(urls) = metainfo_urls {
            let metainfo = payload.join("usr/share/metainfo");
            fs::create_dir_all(&metainfo).unwrap();
            fs::write(
                metainfo.join("org.example.FakeApp.metainfo.xml"),
                format!("<?xml version=\"1.0\"?>\n<component>\n  {urls}\n</component>\n"),
            )
            .unwrap();
        }

        let script = format!(
            "#!/bin/sh\nif [ \"$1\" != \"--appimage-extract\" ]; then exit 0; fi\n\
             mkdir -p squashfs-root\ncp -R '{}/.' squashfs-root/\n",
            payload.display()
        );
        let path = dir.join(file_name);
        let partial = dir.join(format!(".{file_name}.partial"));
        let mut file = File::create(&partial).unwrap();
        file.write_all(script.as_bytes()).unwrap();
        drop(file);
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&partial, &path).unwrap();
        path
    }

    fn install(&self, appimage: &Path, extra: &[&str]) -> Output {
        let mut args = vec!["--yes", "install", appimage.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.run(&args)
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn install_takes_an_update_source_and_refuses_what_is_none() {
    let _serial = serial();
    let home = Home::new();
    let appimage = home.appimage("Fake_App-1.0.0-x86_64.AppImage", None);

    let refused = home.install(&appimage, &["--update-source", "fake/app"]);
    assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("\"fake/app\" is not an update source"),
        "{}",
        stderr(&refused)
    );
    assert!(!home.root.join("data/applications/fake-app.desktop").exists());

    let installed = home.install(&appimage, &["--update-source", "https://github.com/fake/app"]);
    assert!(installed.status.success(), "{}", stderr(&installed));
    assert!(home.entry("fake-app").contains("\nX-AppImg-UpdateSource=github:fake/app\n"));
    assert!(stdout(&installed).contains("updates from github:fake/app"), "{}", stdout(&installed));
}

#[test]
fn the_appstream_suggestion_is_taken_with_yes_and_left_out_on_a_pipe() {
    let _serial = serial();
    let urls = "<url type=\"vcs-browser\">https://github.com/fake/app</url>";

    let home = Home::new();
    let appimage = home.appimage("Fake_App-1.0.0.AppImage", Some(urls));
    let output = home.install(&appimage, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("The AppStream metadata links github:fake/app, so updates come"),
        "{}",
        stdout(&output)
    );
    assert!(home.entry("fake-app").contains("\nX-AppImg-UpdateSource=github:fake/app\n"));

    // On a pipe, without --yes, nobody confirmed it, so it is not set.
    let home = Home::new();
    let appimage = home.appimage("Fake_App-1.0.0.AppImage", Some(urls));
    let output = home.run(&["install", appimage.to_str().unwrap()]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("Pass --update-source github:fake/app"),
        "{}",
        stdout(&output)
    );
    assert!(home.entry("fake-app").contains("\nX-AppImg-UpdateSource=manual\n"));

    // An update source given on the command line is not second-guessed.
    let home = Home::new();
    let appimage = home.appimage("Fake_App-1.0.0.AppImage", Some(urls));
    let output = home.install(&appimage, &["--update-source", "https://example.com/App.AppImage"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stdout(&output).contains("AppStream"), "{}", stdout(&output));
    assert!(home.entry("fake-app").contains("UpdateSource=https://example.com/App.AppImage\n"));
}

#[test]
fn update_source_shows_sets_and_clears() {
    let _serial = serial();
    let home = Home::new();
    let appimage = home.appimage("Fake_App-1.0.0.AppImage", None);
    assert!(home.install(&appimage, &[]).status.success());

    let shown = home.run(&["update-source", "fake-app"]);
    assert!(shown.status.success(), "{}", stderr(&shown));
    let text = stdout(&shown);
    assert!(text.contains(&format!("Origin         {}", appimage.display())), "{text}");
    assert!(text.contains("Update source  manual"), "{text}");
    assert!(text.contains("Updates from   nothing, it is updated manually"), "{text}");
    assert!(text.contains("appimg update-source fake-app <URL|github:owner/repo>"), "{text}");

    let set = home.run(&["update-source", "fake-app", "github:fake/app@continuous"]);
    assert!(set.status.success(), "{}", stderr(&set));
    assert!(stdout(&set).contains("updates from github:fake/app@continuous from now on"));
    assert!(home
        .entry("fake-app")
        .contains("\nX-AppImg-UpdateSource=github:fake/app@continuous\n"));
    let shown = stdout(&home.run(&["update-source", "fake-app"]));
    assert!(shown.contains("github:fake/app, the release tagged continuous"), "{shown}");

    let again = home.run(&["update-source", "fake-app", "github:fake/app@continuous"]);
    assert_eq!(again.status.code(), Some(3));
    assert!(stdout(&again).contains("Nothing changed."));

    let before = home.entry("fake-app");
    let refused = home.run(&["update-source", "fake-app", "nonsense"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        stderr(&refused).contains("\"nonsense\" is not an update source"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(home.entry("fake-app"), before);

    let both = home.run(&["update-source", "fake-app", "github:fake/app", "--clear"]);
    assert_eq!(both.status.code(), Some(2));

    let cleared = home.run(&["update-source", "fake-app", "--clear"]);
    assert!(cleared.status.success(), "{}", stderr(&cleared));
    assert!(stdout(&cleared).contains("is updated manually from now on"));
    assert!(home.entry("fake-app").contains("\nX-AppImg-UpdateSource=manual\n"));
}

#[test]
fn updating_a_manual_app_says_why_and_what_to_do() {
    let _serial = serial();
    let home = Home::new();
    let appimage = home.appimage("Fake_App-1.0.0.AppImage", None);
    assert!(home.install(&appimage, &[]).status.success());

    let one = home.run(&["update", "fake-app"]);
    assert_eq!(one.status.code(), Some(1));
    let message = stderr(&one);
    assert!(message.contains("fake-app is updated manually"), "{message}");
    assert!(message.contains("appimg update-source fake-app <URL|github:owner/repo>"), "{message}");
    assert!(!stdout(&one).contains("up to date"), "{}", stdout(&one));

    // Among all of them it is skipped, and that is no failure.
    let all = home.run(&["update", "--all"]);
    assert_eq!(all.status.code(), Some(3), "{}", stderr(&all));
    let text = stdout(&all);
    assert!(text.contains("Fake App is updated manually, skipped."), "{text}");
    assert!(text.contains("Everything with an update source is up to date."), "{text}");

    let check = stdout(&home.run(&["update", "--all", "--check"]));
    assert!(check.contains("manual"), "{check}");
    assert!(check.contains("no update source, set one with: appimg update-source fake-app"));

    let list = stdout(&home.run(&["list"]));
    assert!(list.contains("manual"), "{list}");
    let json = stdout(&home.run(&["list", "--json"]));
    assert!(json.contains("\"update_source\":\"manual\""), "{json}");
}
