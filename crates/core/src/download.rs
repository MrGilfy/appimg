use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use ureq::config::RedirectAuthHeaders;
use ureq::ResponseExt;

use crate::archive;
use crate::digest::Verified;
use crate::elf;
use crate::error::{Error, Result};
use crate::fs_util::{self, MODE_EXEC};

pub const USER_AGENT: &str = concat!("appimg/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Progress reports during a download. `total` is `None` when the server
/// sends no content length.
pub type ProgressFn<'a> = &'a mut dyn FnMut(u64, Option<u64>);

pub fn is_url(candidate: &str) -> bool {
    candidate.starts_with("http://") || candidate.starts_with("https://")
}

/// The file name a URL suggests, falling back to something usable.
pub fn file_name_from_url(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let name = without_query.rsplit('/').find(|part| !part.is_empty()).unwrap_or("download");
    let name = percent_decode(name);
    if name.to_lowercase().ends_with(".appimage") {
        name
    } else {
        format!("{name}.AppImage")
    }
}

/// Downloads a URL to a local file and marks it executable. Existing files
/// are replaced only after the download finished.
pub fn to_file(url: &str, dest: &Path, progress: Option<ProgressFn<'_>>) -> Result<u64> {
    let response = agent()
        .get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| Error::Download(format!("{url}: {e}")))?;

    let total = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    let partial = dest.with_extension("part");
    if let Some(parent) = partial.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }

    let mut reader = response.into_body().into_reader();
    let mut file = File::create(&partial).map_err(|e| Error::io(&partial, e))?;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    let mut progress = progress;

    loop {
        let read = match std::io::Read::read(&mut reader, &mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&partial);
                return Err(Error::Download(format!("{url}: {e}")));
            }
        };
        if let Err(e) = file.write_all(&buffer[..read]) {
            let _ = std::fs::remove_file(&partial);
            return Err(Error::io(&partial, e));
        }
        written += read as u64;
        if let Some(report) = progress.as_mut() {
            report(written, total);
        }
    }

    file.flush().map_err(|e| Error::io(&partial, e))?;
    drop(file);

    if written == 0 {
        let _ = std::fs::remove_file(&partial);
        return Err(Error::Download(format!("{url}: the server sent an empty response")));
    }
    if let Some(expected) = total {
        if written != expected {
            let _ = std::fs::remove_file(&partial);
            return Err(Error::Download(format!(
                "{url}: expected {expected} bytes, got {written}"
            )));
        }
    }

    fs_util::set_mode(&partial, MODE_EXEC)?;
    std::fs::rename(&partial, dest).map_err(|e| Error::io(dest, e))?;
    Ok(written)
}

/// Downloads an AppImage the way [`to_file`] downloads anything, then makes
/// sure it is one. Both an install and an update from a URL come through
/// here, before the file goes anywhere permanent. A file that fails a check
/// is removed again before the error comes back.
pub fn appimage_to_file(url: &str, dest: &Path, progress: Option<ProgressFn<'_>>) -> Result<u64> {
    let bytes = to_file(url, dest, progress)?;
    check_appimage(url, dest, None)?;
    Ok(bytes)
}

/// What [`appimage_or_archive`] fetched.
#[derive(Debug)]
pub struct Fetched {
    /// What came over the wire.
    pub bytes: u64,
    /// What checking the download against the digest its release publishes
    /// found, when it came out of one.
    pub verified: Option<Verified>,
    /// The name of the AppImage inside the archive the server sent, when it
    /// sent one, for messages only.
    pub unpacked: Option<String>,
}

