//! An application as a command: a symbolic link in `~/.local/bin` to its
//! AppImage, which `install --command`, `appimg command` and `adopt` create,
//! the entry records, an update keeps running the current version, `remove`
//! takes along, `doctor` checks, and export and import carry. Nothing in
//! `~/.local/bin` that appimg did not create is ever written over. A local
//! server stands in for the GitHub API and the release downloads, and every
//! XDG directory is inside a temporary one.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

/// Executing a file while another test thread still holds it open for
/// writing fails with `ETXTBSY`. The tests run one at a time.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The GitHub API and the downloads of its releases: each path it was given
/// a body for answers with that body, anything else with a 404.
struct Server {
    base: String,
    routes: Arc<Mutex<HashMap<String, Vec<u8>>>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
        let routed = Arc::clone(&routes);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                    request.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&request).into_owned();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                let route = path.split('?').next().unwrap_or(&path);
                let (status, body) = match routed.lock().unwrap().get(route) {
                    Some(body) => ("200 OK", body.clone()),
                    None => ("404 Not Found", b"not found".to_vec()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&[head.as_bytes(), &body].concat());
                let _ = stream.shutdown(Shutdown::Write);
            }
        });
        Self { base, routes }
    }

    /// Makes `github:o/{repo}` offer one release, `v{version}`, with the
    /// AppImage `{repo}-{version}.AppImage` in it.
    fn release(&self, repo: &str, version: &str, body: Vec<u8>) {
        let name = format!("{repo}-{version}.AppImage");
        let path = format!("/files/{repo}/{name}");
        let release = format!(
            "[{{\"tag_name\":\"v{version}\",\"draft\":false,\"prerelease\":false,\
             \"published_at\":\"2026-10-01T10:00:00Z\",\"assets\":[{{\"name\":\"{name}\",\
             \"size\":{},\"browser_download_url\":\"{}{path}\"}}]}}]",
            body.len(),
            self.base
        );
        let mut routes = self.routes.lock().unwrap();
        routes.insert(path, body);
        routes.insert(format!("/repos/o/{repo}/releases"), release.into_bytes());
    }
}

struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
    github_api: String,
}

/// Everything below a directory: each file with its mode and contents, each
/// link with where it points, each directory.
type Snapshot = BTreeMap<PathBuf, String>;

