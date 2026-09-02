//! Download and URL-based update, against a local HTTP server only. No test
//! in here ever talks to a real host.

mod common;

use std::io::Cursor;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use appimg_core::desktop_entry::{DesktopEntry, KEY_UPDATE_INFO};
use appimg_core::install::InstallRequest;
use appimg_core::list::InstalledApp;
use appimg_core::{download, install, list, metadata, update, zsync};

use common::{read, walk, FakeAppImage, Sandbox};

/// The path a request was made to, and the `Range` header it carried.
type Asked = (String, Option<String>);

/// A one-file HTTP server on a random port. The body can be swapped between
/// requests, which is how the update tests offer a newer version. Ranged
/// requests are answered with the range that was asked for, so a client that
/// only wants the first few kilobytes gets no more than that.
struct Server {
    address: SocketAddr,
    body: Arc<Mutex<Vec<u8>>>,
    /// How many bytes every request so far was answered with.
    served: Arc<Mutex<Vec<usize>>>,
    /// The path and `Range` header of every request so far.
    asked: Arc<Mutex<Vec<Asked>>>,
    /// The client address of every request so far. A reused connection
    /// keeps its port, so this counts the connections that were opened.
    from: Arc<Mutex<Vec<String>>>,
    stop: Sender<()>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn start(body: Vec<u8>) -> Self {
        let port =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let server = tiny_http::Server::http(address).unwrap();
        let body = Arc::new(Mutex::new(body));
        let offered = Arc::clone(&body);
        let served = Arc::new(Mutex::new(Vec::new()));
        let counted = Arc::clone(&served);
        let (stop, stopped) = channel();

        let asked = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&asked);

