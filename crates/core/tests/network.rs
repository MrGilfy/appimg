//! Download and URL-based update, against a local HTTP server only. No test
//! in here ever talks to a real host.

mod common;

use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use appimg_core::desktop_entry::{DesktopEntry, KEY_RELEASE, KEY_UPDATE_INFO};
use appimg_core::digest::Verified;
use appimg_core::install::InstallRequest;
use appimg_core::list::InstalledApp;
use appimg_core::{download, install, list, metadata, update, zsync};

use common::{read, walk, with_unsquashfs_stand_in, FakeAppImage, Sandbox};

/// The path a request was made to, and the `Range` header it carried.
type Asked = (String, Option<String>);

/// A one-file HTTP server on a random port. The body can be swapped between
/// requests, which is how the update tests offer a newer version. Ranged
/// requests are answered with the range that was asked for, so a client that
/// only wants the first few kilobytes gets no more than that. A path given a
/// body of its own with [`Server::route`] is answered with that one instead,
/// which is how a test serves a GitHub release and its files side by side.
struct Server {
    address: SocketAddr,
    body: Arc<Mutex<Vec<u8>>>,
    routes: Arc<Mutex<HashMap<String, Vec<u8>>>>,
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
        let routes = Arc::new(Mutex::new(HashMap::new()));
        let routed: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::clone(&routes);
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
                    let path = request.url().to_string();
                    let route = path.split('?').next().unwrap_or(&path);
                    let whole = match routed.lock().unwrap().get(route) {
                        Some(body) => body.clone(),
                        None => offered.lock().unwrap().clone(),
                    };
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

        Self { address, body, routes, served, asked, from, stop, handle: Some(handle) }
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

    /// Answers requests for `path`, whatever their query, with `body`.
    fn route(&self, path: &str, body: Vec<u8>) {
        self.routes.lock().unwrap().insert(format!("/{path}"), body);
    }

    /// Where the server is, without a path: what `APPIMG_GITHUB_API` takes.
    fn base(&self) -> String {
        format!("http://{}", self.address)
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

/// A server that answers one request with exactly these bytes and hangs up.
/// Whatever the response says about its own length, the connection ends
/// where the bytes do, which is what a server that dies mid-transfer does.
fn answer_once_and_hang_up(response: Vec<u8>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let url = format!("http://{}/App.AppImage", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // The request is read to its end first: a socket closed with unread
        // bytes in it resets the connection, and a reset is an error the
        // client would report whatever the body looked like.
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
            request.push(byte[0]);
        }
        let _ = stream.write_all(&response);
        let _ = stream.shutdown(Shutdown::Write);
    });
    (url, handle)
}

/// One chunk of a `Transfer-Encoding: chunked` body: its size in hex, the
/// bytes, and the line break after them.
fn chunk(data: &[u8]) -> Vec<u8> {
    [format!("{:x}\r\n", data.len()).as_bytes(), data, b"\r\n"].concat()
}

const CHUNKED_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";

/// The empty chunk that says a chunked body is complete.
const LAST_CHUNK: &[u8] = b"0\r\n\r\n";

/// A chunked body announces no length, so a server that dies partway leaves
/// nothing to compare the bytes against. What gives it away is the empty
/// chunk that never arrives, and ureq treats a body without it as an error
/// rather than a short file, wherever the cut falls.
#[test]
fn a_chunked_download_that_is_cut_off_is_an_error_not_a_short_file() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let first = vec![b'a'; 4096];
    let second = vec![b'b'; 4096];

    // The same body finished properly arrives whole, so the failures below
    // come from the cut and not from how the chunks are written.
    let complete = [CHUNKED_HEAD, &chunk(&first), &chunk(&second), LAST_CHUNK].concat();
    let (url, server) = answer_once_and_hang_up(complete);
    let dest = sandbox.downloads.join("Complete.AppImage");
    assert_eq!(download::to_file(&url, &dest, None).unwrap(), 8192);
    server.join().unwrap();

    let cut_off = [
        ("between two chunks", [CHUNKED_HEAD, &chunk(&first)].concat()),
        ("inside a chunk", [CHUNKED_HEAD, &chunk(&first), &chunk(&second)[..1000]].concat()),
    ];
    for (place, response) in cut_off {
        let (url, server) = answer_once_and_hang_up(response);
        let dest = sandbox.downloads.join("App.AppImage");
        let result = download::to_file(&url, &dest, None);
        server.join().unwrap();

        assert!(result.is_err(), "cut off {place}: {result:?}");
        assert!(!dest.exists(), "cut off {place}");
        assert!(!dest.with_extension("part").exists(), "cut off {place}");
    }
}

/// The head of a response that says nothing about how long the body is: no
/// Content-Length and no chunked encoding, so the body ends wherever the
/// connection does.
const UNSIZED_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n";

/// Where [`elf_with_squashfs`] puts the payload: after the 64-byte ELF
/// header and the one section header that follows it.
const PAYLOAD_OFFSET: usize = 128;

