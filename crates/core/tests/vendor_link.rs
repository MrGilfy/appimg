//! Vendor download links as update sources: a link that redirects to the
//! current file, or a fixed name whose file changes. Against a local HTTP
//! server only, no test in here ever talks to a real host.

mod common;

use std::collections::HashMap;
use std::io::Cursor;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::Path;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use appimg_core::desktop_entry::{DesktopEntry, KEY_REMOTE};
use appimg_core::install::InstallRequest;
use appimg_core::list::InstalledApp;
use appimg_core::metadata::Reading;
use appimg_core::remote::Remote;
use appimg_core::update::{UpdatePath, UpdateStatus};
use appimg_core::{download, install, list, metadata, update};

use common::{read, with_unsquashfs_stand_in, FakeAppImage, Sandbox};

const LAST_MODIFIED: &str = "Sat, 19 Sep 2026 01:52:51 GMT";

/// How the server answers one path.
#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// Whether the body goes out without a `Content-Length`, chunked.
    chunked: bool,
    /// Whether a HEAD request is refused with a 405.
    refuses_head: bool,
}

impl Reply {
    fn file(body: Vec<u8>) -> Self {
        Self { status: 200, headers: Vec::new(), body, chunked: false, refuses_head: false }
    }

    fn redirect(location: &str) -> Self {
        Self { status: 302, ..Self::file(Vec::new()) }.header("Location", location)
    }

    fn page(status: u16) -> Self {
        Self { status, ..Self::file(b"<!DOCTYPE html><html>gone</html>".to_vec()) }
            .header("Content-Type", "text/html; charset=UTF-8")
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

/// What the server saw of one request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    method: String,
    /// The `Host` header, which tells `127.0.0.1` from `localhost`.
    host: String,
    /// The path, query included.
    path: String,
    range: Option<String>,
}

/// A vendor's download server: each path answered the way a test says,
/// changed between requests the way a vendor publishes a new version.
struct Vendor {
    address: SocketAddr,
    routes: Arc<Mutex<HashMap<String, Reply>>>,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Sender<()>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Vendor {
    fn start() -> Self {
        let port =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let server = tiny_http::Server::http(address).unwrap();
        let routes: Arc<Mutex<HashMap<String, Reply>>> = Arc::new(Mutex::new(HashMap::new()));
        let routed = Arc::clone(&routes);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        let (stop, stopped) = channel();

        let handle = thread::spawn(move || loop {
            match server.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(Some(request)) => {
                    let method = request.method().as_str().to_string();
                    let path = request.url().to_string();
                    let range = header(&request, "Range");
                    recorded.lock().unwrap().push(Seen {
                        method: method.clone(),
                        host: header(&request, "Host").unwrap_or_default(),
                        path: path.clone(),
                        range: range.clone(),
                    });

                    let route = path.split('?').next().unwrap_or(&path).to_string();
                    let reply = routed.lock().unwrap().get(&route).cloned();
                    let mut reply = reply.unwrap_or_else(|| Reply::page(404));
                    if reply.refuses_head && method == "HEAD" {
                        reply = Reply { status: 405, body: Vec::new(), ..Reply::file(Vec::new()) };
                    } else if range.as_deref() == Some("bytes=0-0") && reply.status == 200 {
                        let total = reply.body.len();
                        reply.body.truncate(1);
                        reply = Reply { status: 206, ..reply }
                            .header("Content-Range", &format!("bytes 0-0/{total}"));
                    }

                    let headers = reply
                        .headers
                        .iter()
                        .map(|(name, value)| {
                            tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes())
                                .unwrap()
                        })
                        .collect();
                    let length = (!reply.chunked).then_some(reply.body.len());
                    let _ = request.respond(tiny_http::Response::new(
                        tiny_http::StatusCode(reply.status),
                        headers,
                        Cursor::new(reply.body),
                        length,
                        None,
                    ));
                }
                Ok(None) => {
                    if stopped.try_recv().is_ok() {
                        return;
                    }
                }
                Err(_) => return,
            }
        });

