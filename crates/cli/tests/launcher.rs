//! An application out of the launcher: `install --no-launcher`, `hide` and
//! `unhide`, which keep the desktop entry and say `NoDisplay=true`. Such an
//! application gets no icons, keeps that through updates and installs over
//! it, travels through export and import, and `doctor` finds nothing wrong
//! with it. A local server stands in for the GitHub API and the release
//! downloads, and every XDG directory is inside a temporary one.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener};
use std::os::unix::fs::PermissionsExt;
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

    fn entry(&self, slug: &str) -> String {
        fs::read_to_string(self.root.join(format!("data/applications/{slug}.desktop"))).unwrap()
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
            "[Desktop Entry]\nType=Application\nName={name}\nExec=app\nIcon=app\n\
             Categories=Utility;\nX-AppImage-Version={version}\n"
        ),
    )
    .unwrap();
    fs::write(payload.join("app.png"), png(48)).unwrap();
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

/// A PNG header that says `size` by `size`, all an icon needs to be placed.
fn png(size: u32) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    out
}

impl Home {
    /// The icon files installed for `slug`.
    fn icons(&self, slug: &str) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut stack = vec![self.root.join("data/icons")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for path in entries.map(|entry| entry.unwrap().path()) {
                if path.is_dir() {
                    stack.push(path);
                } else if path.file_stem().and_then(|stem| stem.to_str()) == Some(slug) {
                    found.push(path);
                }
            }
        }
        found
    }

    fn hidden(&self, slug: &str) -> bool {
        let entry = self.entry(slug);
        assert_eq!(
            entry.matches("NoDisplay=").count(),
            entry.matches("\nNoDisplay=true\n").count()
        );
        entry.contains("\nNoDisplay=true\n")
    }

    /// `doctor` finds nothing left behind by appimg.
    fn doctor_finds_nothing(&self) {
        let out = text(&self.run(&["doctor"]));
        let files = &out[out.find("Installed files").unwrap()..];
        assert!(files.contains("nothing left behind by appimg"), "{out}");
    }
}

fn install_args<'a>(file: &'a str, source: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["--yes", "install", file, "--update-source", source];
    args.extend(extra);
    args
}

#[test]
fn install_no_launcher_writes_no_display_and_installs_no_icons() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    // Listed, it gets its icons: what the hidden one goes without.
    home.install("Listed App", "l", None);
    assert!(!home.hidden("listed-app"));
    assert_eq!(home.icons("listed-app").len(), 1);

    let file = home.download("Tool", "t");
    let before = home.snapshot();
    let plan = home.exits(0, &["install", &file, "--no-launcher", "--dry-run"]);
    assert!(plan.contains("out of the launcher, with no icons"), "{plan}");
    assert!(plan.contains("NoDisplay=true"), "{plan}");
    assert_eq!(home.snapshot(), before);

    let out = home.exits(0, &install_args(&file, "github:o/t", &["--no-launcher"]));
    assert!(out.contains("out of the launcher, list it with: appimg unhide tool"), "{out}");
    assert!(out.contains("it has no command, so it runs by its path only"), "{out}");
    assert!(home.hidden("tool"));
    assert!(home.entry("tool").contains("\nIcon=application-x-executable\n"));
    assert!(home.icons("tool").is_empty());

    let file = home.download("Cli", "c");
    let out =
        home.exits(0, &install_args(&file, "github:o/c", &["--no-launcher", "--command", "cli"]));
    assert!(!out.contains("runs by its path only"), "{out}");

    let list = home.exits(0, &["list"]);
    assert!(
        list.lines().any(|l| l.starts_with("Tool") && l.ends_with("not in launcher")),
        "{list}"
    );
    assert!(list.lines().any(|l| l.starts_with("Listed App") && !l.contains("launcher")));
    let json = home.exits(0, &["list", "--json"]);
    assert!(json.contains("\"slug\":\"tool\"") && json.contains("\"hidden\":true"), "{json}");
    assert!(json.contains("\"hidden\":false"), "{json}");
    home.doctor_finds_nothing();
}