/// An AppImage as far as its length goes: a 64-bit ELF header whose section
/// table, one empty entry, ends where the payload starts, then a squashfs
/// 4.0 superblock that says the filesystem is `bytes_used` long, that many
/// bytes, and zeros up to the next 4 KiB the way mksquashfs pads.
fn elf_with_squashfs(bytes_used: usize) -> Vec<u8> {
    let mut out = b"\x7fELF\x02\x01\x01".to_vec();
    out.resize(16, 0);
    out.extend_from_slice(&2u16.to_le_bytes()); // e_type
    out.extend_from_slice(&62u16.to_le_bytes()); // e_machine
    out.extend_from_slice(&1u32.to_le_bytes()); // e_version
    out.extend_from_slice(&0u64.to_le_bytes()); // e_entry
    out.extend_from_slice(&0u64.to_le_bytes()); // e_phoff
    out.extend_from_slice(&64u64.to_le_bytes()); // e_shoff
    out.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    out.extend_from_slice(&64u16.to_le_bytes()); // e_ehsize
    out.extend_from_slice(&0u16.to_le_bytes()); // e_phentsize
    out.extend_from_slice(&0u16.to_le_bytes()); // e_phnum
    out.extend_from_slice(&64u16.to_le_bytes()); // e_shentsize
    out.extend_from_slice(&1u16.to_le_bytes()); // e_shnum
    out.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx
    out.resize(PAYLOAD_OFFSET, 0); // the null section header

    // struct squashfs_super_block, 96 bytes, little-endian.
    out.extend_from_slice(b"hsqs");
    out.extend_from_slice(&1u32.to_le_bytes()); // inodes
    out.extend_from_slice(&0u32.to_le_bytes()); // mkfs_time
    out.extend_from_slice(&131_072u32.to_le_bytes()); // block_size
    out.extend_from_slice(&0u32.to_le_bytes()); // fragments
    out.extend_from_slice(&1u16.to_le_bytes()); // compression
    out.extend_from_slice(&17u16.to_le_bytes()); // block_log
    out.extend_from_slice(&0u16.to_le_bytes()); // flags
    out.extend_from_slice(&1u16.to_le_bytes()); // no_ids
    out.extend_from_slice(&4u16.to_le_bytes()); // s_major
    out.extend_from_slice(&0u16.to_le_bytes()); // s_minor
    out.extend_from_slice(&0u64.to_le_bytes()); // root_inode
    out.extend_from_slice(&(bytes_used as u64).to_le_bytes()); // bytes_used
    out.extend_from_slice(&[0xff; 48]); // six table offsets, never read

    out.extend(noise(12, PAYLOAD_OFFSET + bytes_used - out.len()));
    out.resize(out.len().next_multiple_of(4096), 0);
    out
}

/// Serves `body` with no length, the way a server that hangs up early does,
/// and checks that the download is refused as cut short, naming the URL and
/// both lengths, and that nothing is left behind.
fn assert_cut_short(sandbox: &Sandbox, body: &[u8], minimum: usize, place: &str) {
    let (url, server) = answer_once_and_hang_up([UNSIZED_HEAD, body].concat());
    let dest = sandbox.downloads.join("App.AppImage");
    let result = download::appimage_to_file(&url, &dest, None);
    server.join().unwrap();

    let error = match result {
        Ok(bytes) => panic!("cut off {place}: kept {bytes} bytes"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains(&url), "cut off {place}: {error}");
    assert!(error.contains(&format!("at least {minimum} bytes")), "cut off {place}: {error}");
    assert!(error.contains(&format!("sent {}", body.len())), "cut off {place}: {error}");
    assert!(walk(&sandbox.downloads).is_empty(), "cut off {place}");
}

/// A server that sends no length and hangs up early leaves a file whose
/// front is intact, so it starts with an ELF header like any AppImage. The
/// squashfs superblock in that front still says how long the file has to be.
#[test]
fn a_download_shorter_than_its_squashfs_payload_is_refused() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let file = elf_with_squashfs(10_000);
    let minimum = PAYLOAD_OFFSET + 10_000;

    for (place, length) in
        [("after the superblock", PAYLOAD_OFFSET + 96), ("one byte short", minimum - 1)]
    {
        assert_cut_short(&sandbox, &file[..length], minimum, place);
    }
}

/// A complete ELF contains its own section table, so a file that ends
/// before the table its header describes was cut off, whatever its payload.
#[test]
fn a_download_that_ends_before_its_section_table_is_refused() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let file = elf_with_squashfs(10_000);

    // The header is whole in both, the section table behind it is not.
    for (place, length) in
        [("after the header", 64), ("inside the section table", PAYLOAD_OFFSET - 1)]
    {
        assert_cut_short(&sandbox, &file[..length], PAYLOAD_OFFSET, place);
    }
}

/// Behind its section table a complete AppImage carries a squashfs
/// superblock of 96 bytes or an ISO 9660 image of far more, so a file that
/// ends less than 48 bytes behind it was cut off, whichever it carries.
#[test]
fn a_download_that_ends_right_behind_its_section_table_is_refused() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let squashfs = elf_with_squashfs(10_000);
    let mut other = squashfs.clone();
    other[PAYLOAD_OFFSET..PAYLOAD_OFFSET + 4].copy_from_slice(&[0; 4]);

    for (payload, file) in [("squashfs", &squashfs), ("no squashfs", &other)] {
        for (place, length) in
            [("at the payload", PAYLOAD_OFFSET), ("47 bytes in", PAYLOAD_OFFSET + 47)]
        {
            let place = format!("{place}, {payload}");
            assert_cut_short(&sandbox, &file[..length], PAYLOAD_OFFSET + 48, &place);
        }
    }
}

/// The superblock gives a floor, not a length. mksquashfs pads its output to
/// a multiple of 4 KiB, and with `-nopad` it does not.
#[test]
fn a_download_as_long_as_its_squashfs_payload_or_longer_is_kept() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let file = elf_with_squashfs(10_000);
    assert!(file.len() > PAYLOAD_OFFSET + 10_000);

    for (name, length) in [("padded", file.len()), ("unpadded", PAYLOAD_OFFSET + 10_000)] {
        let (url, server) = answer_once_and_hang_up([UNSIZED_HEAD, &file[..length]].concat());
        let dest = sandbox.downloads.join(format!("{name}.AppImage"));
        let result = download::appimage_to_file(&url, &dest, None);
        server.join().unwrap();

        assert_eq!(result.unwrap(), length as u64, "{name}");
        assert_eq!(std::fs::read(&dest).unwrap(), &file[..length], "{name}");
    }
}