        Self { address, routes, seen, stop, handle: Some(handle) }
    }

    /// The URL of `path` under the address the server listens on.
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    /// The same, under another host name for the same server: what a CDN
    /// that rotates its host names hands out.
    fn other_host(&self, path: &str) -> String {
        format!("http://localhost:{}{path}", self.address.port())
    }

    fn route(&self, path: &str, reply: Reply) {
        self.routes.lock().unwrap().insert(path.to_string(), reply);
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// The requests since the first `since` of them.
    fn seen_since(&self, since: usize) -> Vec<Seen> {
        self.seen()[since..].to_vec()
    }
}

impl Drop for Vendor {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn header(request: &tiny_http::Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str().to_string())
}

/// An AppImage that passes the checks an update holds a download to, told
/// apart from the others by `marker`.
fn build(sandbox: &Sandbox, marker: &str) -> Vec<u8> {
    let file = FakeAppImage::new("Vendor App")
        .marker(marker)
        .elf()
        .build(&sandbox.root, &format!("{marker}.AppImage"));
    std::fs::read(file).unwrap()
}

/// Installs from `url` the way `appimg install <url>` does: downloaded,
/// named after what the server named it, and installed with what the
/// server said about it.
fn install_from(sandbox: &Sandbox, url: &str) -> InstalledApp {
    let scratch = tempfile::tempdir().unwrap();
    let dest = scratch.path().join(download::file_name_from_url(url));
    let fetched = download::appimage_or_archive(url, &dest, &|_| Ok(None), None).unwrap();
    let file = match fetched.remote.name.as_deref() {
        Some(name) => {
            let named = scratch.path().join(download::file_name_from_url(name));
            std::fs::rename(&dest, &named).unwrap();
            named
        }
        None => dest,
    };
    with_unsquashfs_stand_in(sandbox, || {
        let info = metadata::inspect(&file, None, Reading::MayRun).unwrap();
        let mut request = InstallRequest::from_info(&file, url, &info);
        request.remote = Some(fetched.remote.clone());
        install::install(&sandbox.paths, &request).unwrap();
    });
    list::find(&sandbox.paths, "vendor-app").unwrap()
}

fn record(app: &InstalledApp) -> Remote {
    let entry = DesktopEntry::read(&app.desktop_entry_path).unwrap();
    let value = entry.get(KEY_REMOTE).expect("the entry holds no record of the download");
    Remote::parse_record(value).unwrap_or_else(|| panic!("not a record: {value}")).1
}

fn check(app: &InstalledApp) -> UpdateStatus {
    update::check(app).unwrap()
}

/// Every request since `since` was a HEAD request: a check downloads
/// nothing.
fn assert_only_head(vendor: &Vendor, since: usize) {
    let seen = vendor.seen_since(since);
    assert!(!seen.is_empty());
    assert!(seen.iter().all(|seen| seen.method == "HEAD"), "{seen:?}");
}

/// LM Studio, Beeper and Cursor: a stable link that redirects to the
/// current file, whose name carries the version. A check costs one HEAD
/// request per redirect and nothing else.
#[test]
fn a_link_that_redirects_to_the_current_version_is_followed_without_a_download() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let vendor = Vendor::start();
    let first = "/builds/1.0.0/Vendor-App-1.0.0-x86_64.AppImage";
    vendor.route(first, Reply::file(build(&sandbox, "v1")).header("ETag", "\"one\""));
    vendor.route("/download/latest/linux", Reply::redirect(&vendor.url(first)));

    let app = install_from(&sandbox, &vendor.url("/download/latest/linux"));
    assert_eq!(app.version.as_deref(), Some("1.0.0"));
    assert_eq!(app.update_source.as_deref(), Some(vendor.url("/download/latest/linux").as_str()));
    let recorded = record(&app);
    assert_eq!(recorded.path, first);
    assert_eq!(recorded.name.as_deref(), Some("Vendor-App-1.0.0-x86_64.AppImage"));
    assert_eq!(recorded.etag.as_deref(), Some("\"one\""));

