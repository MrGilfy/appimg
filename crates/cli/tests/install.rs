//! `appimg install` as a user runs it, against a local HTTP server only. No
//! test in here ever talks to a real host or touches the real `$HOME`.

use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

/// A server that answers one request with this body, a 200 and a
/// Content-Length that matches it, and hangs up. Joining the thread says
/// whether the whole response went out.
fn serve_once(body: Vec<u8>) -> (String, thread::JoinHandle<bool>) {
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
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: \
             close\r\n\r\n",
            body.len()
        );
        let sent = stream.write_all(&[head.as_bytes(), &body].concat()).is_ok();
        let _ = stream.shutdown(Shutdown::Write);
        sent
    });
    (url, handle)
}

/// Every file below `root`, relative to it, sorted.
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    collect(root, root, &mut found);
    found.sort();
    found
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else {
            out.push(path.strip_prefix(root).unwrap().to_path_buf());
        }
    }
}

/// An error page sent with a 200 and a Content-Length that matches it gets
/// through every check on the transfer. With `--yes` nothing asks before an
/// AppImage that does not extract is installed anyway, so the download
/// itself has to be refused, before any of it is written into place.
#[test]
fn installing_a_url_that_serves_no_appimage_installs_nothing() {
    let sandbox = tempfile::Builder::new().prefix("appimg-test-").tempdir().unwrap();
    let home = sandbox.path().join("home");
    let data_home = sandbox.path().join("data");
    let tmp = sandbox.path().join("tmp");
    for dir in [&home, &data_home, &tmp] {
        fs::create_dir_all(dir).unwrap();
    }

    let page = b"<!DOCTYPE html>\n<html><body>Something went wrong</body></html>\n".to_vec();
    let (url, server) = serve_once(page);

    let output = Command::new(env!("CARGO_BIN_EXE_appimg"))
        .args(["--yes", "--no-color", "install", &url])
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &data_home)
        .env("XDG_CONFIG_HOME", sandbox.path().join("config"))
        .env("TMPDIR", &tmp)
        .env_remove("APPIMG_DIR")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The whole page arrived, at the length the server announced for it.
    // Nothing about the transfer gave it away, only what it was.
    assert!(server.join().unwrap());
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains(&url), "{stderr}");
    assert!(stderr.contains("not an AppImage"), "{stderr}");

    // No binary, no desktop entry, no icon, and no download left behind.
    assert_eq!(walk(&data_home), Vec::<PathBuf>::new());
    assert_eq!(walk(&tmp), Vec::<PathBuf>::new());
}
