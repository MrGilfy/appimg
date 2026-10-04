//! `appimg hold` and `unhold`, and what a hold changes: `update --all`,
//! `update --check`, `list`, updating by name, export and import. A local
//! server stands in for the GitHub API and the release downloads, and every
//! XDG directory is inside a temporary one.

use std::collections::HashMap;
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
    /// AppImage `{file}-{version}.AppImage` in it.
    fn release(&self, repo: &str, file: &str, version: &str, body: Vec<u8>) {
        let name = format!("{file}-{version}.AppImage");
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

    /// Makes `github:o/{repo}` a 404, which fails a check of it.
    fn forget(&self, repo: &str) {
        self.routes.lock().unwrap().remove(&format!("/repos/o/{repo}/releases"));
    }
}

struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
    github_api: String,
}

impl Home {
    fn new(server: &Server) -> Self {
        let dir = tempfile::Builder::new().prefix("appimg-test-").tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in ["home/Downloads", "data", "config", "state", "tmp", "tools"] {
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

    /// Runs `appimg` on a pipe, with nobody to answer a question.
    fn run(&self, args: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.root.join("tools").display(),
            std::env::var("PATH").unwrap_or_default()
        );
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

    /// Runs `appimg` and insists on this exit code.
    fn exits(&self, code: i32, args: &[&str]) -> String {
        let output = self.run(args);
        let text = format!("{}{}", stdout(&output), stderr(&output));
        assert_eq!(output.status.code(), Some(code), "{args:?}\n{text}");
        text
    }

    /// Installs `name` at 1.0.0 from a local file, updating from
    /// `github:o/{repo}`.
    fn install(&self, name: &str, repo: &str) {
        let file = self.root.join(format!("home/Downloads/{repo}-1.0.0.AppImage"));
        fs::write(&file, appimage(&self.root, name, "1.0.0")).unwrap();
        let source = format!("github:o/{repo}");
        self.exits(0, &["--yes", "install", file.to_str().unwrap(), "--update-source", &source]);
    }

    fn installed(&self, slug: &str) -> Vec<u8> {
        fs::read(self.root.join(format!("data/appimages/{slug}.AppImage"))).unwrap()
    }

    fn entry(&self, slug: &str) -> String {
        fs::read_to_string(self.root.join(format!("data/applications/{slug}.desktop"))).unwrap()
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn today() -> String {
    let seconds =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
            as i64;
    appimg_core::date::from_seconds(seconds).unwrap()
}

#[test]
fn hold_and_unhold_are_a_key_in_the_entry_and_say_when_nothing_changes() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    home.install("Held App", "held");

    let held = home.exits(0, &["hold", "held-app"]);
    assert!(held.contains("Held App is held at 1.0.0."), "{held}");
    assert!(held.contains("appimg unhold held-app"), "{held}");
    assert!(home.entry("held-app").contains("\nX-AppImg-Hold=true\n"));

    let again = home.exits(3, &["hold", "held-app"]);
    assert!(again.contains("Held App is held already."), "{again}");

    let released = home.exits(0, &["unhold", "held-app"]);
    assert!(released.contains("Held App is no longer held"), "{released}");
    assert!(!home.entry("held-app").contains("X-AppImg-Hold"));

    let again = home.exits(3, &["unhold", "held-app"]);
    assert!(again.contains("Held App is not held."), "{again}");

    home.exits(1, &["hold", "nothing-like-it"]);
}

#[test]
fn update_all_passes_a_held_app_over_and_never_counts_it_as_failed() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    home.install("Held App", "held");
    home.install("Free App", "free");
    home.exits(0, &["hold", "held-app"]);
    let before = home.installed("held-app");

    server.release("held", "held", "2.0.0", appimage(&home.root, "Held App", "2.0.0"));
    server.release("free", "free", "2.0.0", appimage(&home.root, "Free App", "2.0.0"));
    let output = home.exits(0, &["update", "--all"]);
    assert!(
        output.contains(
            "Held App is held, skipped: 2.0.0 is available, appimg update held-app takes it \
             anyway."
        ),
        "{output}"
    );
    assert!(output.contains("1.0.0 -> 2.0.0"), "{output}");
    assert_eq!(home.installed("held-app"), before);
    assert!(home.entry("free-app").contains("\nX-AppImg-Version=2.0.0\n"));
    assert!(home.entry("held-app").contains("\nX-AppImg-Version=1.0.0\n"));

    // A check of the held one that fails is said, and fails nothing.
    server.forget("held");
    let output = home.exits(3, &["update", "--all"]);
    assert!(output.contains("Held App is held, skipped, its check failed:"), "{output}");
    assert!(output.contains("Everything else is up to date."), "{output}");
    assert!(!output.contains("failed\n") && !output.contains("updates failed"), "{output}");
    assert_eq!(home.installed("held-app"), before);
}

#[test]
fn check_and_list_show_the_hold_and_the_update_it_keeps_back() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    home.install("Held App", "held");
    home.install("Free App", "free");
    home.exits(0, &["hold", "held-app"]);

    let list = home.exits(0, &["list"]);
    assert!(list
        .lines()
        .any(|l| l.starts_with("Held App") && l.ends_with("held, not checked yet")));

    server.release("held", "held", "2.0.0", appimage(&home.root, "Held App", "2.0.0"));
    server.release("free", "free", "1.0.0", appimage(&home.root, "Free App", "1.0.0"));
    // Only the held one has an update, which `update --all` would pass
    // over: nothing to do, as the exit code says.
    let check = home.exits(3, &["update", "--all", "--check"]);
    let row = check.lines().find(|l| l.starts_with("Held App")).unwrap();
    assert!(row.contains("2.0.0") && row.ends_with("held, update available"), "{check}");
    let row = check.lines().find(|l| l.starts_with("Free App")).unwrap();
    assert!(row.ends_with("up to date") && !row.contains("held"), "{check}");

    let json = home.exits(3, &["update", "--all", "--check", "--json"]);
    assert!(json.contains("\"slug\":\"held-app\""), "{json}");
    assert!(
        json.contains(
            "\"available\":true,\"source\":\"github:o/held\",\"note\":null,\"held\":true"
        ),
        "{json}"
    );
    assert!(json.contains("\"held\":false"), "{json}");

    // What the check found stays with the hold, for a list to show.
    let list = home.exits(0, &["list"]);
    let row = list.lines().find(|l| l.starts_with("Held App")).unwrap();
    assert!(row.ends_with(&format!("held, 2.0.0 available (checked {})", today())), "{list}");
    let row = list.lines().find(|l| l.starts_with("Free App")).unwrap();
    assert!(row.ends_with("github:o/free"), "{list}");

    let json = home.exits(0, &["list", "--json"]);
    assert!(
        json.contains(&format!(
            "\"held\":true,\"hold_check\":{{\"checked_at\":\"{}\",\"found\":\"available\",\
             \"latest_version\":\"2.0.0\"}}",
            today()
        )),
        "{json}"
    );
    assert!(json.contains("\"held\":false,\"hold_check\":null"), "{json}");

    // A check that finds nothing says so too.
    server.release("held", "held", "1.0.0", appimage(&home.root, "Held App", "1.0.0"));
    home.exits(3, &["update", "--all", "--check"]);
    let list = home.exits(0, &["list"]);
    let row = list.lines().find(|l| l.starts_with("Held App")).unwrap();
    assert!(row.ends_with(&format!("held, up to date (checked {})", today())), "{list}");
}

#[test]
fn updating_a_held_app_by_name_asks_first_and_keeps_the_hold() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    home.install("Held App", "held");
    home.exits(0, &["hold", "held-app"]);
    let before = home.installed("held-app");
    server.release("held", "held", "2.0.0", appimage(&home.root, "Held App", "2.0.0"));