/// What the front of a file cannot vouch for is not held against it. A type
/// 1 AppImage carries an ISO 9660 image where a type 2 carries its squashfs,
/// with no superblock to take a length from, and the header of a 32-bit ELF
/// is not read at all.
#[test]
fn a_download_whose_front_says_nothing_about_its_length_is_kept() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let mut no_squashfs = elf_with_squashfs(10_000);
    no_squashfs[PAYLOAD_OFFSET..PAYLOAD_OFFSET + 4].copy_from_slice(&[0; 4]);
    // Read as a 64-bit header, this one ends before its section table.
    let mut elf32 = elf_with_squashfs(10_000);
    elf32[4] = 1;

    for (name, file, length) in
        [("no squashfs", &no_squashfs, PAYLOAD_OFFSET + 5_000), ("32-bit", &elf32, 100)]
    {
        let (url, server) = answer_once_and_hang_up([UNSIZED_HEAD, &file[..length]].concat());
        let dest = sandbox.downloads.join(format!("{name}.AppImage"));
        let result = download::appimage_to_file(&url, &dest, None);
        server.join().unwrap();

        assert_eq!(result.unwrap(), length as u64, "{name}");
        assert_eq!(std::fs::read(&dest).unwrap(), &file[..length], "{name}");
    }
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
    // A download only replaces the installed file if it starts with an ELF
    // header, so this one does.
    let newer = FakeAppImage::new("Fake App")
        .marker("v2")
        .icon_sizes(&[64])
        .elf()
        .build(&sandbox.root, "build2.AppImage");
    server.serve(std::fs::read(&newer).unwrap());

    let result =
        with_unsquashfs_stand_in(&sandbox, || update::update(&sandbox.paths, &app, None)).unwrap();
    assert!(read(&result.appimage_path).contains("v2"));
    assert!(read(result.backup_path.as_ref().unwrap()).contains("v1"));
    assert!(walk(&sandbox.paths.icons_root).contains(&"64x64/apps/fake-app.png".into()));
    assert!(!sandbox.paths.appimage_dir.join("fake-app.AppImage.new").exists());

    update::confirm(&sandbox.paths, "fake-app").unwrap();
    assert!(!update::backup_path(&sandbox.paths, "fake-app").exists());
}

/// What the issue was about: an AppImage installed from a local file that
/// has since been deleted. It used to show "none" and could not be updated.
/// Given an update source, it updates from there, without a reinstall.
#[test]
fn an_app_installed_from_a_deleted_file_updates_from_the_source_it_is_given() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let local = FakeAppImage::new("Fake App")
        .marker("v1")
        .build(&sandbox.downloads, "Fake_App-1.0.0.AppImage");
    let info = metadata::inspect(&local, None).unwrap();
    install::install(
        &sandbox.paths,
        &InstallRequest::from_info(&local, &local.to_string_lossy(), &info),
    )
    .unwrap();
    std::fs::remove_file(&local).unwrap();

    let app = list::find(&sandbox.paths, "fake-app").unwrap();
    assert_eq!(update::source_for(&app), update::UpdateSource::Manual);

    let newer = FakeAppImage::new("Fake App")
        .marker("v2")
        .icon_sizes(&[64])
        .elf()
        .build(&sandbox.root, "build2.AppImage");
    let server = Server::start(std::fs::read(&newer).unwrap());
    let url = server.url("Fake_App-latest.AppImage");
    assert!(update::set_update_source(&app, Some(&url)).unwrap());

    let app = list::find(&sandbox.paths, "fake-app").unwrap();
    assert_eq!(update::source_for(&app), update::UpdateSource::DirectUrl { url: url.clone() });
    // Where it came from is still recorded, as history.
    assert_eq!(app.origin.as_deref(), Some(&*local.to_string_lossy()));

    let result =
        with_unsquashfs_stand_in(&sandbox, || update::update(&sandbox.paths, &app, None)).unwrap();
    assert!(read(&result.appimage_path).contains("v2"));
    assert!(read(result.backup_path.as_ref().unwrap()).contains("v1"));
    update::confirm(&sandbox.paths, "fake-app").unwrap();
}

/// Installs a build from `server` the way `appimg install <url>` does,
/// under a name of the user's choosing and with edits of the user's own.
fn install_edited_from(sandbox: &Sandbox, server: &Server) -> InstalledApp {
    let url = server.url("Fake_App.AppImage");
    let downloaded = sandbox.downloads.join(download::file_name_from_url(&url));
    download::to_file(&url, &downloaded, None).unwrap();
    let info = metadata::inspect(&downloaded, None).unwrap();
    let mut request = InstallRequest::from_info(&downloaded, &url, &info);
    request.name = "My Renamed App".to_string();
    request.categories = vec!["Graphics".to_string()];
    request.extra_args = vec!["--enable-something".to_string()];
    request.version = Some("1.0.0".to_string());
    let outcome = install::install(&sandbox.paths, &request).unwrap();
    assert_eq!(outcome.slug, "my-renamed-app");
    list::find(&sandbox.paths, "my-renamed-app").unwrap()
}