        let from = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&from);

        let handle = thread::spawn(move || loop {
            match server.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(Some(request)) => {
                    let whole = offered.lock().unwrap().clone();
                    let path = request.url().to_string();
                    let header = request
                        .headers()
                        .iter()
                        .find(|header| header.field.equiv("Range"))
                        .map(|header| header.value.as_str().to_string());
                    let wanted = header.as_deref().and_then(byte_range);
                    recorded.lock().unwrap().push((path.clone(), header));
                    let peer = request.remote_addr().map(|a| a.to_string()).unwrap_or_default();
                    seen.lock().unwrap().push(peer);

                    let (status, bytes, extra) = if let Some(rest) = path.strip_prefix("/moved/") {
                        // What a GitHub release asset does: the download URL
                        // sends the client somewhere else.
                        let location = tiny_http::Header::from_bytes(
                            &b"Location"[..],
                            format!("http://{address}/{rest}").as_bytes(),
                        )
                        .unwrap();
                        (302, Vec::new(), vec![location])
                    } else if path.starts_with("/gone/") {
                        // A file that does not hold the range asked for.
                        (416, Vec::new(), Vec::new())
                    } else if path.starts_with("/askew/") {
                        // A server that answers with a range of its own
                        // choosing: the bytes are real, they belong
                        // somewhere else in the file.
                        let (first, last) = wanted.unwrap_or((0, whole.len() - 1));
                        let last = last.min(whole.len().saturating_sub(1));
                        let range = format!("bytes {}-{}/{}", first + 4096, last, whole.len());
                        let header =
                            tiny_http::Header::from_bytes(&b"Content-Range"[..], range.as_bytes())
                                .unwrap();
                        (206, whole[first..=last].to_vec(), vec![header])
                    } else if path.starts_with("/plain/") {
                        // A server that ignores the range and sends the lot.
                        (200, whole, Vec::new())
                    } else {
                        match wanted {
                            // A range that covers the whole file is answered
                            // the way a plain request would be.
                            Some((0, last)) if last + 1 >= whole.len() => (200, whole, Vec::new()),
                            Some((first, last)) => {
                                let last = last.min(whole.len().saturating_sub(1));
                                let range = format!("bytes {first}-{last}/{}", whole.len());
                                let header = tiny_http::Header::from_bytes(
                                    &b"Content-Range"[..],
                                    range.as_bytes(),
                                )
                                .unwrap();
                                let mut piece = whole[first..=last].to_vec();
                                if path.starts_with("/short/") {
                                    // A server that stops early.
                                    piece.pop();
                                }
                                (206, piece, vec![header])
                            }
                            None => (200, whole, Vec::new()),
                        }
                    };

                    let length = bytes.len();
                    counted.lock().unwrap().push(length);
                    let _ = request.respond(tiny_http::Response::new(
                        tiny_http::StatusCode(status),
                        extra,
                        Cursor::new(bytes),
                        Some(length),
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

        Self { address, body, served, asked, from, stop, handle: Some(handle) }
    }

    /// The size of every response so far.
    fn served(&self) -> Vec<usize> {
        self.served.lock().unwrap().clone()
    }

    /// The path and `Range` header of every request so far.
    fn asked(&self) -> Vec<Asked> {
        self.asked.lock().unwrap().clone()
    }

    /// How many connections the requests so far arrived on.
    fn connections(&self) -> usize {
        let mut ports = self.from.lock().unwrap().clone();
        ports.sort();
        ports.dedup();
        ports.len()
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}/{path}", self.address)
    }

    fn serve(&self, body: Vec<u8>) {
        *self.body.lock().unwrap() = body;
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The bytes a `Range: bytes=4096-8191` header asks for, both ends
/// included.
fn byte_range(value: &str) -> Option<(usize, usize)> {
    let (unit, range) = value.split_once('=')?;
    if unit.trim() != "bytes" {
        return None;
    }
    let (first, last) = range.split_once('-')?;
    Some((first.trim().parse().ok()?, last.trim().parse().ok()?))
}

/// A zsync file as `zsyncmake` writes one: the text header a check reads, a
/// blank line, then block checksums it never looks at.
fn zsync_file(filename: &str, length: u64, sha1: &str) -> Vec<u8> {
    let mut out = format!(
        "zsync: 0.6.2\n\
         Filename: {filename}\n\
         MTime: Sat, 01 Aug 2026 10:00:00 +0000\n\
         Blocksize: 2048\n\
         Length: {length}\n\
         Hash-Lengths: 2,2,4\n\
         URL: {filename}\n\
         SHA-1: {sha1}\n\
         \n"
    )
    .into_bytes();

    // Enough block checksums that a client reading the whole file would be
    // obvious in what the server sent.
    out.extend((0..32 * 1024).map(|i| (i % 251) as u8));
    out
}

#[test]
fn a_url_is_recognised_and_names_its_file() {
    let _serial = common::serial();
    assert!(download::is_url("https://example.com/App.AppImage"));
    assert!(download::is_url("http://example.com/App.AppImage"));
    assert!(!download::is_url("/home/someone/App.AppImage"));
    assert!(!download::is_url("./App.AppImage"));
    assert_eq!(
        download::file_name_from_url("https://example.com/d/App-1.2.AppImage?x=1"),
        "App-1.2.AppImage"
    );
}

#[test]
fn downloading_reports_progress_and_writes_the_file() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let server = Server::start(b"an appimage payload".to_vec());
    let dest = sandbox.downloads.join("App.AppImage");

    let seen = Mutex::new(Vec::new());
    let bytes = download::to_file(
        &server.url("App.AppImage"),
        &dest,
        Some(&mut |done, total| seen.lock().unwrap().push((done, total))),
    )
    .unwrap();

    assert_eq!(bytes, 19);
    assert_eq!(read(&dest), "an appimage payload");
    let seen = seen.into_inner().unwrap();
    assert!(!seen.is_empty());
    assert_eq!(seen.last().unwrap().0, 19);
    assert_eq!(seen.last().unwrap().1, Some(19));
}

#[test]
fn a_download_that_fails_leaves_no_half_file() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let dest = sandbox.downloads.join("App.AppImage");
    // Nothing listens on this port.
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();

    let result = download::to_file(&format!("http://127.0.0.1:{port}/App.AppImage"), &dest, None);
    assert!(result.is_err());
    assert!(!dest.exists());
}

#[test]
fn install_from_a_url_records_it_and_updates_from_it() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let built = FakeAppImage::new("Fake App").marker("v1").build(&sandbox.root, "build.AppImage");
    let server = Server::start(std::fs::read(&built).unwrap());
    let url = server.url("Fake_App-1.0.0.AppImage");

    let downloaded = sandbox.downloads.join(download::file_name_from_url(&url));
    download::to_file(&url, &downloaded, None).unwrap();

    let info = metadata::inspect(&downloaded, None).unwrap();
    let outcome =
        install::install(&sandbox.paths, &InstallRequest::from_info(&downloaded, &url, &info))
            .unwrap();
    assert_eq!(outcome.slug, "fake-app");

    let app = list::find(&sandbox.paths, "fake-app").unwrap();
    assert_eq!(app.origin.as_deref(), Some(url.as_str()));
    assert_eq!(update::source_for(&app), update::UpdateSource::DirectUrl { url: url.clone() });

    // `--check` says only that a re-download is what an update means here.
    let before = walk(&sandbox.paths.data_home);
    let status = update::check(&app).unwrap();
    assert!(status.note.is_some());
    assert_eq!(walk(&sandbox.paths.data_home), before);

    // The server now offers a different build, so the update picks it up.
    let newer = FakeAppImage::new("Fake App")
        .marker("v2")
        .icon_sizes(&[64])
        .build(&sandbox.root, "build2.AppImage");
    server.serve(std::fs::read(&newer).unwrap());

    let result = update::update(&sandbox.paths, &app, None).unwrap();
    assert!(read(&result.appimage_path).contains("v2"));
    assert!(read(result.backup_path.as_ref().unwrap()).contains("v1"));
    assert!(walk(&sandbox.paths.icons_root).contains(&"64x64/apps/fake-app.png".into()));
    assert!(!sandbox.paths.appimage_dir.join("fake-app.AppImage.new").exists());

    update::confirm(&sandbox.paths, "fake-app").unwrap();
    assert!(!update::backup_path(&sandbox.paths, "fake-app").exists());
}

#[test]
fn a_failing_update_leaves_the_installed_version_alone() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let built = FakeAppImage::new("Fake App").marker("v1").build(&sandbox.root, "build.AppImage");
    let url = {
        let server = Server::start(std::fs::read(&built).unwrap());
        let url = server.url("Fake_App-1.0.0.AppImage");
        let downloaded = sandbox.downloads.join("Fake_App-1.0.0.AppImage");
        download::to_file(&url, &downloaded, None).unwrap();
        let info = metadata::inspect(&downloaded, None).unwrap();
        install::install(&sandbox.paths, &InstallRequest::from_info(&downloaded, &url, &info))
            .unwrap();
        url
        // The server goes away here, so the update below has nowhere to go.
    };

    let app = list::find(&sandbox.paths, "fake-app").unwrap();
    assert_eq!(app.origin.as_deref(), Some(url.as_str()));
    assert!(update::update(&sandbox.paths, &app, None).is_err());

    assert!(read(&app.appimage_path).contains("v1"));
    assert!(!sandbox.paths.appimage_dir.join("fake-app.AppImage.new").exists());
}