#[test]
fn hide_and_unhide_switch_it_both_ways_and_say_when_nothing_changes() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    home.install("Fake App", "r", None);
    let icons = home.icons("fake-app");

    assert!(home.exits(3, &["unhide", "fake-app"]).contains("is in the launcher already"));
    let out = home.exits(0, &["hide", "fake-app"]);
    assert!(out.contains("Fake App is out of the launcher."), "{out}");
    assert!(home.hidden("fake-app"));
    // Its icons stay, so listing it again is the way it was.
    assert_eq!(home.icons("fake-app"), icons);
    assert!(home.entry("fake-app").contains("\nIcon=fake-app\n"));
    assert!(home.exits(3, &["hide", "fake-app"]).contains("out of the launcher already"));
    home.doctor_finds_nothing();

    home.exits(0, &["unhide", "fake-app"]);
    assert!(!home.hidden("fake-app"));
    assert_eq!(home.icons("fake-app"), icons);

    // One installed out of the launcher gets the icons it ships once it is
    // listed.
    let file = home.download("Tool", "t");
    home.exits(0, &install_args(&file, "github:o/t", &["--no-launcher"]));
    let out = home.exits(0, &["unhide", "tool"]);
    assert!(out.contains("icons   1 installed"), "{out}");
    assert!(!home.hidden("tool"));
    assert_eq!(home.icons("tool").len(), 1);
    assert!(home.entry("tool").contains("\nIcon=tool\n"));
    home.doctor_finds_nothing();
}

#[test]
fn updates_and_installs_over_it_keep_it_out_of_the_launcher_without_icons() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    let file = home.download("Tool", "t");
    home.exits(0, &install_args(&file, "github:o/t", &["--no-launcher"]));

    server.release("t", "2.0.0", appimage(&home.root, "Tool", "2.0.0"));
    let out = home.exits(0, &["update", "tool"]);
    assert!(out.contains("1.0.0 -> 2.0.0"), "{out}");
    assert!(home.hidden("tool"));
    assert!(home.icons("tool").is_empty());
    assert!(home.entry("tool").contains("\nIcon=application-x-executable\n"));

    let file = home.download("Tool", "t");
    let plan = home.exits(0, &["install", &file, "--dry-run"]);
    assert!(plan.contains("out of the launcher, as the one it replaces"), "{plan}");
    let out = home.exits(0, &["--yes", "install", &file]);
    assert!(out.contains("out of the launcher as before"), "{out}");
    assert!(home.hidden("tool"));
    assert!(home.icons("tool").is_empty());
    home.doctor_finds_nothing();
}

#[test]
fn export_and_import_carry_it() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    let file = home.download("Tool", "t");
    home.exits(0, &install_args(&file, "github:o/t", &["--no-launcher"]));
    home.install("Fake App", "r", None);
    let export = home.root.join("apps.json");
    home.exits(0, &["export", export.to_str().unwrap()]);
    let exported = fs::read_to_string(&export).unwrap();
    assert!(exported.contains("\"hidden\": true"), "{exported}");
    assert!(exported.contains("\"hidden\": false"), "{exported}");

    server.release("t", "1.0.0", appimage(&home.root, "Tool", "1.0.0"));
    server.release("r", "1.0.0", appimage(&home.root, "Fake App", "1.0.0"));
    let other = Home::new(&server);
    let plan = other.exits(0, &["import", export.to_str().unwrap(), "--dry-run"]);
    assert!(plan.lines().any(|l| l.contains("(tool)") && l.ends_with(", out of the launcher")));
    assert!(plan.lines().any(|l| l.contains("(fake-app)") && !l.contains("launcher")), "{plan}");

    let out = other.exits(0, &["import", export.to_str().unwrap()]);
    assert!(out.contains("out of the launcher, as it was"), "{out}");
    assert!(other.hidden("tool"));
    assert!(other.icons("tool").is_empty());
    assert!(!other.hidden("fake-app"));
    assert_eq!(other.icons("fake-app").len(), 1);
}