/// Downloads `url` and leaves a checked AppImage at `dest`, whether the
/// server sent one or an archive with one inside.
///
/// `verify` checks a file against what a release publishes for the
/// download, and removes it when it does not match. An archive goes
/// through it before anything is unpacked, as it arrived, and next to
/// `dest` with the extension `archive`, which is gone again afterwards
/// either way. Out of it comes the one AppImage, written to `dest` and to
/// nothing else, see [`archive::extract_appimage`]. From there both get the
/// checks every download gets, see [`appimage_to_file`].
pub fn appimage_or_archive(
    url: &str,
    dest: &Path,
    verify: &dyn Fn(&Path) -> Result<Option<Verified>>,
    progress: Option<ProgressFn<'_>>,
) -> Result<Fetched> {
    let bytes = to_file(url, dest, progress)?;
    if archive::Kind::of(dest).is_none() {
        check_appimage(url, dest, None)?;
        let verified = verify(dest)?;
        return Ok(Fetched { bytes, verified, unpacked: None });
    }

    let packed = dest.with_extension("archive");
    std::fs::rename(dest, &packed).map_err(|e| Error::io(&packed, e))?;
    let unpacked = verify(&packed).and_then(|verified| {
        let extracted = archive::extract_appimage(&packed, dest).map_err(|error| match error {
            Error::Archive { reason, .. } => Error::Archive { archive: url.to_string(), reason },
            other => other,
        })?;
        Ok((verified, extracted))
    });
    let _ = std::fs::remove_file(&packed);
    let (verified, extracted) = unpacked?;
    check_appimage(url, dest, Some(&extracted.entry))?;
    Ok(Fetched { bytes, verified, unpacked: Some(extracted.entry) })
}

/// The checks a downloaded AppImage gets before it goes anywhere: not
/// empty, an ELF file, and as long as its front says. `entry` names it when
/// it came out of an archive. A file that fails is removed.
fn check_appimage(url: &str, dest: &Path, entry: Option<&str>) -> Result<()> {
    let what = match entry {
        Some(entry) => format!("{url}: {entry} out of the archive"),
        None => url.to_string(),
    };
    if fs_util::file_size(dest).unwrap_or(0) == 0 {
        let _ = std::fs::remove_file(dest);
        return Err(Error::Download(format!("{what}: the downloaded file is empty")));
    }
    // A transfer that arrived whole can still be the wrong file: an error
    // page sent with a 200 and a matching Content-Length passes every check
    // on the transfer. Nothing is installed, and nothing replaces an
    // installed AppImage, unless it at least starts the way every AppImage
    // does. And a server that sends neither a Content-Length nor a chunked
    // body and hangs up early leaves a file whose front is intact, but that
    // front says how long a complete file is at least: the ELF header says
    // where the payload starts, and a squashfs superblock there says how
    // long the payload is. A file on disk gets the same checks before it is
    // installed or adopted.
    if let Err(unfit) = elf::check_whole(dest) {
        let _ = std::fs::remove_file(dest);
        return Err(Error::Download(match unfit {
            elf::Unfit::NoElfHeader => format!(
                "{what}: the server sent a file that is not an AppImage, it does not start with \
                 an ELF header"
            ),
            elf::Unfit::CutShort { minimum, actual } => format!(
                "{what}: the download is cut short, a complete file is at least {minimum} bytes \
                 and the server sent {actual}"
            ),
        }));
    }
    Ok(())
}

/// Fetches at most `max_bytes` from the start of a URL, with a ranged
/// request. A server that ignores the range simply sends more, so the reader
/// is capped either way: the caller gets the beginning of the file and
/// nothing else is downloaded.
pub fn head_bytes(url: &str, max_bytes: usize) -> Result<Vec<u8>> {
    let response = agent()
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Range", format!("bytes=0-{}", max_bytes.saturating_sub(1)))
        .call()
        .map_err(|e| Error::Download(format!("{url}: {e}")))?;

    let mut reader = response.into_body().into_reader().take(max_bytes as u64);
    let mut out = Vec::with_capacity(max_bytes.min(64 * 1024));
    std::io::Read::read_to_end(&mut reader, &mut out)
        .map_err(|e| Error::Download(format!("{url}: {e}")))?;

    if out.is_empty() {
        return Err(Error::Download(format!("{url}: the server sent an empty response")));
    }
    Ok(out)
}

/// What a ranged request came back with. A server that honours the range
/// sends the piece that was asked for; one that does not starts sending the
/// whole file instead, and says so with a 200.
///
/// Either reader has to be read to its end before it is dropped. A body that
/// stops one read short of the end leaves the connection out of the pool,
/// and the next range pays for a new one.
pub enum Ranged {
    /// The bytes that were asked for, in order.
    Partial(Box<dyn Read>),
    /// The whole file from its first byte, because the server ignored the
    /// range.
    Whole(Box<dyn Read>),
}