impl Home {
    fn new(server: &Server) -> Self {
        let dir = tempfile::Builder::new().prefix("appimg-test-").tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in ["home/Downloads", "home/.local/bin", "data", "config", "state", "tmp", "tools"]
        {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        // The fixtures only start like an AppImage, so they extract through
        // a stand-in for `unsquashfs`, which copies the payload they name.
        let unsquashfs = root.join("tools/unsquashfs");
        fs::write(
            &unsquashfs,
            "#!/bin/sh\npayload=$(sed -n 's/^payload=//p' \"$6\")\nmkdir -p \"$5\"\n\
             cp -R \"$payload/.\" \"$5/\"\n",
        )
        .unwrap();
        fs::set_permissions(&unsquashfs, fs::Permissions::from_mode(0o755)).unwrap();
        Self { _dir: dir, root, github_api: server.base.clone() }
    }

    fn bin(&self) -> PathBuf {
        self.root.join("home/.local/bin")
    }

    /// Runs `appimg` on a pipe, with nobody to answer a question, and
    /// without `~/.local/bin` on `PATH`.
    fn run(&self, args: &[&str]) -> Output {
        self.run_with_path(args, false)
    }

    fn run_with_path(&self, args: &[&str], bin_on_path: bool) -> Output {
        let mut path = format!(
            "{}:{}",
            self.root.join("tools").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        if bin_on_path {
            path = format!("{}:{path}", self.bin().display());
        }
        Command::new(env!("CARGO_BIN_EXE_appimg"))
            .arg("--no-color")
            .args(args)
            .env("PATH", path)
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("APPIMG_GITHUB_API", &self.github_api)
            .env_remove("APPIMG_DIR")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// Runs `appimg` and insists on this exit code. Returns what it wrote.
    fn exits(&self, code: i32, args: &[&str]) -> String {
        let output = self.run(args);
        let text = text(&output);
        assert_eq!(output.status.code(), Some(code), "{args:?}\n{text}");
        text
    }

    /// A local file with `name` at 1.0.0 in it, updating from `github:o/{repo}`.
    fn download(&self, name: &str, repo: &str) -> String {
        let file = self.root.join(format!("home/Downloads/{repo}-1.0.0.AppImage"));
        fs::write(&file, appimage(&self.root, name, "1.0.0")).unwrap();
        file.to_str().unwrap().to_string()
    }

    /// Installs `name` at 1.0.0 from a local file, updating from
    /// `github:o/{repo}`, as the command `command` if there is one.
    fn install(&self, name: &str, repo: &str, command: Option<&str>) -> String {
        let file = self.download(name, repo);
        let source = format!("github:o/{repo}");
        let mut args = vec!["--yes", "install", &file, "--update-source", &source];
        if let Some(command) = command {
            args.extend(["--command", command]);
        }
        self.exits(0, &args)
    }

    fn appimage_of(&self, slug: &str) -> PathBuf {
        self.root.join(format!("data/appimages/{slug}.AppImage"))
    }

    fn entry(&self, slug: &str) -> String {
        fs::read_to_string(self.root.join(format!("data/applications/{slug}.desktop"))).unwrap()
    }

    /// Where the link `name` in `~/.local/bin` points, `None` when there is
    /// no link.
    fn link(&self, name: &str) -> Option<PathBuf> {
        fs::read_link(self.bin().join(name)).ok()
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

/// An AppImage as far as appimg looks, named `name`, at `version`.
fn appimage(root: &Path, name: &str, version: &str) -> Vec<u8> {
    let payload = root.join(format!("payloads/{name}-{version}"));
    fs::create_dir_all(&payload).unwrap();
    fs::write(
        payload.join("app.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName={name}\nExec=app\nCategories=Utility;\n\
             X-AppImage-Version={version}\n"
        ),
    )
    .unwrap();
    [
        &b"\x7fELF\x01\x01\x01\x00AI\x02\n"[..],
        format!("payload={}\n# {name} {version}\nhsqs\n", payload.display()).as_bytes(),
    ]
    .concat()
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn install_with_a_command_links_it_records_it_and_lists_it() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);

    // A dry run says so and creates nothing.
    let file = home.download("Fake App", "r");
    let before = home.snapshot();
    let plan = home.exits(0, &["install", &file, "--command", "fake", "--dry-run"]);
    assert!(plan.contains("command fake"), "{plan}");
    assert_eq!(home.snapshot(), before);

    let output = home.install("Fake App", "r", Some("fake"));
    assert!(output.contains("command fake"), "{output}");
    assert!(output.contains("is not on PATH"), "{output}");
    assert_eq!(home.link("fake"), Some(home.appimage_of("fake-app")));
    assert!(home.entry("fake-app").contains("\nX-AppImg-Command=fake\n"));

    let list = home.exits(0, &["list"]);
    let header = list.lines().next().unwrap();
    assert!(header.contains("COMMAND"), "{list}");
    assert!(list.lines().any(|l| l.starts_with("Fake App") && l.contains(" fake ")), "{list}");
    assert!(home.exits(0, &["list", "--json"]).contains("\"command\":\"fake\""));

    let shown = home.exits(0, &["command", "fake-app"]);
    assert!(shown.contains("Fake App runs as `fake`."), "{shown}");
    // With ~/.local/bin on PATH, there is nothing to warn about.
    let output = home.run_with_path(&["command", "fake-app"], true);
    assert!(!text(&output).contains("not on PATH"), "{}", text(&output));
}

#[test]
fn nothing_appimg_did_not_create_is_ever_written_over() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    home.install("Fake App", "r", Some("fake"));
    home.install("Other App", "o", None);
    // Someone else's command, someone else's link, a directory.
    fs::write(home.bin().join("tool"), "#!/bin/sh\necho mine\n").unwrap();
    symlink("/usr/bin/env", home.bin().join("env-link")).unwrap();
    fs::create_dir(home.bin().join("dir")).unwrap();
    let before = home.snapshot();

    for name in ["tool", "env-link", "dir"] {
        let out = home.exits(1, &["command", "other-app", name]);
        assert!(out.contains("appimg did not create it"), "{out}");
        assert!(out.contains("nothing was changed"), "{out}");
        let file = home.download("New App", "n");
        let out = home.exits(1, &["--yes", "install", &file, "--command", name]);
        assert!(out.contains("appimg did not create it"), "{out}");
        fs::remove_file(&file).unwrap();
    }
    // The command of another application is that application's.
    let out = home.exits(1, &["command", "other-app", "fake"]);
    assert!(out.contains("is the command of \"fake-app\" already"), "{out}");
    // A name no command can be is refused before anything else.
    for name in ["../escape", "a b", "."] {
        home.exits(1, &["command", "other-app", name]);
    }
    let out = home.exits(1, &["install", "/nowhere.AppImage", "--command", "a/b"]);
    assert!(out.contains("cannot be a command"), "{out}");

    let mut after = home.snapshot();
    // Only the downloads the refused installs were offered came and went.
    after.retain(|path, _| !path.starts_with(home.root.join("payloads")));
    let mut before = before;
    before.retain(|path, _| !path.starts_with(home.root.join("payloads")));
    assert_eq!(after, before);
}

#[test]
fn a_command_is_added_changed_shown_and_removed() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    home.install("Fake App", "r", None);
    assert!(home.exits(3, &["command", "fake-app"]).contains("Fake App has no command."));
    assert!(home.exits(3, &["command", "fake-app", "--remove"]).contains("has no command"));

    home.exits(0, &["command", "fake-app", "one"]);
    assert_eq!(home.link("one"), Some(home.appimage_of("fake-app")));
    assert!(home.exits(3, &["command", "fake-app", "one"]).contains("runs as `one` already"));

    let out = home.exits(0, &["command", "fake-app", "two"]);
    assert!(out.contains("`one` is gone"), "{out}");
    assert_eq!(home.link("one"), None);
    assert_eq!(home.link("two"), Some(home.appimage_of("fake-app")));
    assert!(home.entry("fake-app").contains("\nX-AppImg-Command=two\n"));

    // A link that went missing is created again by naming it once more.
    fs::remove_file(home.bin().join("two")).unwrap();
    home.exits(0, &["command", "fake-app", "two"]);
    assert_eq!(home.link("two"), Some(home.appimage_of("fake-app")));

    let out = home.exits(0, &["command", "fake-app", "--remove"]);
    assert!(out.contains("no longer runs as `two`"), "{out}");
    assert_eq!(home.link("two"), None);
    assert!(!home.entry("fake-app").contains("X-AppImg-Command"));

    // What took the place of the link stays when the command goes.
    home.exits(0, &["command", "fake-app", "three"]);
    fs::remove_file(home.bin().join("three")).unwrap();
    fs::write(home.bin().join("three"), "mine now").unwrap();
    let out = home.exits(0, &["command", "fake-app", "--remove"]);
    assert!(out.contains("no longer ran it, so it stays"), "{out}");
    assert_eq!(fs::read_to_string(home.bin().join("three")).unwrap(), "mine now");
}

/// The link points at the installed path, and an update swaps the new file
/// in under that path: the command runs the new version without the link
/// ever changing.
#[test]
fn the_command_runs_the_new_version_after_an_update() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    home.install("Fake App", "r", Some("fake"));
    let link = home.bin().join("fake");
    let before = fs::read(&link).unwrap();

    let new = appimage(&home.root, "Fake App", "2.0.0");
    server.release("r", "2.0.0", new.clone());
    let out = home.exits(0, &["update", "fake-app"]);
    assert!(out.contains("1.0.0 -> 2.0.0"), "{out}");

    assert_eq!(home.link("fake"), Some(home.appimage_of("fake-app")));
    assert_ne!(fs::read(&link).unwrap(), before);
    assert_eq!(fs::read(&link).unwrap(), new);
    assert_eq!(fs::read(&link).unwrap(), fs::read(home.appimage_of("fake-app")).unwrap());
    assert!(home.entry("fake-app").contains("\nX-AppImg-Command=fake\n"));
    let doctor = text(&home.run(&["doctor"]));
    assert!(!doctor.contains("its command"), "{doctor}");

    // Installing over it keeps the command, the way it keeps a hold.
    let file = home.download("Fake App", "r");
    let out = home.exits(0, &["--yes", "install", &file]);
    assert!(out.contains("command fake"), "{out}");
    assert_eq!(fs::read(&link).unwrap(), fs::read(&file).unwrap());
    assert!(home.entry("fake-app").contains("\nX-AppImg-Command=fake\n"));
}

#[test]
fn remove_takes_the_link_along_and_nothing_else() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    home.install("Fake App", "r", Some("fake"));
    home.install("Other App", "o", Some("other"));

