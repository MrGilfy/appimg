//! AppImages that come inside a zip or tar.gz archive, installed and
//! updated the way a user does it, against a temporary home and a local
//! HTTP server that stands in for the hosts and for the GitHub API. No test
//! in here talks to any real host or touches the real `$HOME`.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use flate2::write::{DeflateEncoder, GzEncoder};
use flate2::{Compression, Crc};

/// Executing a file while any process still holds it open for writing fails
/// with `ETXTBSY`, and a `fork` in another test thread can hold one for a
/// moment. The tests run one at a time.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// An HTTP server on a random port that answers each path it was given a
/// body for with a 200 and that body, and anything else with a 404. It
/// stands in for the hosts files come from and for the GitHub API.
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

    /// The paths asked for so far that are files, not the API.
    fn downloads(&self) -> Vec<String> {
        self.asked
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.starts_with("/download/"))
            .cloned()
            .collect()
    }

    /// Publishes release `tag` of HarbourMasters/Shipwright with these
    /// files, each with the digest given, or its own when there is none.
    fn release(&self, tag: &str, files: &[(&str, Vec<u8>, Option<String>)]) {
        let assets: Vec<String> = files
            .iter()
            .map(|(name, bytes, digest)| {
                let path = format!("/download/{tag}/{name}");
                self.route(&path, bytes.clone());
                let digest = digest.clone().unwrap_or_else(|| sha256(bytes));
                format!(
                    "{{\"name\":\"{name}\",\"size\":{},\"digest\":\"sha256:{digest}\",\
                     \"browser_download_url\":\"{}\"}}",
                    bytes.len(),
                    self.url(&path)
                )
            })
            .collect();
        let body = format!(
            "[{{\"tag_name\":\"{tag}\",\"draft\":false,\"prerelease\":false,\
             \"published_at\":\"2026-10-01T10:00:00Z\",\"assets\":[{}]}}]",
            assets.join(",")
        );
        self.route("/repos/HarbourMasters/Shipwright/releases", body.into_bytes());
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

    fn file(&self, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    fn installed(&self, slug: &str) -> Vec<u8> {
        fs::read(self.root.join(format!("data/appimages/{slug}.AppImage"))).unwrap()
    }

    fn entry(&self, slug: &str) -> String {
        fs::read_to_string(self.root.join(format!("data/applications/{slug}.desktop"))).unwrap()
    }

    /// Every file below the home, by its path relative to the root, the
    /// caches an install refreshes aside.
    fn files(&self) -> BTreeSet<PathBuf> {
        let mut found = BTreeSet::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                let name = entry.file_name();
                if entry.file_type().unwrap().is_dir() {
                    stack.push(path);
                } else if !matches!(
                    name.to_str(),
                    Some("mimeinfo.cache" | "icon-theme.cache" | "index.theme")
                ) {
                    found.insert(path.strip_prefix(&self.root).unwrap().to_path_buf());
                }
            }
        }
        found
    }

    /// The files next to the installed AppImages: nothing an update stages
    /// may stay.
    fn appimages_dir(&self) -> BTreeSet<String> {
        fs::read_dir(self.root.join("data/appimages"))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect()
    }
}

/// An AppImage as far as appimg looks: the ELF magic, `AI` and type 2
/// where an ELF header leaves room, and the payload the stand-in for
/// `unsquashfs` copies, with a desktop entry naming it `name` at `version`.
fn appimage(root: &Path, name: &str, version: &str) -> Vec<u8> {
    let payload = root.join(format!("payloads/{name}-{version}"));
    fs::create_dir_all(&payload).unwrap();
    fs::write(
        payload.join("soh.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName={name}\nExec=soh.appimage\nIcon=soh\n\
             Categories=Game;\nX-AppImage-Version={version}\n"
        ),
    )
    .unwrap();
    [
        &b"\x7fELF\x01\x01\x01\x00AI\x02\n"[..],
        format!("payload={}\n# {name} {version}\nhsqs\n", payload.display()).as_bytes(),
    ]
    .concat()
}