    let before = vendor.seen().len();
    let status = check(&app);
    assert!(status.nothing_to_do() && status.settled, "{status:?}");
    assert_eq!(status.latest_version.as_deref(), Some("1.0.0"));
    assert_only_head(&vendor, before);
    assert_eq!(vendor.seen_since(before).len(), 2, "one request per hop");

    let second = "/builds/1.1.0/Vendor-App-1.1.0-x86_64.AppImage";
    vendor.route(second, Reply::file(build(&sandbox, "v2")).header("ETag", "\"two\""));
    vendor.route("/download/latest/linux", Reply::redirect(&vendor.url(second)));

    let before = vendor.seen().len();
    let status = check(&app);
    assert!(status.available, "{status:?}");
    assert_eq!(status.latest_version.as_deref(), Some("1.1.0"));
    assert_only_head(&vendor, before);

    let outcome =
        with_unsquashfs_stand_in(&sandbox, || update::update(&sandbox.paths, &app, None)).unwrap();
    assert!(matches!(outcome.path, UpdatePath::FullDownload { .. }), "{:?}", outcome.path);
    assert!(read(&outcome.appimage_path).contains("v2"));
    update::confirm(&sandbox.paths, "vendor-app").unwrap();

    let app = list::find(&sandbox.paths, "vendor-app").unwrap();
    assert_eq!(app.version.as_deref(), Some("1.1.0"));
    assert_eq!(record(&app).path, second);
    assert!(check(&app).nothing_to_do());
}

/// CurseForge and Tuta: one fixed name, and a new file behind it. The
/// `ETag` tells them apart.
#[test]
fn a_fixed_name_is_followed_by_its_etag() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let vendor = Vendor::start();
    let path = "/downloads/vendor-app-latest-linux.AppImage";
    vendor.route(
        path,
        Reply::file(build(&sandbox, "v1"))
            .header("ETag", "\"one\"")
            .header("Last-Modified", LAST_MODIFIED),
    );

    let app = install_from(&sandbox, &vendor.url(path));
    assert_eq!(app.version, None);
    assert_eq!(record(&app).modified, Some(1_789_782_771));
    assert!(check(&app).nothing_to_do());

    vendor.route(
        path,
        Reply::file(build(&sandbox, "v2"))
            .header("ETag", "\"two\"")
            .header("Last-Modified", LAST_MODIFIED),
    );
    let before = vendor.seen().len();
    let status = check(&app);
    assert!(status.available, "{status:?}");
    assert_only_head(&vendor, before);

    let outcome =
        with_unsquashfs_stand_in(&sandbox, || update::update(&sandbox.paths, &app, None)).unwrap();
    assert!(read(&outcome.appimage_path).contains("v2"));
    update::confirm(&sandbox.paths, "vendor-app").unwrap();
    let app = list::find(&sandbox.paths, "vendor-app").unwrap();
    assert_eq!(record(&app).etag.as_deref(), Some("\"two\""));
    assert!(check(&app).nothing_to_do());
}

/// CDNs rotate their host names and sign their URLs with a query that
/// expires. Neither is another file: the same path is the same file. A
/// different path, in the same place, is.
#[test]
fn the_same_landing_path_on_another_host_is_unchanged() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let vendor = Vendor::start();
    let path = "/builds/Vendor-App-x86_64.AppImage";
    vendor.route(path, Reply::file(build(&sandbox, "v1")).header("ETag", "\"one\""));
    vendor.route("/latest", Reply::redirect(&format!("{}?sig=first", vendor.url(path))));
    let app = install_from(&sandbox, &vendor.url("/latest"));
    assert_eq!(record(&app).path, path);

    vendor.route("/latest", Reply::redirect(&format!("{}?sig=second", vendor.other_host(path))));
    let before = vendor.seen().len();
    let status = check(&app);
    let seen = vendor.seen_since(before);
    assert_eq!(seen[1].host, format!("localhost:{}", vendor.address.port()), "{seen:?}");
    assert_eq!(seen[1].path, format!("{path}?sig=second"));
    assert!(!status.available && status.settled, "{status:?}");
    assert_eq!(status.note, None);

    vendor.route("/elsewhere/Vendor-App-x86_64.AppImage", Reply::file(build(&sandbox, "v1")));
    vendor.route(
        "/latest",
        Reply::redirect(&vendor.other_host("/elsewhere/Vendor-App-x86_64.AppImage")),
    );
    assert!(check(&app).available);
}