/// The ranged requests of one update, over as few connections as the server
/// allows.
///
/// ureq keeps its connection pool inside the agent, so a session that asks
/// for one range after another sends them down the connection the last one
/// left open instead of opening a socket and shaking hands over TLS again.
/// Where a redirect took the first range is remembered as well: a GitHub
/// release asset answers every request with a 302 to a CDN, and asking that
/// CDN directly saves a round trip per range.
///
/// A session belongs to a single update and is never stored: the URL a
/// redirect hands out is signed and expires, so it must not be reused for
/// another file, another update, or another run.
pub struct Session {
    agent: ureq::Agent,
    /// A URL that was asked for, and where the redirects led.
    resolved: Option<(String, String)>,
}

impl Session {
    pub fn new() -> Self {
        Self { agent: agent(), resolved: None }
    }

    /// Asks for one byte range of a URL, both ends included, as `Range`
    /// counts.
    ///
    /// Redirects are followed, which is what a GitHub release asset needs:
    /// the download URL answers with a 302 to another host.
    ///
    /// The reader that comes back has to be read to its end for the
    /// connection to go back into the pool, which is what [`Ranged`] says.
    pub fn range(&mut self, url: &str, first: u64, last: u64) -> Result<Ranged> {
        let target = self.target_for(url);
        let mut response = self.ask(&target, first, last);

        // A remembered redirect target is signed and can expire while an
        // update is still running. Anything but a plain "no such range" is
        // reason enough to forget it and ask the URL that was given, which
        // resolves it again at the cost of one request.
        let stale = match &response {
            Ok(_) | Err(ureq::Error::StatusCode(416)) => false,
            Err(_) => target != url,
        };
        if stale {
            self.resolved = None;
            response = self.ask(url, first, last);
        }

        let response = match response {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(416)) => {
                return Err(Error::Download(format!(
                    "{url}: the server has no bytes {first} to {last}, the file it offers is a \
                     different one from the zsync file that described it"
                )));
            }
            Err(e) => return Err(Error::Download(format!("{url}: {e}"))),
        };
        self.remember(url, &response);