/// A shared library, ELF but no AppImage.
fn library() -> Vec<u8> {
    b"\x7fELF\x02\x01\x01\x00\x00\x00\x00 libSDL2".to_vec()
}

/// A zip archive of deflated files, as PKWARE's APPNOTE lays one out.
fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in files {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(data).unwrap();
        let packed = encoder.finish().unwrap();
        let mut crc = Crc::new();
        crc.update(data);
        let offset = out.len() as u32;
        let common = |out: &mut Vec<u8>| {
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&8u16.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&crc.sum().to_le_bytes());
            out.extend_from_slice(&(packed.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
        };
        out.extend_from_slice(b"PK\x03\x04");
        common(&mut out);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&packed);
        central.extend_from_slice(b"PK\x01\x02");
        central.extend_from_slice(&(3u16 << 8 | 20).to_le_bytes());
        common(&mut central);
        // Comment length, disk, internal and external attributes.
        central.extend_from_slice(&[0; 10]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let directory_at = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&directory_at.to_le_bytes());
    out.extend_from_slice(&[0; 2]);
    out
}

/// A gzip-compressed tar file of regular files, in ustar form.
fn tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut tar = Vec::new();
    for (name, data) in files {
        let mut header = vec![0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        header[257..265].copy_from_slice(b"ustar\x0000");
        let sum: u32 = header.iter().map(|byte| *byte as u32).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        tar.extend(header);
        tar.extend_from_slice(data);
        tar.resize(tar.len().next_multiple_of(512), 0);
    }
    tar.extend([0u8; 1024]);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&tar).unwrap();
    encoder.finish().unwrap()
}

fn sha256(bytes: &[u8]) -> String {
    let file = tempfile::NamedTempFile::new().unwrap();
    fs::write(file.path(), bytes).unwrap();
    appimg_core::digest::sha256_file(file.path()).unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Shipwright's Linux zip: the AppImage, lowercase, beside what else a
/// game ships.
fn shipwright_zip(root: &Path, version: &str) -> (Vec<u8>, Vec<u8>) {
    let soh = appimage(root, "Ship of Harkinian", version);
    let zip = zip(&[
        ("readme.txt", b"Ship of Harkinian"),
        ("soh.appimage", &soh),
        ("lib/libSDL2.so", &library()),
    ]);
    (soh, zip)
}

/// Installs Shipwright 9.2.3 from the zip a user downloaded by hand,
/// following its GitHub releases, and returns the AppImage that was in it.
fn install_9_2_3(home: &Home, extra: &[&str]) -> Vec<u8> {
    let (soh, zip) = shipwright_zip(&home.root, "9.2.3");
    let file = home.file("home/Downloads/SoH-Ackbar-Delta-Linux.zip", &zip);
    let mut args = vec![
        "--yes",
        "install",
        file.to_str().unwrap(),
        "--update-source",
        "github:HarbourMasters/Shipwright",
    ];
    args.extend_from_slice(extra);
    home.ok(&args);
    soh
}

#[test]
fn install_from_a_local_zip_takes_the_appimage_by_its_bytes() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    let (soh, zip) = shipwright_zip(&home.root, "9.2.3");
    let file = home.file("home/Downloads/SoH-Ackbar-Delta-Linux.zip", &zip);
    let before = home.files();

    let output = home.ok(&["--yes", "install", file.to_str().unwrap()]);
    let text = stdout(&output);
    assert!(text.contains(&format!("Took soh.appimage out of {}", file.display())), "{text}");
    assert!(text.contains("Installed Ship of Harkinian as ship-of-harkinian"), "{text}");
    assert_eq!(home.installed("ship-of-harkinian"), soh);
    let entry = home.entry("ship-of-harkinian");
    assert!(entry.contains(&format!("\nX-AppImg-Source={}\n", file.display())), "{entry}");
    assert!(entry.contains("\nX-AppImg-Version=9.2.3\n"), "{entry}");

    // What was written: the installed AppImage and its entry, and nothing
    // the archive named. The unpacked copy went with its temporary
    // directory.
    let added: BTreeSet<PathBuf> = home.files().difference(&before).cloned().collect();
    assert_eq!(
        added,
        BTreeSet::from([
            PathBuf::from("data/appimages/ship-of-harkinian.AppImage"),
            PathBuf::from("data/applications/ship-of-harkinian.desktop"),
        ])
    );
}

