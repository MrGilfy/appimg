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
use crate::remote::Remote;

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
    fetch(url, dest, progress).map(|(bytes, _)| bytes)
}

/// [`to_file`], and what the server said about the file it sent.
fn fetch(url: &str, dest: &Path, progress: Option<ProgressFn<'_>>) -> Result<(u64, Remote)> {
    let response = agent()
        .get(url)
        .header("User-Agent", USER_AGENT)
        .config()
        .save_redirect_history(true)
        .build()
        .call()
        .map_err(|e| Error::Download(format!("{url}: {e}")))?;
    let remote = remote_of(url, &response);

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
    Ok((written, remote))
}

/// Asks the server about the file at `url` and downloads none of it: a HEAD
/// request, which follows every redirect, or for a server that refuses HEAD
/// a request for its first byte. One request per redirect, see
/// [`crate::remote`]. A link that ends anywhere but at a file is an error
/// that says where it ended: a link to a version that is gone often
/// redirects to a page that says so.
pub fn probe(url: &str) -> Result<Remote> {
    let agent = agent();
    let ask = |request: ureq::RequestBuilder<ureq::typestate::WithoutBody>| {
        request
            .header("User-Agent", USER_AGENT)
            .config()
            .save_redirect_history(true)
            .http_status_as_error(false)
            .build()
            .call()
            .map_err(|e| Error::Download(format!("{url}: {e}")))
    };

    let mut response = ask(agent.head(url))?;
    if matches!(response.status().as_u16(), 403 | 405 | 501) {
        response = ask(agent.get(url).header("Range", "bytes=0-0"))?;
    }

    let landed = response.get_uri().to_string();
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(Error::Download(format!(
            "{url}: the link leads to no file, the server answered {status} at {landed}"
        )));
    }
    let html = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("text/html"));
    if html {
        return Err(Error::Download(format!(
            "{url}: the link leads to a web page, not a file, at {landed}"
        )));
    }
    Ok(remote_of(url, &response))
}

/// What a response that `url` led to says about its file.
fn remote_of(url: &str, response: &ureq::http::Response<ureq::Body>) -> Remote {
    let hops = match response.get_redirect_history() {
        Some(history) if !history.is_empty() => history.iter().map(|uri| uri.to_string()).collect(),
        _ => vec![url.to_string(), response.get_uri().to_string()],
    };
    let headers = response.headers();
    // The headers of a one-byte request say one byte: the whole file is
    // what its range says, for a 206 only.
    let ranged = response.status().as_u16() == 206;
    Remote::from_response(hops, &|name| {
        if name == "content-range" && !ranged {
            return None;
        }
        headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
    })
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
    /// What the server said about what it sent, for an update from the same
    /// URL to compare with, see [`crate::remote`].
    pub remote: Remote,
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
    let (bytes, remote) = fetch(url, dest, progress)?;
    if archive::Kind::of(dest).is_none() {
        check_appimage(url, dest, None)?;
        let verified = verify(dest)?;
        return Ok(Fetched { bytes, verified, unpacked: None, remote });
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
    Ok(Fetched { bytes, verified, unpacked: Some(extracted.entry), remote })
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

/// A small text file, such as a checksum file next to a release asset: a
/// plain request, never with a token, read to at most `max_bytes`. No
/// `Range` asks for less: some CDNs answer one for a file this small with
/// a length and no body, dl.librewolf.net among them. A file that is
/// longer is refused, whatever it holds.
pub fn small_text(url: &str, max_bytes: u64) -> Result<String> {
    let response = agent()
        .get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| Error::Download(format!("{url}: {e}")))?;
    let mut reader = response.into_body().into_reader().take(max_bytes + 1);
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut out)
        .map_err(|e| Error::Download(format!("{url}: {e}")))?;
    if out.len() as u64 > max_bytes {
        return Err(Error::Download(format!("{url}: longer than {max_bytes} bytes")));
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
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

/// The release API a request goes to. It decides how a token is sent, and
/// what a refusal means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    GitHub,
    GitLab,
    Forgejo,
}

impl Api {
    fn name(self) -> &'static str {
        match self {
            Api::GitHub => "GitHub",
            Api::GitLab => "GitLab",
            Api::Forgejo => "Forgejo",
        }
    }

    /// The value of the `Authorization` header for `token`. Every forge
    /// takes the token there, and nowhere else: ureq drops that header, and
    /// only that one, on every redirect, see [`agent`]. GitLab's own
    /// `PRIVATE-TOKEN` header would follow a redirect to any host.
    fn authorization(self, token: &str) -> String {
        match self {
            Api::GitHub | Api::GitLab => format!("Bearer {token}"),
            Api::Forgejo => format!("token {token}"),
        }
    }
}