        let status = response.status().as_u16();
        let content_range = response
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        match status {
            206 => {
                // A server that answers with a different range than the one
                // that was asked for would quietly put the wrong bytes in
                // the file.
                if let Some(start) = content_range.as_deref().and_then(first_byte_of_content_range)
                {
                    if start != first {
                        return Err(Error::Download(format!(
                            "{url}: asked for byte {first} onwards, the server sent byte {start} \
                             onwards"
                        )));
                    }
                }
                Ok(Ranged::Partial(Box::new(response.into_body().into_reader())))
            }
            200 => Ok(Ranged::Whole(Box::new(response.into_body().into_reader()))),
            other => Err(Error::Download(format!("{url}: the server answered {other}"))),
        }
    }

    fn ask(
        &self,
        url: &str,
        first: u64,
        last: u64,
    ) -> std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        self.agent
            .get(url)
            .header("User-Agent", USER_AGENT)
            .header("Range", format!("bytes={first}-{last}"))
            .call()
    }

    /// Where to send a request for `url`: the redirect target an earlier
    /// range of the same URL ended at, or the URL itself.
    fn target_for(&self, url: &str) -> String {
        match &self.resolved {
            Some((asked, landed)) if asked == url => landed.clone(),
            _ => url.to_string(),
        }
    }

    fn remember(&mut self, url: &str, response: &ureq::http::Response<ureq::Body>) {
        let landed = response.get_uri().to_string();
        if landed != url {
            self.resolved = Some((url.to_string(), landed));
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

/// The first byte a `Content-Range: bytes 4096-8191/99999` header reports.
fn first_byte_of_content_range(value: &str) -> Option<u64> {
    let (unit, range) = value.split_once(' ')?;
    if unit.trim() != "bytes" {
        return None;
    }
    let (first, _) = range.trim().split_once('-')?;
    first.trim().parse().ok()
}

/// Fetches a URL as text, used for the GitHub release API. A GitHub token
/// in the environment goes along, to api.github.com and nowhere else, see
/// [`Credentials`].
pub fn to_string(url: &str) -> Result<String> {
    fetch_text(url, Credentials::from_env().as_ref())
}

fn fetch_text(url: &str, credentials: Option<&Credentials>) -> Result<String> {
    let mut request = agent()
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json");
    let credentials = credentials.filter(|credentials| credentials.belongs_to(url));
    if let Some(credentials) = credentials {
        request = request.header("Authorization", format!("Bearer {}", credentials.token));
    }

    match request.call() {
        Ok(response) => {
            response.into_body().read_to_string().map_err(|e| Error::Network(format!("{url}: {e}")))
        }
        Err(ureq::Error::StatusCode(401)) if credentials.is_some() => Err(Error::Network(format!(
            "{url}: GitHub refused the token in {}",
            credentials.map_or("", |credentials| credentials.variable)
        ))),
        Err(ureq::Error::StatusCode(403 | 429)) => Err(Error::RateLimited),
        Err(e) => Err(Error::Network(format!("{url}: {e}"))),
    }
}

/// A GitHub token, and the one origin it is ever sent to. Only the request
/// for API text carries it: a download never does, and nor does a redirect,
/// see [`agent`].
struct Credentials {
    scheme: &'static str,
    authority: String,
    token: String,
    /// The environment variable the token came from, for an error to name.
    variable: &'static str,
}

impl Credentials {
    /// `GH_TOKEN`, then `GITHUB_TOKEN`, the order `gh` reads them in, for
    /// api.github.com. With a token, the API allows 5000 requests an hour
    /// instead of 60.
    fn from_env() -> Option<Self> {
        Self::github(std::env::var("GH_TOKEN").ok(), std::env::var("GITHUB_TOKEN").ok())
    }

    fn github(gh_token: Option<String>, github_token: Option<String>) -> Option<Self> {
        [("GH_TOKEN", gh_token), ("GITHUB_TOKEN", github_token)].into_iter().find_map(
            |(variable, token)| {
                let token = token?.trim().to_string();
                (!token.is_empty()).then(|| Self {
                    scheme: "https",
                    authority: "api.github.com".to_string(),
                    token,
                    variable,
                })
            },
        )
    }

    /// Whether `url` is on the origin the token belongs to: the same scheme
    /// and exactly the same host and port, with no user name in front.
    fn belongs_to(&self, url: &str) -> bool {
        let Ok(uri) = url.parse::<ureq::http::Uri>() else {
            return false;
        };
        uri.scheme_str() == Some(self.scheme)
            && uri.authority().map(|authority| authority.as_str()) == Some(self.authority.as_str())
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(READ_TIMEOUT))
        // ureq's default, written down because a token depends on it: an
        // `Authorization` header never follows a redirect, wherever it goes.
        .redirect_auth_headers(RedirectAuthHeaders::Never)
        .build()
        .into()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_urls() {
        assert!(is_url("https://example.com/a.AppImage"));
        assert!(is_url("http://example.com/a.AppImage"));
        assert!(!is_url("/home/u/a.AppImage"));
        assert!(!is_url("./a.AppImage"));
    }

    #[test]
    fn a_token_belongs_to_api_github_com_and_nowhere_else() {
        let credentials = Credentials::github(None, Some("secret".to_string())).unwrap();
        assert!(credentials.belongs_to("https://api.github.com/repos/o/r/releases?per_page=30"));
        for url in [
            "http://api.github.com/repos/o/r/releases",
            "https://api.github.com:8443/repos/o/r/releases",
            "https://api.github.com.example.org/repos/o/r/releases",
            "https://user@api.github.com/repos/o/r/releases",
            "https://example.org/https://api.github.com/repos",
            "https://github.com/o/r/releases/download/v1/App-1-x86_64.AppImage",
            "https://objects.githubusercontent.com/github-production-release-asset/1/2",
            "not a url",
        ] {
            assert!(!credentials.belongs_to(url), "{url}");
        }
    }

    #[test]
    fn the_token_comes_from_gh_token_then_github_token() {
        let token = |gh: Option<&str>, github: Option<&str>| {
            Credentials::github(gh.map(str::to_string), github.map(str::to_string))
                .map(|credentials| (credentials.variable, credentials.token))
        };
        assert_eq!(token(Some("a"), Some("b")), Some(("GH_TOKEN", "a".to_string())));
        assert_eq!(token(None, Some(" b\n")), Some(("GITHUB_TOKEN", "b".to_string())));
        assert_eq!(token(Some("  "), Some("b")), Some(("GITHUB_TOKEN", "b".to_string())));
        assert_eq!(token(Some(""), None), None);
        assert_eq!(token(None, None), None);
    }

    /// The path of a request, and the `Authorization` header it came with.
    type Seen = (String, Option<String>);

    /// A server that answers every request with `answer(path)`, and keeps
    /// what each request came with.
    struct Recorder {
        base: String,
        seen: std::sync::Arc<std::sync::Mutex<Vec<Seen>>>,
    }

    impl Recorder {
        fn start(
            answer: impl Fn(&str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> + Send + 'static,
        ) -> Self {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base = format!("http://{}", server.server_addr().to_ip().unwrap());
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let recorded = std::sync::Arc::clone(&seen);
            std::thread::spawn(move || {
                for request in server.incoming_requests() {
                    let authorization = request
                        .headers()
                        .iter()
                        .find(|header| header.field.equiv("Authorization"))
                        .map(|header| header.value.as_str().to_string());
                    let path = request.url().to_string();
                    recorded.lock().unwrap().push((path.clone(), authorization));
                    let _ = request.respond(answer(&path));
                }
            });
            Self { base, seen }
        }

        fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[test]
    fn the_token_goes_to_the_api_host_and_never_to_a_download_host() {
        let download = Recorder::start(|_| tiny_http::Response::from_string("the file"));
        let target = format!("{}/file", download.base);
        let api = Recorder::start(move |path| {
            if path == "/moved" {
                // What an API answer pointing at a download looks like.
                let location = tiny_http::Header::from_bytes(&b"Location"[..], target.as_bytes());
                tiny_http::Response::from_string("")
                    .with_status_code(302)
                    .with_header(location.unwrap())
            } else {
                tiny_http::Response::from_string("[]")
            }
        });
        // The local server stands in for api.github.com, nothing else changes.
        let authority = api.base.trim_start_matches("http://").to_string();
        let credentials = Credentials {
            scheme: "http",
            authority: authority.clone(),
            token: "secret".to_string(),
            variable: "GITHUB_TOKEN",
        };

        assert_eq!(fetch_text(&format!("{}/repos", api.base), Some(&credentials)).unwrap(), "[]");
        assert_eq!(
            fetch_text(&format!("{}/file", download.base), Some(&credentials)).unwrap(),
            "the file"
        );
        // A redirect that leaves the API host leaves the token behind.
        assert_eq!(
            fetch_text(&format!("{}/moved", api.base), Some(&credentials)).unwrap(),
            "the file"
        );
        // The same server under another name is another host.
        let port = authority.rsplit(':').next().unwrap();
        fetch_text(&format!("http://localhost:{port}/other"), Some(&credentials)).unwrap();

        let bearer = Some("Bearer secret".to_string());
        assert_eq!(
            api.seen(),
            vec![
                ("/repos".to_string(), bearer.clone()),
                ("/moved".to_string(), bearer),
                ("/other".to_string(), None),
            ]
        );
        assert_eq!(download.seen(), vec![("/file".to_string(), None), ("/file".to_string(), None)]);

        // A download is never given the token in the first place.
        to_file(
            &format!("{}/file", download.base),
            &tempfile::tempdir().unwrap().path().join("f"),
            None,
        )
        .unwrap();
        assert_eq!(download.seen().last(), Some(&("/file".to_string(), None)));
    }

    #[test]
    fn derives_file_names_from_urls() {
        assert_eq!(file_name_from_url("https://example.com/App-1.0.AppImage"), "App-1.0.AppImage");
        assert_eq!(file_name_from_url("https://example.com/App.AppImage?token=1"), "App.AppImage");
        assert_eq!(file_name_from_url("https://example.com/download"), "download.AppImage");
        assert_eq!(file_name_from_url("https://example.com/My%20App.AppImage"), "My App.AppImage");
    }
}