    let out = home.exits(0, &["--yes", "remove", "fake-app"]);
    assert!(out.contains(&home.bin().join("fake").display().to_string()), "{out}");
    assert!(fs::symlink_metadata(home.bin().join("fake")).is_err());

    // A command whose place someone else took stays theirs.
    fs::remove_file(home.bin().join("other")).unwrap();
    fs::write(home.bin().join("other"), "mine now").unwrap();
    let out = home.exits(0, &["--yes", "remove", "other-app"]);
    assert!(out.contains("stays: the command it records is there, but no longer runs it"));
    assert_eq!(fs::read_to_string(home.bin().join("other")).unwrap(), "mine now");
}

#[test]
fn doctor_reports_a_command_that_does_not_run_its_application() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    home.install("Fine App", "fine", Some("fine"));
    home.install("Missing App", "missing", Some("missing"));
    home.install("Broken App", "broken", Some("broken"));
    home.install("Moved App", "moved", Some("moved"));
    home.install("Taken App", "taken", Some("taken"));
    let doctor = text(&home.run(&["doctor"]));
    assert!(!doctor.contains("its command"), "{doctor}");

    fs::remove_file(home.bin().join("missing")).unwrap();
    fs::remove_file(home.bin().join("broken")).unwrap();
    symlink(home.root.join("gone.AppImage"), home.bin().join("broken")).unwrap();
    fs::remove_file(home.bin().join("moved")).unwrap();
    symlink(home.appimage_of("fine-app"), home.bin().join("moved")).unwrap();
    fs::remove_file(home.bin().join("taken")).unwrap();
    fs::write(home.bin().join("taken"), "mine").unwrap();

    let doctor = text(&home.run(&["doctor"]));
    let line = |slug: &str| {
        doctor
            .lines()
            .find(|line| line.contains(&format!("{slug}: its command")))
            .unwrap_or_else(|| panic!("{slug} is not reported:\n{doctor}"))
            .to_string()
    };
    assert!(line("missing-app").contains("is missing, create it again with `appimg command"));
    assert!(line("broken-app").contains("gone.AppImage, which is not there"));
    assert!(line("moved-app").contains("fine-app.AppImage instead"));
    assert!(line("taken-app").contains("is no link appimg created"));
    assert!(!doctor.contains("fine-app: its command"), "{doctor}");
}