/// `update --check` on a zsync source, without `appimageupdatetool`: the
/// header of the zsync file is enough, and one ranged request gets it.
#[test]
fn checking_a_zsync_source_reads_the_header_and_nothing_else() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let source = FakeAppImage::new("Fake App").build(&sandbox.downloads, "Fake_App-1.0.0.AppImage");
    let info = metadata::inspect(&source, None).unwrap();
    let request = InstallRequest::from_info(&source, &source.to_string_lossy(), &info);
    let installed = install::install(&sandbox.paths, &request).unwrap();

    let length = std::fs::metadata(&installed.appimage_path).unwrap().len();
    let sha1 = zsync::sha1_file(&installed.appimage_path).unwrap();

    // The same file the server offers: nothing to update.
    let server = Server::start(zsync_file("Fake_App-1.0.0.AppImage", length, &sha1));
    let mut entry = DesktopEntry::read(&installed.desktop_entry_path).unwrap();
    entry.set(KEY_UPDATE_INFO, format!("zsync|{}", server.url("Fake_App.AppImage.zsync")));
    entry.write(&installed.desktop_entry_path).unwrap();

    let app = list::find(&sandbox.paths, "fake-app").unwrap();
    assert_eq!(
        update::source_for(&app),
        update::UpdateSource::Zsync {
            update_info: format!("zsync|{}", server.url("Fake_App.AppImage.zsync")),
        }
    );

    let before = walk(&sandbox.paths.data_home);
    let status = without_appimageupdatetool(|| update::check(&app).unwrap());
    assert!(!status.available);
    assert_eq!(status.note, None);
    assert_eq!(status.latest_version.as_deref(), Some("1.0.0"));
    assert_eq!(status.current_version.as_deref(), Some("1.0.0"));
    // A check reads, it never writes.
    assert_eq!(walk(&sandbox.paths.data_home), before);

    // A bigger file is an update, and the sizes are reported as they are.
    server.serve(zsync_file("Fake_App-2.0.0.AppImage", length + 4096, &"0".repeat(40)));
    let status = without_appimageupdatetool(|| update::check(&app).unwrap());
    assert!(status.available);
    assert_eq!(status.latest_version.as_deref(), Some("2.0.0"));
    let note = status.note.unwrap();
    assert!(length < 1024, "the fixture stays small enough for the sizes below");
    assert!(note.contains(&format!("{:.1} KB", (length + 4096) as f64 / 1024.0)), "{note}");
    assert!(note.contains(&format!("{length} B")), "{note}");

    // The same size but a different checksum is an update as well.
    server.serve(zsync_file("Fake_App-1.0.1.AppImage", length, &"0".repeat(40)));
    let status = without_appimageupdatetool(|| update::check(&app).unwrap());
    assert!(status.available);
    assert_eq!(status.latest_version.as_deref(), Some("1.0.1"));
    assert!(status.note.unwrap().contains("checksum"));

    // Three checks, three responses, none of them the whole zsync file.
    let served = server.served();
    assert_eq!(served.len(), 3);
    assert!(served.iter().all(|bytes| *bytes <= 8 * 1024), "{served:?}");
}

