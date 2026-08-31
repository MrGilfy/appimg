//! Download and URL-based update, against a local HTTP server only. No test
//! in here ever talks to a real host.

mod common;

use std::io::Cursor;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use appimg_core::install::InstallRequest;
use appimg_core::{download, install, list, update};

use common::{read, walk, FakeAppImage, Sandbox};

/// A one-file HTTP server on a random port. The body can be swapped between
/// requests, which is how the update tests offer a newer version.
struct Server {
    address: SocketAddr,
    body: Arc<Mutex<Vec<u8>>>,
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
        let served = Arc::clone(&body);
        let (stop, stopped) = channel();

        let handle = thread::spawn(move || loop {
            match server.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(Some(request)) => {
                    let bytes = served.lock().unwrap().clone();
                    let length = bytes.len();
                    let _ = request.respond(tiny_http::Response::new(
                        tiny_http::StatusCode(200),
                        Vec::new(),
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

        Self { address, body, stop, handle: Some(handle) }
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

#[test]
fn a_url_is_recognised_and_names_its_file() {
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
    let sandbox = Sandbox::new();
    let built = FakeAppImage::new("Fake App").marker("v1").build(&sandbox.root, "build.AppImage");
    let server = Server::start(std::fs::read(&built).unwrap());
    let url = server.url("Fake_App-1.0.0.AppImage");

    let downloaded = sandbox.downloads.join(download::file_name_from_url(&url));
    download::to_file(&url, &downloaded, None).unwrap();

    let info = common::inspect(&downloaded, None);
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
    let sandbox = Sandbox::new();
    let built = FakeAppImage::new("Fake App").marker("v1").build(&sandbox.root, "build.AppImage");
    let url = {
        let server = Server::start(std::fs::read(&built).unwrap());
        let url = server.url("Fake_App-1.0.0.AppImage");
        let downloaded = sandbox.downloads.join("Fake_App-1.0.0.AppImage");
        download::to_file(&url, &downloaded, None).unwrap();
        let info = common::inspect(&downloaded, None);
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