#[test]
fn export_and_import_carry_the_command() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    home.install("Fake App", "r", Some("fake"));
    home.install("Other App", "o", Some("other"));
    let export = home.root.join("apps.json");
    home.exits(0, &["export", export.to_str().unwrap()]);
    let exported = fs::read_to_string(&export).unwrap();
    assert!(exported.contains("\"command\": \"fake\""), "{exported}");

    server.release("r", "1.0.0", appimage(&home.root, "Fake App", "1.0.0"));
    server.release("o", "1.0.0", appimage(&home.root, "Other App", "1.0.0"));
    let other = Home::new(&server);
    // On the new machine, `other` is someone else's already.
    fs::write(other.bin().join("other"), "mine").unwrap();

    let plan = other.exits(0, &["import", export.to_str().unwrap(), "--dry-run"]);
    assert!(plan.lines().any(|l| l.contains("fake-app") && l.ends_with("as the command fake")));
    assert!(plan.contains("not as the command other"), "{plan}");

    let out = other.exits(0, &["import", export.to_str().unwrap()]);
    assert!(out.contains("command fake, as it was"), "{out}");
    assert!(out.contains("imported without the command other"), "{out}");
    assert_eq!(other.link("fake"), Some(other.appimage_of("fake-app")));
    assert!(other.entry("fake-app").contains("\nX-AppImg-Command=fake\n"));
    assert!(!other.entry("other-app").contains("X-AppImg-Command"));
    assert_eq!(fs::read_to_string(other.bin().join("other")).unwrap(), "mine");
}

/// The link `adopt` leaves in `~/.local/bin` is the application's command
/// from then on, so `remove` and `doctor` look after it.
#[test]
fn the_link_adopt_leaves_in_local_bin_is_the_command() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    let tool = home.bin().join("tool");
    fs::write(&tool, appimage(&home.root, "Tool", "1.0.0")).unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();

    let out = home.exits(0, &["--yes", "adopt", tool.to_str().unwrap()]);
    assert!(out.contains("command tool"), "{out}");
    assert_eq!(home.link("tool"), Some(home.appimage_of("tool")));
    assert!(home.entry("tool").contains("\nX-AppImg-Command=tool\n"));
    let doctor = text(&home.run(&["doctor"]));
    assert!(!doctor.contains("its command"), "{doctor}");

    fs::remove_file(&tool).unwrap();
    assert!(text(&home.run(&["doctor"])).contains("tool: its command `tool` does not run it"));
    home.exits(0, &["command", "tool", "tool"]);

    home.exits(0, &["--yes", "remove", "tool"]);
    assert!(fs::symlink_metadata(&tool).is_err());
}