#[test]
fn install_from_an_archive_url_and_update_from_it() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    let v1 = appimage(&home.root, "Ship of Harkinian", "9.2.3");
    server.route("/files/soh-linux.tar.gz", tar_gz(&[("soh/README", b"hi"), ("soh/soh", &v1)]));

    let output = home.ok(&["--yes", "install", &server.url("/files/soh-linux.tar.gz")]);
    assert!(stdout(&output).contains("took soh/soh out of the archive"), "{}", stdout(&output));
    assert_eq!(home.installed("ship-of-harkinian"), v1);
    assert_eq!(home.appimages_dir(), BTreeSet::from(["ship-of-harkinian.AppImage".to_string()]));

    // The URL serves a newer archive: the update unpacks it the same way.
    let v2 = appimage(&home.root, "Ship of Harkinian", "9.3.0");
    server.route("/files/soh-linux.tar.gz", tar_gz(&[("soh/soh", &v2)]));
    let output = home.ok(&["update", "ship-of-harkinian"]);
    assert!(stdout(&output).contains("took soh/soh out of it"), "{}", stdout(&output));
    assert_eq!(home.installed("ship-of-harkinian"), v2);
    assert_eq!(home.appimages_dir(), BTreeSet::from(["ship-of-harkinian.AppImage".to_string()]));
}

/// What Shipwright does from one release to the next: a new codename in
/// every file name, and one zip per platform.
#[test]
fn an_update_follows_a_new_codename_and_checks_the_archive_first() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    install_9_2_3(&home, &[]);

    let (soh, linux) = shipwright_zip(&home.root, "9.3.0");
    server.release(
        "9.3.0",
        &[
            ("SoH-Blue-Echo-Linux.zip", linux, None),
            ("SoH-Blue-Echo-Mac.zip", b"mac".to_vec(), None),
            ("SoH-Blue-Echo-Win64.zip", b"win".to_vec(), None),
        ],
    );

    let check = home.run(&["update", "--all", "--check"]);
    assert_eq!(check.status.code(), Some(0), "{}", stderr(&check));
    assert!(stdout(&check).contains("update available"), "{}", stdout(&check));

    let output = home.ok(&["update", "ship-of-harkinian"]);
    let text = stdout(&output);
    assert!(text.contains("9.2.3 -> 9.3.0"), "{text}");
    assert!(text.contains("took soh.appimage out of it"), "{text}");
    assert!(text.contains("sha256 matches the digest GitHub publishes"), "{text}");
    assert_eq!(home.installed("ship-of-harkinian"), soh);
    assert_eq!(server.downloads(), ["/download/9.3.0/SoH-Blue-Echo-Linux.zip"]);
    assert_eq!(home.appimages_dir(), BTreeSet::from(["ship-of-harkinian.AppImage".to_string()]));

    // An archive that is not what the release publishes is refused before
    // anything comes out of it: this one holds two AppImages, which
    // unpacking would have said instead.
    let two = zip(&[
        ("soh.appimage", &appimage(&home.root, "Ship of Harkinian", "9.4.0")),
        ("other.appimage", &appimage(&home.root, "Other", "1")),
    ]);
    server.release("9.4.0", &[("SoH-Charlie-Foxtrot-Linux.zip", two, Some("ab".repeat(32)))]);
    let refused = home.run(&["update", "ship-of-harkinian"]);
    assert_eq!(refused.status.code(), Some(1));
    let message = stderr(&refused);
    assert!(
        message.contains(&format!("the GitHub release publishes sha256:{}", "ab".repeat(32))),
        "{message}"
    );
    assert!(!message.contains("AppImages"), "{message}");
    assert_eq!(home.installed("ship-of-harkinian"), soh);
    assert_eq!(home.appimages_dir(), BTreeSet::from(["ship-of-harkinian.AppImage".to_string()]));
}

