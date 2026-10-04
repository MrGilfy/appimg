//! `appimg export` and `appimg import` as a user runs them, against a
//! temporary home and a local HTTP server that stands in for the hosts and
//! for the GitHub API. No test in here talks to any real host or touches the
//! real `$HOME`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use appimg_core::export::{self, ExportedApp};

/// Executing a file while any process still holds it open for writing fails
/// with `ETXTBSY`, and a `fork` in another test thread can hold one for a
/// moment. The tests run one at a time.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// An HTTP server on a random port that answers each path it was given a
/// body for with a 200 and that body, and anything else with a 404. It
/// stands in for the hosts AppImages come from and for the GitHub API.
struct Server {
    base: String,
    routes: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
        let asked: Arc<Mutex<Vec<String>>> = Arc::default();
        let (routed, recorded) = (Arc::clone(&routes), Arc::clone(&asked));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                // The request is read to its end first: a socket closed with
                // unread bytes in it resets the connection.
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                    request.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&request).into_owned();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                recorded.lock().unwrap().push(path.clone());
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
        Self { base, routes, asked }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn route(&self, path: &str, body: Vec<u8>) {
        self.routes.lock().unwrap().insert(path.to_string(), body);
    }

    /// The path of every request so far.
    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
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
        for sub in ["home/Downloads", "data", "config", "tmp", "tools"] {
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
        Self { _dir: dir, root, github_api: server.base.clone() }
    }

    /// Runs `appimg` with these arguments, on a pipe, with nobody to answer
    /// a question.
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
            .env("TMPDIR", self.root.join("tmp"))
            .env("APPIMG_GITHUB_API", &self.github_api)
            .env_remove("APPIMG_DIR")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// Runs `appimg` and insists that it succeeds.
    fn ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(output.status.success(), "{args:?}\n{}\n{}", stdout(&output), stderr(&output));
        output
    }

    fn data(&self, relative: &str) -> PathBuf {
        self.root.join("data").join(relative)
    }

    /// Writes an executable file below the home.
    fn file(&self, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
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

/// A file that starts the way an AppImage does, whose desktop entry names
/// it `name` at `version`, with a payload the stand-in for `unsquashfs`
/// extracts. The payload lives below `root`.
fn fake(root: &Path, name: &str, version: &str) -> Vec<u8> {
    let payload = root.join(format!("payloads/{name}-{version}"));
    let icons = payload.join("usr/share/icons/hicolor/48x48/apps");
    fs::create_dir_all(&icons).unwrap();
    fs::write(
        payload.join("fakeapp.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName={name}\nExec=AppRun %U\nIcon=fakeapp\n\
             Categories=Utility;\nX-AppImage-Version={version}\n"
        ),
    )
    .unwrap();
    fs::write(icons.join("fakeapp.png"), png(48)).unwrap();
    format!("\x7fELF\npayload={}\n# {name} {version}\nhsqs\n", payload.display()).into_bytes()
}

/// `rest` behind the front of a 64-bit ELF file whose `.upd_info` section
/// holds `update_info`, the way an AppImage carries its update information.
/// It is a relocatable file, which nothing runs.
fn with_update_info(update_info: &str, rest: &[u8]) -> Vec<u8> {
    let names = b"\0.shstrtab\0.upd_info\0";
    let info = update_info.as_bytes();
    let (names_at, info_at) = (64u64, 64 + names.len() as u64);
    let table_at = (info_at + info.len() as u64).next_multiple_of(8);

    let mut out = b"\x7fELF\x02\x01\x01".to_vec();
    out.resize(16, 0);
    out.extend_from_slice(&1u16.to_le_bytes()); // e_type: relocatable
    out.extend_from_slice(&62u16.to_le_bytes()); // e_machine
    out.extend_from_slice(&1u32.to_le_bytes()); // e_version
    out.extend_from_slice(&0u64.to_le_bytes()); // e_entry
    out.extend_from_slice(&0u64.to_le_bytes()); // e_phoff
    out.extend_from_slice(&table_at.to_le_bytes()); // e_shoff
    out.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    out.extend_from_slice(&64u16.to_le_bytes()); // e_ehsize
    out.extend_from_slice(&0u16.to_le_bytes()); // e_phentsize
    out.extend_from_slice(&0u16.to_le_bytes()); // e_phnum
    out.extend_from_slice(&64u16.to_le_bytes()); // e_shentsize
    out.extend_from_slice(&3u16.to_le_bytes()); // e_shnum
    out.extend_from_slice(&1u16.to_le_bytes()); // e_shstrndx
    out.extend_from_slice(names);
    out.extend_from_slice(info);
    out.resize(table_at as usize, 0);

    let section = |name: u32, kind: u32, at: u64, size: u64| {
        let mut header = Vec::new();
        header.extend_from_slice(&name.to_le_bytes());
        header.extend_from_slice(&kind.to_le_bytes());
        header.extend_from_slice(&[0; 16]); // sh_flags, sh_addr
        header.extend_from_slice(&at.to_le_bytes());
        header.extend_from_slice(&size.to_le_bytes());
        header.extend_from_slice(&[0; 8]); // sh_link, sh_info
        header.extend_from_slice(&1u64.to_le_bytes()); // sh_addralign
        header.extend_from_slice(&0u64.to_le_bytes()); // sh_entsize
        header
    };
    out.extend_from_slice(&[0; 64]);
    out.extend(section(1, 3, names_at, names.len() as u64));
    out.extend(section(11, 1, info_at, info.len() as u64));
    out.push(b'\n');
    out.extend_from_slice(rest);
    out
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

fn sha256(bytes: &[u8]) -> String {
    let file = tempfile::NamedTempFile::new().unwrap();
    fs::write(file.path(), bytes).unwrap();
    appimg_core::digest::sha256_file(file.path()).unwrap()
}

/// A release as the GitHub API lists one, with these assets and the digest
/// it publishes for each.
fn release_json(tag: &str, assets: &[(&str, &str)]) -> String {
    let assets: Vec<String> = assets
        .iter()
        .map(|(url, sha256)| {
            let name = url.rsplit('/').next().unwrap();
            format!(
                "{{\"name\":\"{name}\",\"size\":1,\"digest\":\"sha256:{sha256}\",\
                 \"browser_download_url\":\"{url}\"}}"
            )
        })
        .collect();
    format!(
        "[{{\"tag_name\":\"{tag}\",\"draft\":false,\"prerelease\":false,\
         \"published_at\":\"2026-10-01T10:00:00Z\",\"assets\":[{}]}}]",
        assets.join(",")
    )
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// An application as an export lists it, with the update source and origin
/// that decide where an import gets it from.
fn listed(slug: &str, name: &str, update_source: &str, origin: &str) -> ExportedApp {
    ExportedApp {
        slug: slug.to_string(),
        name: name.to_string(),
        comment: None,
        categories: vec!["Utility".to_string()],
        arguments: Vec::new(),
        terminal: false,
        update_source: Some(update_source.to_string()),
        origin: Some(origin.to_string()),
        installed_version: None,
        held: false,
    }
}

/// A home with three applications, exported, the way a user would leave an
/// old machine: one that follows the releases of `github:o/r`, installed
/// from a local file, with every field of its entry set; one installed from
/// a URL that updates from another URL; and one installed from a local file
/// that updates manually. By the time of the import, the server offers a
/// newer release and a newer file at that other URL.
struct Exported {
    old: Home,
    file: PathBuf,
    github_v2: Vec<u8>,
    url_v2: Vec<u8>,
}

fn exported(server: &Server) -> Exported {
    let old = Home::new(server);
    let github_v1 =
        old.file("home/Downloads/Github_App-1.0.0.AppImage", &fake(&old.root, "Github", "1.0.0"));
    old.ok(&[
        "--yes",
        "install",
        github_v1.to_str().unwrap(),
        "--name",
        "Github App",
        "--comment",
        "Out of \"releases\", with a \\ in it",
        "--categories",
        "Graphics,Development",
        "--args",
        "--flag 'two words'",
        "--terminal",
        "--update-source",
        "github:o/r",
    ]);
    server.route("/files/Url_App-1.0.AppImage", fake(&old.root, "Url", "1.0"));
    old.ok(&[
        "--yes",
        "install",
        &server.url("/files/Url_App-1.0.AppImage"),
        "--name",
        "Url App",
        "--update-source",
        &server.url("/latest/Url_App.AppImage"),
    ]);
    let local = old.file("home/Downloads/Local_App.AppImage", &fake(&old.root, "Local", "3"));
    old.ok(&["--yes", "install", local.to_str().unwrap(), "--name", "Local App"]);

    let file = old.root.join("apps.json");
    old.ok(&["export", file.to_str().unwrap()]);

    let github_v2 = fake(&old.root, "Github", "2.0.0");
    let asset = server.url("/files/Github_App-2.0.0.AppImage");
    server.route("/files/Github_App-2.0.0.AppImage", github_v2.clone());
    let release = release_json("v2.0.0", &[(&asset, &sha256(&github_v2))]);
    server.route("/repos/o/r/releases", release.into_bytes());
    let url_v2 = fake(&old.root, "Url", "2.0");
    server.route("/latest/Url_App.AppImage", url_v2.clone());
    Exported { old, file, github_v2, url_v2 }
}

/// What an export holds that the user decided, which an import reproduces:
/// all of it but where the app came from and which version it was.
fn decided(app: &ExportedApp) -> ExportedApp {
    ExportedApp { origin: None, installed_version: None, ..app.clone() }
}

/// Exported on one machine and imported into an empty home, every entry
/// comes back as it was, and every application as its current version: the
/// newest release of the one that follows releases, and the one installed
/// from a URL out of the URL it updates from. The one that was
/// installed from a file on the old machine is listed with what it needs.
#[test]
fn an_export_imported_into_an_empty_home_reproduces_the_entries() {
    let _serial = serial();
    let server = Server::start();
    let Exported { old, file, github_v2, url_v2 } = exported(&server);
    let before = export::from_json(&fs::read_to_string(&file).unwrap()).unwrap();
    // The export holds what was set, not defaults.
    let github = before.iter().find(|app| app.slug == "github-app").unwrap();
    assert_eq!(github.arguments, ["--flag", "two words"]);
    assert!(github.terminal);
    assert_eq!(github.installed_version.as_deref(), Some("1.0.0"));

    let new = Home::new(&server);
    let output = new.ok(&["--yes", "import", file.to_str().unwrap()]);
    let out = stdout(&output);

    let after = export::from_json(&stdout(&new.ok(&["export"]))).unwrap();
    let expected: Vec<_> =
        before.iter().filter(|app| app.slug != "local-app").map(decided).collect();
    assert_eq!(after.iter().map(decided).collect::<Vec<_>>(), expected);

    assert_eq!(fs::read(new.data("appimages/github-app.AppImage")).unwrap(), github_v2);
    assert_eq!(fs::read(new.data("appimages/url-app.AppImage")).unwrap(), url_v2);
    let entry = fs::read_to_string(new.data("applications/github-app.desktop")).unwrap();
    assert!(entry.contains("\nX-AppImg-Release=github:o/r@v2.0.0\n"), "{entry}");
    assert!(entry.contains("\nX-AppImg-Version=2.0.0\n"), "{entry}");

    let local = old.root.join("home/Downloads/Local_App.AppImage");
    assert!(
        out.contains(&format!(
            "Nothing to download for 1 app, it needs its AppImage file:\n  Local App \
             (local-app): it was installed from {} on the machine it was exported from\n",
            local.display()
        )),
        "{out}"
    );
    assert!(!new.data("appimages/local-app.AppImage").exists());
    assert!(out.contains("Imported 2 of 3 apps, 1 needs its AppImage file.\n"), "{out}");
}

/// An application whose slug is installed already is skipped and left as
/// it is, and an import with nothing left to do changes nothing at all.
#[test]
fn an_app_that_is_installed_already_is_skipped() {
    let _serial = serial();
    let server = Server::start();
    let Exported { old: _old, file, url_v2, .. } = exported(&server);

    let new = Home::new(&server);
    new.ok(&["--yes", "install", &server.url("/files/Url_App-1.0.AppImage"), "--name", "Url App"]);
    let installed = fs::read(new.data("appimages/url-app.AppImage")).unwrap();
    assert_ne!(installed, url_v2);

    let output = new.ok(&["--yes", "import", file.to_str().unwrap()]);
    let out = stdout(&output);
    assert!(out.contains("Url App (url-app) is already installed, skipped.\n"), "{out}");
    assert_eq!(fs::read(new.data("appimages/url-app.AppImage")).unwrap(), installed);
    assert!(new.data("appimages/github-app.AppImage").is_file());
    assert!(
        out.contains("Imported 1 of 3 apps, 1 already installed, 1 needs its AppImage file.\n"),
        "{out}"
    );

    let before = new.snapshot();
    let output = new.run(&["--yes", "import", file.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(stdout(&output).contains("Github App (github-app) is already installed, skipped."));
    assert_eq!(new.snapshot(), before);
}

/// An application that fails to import is reported and the others are
/// imported all the same; the exit code says something failed. A digest that
/// does not match is such a failure, and so is a file that is gone. What
/// failed leaves nothing behind. An app is installed under the slug it had,
/// whatever its name says.
#[test]
fn a_failing_app_does_not_stop_the_rest() {
    let _serial = serial();
    let server = Server::start();
    let new = Home::new(&server);

    let bad = fake(&new.root, "Bad", "2");
    server.route("/files/Bad-2.AppImage", bad);
    let release = release_json("v2", &[(&server.url("/files/Bad-2.AppImage"), &"0".repeat(64))]);
    server.route("/repos/o/bad/releases", release.into_bytes());
    server.route("/files/Fine-1.AppImage", fake(&new.root, "Fine", "1"));
    let apps = [
        listed("bad-digest", "Bad Digest", "github:o/bad", "/old/Bad-1.AppImage"),
        listed("gone", "Gone", "manual", &server.url("/files/Gone-1.AppImage")),
        listed("the-fine-one", "Fine", "manual", &server.url("/files/Fine-1.AppImage")),
        listed("by-hand", "By Hand", "manual", "/old/By_Hand.AppImage"),
        listed("also-by-hand", "Also By Hand", "manual", "/old/Also.AppImage"),
    ];
    let file = new.file("apps.json", export::to_json(&apps).as_bytes());

    let output = new.run(&["--yes", "import", file.to_str().unwrap()]);
    let (out, err) = (stdout(&output), stderr(&output));
    assert_eq!(output.status.code(), Some(1), "{out}\n{err}");
    assert!(err.contains("warning: Bad Digest: "), "{err}");
    assert!(err.contains("the GitHub release publishes sha256:0000"), "{err}");
    assert!(err.contains("warning: Gone: "), "{err}");
    assert!(err.contains("error: 2 of 5 apps could not be imported"), "{err}");
    assert!(
        out.contains(
            "Imported 1 of 5 apps, 2 need their AppImage file, 2 failed: Bad Digest, Gone."
        ),
        "{out}"
    );
    assert!(
        out.contains("Nothing to download for 2 apps, each needs its AppImage file:\n"),
        "{out}"
    );

    assert!(new.data("appimages/the-fine-one.AppImage").is_file());
    let entry = fs::read_to_string(new.data("applications/the-fine-one.desktop")).unwrap();
    assert!(entry.contains("\nName=Fine\n"), "{entry}");
    let mut left: Vec<String> = fs::read_dir(new.data("appimages"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["the-fine-one.AppImage"]);
}

/// An app that follows `github:` releases and was installed from a URL comes
/// out of its newest release straight away: the URL it was installed from is
/// not asked at all.
#[test]
fn a_github_source_comes_before_the_url_it_was_installed_from() {
    let _serial = serial();
    let server = Server::start();
    let new = Home::new(&server);

    server.route("/files/Both-1.AppImage", fake(&new.root, "Both", "1"));
    let newest = fake(&new.root, "Both", "2");
    server.route("/files/Both-2.AppImage", newest.clone());
    let release = release_json("v2", &[(&server.url("/files/Both-2.AppImage"), &sha256(&newest))]);
    server.route("/repos/o/both/releases", release.into_bytes());
    let both = listed("both", "Both", "github:o/both", &server.url("/files/Both-1.AppImage"));
    let file = new.file("apps.json", export::to_json(&[both]).as_bytes());

    new.ok(&["--yes", "import", file.to_str().unwrap()]);
    assert_eq!(fs::read(new.data("appimages/both.AppImage")).unwrap(), newest);
    let asked: Vec<String> =
        server.asked().iter().map(|path| path.split('?').next().unwrap().to_string()).collect();
    assert_eq!(asked, ["/repos/o/both/releases", "/files/Both-2.AppImage"]);
}

/// An application installed from a file on the old machine that updates
/// from a URL is installed from that URL: the file stayed behind, the URL
/// did not. It is downloaded once, not again to bring it up to date from
/// the same URL.
#[test]
fn a_local_origin_with_a_url_update_source_is_installed_from_that_url() {
    let _serial = serial();
    let server = Server::start();
    let new = Home::new(&server);
    let served = fake(&new.root, "Src", "4.2");
    let source = server.url("/latest/Src.AppImage");
    server.route("/latest/Src.AppImage", served.clone());
    let app = listed("src-app", "Src App", &source, "/home/old/Downloads/Src-4.1.AppImage");
    let file = new.file("apps.json", export::to_json(&[app]).as_bytes());

    let output = new.ok(&["--yes", "import", file.to_str().unwrap()]);
    let out = stdout(&output);
    assert!(out.contains("Imported 1 of 1 app.\n"), "{out}");
    assert!(!out.contains("Nothing to download"), "{out}");
    assert_eq!(fs::read(new.data("appimages/src-app.AppImage")).unwrap(), served);
    let entry = fs::read_to_string(new.data("applications/src-app.desktop")).unwrap();
    assert!(entry.contains(&format!("\nX-AppImg-Source={source}\n")), "{entry}");
    assert!(entry.contains(&format!("\nX-AppImg-UpdateSource={source}\n")), "{entry}");
    assert!(entry.contains("\nX-AppImg-Version=4.2\n"), "{entry}");
    assert_eq!(server.asked(), ["/latest/Src.AppImage"]);
}

/// An application installed from the URL it came from, whose AppImage says
/// where its updates come from, is brought up to date right away. When that
/// fails, it stays installed at the version that URL holds, and the output
/// says so: only the update failed. The exit code still says something
/// went wrong.
#[test]
fn an_update_that_fails_after_the_install_leaves_it_installed_and_says_so() {
    let _serial = serial();
    let server = Server::start();
    let new = Home::new(&server);
    // The zsync file its update information names is not there.
    let update_info = format!("zsync|{}", server.url("/zsync/Zsync.AppImage.zsync"));
    let v1 = with_update_info(&update_info, &fake(&new.root, "Zsync", "1"));
    let origin = server.url("/files/Zsync-1.AppImage");
    server.route("/files/Zsync-1.AppImage", v1.clone());
    let app = listed("zsync-app", "Zsync App", "manual", &origin);
    let file = new.file("apps.json", export::to_json(&[app]).as_bytes());

    let output = new.run(&["--yes", "import", file.to_str().unwrap()]);
    let (out, err) = (stdout(&output), stderr(&output));
    assert_eq!(output.status.code(), Some(1), "{out}\n{err}");
    assert!(
        err.contains(&format!(
            "warning: Zsync App: it is installed at version 1 from {origin}, only bringing it up \
             to date failed: "
        )),
        "{err}"
    );
    assert!(
        err.contains("error: 1 app was imported but could not be brought up to date\n"),
        "{err}"
    );
    assert!(out.contains("Imported 1 of 1 app, 1 not brought up to date: Zsync App.\n"), "{out}");
    assert!(!out.contains("failed:"), "{out}");

    // The update was tried, and changed nothing.
    assert!(server.asked().iter().any(|path| path.starts_with("/zsync/")));
    assert_eq!(fs::read(new.data("appimages/zsync-app.AppImage")).unwrap(), v1);
    let left: Vec<_> = fs::read_dir(new.data("appimages")).unwrap().collect();
    assert_eq!(left.len(), 1);
    let entry = fs::read_to_string(new.data("applications/zsync-app.desktop")).unwrap();
    assert!(entry.contains(&format!("\nX-AppImg-UpdateInfo={update_info}\n")), "{entry}");
    assert!(entry.contains("\nX-AppImg-Version=1\n"), "{entry}");
}

/// An export of a format version this appimg does not know is refused as a
/// whole, before anything is downloaded or written.
#[test]
fn an_unknown_format_version_is_refused() {
    let _serial = serial();
    let server = Server::start();
    let Exported { old: _old, file, .. } = exported(&server);
    let text = fs::read_to_string(&file).unwrap().replace("\"version\": 1", "\"version\": 2");
    let new = Home::new(&server);
    let file = new.file("apps.json", text.as_bytes());
    let (before, asked) = (new.snapshot(), server.asked().len());

    let output = new.run(&["--yes", "import", file.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output)
            .contains("the export has format version 2, which this appimg does not know"),
        "{}",
        stderr(&output)
    );
    assert_eq!(new.snapshot(), before);
    assert_eq!(server.asked().len(), asked);
}

/// A dry run says where each application would come from and what the
/// others need, and changes nothing: nothing is downloaded, nothing written.
#[test]
fn a_dry_run_shows_the_plan_and_changes_nothing() {
    let _serial = serial();
    let server = Server::start();
    let Exported { old: _old, file, .. } = exported(&server);
    let new = Home::new(&server);
    let (before, asked) = (new.snapshot(), server.asked().len());

    let output = new.ok(&["import", "--dry-run", file.to_str().unwrap()]);
    let out = stdout(&output);
    assert!(
        out.contains(
            "  Github App (github-app): the newest AppImage of github:o/r, the one like \
             Github_App-1.0.0.AppImage\n"
        ),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "  Url App (url-app): {}, which it updates from\n",
            server.url("/latest/Url_App.AppImage")
        )),
        "{out}"
    );
    assert!(out.contains("  Local App (local-app): it was installed from"), "{out}");
    assert!(out.contains("Would import 2 of 3 apps, 1 needs its AppImage file.\n"), "{out}");
    assert_eq!(new.snapshot(), before);
    assert_eq!(server.asked().len(), asked);
}