#[test]
fn an_update_keeps_manual_edits() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let built = FakeAppImage::new("Fake App").marker("v1").build(&sandbox.root, "build.AppImage");
    let server = Server::start(std::fs::read(&built).unwrap());
    let app = install_edited_from(&sandbox, &server);
    assert!(matches!(update::source_for(&app), update::UpdateSource::DirectUrl { .. }));

    let newer = FakeAppImage::new("Fake App")
        .marker("v2")
        .icon_sizes(&[128])
        .elf()
        .build(&sandbox.root, "build2.AppImage");
    server.serve(std::fs::read(&newer).unwrap());

    // A release recorded from before goes once an update comes from
    // somewhere else: it no longer says what the file is.
    let entry_path = sandbox.paths.desktop_entry_path("my-renamed-app");
    let mut entry = DesktopEntry::read(&entry_path).unwrap();
    entry.set(KEY_RELEASE, "github:someone/else@v1.0.0");
    entry.write(&entry_path).unwrap();

    let result =
        with_unsquashfs_stand_in(&sandbox, || update::update(&sandbox.paths, &app, None)).unwrap();
    assert!(read(&result.appimage_path).contains("v2"));
    assert_eq!(DesktopEntry::read(&entry_path).unwrap().get(KEY_RELEASE), None);
    let backup = result.backup_path.clone().unwrap();
    assert!(backup.is_file());
    assert!(read(&backup).contains("v1"));

    let entry = DesktopEntry::read(&sandbox.paths.desktop_entry_path("my-renamed-app")).unwrap();
    assert_eq!(entry.get("Name"), Some("My Renamed App"));
    assert_eq!(entry.categories(), vec!["Graphics"]);
    assert!(entry.get("Exec").unwrap().contains("--enable-something"));

    // Icons come from the new version.
    let icons = walk(&sandbox.paths.icons_root);
    assert!(icons.contains(&"128x128/apps/my-renamed-app.png".into()), "{icons:?}");

    // Whatever else an update left next to the AppImage goes with the
    // backup: `appimageupdatetool` left these when an older appimg fell back
    // to it, and nothing else ever cleans them up.
    let zs_old = sandbox.paths.appimage_dir.join("my-renamed-app.AppImage.zs-old");
    std::fs::write(&zs_old, "the previous version, a second time").unwrap();

    update::confirm(&sandbox.paths, "my-renamed-app").unwrap();
    assert!(!backup.exists());
    assert!(!zs_old.exists());
    assert!(update::leftovers(&sandbox.paths, "my-renamed-app").is_empty());
}

#[test]
fn a_rollback_puts_the_previous_version_back() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let built = FakeAppImage::new("Fake App").marker("v1").build(&sandbox.root, "build.AppImage");
    let server = Server::start(std::fs::read(&built).unwrap());
    let app = install_edited_from(&sandbox, &server);

    let newer =
        FakeAppImage::new("Fake App").marker("v2").elf().build(&sandbox.root, "b2.AppImage");
    server.serve(std::fs::read(&newer).unwrap());
    let result =
        with_unsquashfs_stand_in(&sandbox, || update::update(&sandbox.paths, &app, None)).unwrap();
    assert!(read(&result.appimage_path).contains("v2"));

    update::rollback(&sandbox.paths, "my-renamed-app").unwrap();
    assert!(read(&result.appimage_path).contains("v1"));
    assert!(common::is_executable(&result.appimage_path));
    assert!(!update::backup_path(&sandbox.paths, "my-renamed-app").exists());
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

/// A transfer that arrives whole is not yet an AppImage. An error page sent
/// with a 200 and a Content-Length that matches it gets through every check
/// on the transfer, and has to be stopped before it replaces anything.
#[test]
fn a_download_that_is_not_an_appimage_never_replaces_the_installed_one() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let built = FakeAppImage::new("Fake App").marker("v1").build(&sandbox.root, "build.AppImage");
    let server = Server::start(std::fs::read(&built).unwrap());
    let url = server.url("Fake_App-1.0.0.AppImage");

    let downloaded = sandbox.downloads.join(download::file_name_from_url(&url));
    download::to_file(&url, &downloaded, None).unwrap();
    let info = metadata::inspect(&downloaded, None).unwrap();
    install::install(&sandbox.paths, &InstallRequest::from_info(&downloaded, &url, &info)).unwrap();
    let app = list::find(&sandbox.paths, "fake-app").unwrap();
    let entry = read(&app.desktop_entry_path);

    let page = b"<!DOCTYPE html>\n<html><body>Something went wrong</body></html>\n".to_vec();
    server.serve(page.clone());
    let error = update::update(&sandbox.paths, &app, None).unwrap_err().to_string();

    // The whole page arrived, at the length the server announced for it.
    // Nothing about the transfer gave it away, only what it was.
    assert_eq!(server.served().last(), Some(&page.len()));
    assert!(error.contains(&url), "{error}");
    assert!(error.contains("not an AppImage"), "{error}");

    assert!(read(&app.appimage_path).contains("v1"));
    assert_eq!(read(&app.desktop_entry_path), entry);
    assert!(!sandbox.paths.appimage_dir.join("fake-app.AppImage.new").exists());
    assert!(!update::backup_path(&sandbox.paths, "fake-app").exists());
}