/// Issue #28, the first decision: the server swapped the file behind a name
/// that carries a version that is installed already. That is no update,
/// and the note says what happened.
#[test]
fn a_changed_etag_under_the_same_versioned_name_is_only_a_note() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let vendor = Vendor::start();
    let path = "/builds/Vendor-App-4.3.160-x86_64.AppImage";
    vendor.route(path, Reply::file(build(&sandbox, "v1")).header("ETag", "\"one\""));
    let app = install_from(&sandbox, &vendor.url(path));

    vendor.route(path, Reply::file(build(&sandbox, "v1b")).header("ETag", "\"two\""));
    let status = check(&app);
    assert!(!status.available && status.settled, "{status:?}");
    assert!(status.nothing_to_do());
    let note = status.note.unwrap();
    assert!(note.contains("Vendor-App-4.3.160-x86_64.AppImage"), "{note}");
    assert!(note.contains("without a new version"), "{note}");
}

/// A server that refuses HEAD is asked for its first byte instead, which
/// says the same about the file.
#[test]
fn a_server_that_refuses_head_is_asked_for_one_byte() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let vendor = Vendor::start();
    let path = "/get/vendor-app.AppImage";
    let body = build(&sandbox, "v1");
    let length = body.len() as u64;
    vendor.route(path, Reply { refuses_head: true, ..Reply::file(body) }.header("ETag", "\"one\""));
    let app = install_from(&sandbox, &vendor.url(path));
    assert_eq!(record(&app).length, Some(length));

    let before = vendor.seen().len();
    let status = check(&app);
    assert!(status.nothing_to_do() && status.settled, "{status:?}");
    let asked: Vec<(String, Option<String>)> =
        vendor.seen_since(before).into_iter().map(|seen| (seen.method, seen.range)).collect();
    assert_eq!(
        asked,
        vec![("HEAD".to_string(), None), ("GET".to_string(), Some("bytes=0-0".to_string()))]
    );
}

/// A link to a version that is gone often ends at a page saying so, with a
/// 404 or with a 200. Either is an error that says where the link ended.
#[test]
fn a_link_that_ends_at_a_page_is_an_error_that_says_where() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let vendor = Vendor::start();
    let path = "/builds/Vendor-App-1.0.0-x86_64.AppImage";
    vendor.route(path, Reply::file(build(&sandbox, "v1")));
    vendor.route("/latest", Reply::redirect(&vendor.url(path)));
    let app = install_from(&sandbox, &vendor.url("/latest"));
    let entry = read(&app.desktop_entry_path);

    vendor.route("/latest", Reply::redirect(&vendor.url("/not-found")));
    let error = update::check(&app).unwrap_err().to_string();
    assert!(error.contains("404"), "{error}");
    assert!(error.contains(&vendor.url("/not-found")), "{error}");

    vendor.route("/not-found", Reply::page(200));
    let error = update::check(&app).unwrap_err().to_string();
    assert!(error.contains("web page"), "{error}");
    assert!(error.contains(&vendor.url("/not-found")), "{error}");

    assert_eq!(read(&app.desktop_entry_path), entry);
}

/// A record belongs to the file it was taken of. Once the installed file is
/// another one, it says nothing about it, and what the check falls back on
/// shows it was not used: without one, only the date is left.
#[test]
fn a_record_of_another_file_is_not_used() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let vendor = Vendor::start();
    let path = "/downloads/vendor-app-latest-linux.AppImage";
    vendor.route(
        path,
        Reply::file(build(&sandbox, "v1"))
            .header("ETag", "\"one\"")
            .header("Last-Modified", LAST_MODIFIED),
    );
    let app = install_from(&sandbox, &vendor.url(path));
    let status = check(&app);
    assert!(status.settled && status.note.is_none(), "{status:?}");

    std::fs::write(&app.appimage_path, build(&sandbox, "by hand")).unwrap();
    let status = check(&app);
    assert!(status.note.as_deref().unwrap_or("").contains("judged by date"), "{status:?}");
}

