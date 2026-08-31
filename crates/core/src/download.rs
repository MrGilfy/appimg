use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

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