/// `update --check` on a zsync source: the header of the zsync file is
/// enough, and one ranged request gets it.
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
    let status = update::check(&app).unwrap();
    assert!(!status.available);
    assert_eq!(status.note, None);
    assert_eq!(status.latest_version.as_deref(), Some("1.0.0"));
    assert_eq!(status.current_version.as_deref(), Some("1.0.0"));
    // A check reads, it never writes.
    assert_eq!(walk(&sandbox.paths.data_home), before);

    // A bigger file is an update, and the sizes are reported as they are.
    server.serve(zsync_file("Fake_App-2.0.0.AppImage", length + 4096, &"0".repeat(40)));
    let status = update::check(&app).unwrap();
    assert!(status.available);
    assert_eq!(status.latest_version.as_deref(), Some("2.0.0"));
    let note = status.note.unwrap();
    assert!(length < 1024, "the fixture stays small enough for the sizes below");
    assert!(note.contains(&format!("{:.1} KB", (length + 4096) as f64 / 1024.0)), "{note}");
    assert!(note.contains(&format!("{length} B")), "{note}");

    // The same size but a different checksum is an update as well.
    server.serve(zsync_file("Fake_App-1.0.1.AppImage", length, &"0".repeat(40)));
    let status = update::check(&app).unwrap();
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
/// front, so a delta update has exactly one block to fetch. Both start with
/// an ELF header, as an AppImage a full download accepts has to, so they
/// only extract through the stand-in for `unsquashfs`. `payload_path`
/// decides how the AppImage is served: a plain name is answered with ranges,
/// `plain/...` is a server that ignores them, `short/...` one whose ranges
/// all stop a byte early while a plain request gets the whole file.
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
            .elf()
            .build(&sandbox.root, "App.AppImage")
    };

    let one = sandbox.root.join("build1.AppImage");
    std::fs::copy(build("1.0.0"), &one).unwrap();

    let info = with_unsquashfs_stand_in(sandbox, || metadata::inspect(&one, None)).unwrap();
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

    let outcome =
        with_unsquashfs_stand_in(&sandbox, || update::update(&sandbox.paths, &delta.app, None))
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
    let files = walk(&sandbox.paths.data_home);

    // The server answers the range with the right number of bytes, at the
    // right offset, with a 206 like any other: they are simply not the bytes
    // the zsync file describes. Nothing before the whole-file checksum can
    // tell the difference. The ELF header is left alone, so the whole file
    // that is downloaded instead looks like an AppImage as well, and only
    // the checksum can tell the difference there either.
    let mut wrong = delta.payload.clone();
    wrong[16..2048].fill(b'#');
    delta.payload_server.serve(wrong);

    let error = update::update(&sandbox.paths, &delta.app, None).unwrap_err().to_string();

    // The checksum caught the delta, the whole file was tried instead, and
    // the checksum caught that too. The message says what did not match.
    let expected = zsync::sha1_file(&sandbox.root.join("build2.AppImage")).unwrap();
    assert!(error.contains("the assembled file is checksummed"), "{error}");
    assert!(error.contains("downloading the whole file instead failed too"), "{error}");
    assert!(error.contains("the downloaded file is checksummed"), "{error}");
    assert!(error.contains(&expected), "{error}");

    // Nothing was installed, nothing was staged, nothing was backed up.
    assert_eq!(std::fs::read(&delta.installed).unwrap(), before);
    assert_eq!(walk(&sandbox.paths.data_home), files);
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

    let outcome = update::update(&sandbox.paths, &delta.app, None).unwrap();

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

/// What replaced the fallback to `appimageupdatetool`: every range the
/// delta asks for comes back a byte short, so the delta fails. The complete
/// file the zsync file names is downloaded instead, held to the same
/// checksum, and installed like any other update.
#[test]
fn a_delta_whose_ranges_fail_ends_in_a_correct_full_download() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let delta = delta_fixture(&sandbox, "short/Fake_App-2.0.0.AppImage");

    let outcome =
        with_unsquashfs_stand_in(&sandbox, || update::update(&sandbox.paths, &delta.app, None))
            .unwrap();

    // The installed file is the one the zsync file described, byte for
    // byte, and the previous version is the backup a rollback uses.
    assert_eq!(std::fs::read(&outcome.appimage_path).unwrap(), delta.payload);
    assert_eq!(std::fs::read(outcome.backup_path.as_ref().unwrap()).unwrap(), delta.v1);
    assert!(!sandbox.paths.appimage_dir.join("fake-app.AppImage.new").exists());
    assert_eq!(outcome.to_version.as_deref(), Some("2.0.0"));

    // The delta was tried first: a range, which came back short. Then the
    // whole file, without one.
    let asked = delta.payload_server.asked();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert!(asked[0].1.is_some(), "{asked:?}");
    assert_eq!(asked[1], ("/short/Fake_App-2.0.0.AppImage".to_string(), None));
    assert_eq!(delta.payload_server.served()[1], delta.payload.len());

    // And the update says so, with the reason the delta gave up.
    match &outcome.path {
        update::UpdatePath::DeltaFailed { reason, bytes } => {
            assert_eq!(*bytes, delta.payload.len() as u64);
            assert!(reason.contains("/short/Fake_App-2.0.0.AppImage"), "{reason}");
        }
        other => panic!("a full download after the delta should have been reported: {other:?}"),
    }
    let described = outcome.path.describe();
    assert!(described.contains("the delta failed"), "{described}");
    assert!(described.contains("downloaded the whole file instead"), "{described}");
}

/// Without a zsync file to read there is no delta to try, and no URL of a
/// complete file to fall back on either. The update fails and changes
/// nothing.
#[test]
fn a_zsync_file_that_cannot_be_read_changes_nothing() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let delta = delta_fixture(&sandbox, "Fake_App-2.0.0.AppImage");
    let broken = Server::start(b"not a zsync file at all".to_vec());
    let mut entry = DesktopEntry::read(&delta.app.desktop_entry_path).unwrap();
    entry.set(KEY_UPDATE_INFO, format!("zsync|{}", broken.url("Fake_App.AppImage.zsync")));
    entry.write(&delta.app.desktop_entry_path).unwrap();
    let app = list::find(&sandbox.paths, "fake-app").unwrap();
    let files = walk(&sandbox.paths.data_home);

    let error = update::update(&sandbox.paths, &app, None).unwrap_err().to_string();

    assert!(error.contains("Fake_App.AppImage.zsync"), "{error}");
    assert_eq!(std::fs::read(&delta.installed).unwrap(), delta.v1);
    assert_eq!(walk(&sandbox.paths.data_home), files);
    // Not a byte of the AppImage was asked for.
    assert!(delta.payload_server.asked().is_empty());
}