#[test]
fn asset_picks_the_file_when_the_names_cannot() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    install_9_2_3(&home, &[]);

    // Two builds for Linux: nothing to go by but a pattern.
    let (soh, linux) = shipwright_zip(&home.root, "9.3.0");
    let (_, debug) = shipwright_zip(&home.root, "9.3.0-debug");
    server.release(
        "9.3.0",
        &[
            ("SoH-Blue-Echo-Linux.zip", linux, None),
            ("SoH-Blue-Echo-Linux-Debug.zip", debug, None),
            ("SoH-Blue-Echo-Win64.zip", b"win".to_vec(), None),
        ],
    );
    let refused = home.run(&["update", "ship-of-harkinian"]);
    assert_eq!(refused.status.code(), Some(1));
    let message = stderr(&refused);
    assert!(message.contains("2 are left for this machine"), "{message}");
    assert!(message.contains("--asset"), "{message}");

    let set = home.ok(&["update-source", "ship-of-harkinian", "--asset", "SoH-*-Linux.zip"]);
    assert!(stdout(&set).contains("github:HarbourMasters/Shipwright#SoH-*-Linux.zip"));
    assert!(home
        .entry("ship-of-harkinian")
        .contains("\nX-AppImg-UpdateSource=github:HarbourMasters/Shipwright#SoH-*-Linux.zip\n"));
    let shown = stdout(&home.ok(&["update-source", "ship-of-harkinian"]));
    assert!(shown.contains("the newest release with an asset matching SoH-*-Linux.zip"), "{shown}");
    // It travels with the update source.
    let export = stdout(&home.ok(&["export"]));
    assert!(export.contains("github:HarbourMasters/Shipwright#SoH-*-Linux.zip"), "{export}");

    home.ok(&["update", "ship-of-harkinian"]);
    assert_eq!(home.installed("ship-of-harkinian"), soh);
    assert_eq!(server.downloads(), ["/download/9.3.0/SoH-Blue-Echo-Linux.zip"]);

    // Setting the source again without one drops it.
    home.ok(&["update-source", "ship-of-harkinian", "github:HarbourMasters/Shipwright"]);
    assert!(home
        .entry("ship-of-harkinian")
        .contains("\nX-AppImg-UpdateSource=github:HarbourMasters/Shipwright\n"));
}

#[test]
fn install_keeps_the_asset_pattern_and_refuses_it_without_a_github_source() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    install_9_2_3(&home, &["--asset", "SoH-*-Linux.zip"]);
    assert!(home
        .entry("ship-of-harkinian")
        .contains("\nX-AppImg-UpdateSource=github:HarbourMasters/Shipwright#SoH-*-Linux.zip\n"));

    let other = Home::new(&server);
    let (_, zip) = shipwright_zip(&other.root, "9.2.3");
    let file = other.file("home/Downloads/SoH.zip", &zip);
    let refused = other.run(&["--yes", "install", file.to_str().unwrap(), "--asset", "SoH-*.zip"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        stderr(&refused).contains("--asset picks a file out of a GitHub release"),
        "{}",
        stderr(&refused)
    );
    let refused = other.run(&[
        "--yes",
        "install",
        file.to_str().unwrap(),
        "--update-source",
        "github:HarbourMasters/Shipwright",
        "--asset",
        "SoH Linux.zip",
    ]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(stderr(&refused).contains("is no asset pattern"), "{}", stderr(&refused));
    assert!(!other.root.join("data/applications/ship-of-harkinian.desktop").exists());
}

#[test]
fn an_archive_with_no_or_several_appimages_installs_nothing() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    let a = appimage(&home.root, "A", "1");
    let b = appimage(&home.root, "B", "1");
    let before = home.files();

    let none =
        home.file("home/Downloads/none.zip", &zip(&[("readme.txt", b"x"), ("soh", &library())]));
    let refused = home.run(&["--yes", "install", none.to_str().unwrap()]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        stderr(&refused).contains("it holds no AppImage, only readme.txt, soh"),
        "{}",
        stderr(&refused)
    );

    let two = home.file("home/Downloads/two.tar.gz", &tar_gz(&[("a/A.AppImage", &a), ("b/b", &b)]));
    let refused = home.run(&["--yes", "install", two.to_str().unwrap()]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        stderr(&refused)
            .contains("it holds 2 AppImages, and appimg takes exactly one: a/A.AppImage, b/b"),
        "{}",
        stderr(&refused)
    );

    // Nothing installed, nothing left in the temporary directory.
    let added: BTreeSet<PathBuf> = home.files().difference(&before).cloned().collect();
    assert_eq!(
        added,
        BTreeSet::from([
            PathBuf::from("home/Downloads/none.zip"),
            PathBuf::from("home/Downloads/two.tar.gz")
        ])
    );
}

