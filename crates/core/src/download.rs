use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use ureq::ResponseExt;

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

/// Fetches a URL as text, used for the GitHub release API.
pub fn to_string(url: &str) -> Result<String> {
    let response = agent()
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .call();

    match response {
        Ok(response) => {
            response.into_body().read_to_string().map_err(|e| Error::Network(format!("{url}: {e}")))
        }
        Err(ureq::Error::StatusCode(403 | 429)) => Err(Error::RateLimited),
        Err(e) => Err(Error::Network(format!("{url}: {e}"))),
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(READ_TIMEOUT))
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
    fn derives_file_names_from_urls() {
        assert_eq!(file_name_from_url("https://example.com/App-1.0.AppImage"), "App-1.0.AppImage");
        assert_eq!(file_name_from_url("https://example.com/App.AppImage?token=1"), "App.AppImage");
        assert_eq!(file_name_from_url("https://example.com/download"), "download.AppImage");
        assert_eq!(file_name_from_url("https://example.com/My%20App.AppImage"), "My App.AppImage");
    }
}