/// Runs something with the GitHub API at `server`, which serves the release
/// JSON a test wrote. The tests run one at a time, which is what makes this
/// safe.
fn with_github_api<T>(server: &Server, run: impl FnOnce() -> T) -> T {
    let previous = std::env::var_os("APPIMG_GITHUB_API");
    std::env::set_var("APPIMG_GITHUB_API", server.base());
    let result = run();
    match previous {
        Some(base) => std::env::set_var("APPIMG_GITHUB_API", base),
        None => std::env::remove_var("APPIMG_GITHUB_API"),
    }
    result
}

/// A release as the GitHub API returns one, with these assets: a download
/// URL each, and the `digest` GitHub publishes for it, `None` for an asset
/// from before it published any.
fn release_json(tag: &str, assets: &[(&str, Option<&str>)]) -> String {
    let assets: Vec<String> = assets
        .iter()
        .map(|(url, digest)| {
            let name = url.rsplit('/').next().unwrap();
            let digest = digest.map_or("null".to_string(), |d| format!("\"sha256:{d}\""));
            format!(
                "{{\"url\":\"https://api.github.com/assets/1\",\"name\":\"{name}\",\
                 \"uploader\":{{\"login\":\"bot\"}},\"size\":1,\"digest\":{digest},\
                 \"browser_download_url\":\"{url}\"}}"
            )
        })
        .collect();
    format!(
        "{{\"assets_url\":\"https://api.github.com/assets\",\"tag_name\":\"{tag}\",\
         \"target_commitish\":\"main\",\"draft\":false,\"prerelease\":false,\
         \"published_at\":\"2026-10-01T10:00:00Z\",\"assets\":[{}],\"body\":\"notes\"}}",
        assets.join(",")
    )
}

/// An application installed from a local file that updates from
/// `github:o/r`, and a server that offers release v2.0.0 of it, whose one
/// AppImage is `v2`. What GitHub publishes for that AppImage is up to the
/// test.
struct GitHubFixture {
    app: InstalledApp,
    v1: Vec<u8>,
    v2: Vec<u8>,
    /// The SHA-256 of `v2`, which a release that is honest publishes.
    v2_sha256: String,
    asset_url: String,
    server: Server,
}

impl GitHubFixture {
    fn new(sandbox: &Sandbox) -> Self {
        let one = FakeAppImage::new("Fake App")
            .marker("v1")
            .build(&sandbox.root, "Fake_App-1.0.0.AppImage");
        let info = metadata::inspect(&one, None).unwrap();
        let request = InstallRequest::from_info(&one, &one.to_string_lossy(), &info);
        install::install(&sandbox.paths, &request).unwrap();
        let installed = list::find(&sandbox.paths, "fake-app").unwrap();
        update::set_update_source(&installed, Some("github:o/r")).unwrap();

        // A download only replaces the installed file if it starts with an
        // ELF header, so this one does.
        let two = FakeAppImage::new("Fake App")
            .marker("v2")
            .elf()
            .build(&sandbox.root, "Fake_App-2.0.0.AppImage");
        let server = Server::start(Vec::new());
        let asset_url = server.url("download/v2.0.0/Fake_App-2.0.0.AppImage");
        server.route("download/v2.0.0/Fake_App-2.0.0.AppImage", std::fs::read(&two).unwrap());

        Self {
            app: list::find(&sandbox.paths, "fake-app").unwrap(),
            v1: std::fs::read(&one).unwrap(),
            v2: std::fs::read(&two).unwrap(),
            v2_sha256: appimg_core::digest::sha256_file(&two).unwrap(),
            asset_url,
            server,
        }
    }

    /// Publishes release v2.0.0 with this digest for its AppImage.
    fn publish(&self, digest: Option<&str>) {
        let release = release_json("v2.0.0", &[(&self.asset_url, digest)]);
        self.server.route("repos/o/r/releases", format!("[{release}]").into_bytes());
    }

    fn update(&self, sandbox: &Sandbox) -> appimg_core::Result<update::UpdateOutcome> {
        with_github_api(&self.server, || update::update(&sandbox.paths, &self.app, None))
    }
}

#[test]
fn an_update_out_of_a_github_release_is_checked_against_its_digest() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let fixture = GitHubFixture::new(&sandbox);
    fixture.publish(Some(&fixture.v2_sha256));

    let outcome = fixture.update(&sandbox).unwrap();

    assert_eq!(outcome.digest, Some(Verified::Matches(fixture.v2_sha256.clone())));
    assert_eq!(outcome.digest.unwrap().describe(), "sha256 matches the digest GitHub publishes");
    assert_eq!(std::fs::read(&outcome.appimage_path).unwrap(), fixture.v2);
    assert_eq!(std::fs::read(outcome.backup_path.as_ref().unwrap()).unwrap(), fixture.v1);
    assert!(!sandbox.paths.appimage_dir.join("fake-app.AppImage.new").exists());
}

/// What the check is for: the release says one thing, the file that
/// arrived is another. It is refused, and nothing of it stays anywhere.
#[test]
fn a_github_download_that_does_not_match_its_digest_replaces_nothing() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let fixture = GitHubFixture::new(&sandbox);
    let elsewhere = "e3".repeat(32);
    fixture.publish(Some(&elsewhere));

    let entry = read(&fixture.app.desktop_entry_path);
    let files = walk(&sandbox.paths.data_home);

    let error = fixture.update(&sandbox).unwrap_err();

    // The whole file did arrive: the download itself was fine.
    let asked: Vec<String> = fixture.server.asked().into_iter().map(|(path, _)| path).collect();
    assert!(asked.contains(&"/download/v2.0.0/Fake_App-2.0.0.AppImage".to_string()), "{asked:?}");
    assert!(fixture.server.served().contains(&fixture.v2.len()));

    // The error names the file and both digests.
    assert!(matches!(error, appimg_core::Error::DigestMismatch { .. }), "{error:?}");
    let message = error.to_string();
    assert!(message.contains(&fixture.asset_url), "{message}");
    assert!(message.contains(&fixture.v2_sha256), "{message}");
    assert!(message.contains(&elsewhere), "{message}");

    // Nothing was replaced, and nothing was left behind: no staged file, no
    // backup, not one file more or less than before, and the same entry.
    assert_eq!(std::fs::read(&fixture.app.appimage_path).unwrap(), fixture.v1);
    assert_eq!(read(&fixture.app.desktop_entry_path), entry);
    assert_eq!(walk(&sandbox.paths.data_home), files);
    assert!(!sandbox.paths.appimage_dir.join("fake-app.AppImage.new").exists());
    assert!(!update::backup_path(&sandbox.paths, "fake-app").exists());
}