#[test]
fn a_zsync_url_that_serves_something_else_is_an_error() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let source = FakeAppImage::new("Fake App").build(&sandbox.downloads, "Fake_App-1.0.0.AppImage");
    let info = metadata::inspect(&source, None).unwrap();
    let request = InstallRequest::from_info(&source, &source.to_string_lossy(), &info);
    let installed = install::install(&sandbox.paths, &request).unwrap();

    let server = Server::start(b"<html><body>404 not found</body></html>\n\n".to_vec());
    let mut entry = DesktopEntry::read(&installed.desktop_entry_path).unwrap();
    entry.set(KEY_UPDATE_INFO, format!("zsync|{}", server.url("gone.zsync")));
    entry.write(&installed.desktop_entry_path).unwrap();

    let app = list::find(&sandbox.paths, "fake-app").unwrap();
    let error = update::check(&app).unwrap_err().to_string();
    assert!(error.contains("zsync"), "{error}");
}

/// Runs a check with an empty `PATH`, so no external tool can be found. The
/// tests run one at a time, which is what makes this safe.
fn without_appimageupdatetool<T>(check: impl FnOnce() -> T) -> T {
    let previous = std::env::var_os("PATH");
    std::env::set_var("PATH", "");
    let result = check();
    match previous {
        Some(path) => std::env::set_var("PATH", path),
        None => std::env::remove_var("PATH"),
    }
    result
}

/// Bytes that do not repeat, so a block of them only matches where it
/// belongs.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    (0..len as u64)
        .map(|at| {
            let mut x = seed ^ at.wrapping_mul(0x9e37_79b9_7f4a_7c15);
            x ^= x >> 30;
            x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
            x ^= x >> 27;
            x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
            (x >> 31) as u8
        })
        .collect()
}

/// A control file for these bytes as `zsyncmake` writes one: two bytes of
/// rolling checksum and four of MD4 per block.
fn control_file(name: &str, url: &str, target: &[u8], blocksize: usize, sha1: &str) -> Vec<u8> {
    let mut out = format!(
        "zsync: 0.6.2\n\
         Filename: {name}\n\
         Blocksize: {blocksize}\n\
         Length: {}\n\
         Hash-Lengths: 2,2,4\n\
         URL: {url}\n\
         SHA-1: {sha1}\n\
         \n",
        target.len(),
    )
    .into_bytes();

    for start in (0..target.len()).step_by(blocksize) {
        let mut block = vec![0u8; blocksize];
        let end = (start + blocksize).min(target.len());
        block[..end - start].copy_from_slice(&target[start..end]);

        out.extend_from_slice(&zsync::Rsum::of(&block).value().to_be_bytes()[2..]);
        out.extend_from_slice(&zsync::md4(&block)[..4]);
    }
    out
}