/// Fetches a URL as text, used for the GitHub release API. A GitHub token
/// in the environment goes along, to api.github.com and nowhere else, see
/// [`Credentials`].
pub fn to_string(url: &str) -> Result<String> {
    api_text(url, Api::GitHub)
}

/// Fetches the answer of a release API as text, with the token the
/// environment holds for the host it goes to, if any, see
/// [`Credentials::for_url`].
pub fn api_text(url: &str, api: Api) -> Result<String> {
    let credentials = Credentials::for_url(url, &|name| std::env::var(name).ok());
    fetch_text(url, api, credentials.as_ref())
}

fn fetch_text(url: &str, api: Api, credentials: Option<&Credentials>) -> Result<String> {
    let accept = match api {
        Api::GitHub => "application/vnd.github+json",
        Api::GitLab | Api::Forgejo => "application/json",
    };
    let mut request = agent().get(url).header("User-Agent", USER_AGENT).header("Accept", accept);
    let credentials = credentials.filter(|credentials| credentials.belongs_to(url));
    if let Some(credentials) = credentials {
        request = request.header("Authorization", api.authorization(&credentials.token));
    }

    match request.call() {
        Ok(response) => {
            response.into_body().read_to_string().map_err(|e| Error::Network(format!("{url}: {e}")))
        }
        Err(ureq::Error::StatusCode(401)) if credentials.is_some() => Err(Error::Network(format!(
            "{url}: {} refused the token in {}",
            api.name(),
            credentials.map_or("", |credentials| credentials.variable.as_str())
        ))),
        // GitHub answers an exhausted limit with a 403 as often as with a
        // 429. GitLab and Forgejo mean something else by a 403.
        Err(ureq::Error::StatusCode(403 | 429)) if api == Api::GitHub => {
            Err(Error::RateLimited("GitHub".to_string()))
        }
        Err(ureq::Error::StatusCode(429)) => Err(Error::RateLimited(host_of(url))),
        Err(e) => Err(Error::Network(format!("{url}: {e}"))),
    }
}

/// The host and port of a URL, for a message.
fn host_of(url: &str) -> String {
    url.parse::<ureq::http::Uri>()
        .ok()
        .and_then(|uri| uri.authority().map(|authority| authority.host().to_string()))
        .unwrap_or_else(|| url.to_string())
}

/// A token, and the one origin it is ever sent to. Only the request for API
/// text carries it: a download never does, and nor does a redirect, see
/// [`agent`].
struct Credentials {
    scheme: &'static str,
    authority: String,
    token: String,
    /// The environment variable the token came from, for an error to name.
    variable: String,
}

