//! `appimg install`, `update --check` and `update` with a vendor's download
//! link that redirects to the current version. Against a local server only,
//! with every XDG directory inside a temporary one.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

/// What the server answers for a path: a file with an `ETag`, or a
/// redirect to another path.
#[derive(Clone)]
enum Answer {
    File { body: Vec<u8>, etag: String },
    Redirect(String),
}

/// A vendor's download server. Every request is kept as `METHOD path`.
struct Vendor {
    base: String,
    routes: Arc<Mutex<HashMap<String, Answer>>>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Vendor {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes: Arc<Mutex<HashMap<String, Answer>>> = Arc::default();
        let asked: Arc<Mutex<Vec<String>>> = Arc::default();
        let (routed, recorded) = (Arc::clone(&routes), Arc::clone(&asked));
        let origin = base.clone();
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
                let mut words = head.split_whitespace();
                let method = words.next().unwrap_or("GET").to_string();
                let path = words.next().unwrap_or("/").to_string();
                recorded.lock().unwrap().push(format!("{method} {path}"));

                let answer = routed.lock().unwrap().get(&path).cloned();
                let (status, headers, body) = match answer {
                    Some(Answer::File { body, etag }) => (
                        "200 OK",
                        format!("ETag: {etag}\r\nContent-Length: {}\r\n", body.len()),
                        body,
                    ),
                    Some(Answer::Redirect(to)) => (
                        "302 Found",
                        format!("Location: {origin}{to}\r\nContent-Length: 0\r\n"),
                        Vec::new(),
                    ),
                    None => (
                        "404 Not Found",
                        "Content-Length: 9\r\n".to_string(),
                        b"not found".to_vec(),
                    ),
                };
                let body = if method == "HEAD" { Vec::new() } else { body };
                let head = format!("HTTP/1.1 {status}\r\n{headers}Connection: close\r\n\r\n");
                let _ = stream.write_all(&[head.as_bytes(), &body].concat());
                let _ = stream.shutdown(Shutdown::Write);
            }
        });
        Self { base, routes, asked }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn route(&self, path: &str, answer: Answer) {
        self.routes.lock().unwrap().insert(path.to_string(), answer);
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

/// A throwaway home with a stand-in for `unsquashfs`, which is the one way
/// the fixtures extract.
struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::Builder::new().prefix("appimg-test-").tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in ["home", "data", "config", "tmp", "tools"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
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

    /// Runs `appimg` and insists that it succeeds.
    fn ok(&self, args: &[&str]) -> String {
        let (success, text) = self.run(args);
        assert!(success, "{args:?}\n{text}");
        text
    }

    /// Runs `appimg`, with nobody to answer a question: whether it
    /// succeeded, and what it wrote.
    fn run(&self, args: &[&str]) -> (bool, String) {
        let path = format!(
            "{}:{}",
            self.root.join("tools").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let output: Output = Command::new(env!("CARGO_BIN_EXE_appimg"))
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
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        (output.status.success(), text)
    }

    fn entry(&self) -> String {
        fs::read_to_string(self.root.join("data/applications/vendor-app.desktop")).unwrap()
    }
}

/// An AppImage as far as appimg looks, whose metadata declares no version:
/// the name of the file is all that says which one it is.
fn appimage(root: &Path, marker: &str) -> Vec<u8> {
    let payload = root.join(format!("payloads/{marker}"));
    fs::create_dir_all(&payload).unwrap();
    fs::write(
        payload.join("vendor.desktop"),
        "[Desktop Entry]\nType=Application\nName=Vendor App\nExec=vendor\nCategories=Utility;\n",
    )
    .unwrap();
    [
        &b"\x7fELF\x01\x01\x01\x00AI\x02\n"[..],
        format!("payload={}\n# {marker}\nhsqs\n", payload.display()).as_bytes(),
    ]
    .concat()
}

#[test]
fn a_vendor_link_installs_checks_and_updates_by_the_file_it_lands_on() {
    let home = Home::new();
    let vendor = Vendor::start();
    let first = "/builds/Vendor-App-1.2.0-x86_64.AppImage";
    vendor.route(first, Answer::File { body: appimage(&home.root, "v1"), etag: "\"one\"".into() });
    vendor.route("/download/latest/linux", Answer::Redirect(first.to_string()));
    let link = vendor.url("/download/latest/linux");

    home.ok(&["--yes", "install", &link]);
    let entry = home.entry();
    // The version is in the name of the file the link landed on, and in
    // nothing else.
    assert!(entry.contains("X-AppImg-Version=1.2.0\n"), "{entry}");
    assert!(entry.contains(&format!("X-AppImg-UpdateSource={link}\n")), "{entry}");
    assert!(
        entry.contains(&format!(" %22one%22 {first} Vendor-App-1.2.0-x86_64.AppImage\n")),
        "{entry}"
    );

    let before = vendor.asked().len();
    // Nothing to update is what the exit status says too.
    let (updates, check) = home.run(&["update", "--all", "--check"]);
    assert!(!updates && check.contains("up to date"), "{check}");
    assert_eq!(
        vendor.asked()[before..],
        ["HEAD /download/latest/linux".to_string(), format!("HEAD {first}")]
    );
    let before = vendor.asked().len();
    let (updated, update) = home.run(&["update", "vendor-app"]);
    assert!(!updated && update.contains("Vendor App is up to date."), "{update}");
    assert!(vendor.asked()[before..].iter().all(|asked| asked.starts_with("HEAD ")));

    let second = "/builds/Vendor-App-1.3.0-x86_64.AppImage";
    vendor.route(second, Answer::File { body: appimage(&home.root, "v2"), etag: "\"two\"".into() });
    vendor.route("/download/latest/linux", Answer::Redirect(second.to_string()));

    let check = home.ok(&["update", "--all", "--check"]);
    assert!(check.contains("1.3.0") && check.contains("update available"), "{check}");
    let update = home.ok(&["update", "vendor-app"]);
    assert!(update.contains("1.2.0 -> 1.3.0"), "{update}");
    assert!(vendor.asked().contains(&format!("GET {second}")));
    let entry = home.entry();
    assert!(entry.contains("X-AppImg-Version=1.3.0\n"), "{entry}");
    assert!(entry.contains(&format!(" %22two%22 {second} ")), "{entry}");
}