/// Releases from before GitHub published digests carry `null`. That is no
/// reason to refuse the update, only nothing to check it against, and the
/// update says so.
#[test]
fn a_github_release_without_a_digest_updates_and_says_it_was_not_checked() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let fixture = GitHubFixture::new(&sandbox);
    fixture.publish(None);

    let outcome = fixture.update(&sandbox).unwrap();

    assert_eq!(std::fs::read(&outcome.appimage_path).unwrap(), fixture.v2);
    let said = outcome.digest.as_ref().unwrap().describe();
    assert_eq!(said, "not checked, GitHub publishes no digest for this file");
}

/// A source that is no GitHub release has no digest to check, and says
/// nothing about one.
#[test]
fn an_update_out_of_no_release_says_nothing_about_a_digest() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let built = FakeAppImage::new("Fake App").marker("v1").build(&sandbox.root, "build.AppImage");
    let server = Server::start(std::fs::read(&built).unwrap());
    let url = server.url("Fake_App-1.0.0.AppImage");
    let downloaded = sandbox.downloads.join(download::file_name_from_url(&url));
    download::to_file(&url, &downloaded, None).unwrap();
    let info = metadata::inspect(&downloaded, None).unwrap();
    install::install(&sandbox.paths, &InstallRequest::from_info(&downloaded, &url, &info)).unwrap();

    let newer =
        FakeAppImage::new("Fake App").marker("v2").elf().build(&sandbox.root, "build2.AppImage");
    server.serve(std::fs::read(&newer).unwrap());
    let app = list::find(&sandbox.paths, "fake-app").unwrap();

    let outcome = update::update(&sandbox.paths, &app, None).unwrap();
    assert_eq!(outcome.digest, None);
}

/// A `gh-releases-zsync` application: v1 installed, and release v2.0.0 with
/// the AppImage and the zsync file that describes it, side by side as
/// `zsyncmake` leaves them, the AppImage named by a relative URL. The
/// release is served below `prefix`, which picks how the server answers,
/// see [`delta_fixture`].
struct GitHubDelta {
    app: InstalledApp,
    v1: Vec<u8>,
    payload: Vec<u8>,
    payload_sha256: String,
    asset_url: String,
    zsync_url: String,
    server: Server,
}

impl GitHubDelta {
    fn new(sandbox: &Sandbox, prefix: &str) -> Self {
        let filler: String =
            noise(4, 256 * 1024).into_iter().map(|byte| char::from(b'a' + byte % 26)).collect();
        let build = |version: &str| {
            FakeAppImage::new("Fake App")
                .key("X-AppImage-Version", version)
                .marker(&format!("{version}{filler}"))
                .elf()
                .build(&sandbox.root, "App.AppImage")
        };

        let one = sandbox.root.join("build1.AppImage");
        std::fs::copy(build("1.0.0"), &one).unwrap();
        let info = with_unsquashfs_stand_in(sandbox, || metadata::inspect(&one, None)).unwrap();
        let request = InstallRequest::from_info(&one, &one.to_string_lossy(), &info);
        let installed = install::install(&sandbox.paths, &request).unwrap();

        let two = sandbox.root.join("build2.AppImage");
        std::fs::copy(build("2.0.0"), &two).unwrap();
        let payload = std::fs::read(&two).unwrap();

        let server = Server::start(Vec::new());
        let asset = format!("{prefix}download/v2.0.0/Fake_App-2.0.0.AppImage");
        let zsync = format!("{asset}.zsync");
        let control = control_file(
            "Fake_App-2.0.0.AppImage",
            "Fake_App-2.0.0.AppImage",
            &payload,
            2048,
            &zsync::sha1_file(&two).unwrap(),
        );
        server.route(&asset, payload.clone());
        server.route(&zsync, control);

        let mut entry = DesktopEntry::read(&installed.desktop_entry_path).unwrap();
        entry.set(KEY_UPDATE_INFO, "gh-releases-zsync|o|r|latest|Fake_App-*.AppImage.zsync");
        entry.write(&installed.desktop_entry_path).unwrap();

        Self {
            app: list::find(&sandbox.paths, "fake-app").unwrap(),
            v1: std::fs::read(&one).unwrap(),
            payload_sha256: appimg_core::digest::sha256_file(&two).unwrap(),
            payload,
            asset_url: server.url(&asset),
            zsync_url: server.url(&zsync),
            server,
        }
    }

    /// Publishes release v2.0.0 with this digest for its AppImage, and
    /// none for the zsync file, which GitHub hashes like any other asset
    /// but nothing here checks.
    fn publish(&self, digest: Option<&str>) {
        let release = release_json("v2.0.0", &[(&self.asset_url, digest), (&self.zsync_url, None)]);
        self.server.route("repos/o/r/releases", format!("[{release}]").into_bytes());
    }

    fn update(&self, sandbox: &Sandbox) -> appimg_core::Result<update::UpdateOutcome> {
        with_unsquashfs_stand_in(sandbox, || {
            with_github_api(&self.server, || update::update(&sandbox.paths, &self.app, None))
        })
    }
}