    // Nobody on the pipe to answer the question.
    let refused = home.exits(1, &["update", "held-app"]);
    assert!(refused.contains("Held App is held at 1.0.0. Update it to 2.0.0 anyway?"), "{refused}");
    assert!(refused.contains("pass --yes"), "{refused}");
    assert_eq!(home.installed("held-app"), before);

    let updated = home.exits(0, &["--yes", "update", "held-app"]);
    assert!(updated.contains("1.0.0 -> 2.0.0"), "{updated}");
    assert!(
        updated.contains("Held App stays held, release it with: appimg unhold held-app"),
        "{updated}"
    );
    let entry = home.entry("held-app");
    assert!(entry.contains("\nX-AppImg-Version=2.0.0\n"), "{entry}");
    assert!(entry.contains("\nX-AppImg-Hold=true\n"), "{entry}");
    // What a check found was about the file that is gone.
    assert!(!entry.contains("X-AppImg-HoldCheck"), "{entry}");

    // Up to date, there is nothing to ask.
    let current = home.exits(3, &["update", "held-app"]);
    assert!(current.contains("Held App is up to date."), "{current}");
    assert!(!current.contains("anyway?"), "{current}");
}

#[test]
fn export_and_import_carry_the_hold() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    home.install("Held App", "held");
    home.install("Free App", "free");
    home.exits(0, &["hold", "held-app"]);

    let export = home.root.join("apps.json");
    home.exits(0, &["export", export.to_str().unwrap()]);
    let text = fs::read_to_string(&export).unwrap();
    let held = text.find("\"slug\": \"held-app\"").unwrap();
    let free = text.find("\"slug\": \"free-app\"").unwrap();
    let (first, second) = if held < free {
        (&text[held..free], &text[free..])
    } else {
        (&text[held..], &text[free..held])
    };
    assert!(first.contains("\"held\": true"), "{text}");
    assert!(second.contains("\"held\": false"), "{text}");

    server.release("held", "held", "1.0.0", appimage(&home.root, "Held App", "1.0.0"));
    server.release("free", "free", "1.0.0", appimage(&home.root, "Free App", "1.0.0"));
    let other = Home::new(&server);
    let plan = other.exits(0, &["import", export.to_str().unwrap(), "--dry-run"]);
    assert!(plan.lines().any(|l| l.contains("held-app") && l.ends_with(", and held")), "{plan}");
    assert!(plan.lines().any(|l| l.contains("free-app") && !l.contains("held")), "{plan}");

    let imported = other.exits(0, &["import", export.to_str().unwrap()]);
    assert!(imported.contains("held, as it was"), "{imported}");
    assert!(other.entry("held-app").contains("\nX-AppImg-Hold=true\n"));
    assert!(!other.entry("free-app").contains("X-AppImg-Hold"));
    let check = other.exits(3, &["update", "--all", "--check"]);
    assert!(check.lines().any(|l| l.starts_with("Held App") && l.contains("held, up to date")));
}