/// A server that sends no `ETag`, no date and no length leaves nothing to
/// compare. The update downloads the file, and keeps the installed one when
/// it is the same file, which it then records.
#[test]
fn a_server_that_says_nothing_gets_a_download_that_keeps_the_same_file() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let vendor = Vendor::start();
    let body = build(&sandbox, "v1");
    vendor.route("/download", Reply { chunked: true, ..Reply::file(body.clone()) });
    let app = install_from(&sandbox, &vendor.url("/download"));
    let installed = std::fs::metadata(&app.appimage_path).unwrap().modified().unwrap();

    let status = check(&app);
    assert!(!status.available && !status.settled, "{status:?}");
    assert!(status.note.as_deref().unwrap().contains("re-downloads"));
    assert!(!status.nothing_to_do());

    let outcome = update::update(&sandbox.paths, &app, None).unwrap();
    assert_eq!(outcome.path, UpdatePath::Unchanged { bytes: body.len() as u64 });
    assert_eq!(outcome.backup_path, None);
    assert_eq!(std::fs::metadata(&app.appimage_path).unwrap().modified().unwrap(), installed);
    assert!(update::leftovers(&sandbox.paths, "vendor-app").is_empty());
    assert_eq!(record(&app).path, "/download");
}

/// Bitwarden: the link redirects to a GitHub release, which redirects to a
/// signed URL that names nothing. The name comes from the URL before it, or
/// from `Content-Disposition` when the server sends one.
#[test]
fn the_name_comes_from_where_the_server_gives_it() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let vendor = Vendor::start();
    vendor.route(
        "/blob/26191cb1",
        Reply::file(build(&sandbox, "v1")).header(
            "Content-Disposition",
            "attachment; filename=Vendor-App-2026.9.1-x86_64.AppImage",
        ),
    );
    vendor.route("/latest", Reply::redirect(&vendor.url("/blob/26191cb1?sig=abc&se=2026")));
    let app = install_from(&sandbox, &vendor.url("/latest"));
    assert_eq!(app.version.as_deref(), Some("2026.9.1"));
    let recorded = record(&app);
    assert_eq!(recorded.path, "/blob/26191cb1");
    assert_eq!(recorded.name.as_deref(), Some("Vendor-App-2026.9.1-x86_64.AppImage"));
    assert!(check(&app).nothing_to_do());
}

/// The install from a vendor link records what the server said, the update
/// source is the link as given, and the entry holds the record whole.
#[test]
fn the_record_lives_in_the_entry_next_to_the_stamp() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let vendor = Vendor::start();
    vendor.route(
        "/builds/Vendor%20App%201.0.0.AppImage",
        Reply::file(build(&sandbox, "v1")).header("ETag", "W/\"a b;c\""),
    );
    let app = install_from(&sandbox, &vendor.url("/builds/Vendor%20App%201.0.0.AppImage"));
    let entry = DesktopEntry::read(&app.desktop_entry_path).unwrap();
    let value = entry.get(KEY_REMOTE).unwrap();
    assert_eq!(value.split(' ').count(), 6, "{value}");
    let (sha1, recorded) = Remote::parse_record(value).unwrap();
    assert_eq!(sha1, appimg_core::zsync::sha1_file(Path::new(&app.appimage_path)).unwrap());
    assert_eq!(recorded.etag.as_deref(), Some("W/\"a b;c\""));
    assert_eq!(recorded.name.as_deref(), Some("Vendor App 1.0.0.AppImage"));
    assert_eq!(recorded.path, "/builds/Vendor%20App%201.0.0.AppImage");
}