/// A target of 128 blocks, and a local file that is that target with two
/// runs of five blocks rewritten: ten blocks have to be fetched, in two
/// runs far enough apart not to be merged.
fn target_and_seed() -> (Vec<u8>, tempfile::NamedTempFile) {
    let blocksize = 2048;
    let target = noise(1, 128 * blocksize);

    let mut seed = target.clone();
    seed[20 * blocksize..25 * blocksize].fill(0x5a);
    seed[89 * blocksize..94 * blocksize].fill(0xa5);

    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), &seed).unwrap();
    (target, file)
}

#[test]
fn fetches_only_the_ranges_a_scan_did_not_find() {
    let _serial = common::serial();
    let (target, seed) = target_and_seed();
    let control = zsync::parse_control(&control_file(
        "App.AppImage",
        "App.AppImage",
        &target,
        2048,
        &"0".repeat(40),
    ))
    .unwrap();

    let map = zsync::scan_file(&control, seed.path()).unwrap();
    assert_eq!(map.matched(), 118, "ten blocks were rewritten");

    let server = Server::start(target.clone());
    let mut assembled = std::fs::read(seed.path()).unwrap();
    let report = zsync::fetch_missing(&server.url("App.AppImage"), &control, &map, |at, bytes| {
        assembled[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
        Ok(())
    })
    .unwrap();

    // Two runs, two requests, ten blocks of bytes and nothing else.
    assert_eq!(report.requests, 2);
    assert_eq!(report.received, 10 * 2048);
    assert!(!report.whole_file);
    assert!(report.received < target.len() as u64 / 10);

    // The ranges the server was asked for, and only those.
    let asked: Vec<Option<String>> = server.asked().into_iter().map(|(_, range)| range).collect();
    assert_eq!(
        asked,
        vec![Some("bytes=40960-51199".to_string()), Some("bytes=182272-192511".to_string()),]
    );

    // Both of them down one connection: a range that opened its own would
    // pay for a handshake it does not need.
    assert_eq!(server.connections(), 1);

    // What arrived is what was missing, in the right places.
    assert_eq!(assembled, target);
}

#[test]
fn a_seed_that_holds_everything_asks_for_nothing() {
    let _serial = common::serial();
    let target = noise(2, 16 * 2048 + 77);
    let control = zsync::parse_control(&control_file(
        "App.AppImage",
        "App.AppImage",
        &target,
        2048,
        &"0".repeat(40),
    ))
    .unwrap();

    let seed = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(seed.path(), &target).unwrap();
    let map = zsync::scan_file(&control, seed.path()).unwrap();
    assert!(map.is_complete());

    let server = Server::start(target);
    let report = zsync::fetch_missing(&server.url("App.AppImage"), &control, &map, |_, _| {
        panic!("nothing should have been fetched")
    })
    .unwrap();

    assert_eq!(report, zsync::FetchReport { received: 0, requests: 0, whole_file: false });
    assert!(server.asked().is_empty());
}

#[test]
fn follows_a_redirect_and_still_asks_for_the_range() {
    let _serial = common::serial();
    let (target, seed) = target_and_seed();
    let control = zsync::parse_control(&control_file(
        "App.AppImage",
        "App.AppImage",
        &target,
        2048,
        &"0".repeat(40),
    ))
    .unwrap();
    let map = zsync::scan_file(&control, seed.path()).unwrap();

    let server = Server::start(target.clone());
    let mut assembled = std::fs::read(seed.path()).unwrap();
    let report =
        zsync::fetch_missing(&server.url("moved/App.AppImage"), &control, &map, |at, bytes| {
            assembled[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
            Ok(())
        })
        .unwrap();

    assert_eq!(report.received, 10 * 2048);
    assert!(!report.whole_file);
    assert_eq!(assembled, target);

    // The first range was asked for twice, once at the URL that moved and
    // once where it moved to, and the second request still carried the
    // range. The second range went straight to where the first one landed,
    // so it cost one request, not two.
    let asked = server.asked();
    assert_eq!(asked.len(), 3);
    assert_eq!(asked[0].0, "/moved/App.AppImage");
    assert_eq!(asked[1].0, "/App.AppImage");
    assert_eq!(asked[0].1, asked[1].1);
    assert_eq!(asked[1].1.as_deref(), Some("bytes=40960-51199"));
    assert_eq!(asked[2].0, "/App.AppImage");
    assert_eq!(asked[2].1.as_deref(), Some("bytes=182272-192511"));

    assert_eq!(server.connections(), 1);
}

#[test]
fn a_range_the_file_does_not_hold_is_an_error() {
    let _serial = common::serial();
    let (target, seed) = target_and_seed();
    let control = zsync::parse_control(&control_file(
        "App.AppImage",
        "App.AppImage",
        &target,
        2048,
        &"0".repeat(40),
    ))
    .unwrap();
    let map = zsync::scan_file(&control, seed.path()).unwrap();

    let server = Server::start(target);
    let error = zsync::fetch_missing(&server.url("gone/App.AppImage"), &control, &map, |_, _| {
        panic!("nothing should have been written")
    })
    .unwrap_err()
    .to_string();

    assert!(error.contains("no bytes 40960 to 51199"), "{error}");
}

#[test]
fn a_range_that_starts_somewhere_else_is_an_error() {
    let _serial = common::serial();
    let (target, seed) = target_and_seed();
    let control = zsync::parse_control(&control_file(
        "App.AppImage",
        "App.AppImage",
        &target,
        2048,
        &"0".repeat(40),
    ))
    .unwrap();
    let map = zsync::scan_file(&control, seed.path()).unwrap();

    let server = Server::start(target);
    let error = zsync::fetch_missing(&server.url("askew/App.AppImage"), &control, &map, |_, _| {
        panic!("bytes from the wrong place must never be written")
    })
    .unwrap_err()
    .to_string();

    assert!(error.contains("asked for byte 40960 onwards"), "{error}");
    assert!(error.contains("sent byte 45056 onwards"), "{error}");
}

#[test]
fn a_server_that_ignores_the_range_becomes_a_plain_download() {
    let _serial = common::serial();
    let (target, seed) = target_and_seed();
    let control = zsync::parse_control(&control_file(
        "App.AppImage",
        "App.AppImage",
        &target,
        2048,
        &"0".repeat(40),
    ))
    .unwrap();
    let map = zsync::scan_file(&control, seed.path()).unwrap();

    let server = Server::start(target.clone());
    let mut assembled = vec![0u8; target.len()];
    let report =
        zsync::fetch_missing(&server.url("plain/App.AppImage"), &control, &map, |at, bytes| {
            assembled[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
            Ok(())
        })
        .unwrap();

    // One request, the whole file, and the caller was handed all of it from
    // the first byte, so what it holds is complete either way.
    assert!(report.whole_file);
    assert_eq!(report.requests, 1);
    assert_eq!(report.received, target.len() as u64);
    assert_eq!(server.asked().len(), 1);
    assert_eq!(assembled, target);
}

#[test]
fn a_range_that_stops_early_is_an_error() {
    let _serial = common::serial();
    let (target, seed) = target_and_seed();
    let control = zsync::parse_control(&control_file(
        "App.AppImage",
        "App.AppImage",
        &target,
        2048,
        &"0".repeat(40),
    ))
    .unwrap();
    let map = zsync::scan_file(&control, seed.path()).unwrap();

    let server = Server::start(target);
    let error =
        zsync::fetch_missing(&server.url("short/App.AppImage"), &control, &map, |_, _| Ok(()))
            .unwrap_err()
            .to_string();

    assert!(error.contains("10240"), "{error}");
    assert!(error.contains("10239"), "{error}");
}

/// An installed version one, the version two that a zsync update should
/// arrive at, and the two servers it talks to: one holding the zsync file,
/// one holding the AppImage that file describes.
struct Delta {
    app: InstalledApp,
    installed: PathBuf,
    v1: Vec<u8>,
    payload: Vec<u8>,
    payload_server: Server,
    _zsync_server: Server,
}

/// Two builds of the same fake AppImage that differ in one byte near the
/// front, so a delta update has exactly one block to fetch. `payload_path`
/// decides how the AppImage is served: a plain name is answered with ranges,
/// `plain/...` is a server that ignores them.
fn delta_fixture(sandbox: &Sandbox, payload_path: &str) -> Delta {
    // A quarter of a megabyte of filler inside the runtime's comment line,
    // so the two builds share 127 blocks of 2048 bytes.
    let filler: String =
        noise(4, 256 * 1024).into_iter().map(|byte| char::from(b'a' + byte % 26)).collect();

    // Both builds are written under the same name, so the only difference
    // between the two files is the version in the runtime's comment: one
    // byte, in the first block. Version one is installed before version two
    // is built, because the second build replaces what the first extracts.
    let build = |version: &str| {
        FakeAppImage::new("Fake App")
            .key("X-AppImage-Version", version)
            .marker(&format!("{version}{filler}"))
            .build(&sandbox.root, "App.AppImage")
    };

    let one = sandbox.root.join("build1.AppImage");
    std::fs::copy(build("1.0.0"), &one).unwrap();

    let info = metadata::inspect(&one, None).unwrap();
    let request = InstallRequest::from_info(&one, &one.to_string_lossy(), &info);
    let installed = install::install(&sandbox.paths, &request).unwrap();

    let two = sandbox.root.join("build2.AppImage");
    std::fs::copy(build("2.0.0"), &two).unwrap();
    let payload = std::fs::read(&two).unwrap();
    let payload_server = Server::start(payload.clone());
    let control = control_file(
        "Fake_App-2.0.0.AppImage",
        &payload_server.url(payload_path),
        &payload,
        2048,
        &zsync::sha1_file(&two).unwrap(),
    );
    let zsync_server = Server::start(control);

    let mut entry = DesktopEntry::read(&installed.desktop_entry_path).unwrap();
    entry.set(KEY_UPDATE_INFO, format!("zsync|{}", zsync_server.url("Fake_App.AppImage.zsync")));
    entry.write(&installed.desktop_entry_path).unwrap();

    Delta {
        app: list::find(&sandbox.paths, "fake-app").unwrap(),
        installed: installed.appimage_path,
        v1: std::fs::read(&one).unwrap(),
        payload,
        payload_server,
        _zsync_server: zsync_server,
    }
}

#[test]
fn a_zsync_update_applies_the_delta_itself_and_reports_what_it_did() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let delta = delta_fixture(&sandbox, "Fake_App-2.0.0.AppImage");

    // The tool is there and refuses to work: if the native path had not
    // done this, there would be no update at all.
    let outcome = with_failing_appimageupdatetool(&sandbox, || {
        update::update(&sandbox.paths, &delta.app, None)
    })
    .unwrap();

    // The installed file is the one the zsync file described, byte for byte.
    assert_eq!(std::fs::read(&outcome.appimage_path).unwrap(), delta.payload);
    assert_eq!(std::fs::read(outcome.backup_path.as_ref().unwrap()).unwrap(), delta.v1);
    assert!(!sandbox.paths.appimage_dir.join("fake-app.AppImage.new").exists());
    assert_eq!(outcome.to_version.as_deref(), Some("2.0.0"));

    // One block differs between the two builds, so one block was fetched.
    match outcome.path {
        update::UpdatePath::Delta { blocks, reused, fetched, requests } => {
            assert_eq!(blocks, 129);
            assert_eq!(reused, 128);
            assert_eq!(fetched, 2048);
            assert_eq!(requests, 1);
        }
        ref other => panic!("the native path should have run: {other:?}"),
    }

    let described = outcome.path.describe();
    assert!(described.contains("reused 128 of 129 blocks"), "{described}");

    // And the server sent that one block and nothing else.
    assert_eq!(delta.payload_server.served(), vec![2048]);
    assert!(delta.payload.len() > 100 * 2048);
}

#[test]
fn bytes_that_do_not_assemble_into_the_right_file_are_thrown_away() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let delta = delta_fixture(&sandbox, "Fake_App-2.0.0.AppImage");
    let before = std::fs::read(&delta.installed).unwrap();

    // The server answers the range with the right number of bytes, at the
    // right offset, with a 206 like any other: they are simply not the bytes
    // the zsync file describes. Nothing before the whole-file checksum can
    // tell the difference.
    let mut wrong = delta.payload.clone();
    wrong[..2048].fill(b'#');
    delta.payload_server.serve(wrong);

    let error = with_failing_appimageupdatetool(&sandbox, || {
        update::update(&sandbox.paths, &delta.app, None)
    })
    .unwrap_err()
    .to_string();

    // The checksum caught it, and the message says what did not match.
    assert!(error.contains("checksummed"), "{error}");
    assert!(
        error.contains(&zsync::sha1_file(&sandbox.root.join("build2.AppImage")).unwrap()),
        "{error}"
    );
    // The fallback was tried, and it failed too, so nothing was installed.
    assert!(error.contains("appimageupdatetool could not take over"), "{error}");

    // Nothing was installed, nothing was staged, nothing was backed up.
    assert_eq!(std::fs::read(&delta.installed).unwrap(), before);
    assert!(!sandbox.paths.appimage_dir.join("fake-app.AppImage.new").exists());
    assert!(!update::backup_path(&sandbox.paths, "fake-app").exists());

    // And the entry still describes the version that is on disk.
    let app = list::find(&sandbox.paths, "fake-app").unwrap();
    assert_eq!(app.version.as_deref(), Some("1.0.0"));
}

#[test]
fn a_server_that_ignores_ranges_still_updates_and_says_so() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let delta = delta_fixture(&sandbox, "plain/Fake_App-2.0.0.AppImage");

    let outcome = with_failing_appimageupdatetool(&sandbox, || {
        update::update(&sandbox.paths, &delta.app, None)
    })
    .unwrap();

    assert_eq!(std::fs::read(&outcome.appimage_path).unwrap(), delta.payload);
    match outcome.path {
        update::UpdatePath::ZsyncWithoutRanges { bytes } => {
            assert_eq!(bytes, delta.payload.len() as u64);
        }
        ref other => panic!("a plain download should have been reported as one: {other:?}"),
    }
    let described = outcome.path.describe();
    assert!(described.contains("ignored the range requests"), "{described}");
}