/// What a hold is for: hold an application, then install an older version
/// that works over it. The hold stays, and what a check found under it goes
/// with the file it was about.
#[test]
fn installing_over_a_held_app_keeps_the_hold() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    home.install("Held App", "held");
    home.install("Free App", "free");
    home.exits(0, &["hold", "held-app"]);
    server.release("held", "held", "2.0.0", appimage(&home.root, "Held App", "2.0.0"));
    server.release("free", "free", "1.0.0", appimage(&home.root, "Free App", "1.0.0"));
    home.exits(3, &["update", "--all", "--check"]);
    assert!(home.entry("held-app").contains("\nX-AppImg-HoldCheck="));

    let older = home.root.join("home/Downloads/held-0.9.0.AppImage");
    fs::write(&older, appimage(&home.root, "Held App", "0.9.0")).unwrap();
    let older = older.to_str().unwrap();
    let source = ["--update-source", "github:o/held"];

    let plan = home.exits(0, &[&["install", older, "--dry-run"][..], &source].concat());
    assert!(plan.contains("it is held, and stays held"), "{plan}");

    let output = home.exits(0, &[&["--yes", "install", older][..], &source].concat());
    assert!(output.contains("Replaced Held App as held-app"), "{output}");
    assert!(output.contains("stays held, release it with: appimg unhold held-app"), "{output}");
    let entry = home.entry("held-app");
    assert!(entry.contains("\nX-AppImg-Hold=true\n"), "{entry}");
    assert!(entry.contains("\nX-AppImg-Version=0.9.0\n"), "{entry}");
    assert!(!entry.contains("X-AppImg-HoldCheck"), "{entry}");

    // Still held: the newer version stays where it is.
    let list = home.exits(0, &["list"]);
    assert!(list
        .lines()
        .any(|l| l.starts_with("Held App") && l.ends_with("held, not checked yet")));
    let update = home.exits(3, &["update", "--all"]);
    assert!(update.contains("Held App is held, skipped: 2.0.0 is available"), "{update}");
    assert!(home.entry("held-app").contains("\nX-AppImg-Version=0.9.0\n"));

    // Replacing one that is not held holds nothing.
    let other = home.root.join("home/Downloads/free-0.9.0.AppImage");
    fs::write(&other, appimage(&home.root, "Free App", "0.9.0")).unwrap();
    let output = home.exits(0, &["--yes", "install", other.to_str().unwrap()]);
    assert!(!output.contains("held"), "{output}");
    assert!(!home.entry("free-app").contains("X-AppImg-Hold"));
}