/// An archive whose entries climb out of wherever they would be unpacked:
/// the AppImage still installs under its slug, and nothing else is written.
#[test]
fn no_path_out_of_an_archive_is_ever_written() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    let id = home.root.file_name().unwrap().to_string_lossy().into_owned();
    let soh = appimage(&home.root, "Ship of Harkinian", "9.2.3");
    let escape = format!("../../../escape-{id}.AppImage");
    let absolute = format!("/tmp/absolute-{id}");
    let file = home.file(
        "home/Downloads/hostile.zip",
        &zip(&[(&escape, &soh), (&absolute, b"x"), ("../../evil.desktop", b"[Desktop Entry]")]),
    );
    let before = home.files();

    let output = home.ok(&["--yes", "install", file.to_str().unwrap()]);
    assert!(stdout(&output).contains(&format!("Took {escape} out of")), "{}", stdout(&output));
    assert_eq!(home.installed("ship-of-harkinian"), soh);
    let added: BTreeSet<PathBuf> = home.files().difference(&before).cloned().collect();
    assert_eq!(
        added,
        BTreeSet::from([
            PathBuf::from("data/appimages/ship-of-harkinian.AppImage"),
            PathBuf::from("data/applications/ship-of-harkinian.desktop"),
        ])
    );
    let outside = home.root.parent().unwrap();
    assert!(!outside.join(format!("escape-{id}.AppImage")).exists());
    assert!(!Path::new(&absolute).exists());
    assert!(!outside.join("evil.desktop").exists());
}

/// What comes out of an archive gets the checks any AppImage gets: this
/// one's ELF header puts its section table far behind its end, so it was
/// cut short before it was packed.
#[test]
fn an_appimage_cut_short_inside_an_archive_is_refused() {
    let _serial = serial();
    let server = Server::start();
    let home = Home::new(&server);
    let mut short = b"\x7fELF\x02\x01\x01\x00AI\x02".to_vec();
    short.resize(64, 0);
    short[40..48].copy_from_slice(&1_000_000u64.to_le_bytes()); // e_shoff
    short[58..60].copy_from_slice(&64u16.to_le_bytes()); // e_shentsize
    short[60..62].copy_from_slice(&1u16.to_le_bytes()); // e_shnum
    let packed = zip(&[("soh.appimage", &short)]);

    let file = home.file("home/Downloads/short.zip", &packed);
    let refused = home.run(&["--yes", "install", file.to_str().unwrap()]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        stderr(&refused).contains("is cut short, a complete file is at least 1000064 bytes"),
        "{}",
        stderr(&refused)
    );

    server.route("/files/short.zip", packed);
    let refused = home.run(&["--yes", "install", &server.url("/files/short.zip")]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        stderr(&refused).contains("soh.appimage out of the archive: the download is cut short"),
        "{}",
        stderr(&refused)
    );
    // Nothing was installed.
    assert!(home.files().iter().all(|file| !file.starts_with("data")), "{:?}", home.files());
}