#[test]
fn a_native_path_that_fails_hands_over_to_appimageupdatetool_and_says_so() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let delta = delta_fixture(&sandbox, "Fake_App-2.0.0.AppImage");

    // A zsync file appimg cannot read at all, so the native path is out.
    let broken = Server::start(b"not a zsync file at all".to_vec());
    let mut entry = DesktopEntry::read(&delta.app.desktop_entry_path).unwrap();
    entry.set(KEY_UPDATE_INFO, format!("zsync|{}", broken.url("Fake_App.AppImage.zsync")));
    entry.write(&delta.app.desktop_entry_path).unwrap();
    let app = list::find(&sandbox.paths, "fake-app").unwrap();

    // A stand-in for the tool that does what it does: replaces the file and
    // leaves the previous version behind as `.zs-old`.
    let tools = sandbox.root.join("tools");
    std::fs::create_dir_all(&tools).unwrap();
    let tool = tools.join("appimageupdatetool");
    std::fs::write(
        &tool,
        format!(
            "#!/bin/sh\ntarget=\"$2\"\ncp \"$target\" \"$target.zs-old\"\ncp '{}' \"$target\"\n",
            sandbox.root.join("build2.AppImage").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&tool, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let outcome =
        with_tools_on_path(&tools, || update::update(&sandbox.paths, &app, None)).unwrap();

    assert_eq!(std::fs::read(&outcome.appimage_path).unwrap(), delta.payload);
    // The copy the tool left behind became the backup a rollback uses.
    assert_eq!(std::fs::read(outcome.backup_path.as_ref().unwrap()).unwrap(), delta.v1);

    match &outcome.path {
        update::UpdatePath::ExternalTool { reason } => {
            assert!(reason.contains("zsync"), "{reason}");
        }
        other => panic!("the fallback should have been reported: {other:?}"),
    }
    let described = outcome.path.describe();
    assert!(described.contains("appimageupdatetool"), "{described}");
}

/// Runs something with an `appimageupdatetool` on `PATH` that always fails.
/// Whether the real one is installed on the machine running the tests is
/// then neither here nor there.
fn with_failing_appimageupdatetool<T>(sandbox: &Sandbox, run: impl FnOnce() -> T) -> T {
    let dir = sandbox.root.join("failing-tool");
    std::fs::create_dir_all(&dir).unwrap();
    let tool = dir.join("appimageupdatetool");
    std::fs::write(&tool, "#!/bin/sh\necho 'stand-in, not the real thing' >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&tool, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    with_tools_on_path(&dir, run)
}

/// Runs something with a directory prepended to `PATH`. The tests run one at
/// a time, which is what makes this safe.
fn with_tools_on_path<T>(dir: &Path, run: impl FnOnce() -> T) -> T {
    let previous = std::env::var_os("PATH");
    let mut path = dir.as_os_str().to_os_string();
    if let Some(existing) = &previous {
        path.push(":");
        path.push(existing);
    }
    std::env::set_var("PATH", path);
    let result = run();
    match previous {
        Some(path) => std::env::set_var("PATH", path),
        None => std::env::remove_var("PATH"),
    }
    result
}