/// Adopting never replaces an application appimg manages, held or not: the
/// slug is taken, and the hold is left as it was.
#[test]
fn adopting_over_a_held_app_is_refused_and_leaves_the_hold() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    home.install("Held App", "held");
    home.exits(0, &["hold", "held-app"]);
    let entry = home.entry("held-app");
    let before = home.installed("held-app");

    let stray = home.root.join("home/Downloads/stray/Held_App-0.9.0.AppImage");
    fs::create_dir_all(stray.parent().unwrap()).unwrap();
    fs::write(&stray, appimage(&home.root, "Held App", "0.9.0")).unwrap();
    fs::set_permissions(&stray, fs::Permissions::from_mode(0o755)).unwrap();
    let refused = home.exits(1, &["--yes", "adopt", stray.to_str().unwrap()]);
    assert!(refused.contains("held-app"), "{refused}");
    assert_eq!(home.entry("held-app"), entry);
    assert_eq!(home.installed("held-app"), before);
    assert!(stray.exists());
}

/// `appimage`, with AppStream metainfo that links `github:o/{repo}`, which
/// an install suggests as the update source.
fn appimage_linking(root: &Path, name: &str, version: &str, repo: &str) -> Vec<u8> {
    let bytes = appimage(root, name, version);
    let metainfo = root.join(format!("payloads/{name}-{version}/usr/share/metainfo"));
    fs::create_dir_all(&metainfo).unwrap();
    fs::write(
        metainfo.join("app.metainfo.xml"),
        format!(
            "<?xml version=\"1.0\"?>\n<component type=\"desktop-application\">\n  \
             <id>org.example.App</id>\n  <url type=\"vcs-browser\">https://github.com/o/{repo}</url>\n\
             </component>\n"
        ),
    )
    .unwrap();
    bytes
}

/// Installing over an installed application from a file keeps the update
/// source it had, held or not, rather than leaving it to update manually.
/// What names a source of its own wins: `--update-source`, and the URL an
/// install downloads from.
#[test]
fn a_replace_keeps_the_update_source_when_nothing_else_names_one() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    home.install("Free App", "free");
    let file = |version: &str, bytes: Vec<u8>| {
        let path = home.root.join(format!("home/Downloads/free-{version}.AppImage"));
        fs::write(&path, bytes).unwrap();
        path.to_str().unwrap().to_string()
    };
    let source = |slug: &str| {
        home.entry(slug)
            .lines()
            .find_map(|line| line.strip_prefix("X-AppImg-UpdateSource="))
            .unwrap()
            .to_string()
    };

    let older = file("0.9.0", appimage(&home.root, "Free App", "0.9.0"));
    let plan = home.exits(0, &["install", &older, "--dry-run"]);
    assert!(plan.contains("keeps its update source github:o/free"), "{plan}");
    let output = home.exits(0, &["--yes", "install", &older]);
    assert!(output.contains("kept its update source github:o/free"), "{output}");
    assert!(!output.contains("held"), "{output}");
    assert_eq!(source("free-app"), "github:o/free");
    assert!(home.entry("free-app").contains("\nX-AppImg-Version=0.9.0\n"));

    // The source it had comes before what the metainfo suggests, which
    // `--yes` would take otherwise.
    let linking = file("0.8.0", appimage_linking(&home.root, "Free App", "0.8.0", "elsewhere"));
    let output = home.exits(0, &["--yes", "install", &linking]);
    assert!(output.contains("kept its update source github:o/free"), "{output}");
    assert!(!output.contains("github:o/elsewhere"), "{output}");
    assert_eq!(source("free-app"), "github:o/free");

    // `--update-source` wins.
    let output = home.exits(0, &["--yes", "install", &older, "--update-source", "github:o/other"]);
    assert!(!output.contains("kept its update source"), "{output}");
    assert_eq!(source("free-app"), "github:o/other");

    // So does the URL an install downloads from.
    server.release("free", "free", "0.7.0", appimage(&home.root, "Free App", "0.7.0"));
    let url = format!("{}/files/free/free-0.7.0.AppImage", server.base);
    let output = home.exits(0, &["--yes", "install", &url]);
    assert!(!output.contains("kept its update source"), "{output}");
    assert_eq!(source("free-app"), url);

    // An application that updates manually has nothing to keep.
    let manual = home.root.join("home/Downloads/manual-1.0.0.AppImage");
    fs::write(&manual, appimage(&home.root, "Manual App", "1.0.0")).unwrap();
    let manual = manual.to_str().unwrap();
    home.exits(0, &["--yes", "install", manual]);
    let output = home.exits(0, &["--yes", "install", manual]);
    assert!(!output.contains("kept its update source"), "{output}");
    assert_eq!(source("manual-app"), "manual");
}