impl Credentials {
    /// The token `env` holds for the origin of `url`, which has to be
    /// https, with no user name in front:
    ///
    /// - api.github.com: `GH_TOKEN`, then `GITHUB_TOKEN`, the order `gh`
    ///   reads them in. With one, the API allows 5000 requests an hour
    ///   instead of 60.
    /// - gitlab.com: `GITLAB_TOKEN`.
    /// - codeberg.org: `CODEBERG_TOKEN`.
    /// - any host, these included: `APPIMG_TOKEN_<HOST>`, the host and port
    ///   in capitals with everything but letters and digits as `_`, such as
    ///   `APPIMG_TOKEN_INVENT_KDE_ORG` for invent.kde.org.
    fn for_url(url: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<Self> {
        let uri = url.parse::<ureq::http::Uri>().ok()?;
        let authority = uri.authority()?.as_str();
        if uri.scheme_str() != Some("https") || authority.contains('@') {
            return None;
        }
        let known: &[&str] = match authority {
            "api.github.com" => &["GH_TOKEN", "GITHUB_TOKEN"],
            "gitlab.com" => &["GITLAB_TOKEN"],
            "codeberg.org" => &["CODEBERG_TOKEN"],
            _ => &[],
        };
        let generic = format!(
            "APPIMG_TOKEN_{}",
            authority
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
                .collect::<String>()
        );
        known.iter().map(|name| name.to_string()).chain([generic]).find_map(|variable| {
            let token = env(&variable)?.trim().to_string();
            (!token.is_empty()).then(|| Self {
                scheme: "https",
                authority: authority.to_string(),
                token,
                variable,
            })
        })
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

pub(crate) fn percent_decode(value: &str) -> String {
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

    /// The environment of a user who set `variables`.
    fn env(variables: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let variables: Vec<(String, String)> =
            variables.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect();
        move |name: &str| {
            variables.iter().find(|(variable, _)| variable == name).map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn a_token_belongs_to_api_github_com_and_nowhere_else() {
        let credentials = Credentials::for_url(
            "https://api.github.com/repos",
            &env(&[("GITHUB_TOKEN", "secret")]),
        )
        .unwrap();
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
            let mut variables = Vec::new();
            variables.extend(gh.map(|value| ("GH_TOKEN", value)));
            variables.extend(github.map(|value| ("GITHUB_TOKEN", value)));
            Credentials::for_url("https://api.github.com/repos/o/r", &env(&variables))
                .map(|credentials| (credentials.variable, credentials.token))
        };
        let found = |variable: &str, token: &str| Some((variable.to_string(), token.to_string()));
        assert_eq!(token(Some("a"), Some("b")), found("GH_TOKEN", "a"));
        assert_eq!(token(None, Some(" b\n")), found("GITHUB_TOKEN", "b"));
        assert_eq!(token(Some("  "), Some("b")), found("GITHUB_TOKEN", "b"));
        assert_eq!(token(Some(""), None), None);
        assert_eq!(token(None, None), None);
    }

    /// Each forge has a variable of its own, and every host one by its
    /// name. None of them goes anywhere but https and the host it names.
    #[test]
    fn each_host_takes_the_token_its_own_variable_holds() {
        let all = env(&[
            ("GITHUB_TOKEN", "github"),
            ("GITLAB_TOKEN", "gitlab"),
            ("CODEBERG_TOKEN", "codeberg"),
            ("APPIMG_TOKEN_INVENT_KDE_ORG", "kde"),
            ("APPIMG_TOKEN_GIT_EXAMPLE_ORG_8443", "example"),
        ]);
        let token = |url: &str| {
            Credentials::for_url(url, &all)
                .map(|credentials| (credentials.variable, credentials.token))
        };
        let found = |variable: &str, token: &str| Some((variable.to_string(), token.to_string()));
        assert_eq!(
            token("https://api.github.com/repos/o/r/releases"),
            found("GITHUB_TOKEN", "github")
        );
        assert_eq!(
            token("https://gitlab.com/api/v4/projects/a%2Fb/releases"),
            found("GITLAB_TOKEN", "gitlab")
        );
        assert_eq!(
            token("https://codeberg.org/api/v1/repos/o/r/releases"),
            found("CODEBERG_TOKEN", "codeberg")
        );
        assert_eq!(
            token("https://invent.kde.org/api/v4/projects/a%2Fb/releases"),
            found("APPIMG_TOKEN_INVENT_KDE_ORG", "kde")
        );
        assert_eq!(
            token("https://git.example.org:8443/api/v1/repos/o/r/releases"),
            found("APPIMG_TOKEN_GIT_EXAMPLE_ORG_8443", "example")
        );
        // Another host, plain http, a user name in front: no token at all.
        for url in [
            "https://github.com/o/r/releases/download/v1/App.AppImage",
            "https://git.example.org/api/v1/repos/o/r/releases",
            "http://gitlab.com/api/v4/projects/a%2Fb/releases",
            "http://codeberg.org/api/v1/repos/o/r/releases",
            "https://user@codeberg.org/api/v1/repos/o/r/releases",
            "https://gitlab.com.example.org/api/v4/projects/a%2Fb/releases",
        ] {
            assert_eq!(token(url), None, "{url}");
        }
        let credentials = Credentials::for_url("https://codeberg.org/api", &all).unwrap();
        assert!(!credentials.belongs_to("https://gitlab.com/api/v4"));
        assert!(!credentials.belongs_to("https://codeberg.org:444/api"));
    }

    /// The path of a request, and the `Authorization` header it came with.
    type Seen = (String, Option<String>);

    /// Whether any request a server saw carried GitLab's own token header,
    /// which nothing ever sends.
    static PRIVATE_TOKEN_SEEN: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

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
                    if request.headers().iter().any(|header| header.field.equiv("PRIVATE-TOKEN")) {
                        PRIVATE_TOKEN_SEEN.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
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
            variable: "GITHUB_TOKEN".to_string(),
        };
        let fetch = |url: &str| fetch_text(url, Api::GitHub, Some(&credentials));

        assert_eq!(fetch(&format!("{}/repos", api.base)).unwrap(), "[]");
        assert_eq!(fetch(&format!("{}/file", download.base)).unwrap(), "the file");
        // A redirect that leaves the API host leaves the token behind.
        assert_eq!(fetch(&format!("{}/moved", api.base)).unwrap(), "the file");
        // The same server under another name is another host.
        let port = authority.rsplit(':').next().unwrap();
        fetch(&format!("http://localhost:{port}/other")).unwrap();

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

    /// GitLab and Forgejo take the token the way each wants it, and in the
    /// `Authorization` header only, so that a 302 to another host leaves it
    /// behind the way it does for GitHub. GitLab's `PRIVATE-TOKEN` header
    /// would follow the redirect, and is never sent.
    #[test]
    fn a_forge_token_never_follows_a_redirect_to_another_host() {
        for (api, sent) in [(Api::GitLab, "Bearer secret"), (Api::Forgejo, "token secret")] {
            let elsewhere = Recorder::start(|_| tiny_http::Response::from_string("[]"));
            let target = format!("{}/releases", elsewhere.base);
            let forge = Recorder::start(move |path| {
                if path == "/moved" {
                    let location =
                        tiny_http::Header::from_bytes(&b"Location"[..], target.as_bytes()).unwrap();
                    tiny_http::Response::from_string("").with_status_code(302).with_header(location)
                } else {
                    tiny_http::Response::from_string("[]")
                }
            });
            let credentials = Credentials {
                scheme: "http",
                authority: forge.base.trim_start_matches("http://").to_string(),
                token: "secret".to_string(),
                variable: "APPIMG_TOKEN_TEST".to_string(),
            };

            fetch_text(&format!("{}/releases", forge.base), api, Some(&credentials)).unwrap();
            fetch_text(&format!("{}/moved", forge.base), api, Some(&credentials)).unwrap();

            let sent = Some(sent.to_string());
            assert_eq!(
                forge.seen(),
                vec![("/releases".to_string(), sent.clone()), ("/moved".to_string(), sent)],
                "{api:?}"
            );
            assert_eq!(elsewhere.seen(), vec![("/releases".to_string(), None)], "{api:?}");
        }
        assert!(!PRIVATE_TOKEN_SEEN.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn derives_file_names_from_urls() {
        assert_eq!(file_name_from_url("https://example.com/App-1.0.AppImage"), "App-1.0.AppImage");
        assert_eq!(file_name_from_url("https://example.com/App.AppImage?token=1"), "App.AppImage");
        assert_eq!(file_name_from_url("https://example.com/download"), "download.AppImage");
        assert_eq!(file_name_from_url("https://example.com/My%20App.AppImage"), "My App.AppImage");
    }
}