#[test]
fn a_delta_out_of_a_github_release_is_checked_against_its_digest_too() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let delta = GitHubDelta::new(&sandbox, "");
    delta.publish(Some(&delta.payload_sha256));

    let outcome = delta.update(&sandbox).unwrap();

    assert!(matches!(outcome.path, update::UpdatePath::Delta { .. }), "{:?}", outcome.path);
    assert_eq!(outcome.digest, Some(Verified::Matches(delta.payload_sha256.clone())));
    assert_eq!(std::fs::read(&outcome.appimage_path).unwrap(), delta.payload);
}

/// The zsync file and the bytes the server sends agree with each other, so
/// the zsync checksum passes. The release publishes a different digest for
/// the AppImage, and that is what refuses it.
#[test]
fn a_delta_that_does_not_match_its_digest_replaces_nothing() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let delta = GitHubDelta::new(&sandbox, "");
    let elsewhere = "5a".repeat(32);
    delta.publish(Some(&elsewhere));
    let files = walk(&sandbox.paths.data_home);
    let entry = read(&delta.app.desktop_entry_path);

    let error = delta.update(&sandbox).unwrap_err();

    let message = error.to_string();
    assert!(matches!(error, appimg_core::Error::DigestMismatch { .. }), "{message}");
    assert!(message.contains(&delta.payload_sha256) && message.contains(&elsewhere), "{message}");
    // The delta itself worked, the digest refused what it assembled.
    let asked = delta.server.asked();
    assert!(asked.iter().all(|(path, range)| !path.ends_with(".AppImage") || range.is_some()));

    assert_eq!(std::fs::read(&delta.app.appimage_path).unwrap(), delta.v1);
    assert_eq!(read(&delta.app.desktop_entry_path), entry);
    assert_eq!(walk(&sandbox.paths.data_home), files);
}

/// A full download after a failed delta goes the way every other update
/// goes, the digest check included: refused when the release publishes
/// another digest, installed when it publishes this one.
#[test]
fn a_full_download_after_a_failed_delta_is_checked_against_the_digest() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let delta = GitHubDelta::new(&sandbox, "short/");
    let files = walk(&sandbox.paths.data_home);

    delta.publish(Some(&"77".repeat(32)));
    let error = delta.update(&sandbox).unwrap_err();
    assert!(matches!(error, appimg_core::Error::DigestMismatch { .. }), "{error}");
    assert_eq!(std::fs::read(&delta.app.appimage_path).unwrap(), delta.v1);
    assert_eq!(walk(&sandbox.paths.data_home), files);

    delta.publish(Some(&delta.payload_sha256));
    let outcome = delta.update(&sandbox).unwrap();
    assert!(matches!(outcome.path, update::UpdatePath::DeltaFailed { .. }), "{:?}", outcome.path);
    assert_eq!(outcome.digest, Some(Verified::Matches(delta.payload_sha256.clone())));
    assert_eq!(std::fs::read(&outcome.appimage_path).unwrap(), delta.payload);
}

/// An install from a GitHub release download URL asks for that release,
/// once, and checks the downloaded file against what it publishes. The URL
/// is github.com's, which no test talks to: only the API is asked, and the
/// file was downloaded from the local server beforehand.
#[test]
fn a_download_from_a_github_release_url_is_checked_before_anything_installs_it() {
    let _serial = common::serial();
    let sandbox = Sandbox::new();
    let built = FakeAppImage::new("Fake App").elf().build(&sandbox.root, "build.AppImage");
    let bytes = std::fs::read(&built).unwrap();
    let sha256 = appimg_core::digest::sha256_file(&built).unwrap();
    let server = Server::start(bytes.clone());
    let github = "https://github.com/o/r/releases/download/v2.0.0/Fake_App-2.0.0.AppImage";
    let downloaded = sandbox.downloads.join("Fake_App-2.0.0.AppImage");

    let check = |digest: Option<&str>| {
        server.route(
            "repos/o/r/releases/tags/v2.0.0",
            release_json("v2.0.0", &[(github, digest)]).into_bytes(),
        );
        download::appimage_to_file(&server.url("Fake_App-2.0.0.AppImage"), &downloaded, None)
            .unwrap();
        with_github_api(&server, || install::verify_download(&downloaded, github))
    };

    let matched = check(Some(&sha256)).unwrap();
    assert_eq!(matched, Some(Verified::Matches(sha256.clone())));
    assert!(downloaded.exists());

    let unchecked = check(None).unwrap().unwrap();
    assert_eq!(unchecked.describe(), "not checked, GitHub publishes no digest for this file");
    assert!(downloaded.exists());

    let elsewhere = "0f".repeat(32);
    let message = check(Some(&elsewhere)).unwrap_err().to_string();
    assert!(message.contains(&sha256) && message.contains(&elsewhere), "{message}");
    assert!(message.contains(github), "{message}");
    // Nothing of it is left to install by accident.
    assert!(!downloaded.exists());
    assert_eq!(walk(&sandbox.paths.data_home), Vec::<PathBuf>::new());

    // One request for the release per check, to the tag the URL names.
    let api: Vec<String> = server
        .asked()
        .into_iter()
        .map(|(path, _)| path)
        .filter(|path| path.starts_with("/repos/"))
        .collect();
    assert_eq!(api, vec!["/repos/o/r/releases/tags/v2.0.0"; 3]);

    // A URL that is no release download is not asked about at all.
    let plain = server.url("Fake_App-2.0.0.AppImage");
    download::appimage_to_file(&plain, &downloaded, None).unwrap();
    assert_eq!(install::verify_download(&downloaded, &plain).unwrap(), None);
}
