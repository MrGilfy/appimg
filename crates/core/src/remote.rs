//! What a server says about the file behind a URL, and what a check makes
//! of it.
//!
//! A vendor link such as `https://lmstudio.ai/download/latest/linux/x64`
//! names no version. It either redirects to the current file, whose name
//! carries one, or keeps a fixed name and changes what is behind it. A check
//! asks the server with a HEAD request, which follows every redirect and
//! downloads nothing, and compares the answer with what the server said
//! about the installed file when it was downloaded. An install or update
//! from a URL keeps that in [`KEY_REMOTE`], tied to the checksum of the
//! installed file in [`KEY_SHA1`], so a file replaced by any other means
//! leaves a record that no longer applies to it.
//!
//! What counts, most trusted first, see [`judge`]: the version in the file
//! name, then the path the link lands on, then the `ETag`, the
//! `Last-Modified` date and the `Content-Length`. The host is never part of
//! it, since CDNs rotate their host names, and neither is a query, which is
//! where signed URLs keep a signature that expires.

use std::cmp::Ordering;

use crate::desktop_entry::{DesktopEntry, KEY_REMOTE, KEY_SHA1};
use crate::stamp::Stamp;
use crate::{archive, date, download, version};

/// What a server said about a file: the file a URL ended at, after every
/// redirect, and the headers that answered for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Remote {
    /// Every URL the request went to, the one asked for first and the one
    /// that answered last. Not recorded: only a check needs them.
    pub hops: Vec<String>,
    /// The path of the URL that answered, without host or query.
    pub path: String,
    /// The file name the server gives, see [`file_name`].
    pub name: Option<String>,
    pub etag: Option<String>,
    /// `Last-Modified`, in seconds since the epoch.
    pub modified: Option<i64>,
    pub length: Option<u64>,
}

impl Remote {
    /// What a response says about the file. `hops` are the URLs it went
    /// through, the last one the URL that answered. `header` looks up a
    /// response header by its lowercase name.
    pub fn from_response(hops: Vec<String>, header: &dyn Fn(&str) -> Option<String>) -> Self {
        let landed = hops.last().map(String::as_str).unwrap_or("");
        let length = match header("content-range") {
            // A one-byte request in place of a HEAD request: the length is
            // what the range says the whole file is.
            Some(range) => range.rsplit('/').next().and_then(|total| total.trim().parse().ok()),
            None => header("content-length").and_then(|value| value.trim().parse().ok()),
        };
        Self {
            path: path_of(landed),
            name: file_name(&hops, header("content-disposition").as_deref()),
            etag: header("etag").map(|value| value.trim().to_string()).filter(|v| !v.is_empty()),
            modified: header("last-modified").as_deref().and_then(date::http_date_seconds),
            length,
            hops,
        }
    }

    /// The version the file name carries, only a dotted one: a bare number
    /// in a file name is as likely the `64` of `x86_64`.
    pub fn version(&self) -> Option<String> {
        self.name
            .as_deref()
            .filter(|name| version::names_a_version(name))
            .and_then(version::extract)
    }

    /// The value of [`KEY_REMOTE`] for the file whose checksum is `sha1`.
    pub fn record(&self, sha1: &str) -> String {
        let field = |value: Option<&str>| value.map_or_else(|| "-".to_string(), encode);
        [
            encode(sha1),
            field(self.length.map(|length| length.to_string()).as_deref()),
            field(self.modified.map(|modified| modified.to_string()).as_deref()),
            field(self.etag.as_deref()),
            field(Some(&self.path)),
            field(self.name.as_deref()),
        ]
        .join(" ")
    }

    /// A [`KEY_REMOTE`] value, as the checksum of the file it was recorded
    /// for and what the server said about it. `None` for anything that is
    /// not one.
    pub fn parse_record(value: &str) -> Option<(String, Remote)> {
        let fields: Vec<Option<String>> = value
            .split(' ')
            .map(|field| (field != "-").then(|| download::percent_decode(field)))
            .collect();
        let [sha1, length, modified, etag, path, name] =
            <[Option<String>; 6]>::try_from(fields).ok()?;
        let remote = Remote {
            hops: Vec::new(),
            path: path?,
            name,
            etag,
            modified: number(modified)?,
            length: number(length)?,
        };
        Some((sha1.filter(|sha1| !sha1.is_empty())?, remote))
    }

    /// The name to show for the file: the one it was given, or the end of
    /// its path.
    fn shown(&self) -> &str {
        self.name.as_deref().unwrap_or_else(|| self.path.rsplit('/').next().unwrap_or(""))
    }
}

/// An optional number field of a record: `Some(None)` for none, `None` for
/// one that is no number.
fn number<T: std::str::FromStr>(field: Option<String>) -> Option<Option<T>> {
    match field {
        Some(text) => text.parse().ok().map(Some),
        None => Some(None),
    }
}

/// Records what the server said about the file stamped in `entry`, see
/// [`crate::stamp::record`], or takes any record out when the file came from
/// nowhere a server said anything about, or has no stamp to tie it to.
pub fn record_in(entry: &mut DesktopEntry, remote: Option<&Remote>) {
    let sha1 = entry.get(KEY_SHA1).and_then(Stamp::parse).map(|stamp| stamp.sha1);
    match (remote, sha1) {
        (Some(remote), Some(sha1)) => entry.set(KEY_REMOTE, remote.record(&sha1)),
        _ => entry.remove(KEY_REMOTE),
    }
}

/// What a check makes of what the server says now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub available: bool,
    /// Whether that is settled: an update has nothing to download even when
    /// there is a note, which is then only information.
    pub settled: bool,
    pub note: Option<String>,
}

impl Verdict {
    fn new(available: bool, settled: bool, note: Option<String>) -> Self {
        Self { available, settled, note }
    }
}

/// Whether `offered` is another file than the installed one, in this order
/// of trust:
///
/// 1. The version in the file name, against the one in the name of the
///    installed download, or the installed version without a record. Newer
///    is an update, older is none.
/// 2. With a record of the installed download: another path or name is
///    another file. On the same one, the `ETag`, else a later
///    `Last-Modified`, else another `Content-Length`. A file whose name
///    carries a version is not updated for that: the server changed the
///    file under a version already installed, and the note says so.
/// 3. Without a record: the same version is the same file. Otherwise a
///    `Last-Modified` after `since`, when the installed file was written, is
///    an update, and one before is none.
///
/// What none of that settles is left to an update, which downloads the file
/// and keeps the installed one when it turns out to be the same.
pub fn judge(
    offered: &Remote,
    recorded: Option<&Remote>,
    installed: Option<&str>,
    since: Option<i64>,
) -> Verdict {
    let offered_version = offered.version();
    let baseline = recorded.and_then(Remote::version).or_else(|| installed.map(str::to_string));
    let versions = offered_version
        .as_deref()
        .zip(baseline.as_deref())
        .filter(|(new, old)| version::comparable(new, old) && !version::same_but_label(new, old));
    if let Some((new, old)) = versions {
        match version::compare(new, old) {
            Ordering::Greater => return Verdict::new(true, false, None),
            Ordering::Less => {
                return Verdict::new(
                    false,
                    true,
                    Some(format!("the link offers {new}, older than the installed {old}")),
                );
            }
            Ordering::Equal => {}
        }
    }
    let same_version = versions.is_some_and(|(new, old)| version::compare(new, old).is_eq())
        || offered_version
            .as_deref()
            .zip(baseline.as_deref())
            .is_some_and(|(new, old)| version::same_but_label(new, old));

    if let Some(recorded) = recorded {
        let renamed =
            matches!((&offered.name, &recorded.name), (Some(new), Some(old)) if new != old);
        if offered.path != recorded.path || renamed {
            return Verdict::new(true, false, None);
        }
        match changed(offered, recorded) {
            Some((true, _)) if offered_version.is_some() => {
                return Verdict::new(
                    false,
                    true,
                    Some(format!(
                        "the server changed {} without a new version in its name, the installed \
                         file stays",
                        offered.shown()
                    )),
                );
            }
            Some((true, _)) => return Verdict::new(true, false, None),
            Some((false, Basis::Length)) => {
                return Verdict::new(
                    false,
                    true,
                    Some("only the size can be compared, and it is the same".to_string()),
                );
            }
            Some((false, _)) => return Verdict::new(false, true, None),
            None => {}
        }
    } else if same_version {
        return Verdict::new(false, true, None);
    } else if let (Some(modified), Some(since)) = (offered.modified, since) {
        return if modified > since {
            Verdict::new(
                true,
                false,
                Some(
                    "judged by date, the server's file is newer than the installed one".to_string(),
                ),
            )
        } else {
            Verdict::new(
                false,
                true,
                Some(
                    "judged by date, the server's file is not newer than the installed one; the \
                     next update records what the server says about it"
                        .to_string(),
                ),
            )
        };
    }

    Verdict::new(
        false,
        false,
        Some(
            "the server says nothing that tells one file from another, updating re-downloads it"
                .to_string(),
        ),
    )
}

/// What told two answers about the same path apart, or found them alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Basis {
    ETag,
    Modified,
    Length,
}

/// Whether the file at the same path changed, by the first thing both
/// answers carry. `None` when they have nothing in common.
fn changed(offered: &Remote, recorded: &Remote) -> Option<(bool, Basis)> {
    if let (Some(new), Some(old)) = (&offered.etag, &recorded.etag) {
        // A weak tag says as much about whether the file changed. Some
        // servers make a strong one weak when they compress the response.
        let strong = |tag: &str| tag.trim_start_matches("W/").to_string();
        return Some((strong(new) != strong(old), Basis::ETag));
    }
    if let (Some(new), Some(old)) = (offered.modified, recorded.modified) {
        return Some((new > old, Basis::Modified));
    }
    if let (Some(new), Some(old)) = (offered.length, recorded.length) {
        return Some((new != old, Basis::Length));
    }
    None
}

/// The file name a server gives: the one `Content-Disposition` names,
/// otherwise the last URL on the way whose path ends in an AppImage or an
/// archive. A signed CDN URL often ends in nothing of the kind, while the
/// URL that redirected to it named the file.
fn file_name(hops: &[String], disposition: Option<&str>) -> Option<String> {
    if let Some(name) = disposition.and_then(disposition_file_name) {
        return Some(name);
    }
    hops.iter().rev().find_map(|hop| {
        let name = path_of(hop).rsplit('/').next().map(download::percent_decode)?;
        let lower = name.to_ascii_lowercase();
        (lower.ends_with(".appimage") || archive::is_archive_name(&name)).then_some(name)
    })
}

/// The `filename*` or `filename` of a `Content-Disposition` header, without
/// any directory in front.
fn disposition_file_name(value: &str) -> Option<String> {
    let mut plain = None;
    for parameter in value.split(';').skip(1) {
        let Some((key, raw)) = parameter.split_once('=') else {
            continue;
        };
        let raw = raw.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            // `UTF-8''LM%20Studio.AppImage`: charset, language, then the
            // percent-encoded name.
            "filename*" => {
                let encoded = raw.splitn(3, '\'').nth(2)?;
                return base_name(&download::percent_decode(encoded));
            }
            "filename" => plain = base_name(raw.trim_matches('"')),
            _ => {}
        }
    }
    plain
}

fn base_name(name: &str) -> Option<String> {
    let name = name.rsplit(['/', '\\']).next()?.trim();
    (!name.is_empty() && name != "." && name != "..").then(|| name.to_string())
}

/// The path of a URL, without scheme, host, query or fragment.
fn path_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    match rest.find('/') {
        Some(start) => rest[start..].to_string(),
        None => "/".to_string(),
    }
}

/// Percent-encodes a field of [`KEY_REMOTE`]: everything but what a URL path
/// holds unencoded, so that no space, quote, backslash, semicolon or byte
/// outside ASCII can shift a field or mean something in a desktop entry.
/// A field that is a single `-` is encoded too, `-` stands for no field.
fn encode(value: &str) -> String {
    if value == "-" {
        return "%2D".to_string();
    }
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/:@!$&'()*+,=".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA1: &str = "a9993e364706816aba3e25717850c26c9cd0d89d";

    fn remote(path: &str) -> Remote {
        Remote {
            hops: vec![format!("https://cdn.example.com{path}")],
            path: path.to_string(),
            name: path.rsplit('/').next().map(str::to_string),
            ..Remote::default()
        }
    }

    fn with(
        mut remote: Remote,
        etag: Option<&str>,
        modified: Option<i64>,
        length: Option<u64>,
    ) -> Remote {
        remote.etag = etag.map(str::to_string);
        remote.modified = modified;
        remote.length = length;
        remote
    }

    #[test]
    fn a_record_reads_back_what_it_wrote() {
        let written = Remote {
            hops: Vec::new(),
            path: "/linux/x64/0.4.25-1/LM-Studio-0.4.25-1-x64.AppImage".to_string(),
            name: Some("LM-Studio-0.4.25-1-x64.AppImage".to_string()),
            etag: Some("\"abf7437f53ba76c071517bdc1fd5b334\"".to_string()),
            modified: Some(1_789_782_771),
            length: Some(1_012_171_087),
        };
        let value = written.record(SHA1);
        assert_eq!(
            value,
            format!(
                "{SHA1} 1012171087 1789782771 %22abf7437f53ba76c071517bdc1fd5b334%22 \
                 /linux/x64/0.4.25-1/LM-Studio-0.4.25-1-x64.AppImage LM-Studio-0.4.25-1-x64.AppImage"
            )
        );
        assert_eq!(Remote::parse_record(&value), Some((SHA1.to_string(), written)));

        let bare = Remote { path: "/".to_string(), ..Remote::default() };
        assert_eq!(bare.record(SHA1), format!("{SHA1} - - - / -"));
        assert_eq!(Remote::parse_record(&bare.record(SHA1)), Some((SHA1.to_string(), bare)));
    }

    /// A space, a quote or a lone `-` in a field would shift every field
    /// behind it, or turn into no field at all.
    #[test]
    fn no_field_can_shift_the_others() {
        let written = Remote {
            hops: Vec::new(),
            path: "/builds/My App;v2/-".to_string(),
            name: Some("-".to_string()),
            etag: Some("W/\"a b\\c\" - %20".to_string()),
            modified: None,
            length: Some(7),
        };
        let value = written.record(SHA1);
        assert_eq!(value.split(' ').count(), 6, "{value}");
        assert!(!value.contains(['"', ';', '\\']), "{value}");
        assert_eq!(Remote::parse_record(&value), Some((SHA1.to_string(), written)));
    }

    #[test]
    fn a_record_that_is_not_one_is_ignored() {
        for value in [
            "",
            SHA1,
            &format!("{SHA1} 1 2 - /a"),
            &format!("{SHA1} 1 2 - /a b c"),
            &format!("{SHA1} x 2 - /a b"),
            &format!("{SHA1} 1 x - /a b"),
            &format!("{SHA1} 1 2 - - b"),
            "- 1 2 - /a b",
        ] {
            assert_eq!(Remote::parse_record(value), None, "{value:?}");
        }
    }

    #[test]
    fn the_name_comes_from_content_disposition_then_from_the_last_named_hop() {
        let hops = vec![
            "https://vault.bitwarden.com/download/?app=desktop&platform=linux".to_string(),
            "https://github.com/bitwarden/clients/releases/download/desktop-v2026.9.1/\
             Bitwarden-2026.9.1-x86_64.AppImage"
                .to_string(),
            "https://release-assets.githubusercontent.com/github-production-release-asset/\
             53538899/26191cb1-0f9e?sig=abc&rscd=attachment"
                .to_string(),
        ];
        assert_eq!(file_name(&hops, None).as_deref(), Some("Bitwarden-2026.9.1-x86_64.AppImage"));
        assert_eq!(
            file_name(&hops, Some("attachment; filename=Other-1.0.AppImage")).as_deref(),
            Some("Other-1.0.AppImage")
        );
        assert_eq!(
            file_name(
                &hops,
                Some("attachment; filename=\"plain.AppImage\"; filename*=UTF-8''My%20App-2.0.AppImage")
            )
            .as_deref(),
            Some("My App-2.0.AppImage")
        );
        // Nothing a server names is a path.
        assert_eq!(
            file_name(&[], Some("attachment; filename=\"../../etc/App-1.0.AppImage\"")).as_deref(),
            Some("App-1.0.AppImage")
        );
        assert_eq!(file_name(&[], Some("attachment; filename=\"..\"")), None);
        assert_eq!(
            file_name(&["https://lmstudio.ai/download/latest/linux/x64".to_string()], None),
            None
        );
        assert_eq!(
            file_name(&["https://example.com/curseforge-latest-linux.zip?x=1".to_string()], None)
                .as_deref(),
            Some("curseforge-latest-linux.zip")
        );
    }

    #[test]
    fn a_response_says_where_it_landed_and_what_the_file_is() {
        let headers = |name: &str| match name {
            "etag" => Some("\"abc\"".to_string()),
            "last-modified" => Some("Sat, 19 Sep 2026 01:52:51 GMT".to_string()),
            "content-length" => Some("1012171087".to_string()),
            _ => None,
        };
        let hops = vec![
            "https://lmstudio.ai/download/latest/linux/x64".to_string(),
            "https://installers.lmstudio.ai/linux/x64/0.4.25-1/LM-Studio-0.4.25-1-x64.AppImage"
                .to_string(),
        ];
        let remote = Remote::from_response(hops, &headers);
        assert_eq!(remote.path, "/linux/x64/0.4.25-1/LM-Studio-0.4.25-1-x64.AppImage");
        assert_eq!(remote.name.as_deref(), Some("LM-Studio-0.4.25-1-x64.AppImage"));
        assert_eq!(remote.version().as_deref(), Some("0.4.25-1"));
        assert_eq!(remote.etag.as_deref(), Some("\"abc\""));
        assert_eq!(remote.modified, Some(1_789_782_771));
        assert_eq!(remote.length, Some(1_012_171_087));

        // One byte by GET, where HEAD was refused: the length of the whole
        // file is in the range.
        let ranged = |name: &str| match name {
            "content-length" => Some("1".to_string()),
            "content-range" => Some("bytes 0-0/150896053".to_string()),
            _ => None,
        };
        let remote = Remote::from_response(vec!["https://e.com/a.AppImage".to_string()], &ranged);
        assert_eq!(remote.length, Some(150_896_053));
        assert_eq!(remote.version(), None);
    }

    #[test]
    fn a_newer_version_in_the_name_is_an_update_and_an_older_one_is_not() {
        let installed = remote("/linux/x64/0.4.25-1/LM-Studio-0.4.25-1-x64.AppImage");
        let newer = remote("/linux/x64/0.4.26-1/LM-Studio-0.4.26-1-x64.AppImage");
        let verdict = judge(&newer, Some(&installed), Some("0.4.25"), None);
        assert!(verdict.available && !verdict.settled, "{verdict:?}");

        // Without a record the installed version is what it is held to.
        assert!(judge(&newer, None, Some("0.4.25"), None).available);
        let verdict = judge(&installed, None, Some("0.4.26"), None);
        assert!(!verdict.available && verdict.settled, "{verdict:?}");
        assert!(verdict.note.unwrap().contains("older"));

        // The same version without a record is the same file.
        assert_eq!(judge(&installed, None, Some("0.4.25"), None), Verdict::new(false, true, None));
    }

    /// What `update --check` said about LM Studio: the installed metadata
    /// says `0.4.25+1`, the link lands on `LM-Studio-0.4.25-1-x64.AppImage`.
    /// That is the same build, and the next one is newer.
    #[test]
    fn the_build_number_in_the_name_is_part_of_the_version() {
        let landed = remote("/linux/x64/0.4.25-1/LM-Studio-0.4.25-1-x64.AppImage");
        assert_eq!(landed.version().as_deref(), Some("0.4.25-1"));
        assert_eq!(judge(&landed, None, Some("0.4.25+1"), None), Verdict::new(false, true, None));

        let rebuilt = remote("/linux/x64/0.4.25-2/LM-Studio-0.4.25-2-x64.AppImage");
        assert!(judge(&rebuilt, None, Some("0.4.25+1"), None).available);
        assert!(judge(&rebuilt, Some(&landed), Some("0.4.25+1"), None).available);

        let next = remote("/linux/x64/0.4.26-1/LM-Studio-0.4.26-1-x64.AppImage");
        assert!(judge(&next, Some(&landed), Some("0.4.25+1"), None).available);
    }

    /// Issue #28, the first decision: a file under the same versioned name
    /// whose `ETag` changed is not downloaded, and the note says why.
    #[test]
    fn a_changed_etag_under_the_same_versioned_name_is_only_a_note() {
        let path = "/builds/Beeper-4.3.160-x86_64.AppImage";
        let installed = with(remote(path), Some("\"a\""), Some(1), Some(10));
        let reuploaded = with(remote(path), Some("\"b\""), Some(2), Some(11));
        let verdict = judge(&reuploaded, Some(&installed), Some("4.3.160"), None);
        assert!(!verdict.available && verdict.settled, "{verdict:?}");
        let note = verdict.note.unwrap();
        assert!(note.contains("Beeper-4.3.160-x86_64.AppImage"), "{note}");
        assert!(note.contains("without a new version"), "{note}");
    }

    #[test]
    fn a_fixed_name_is_judged_by_etag_then_date_then_size() {
        let path = "/downloads/curseforge-latest-linux.AppImage";
        let installed = with(remote(path), Some("\"a\""), Some(100), Some(10));

        let same = with(remote(path), Some("W/\"a\""), Some(200), Some(11));
        assert_eq!(judge(&same, Some(&installed), None, None), Verdict::new(false, true, None));
        let other = with(remote(path), Some("\"b\""), Some(100), Some(10));
        assert!(judge(&other, Some(&installed), None, None).available);

        let installed = with(remote(path), None, Some(100), Some(10));
        assert!(
            judge(&with(remote(path), None, Some(101), Some(10)), Some(&installed), None, None)
                .available
        );
        assert!(
            !judge(&with(remote(path), None, Some(99), Some(11)), Some(&installed), None, None)
                .available
        );

        let installed = with(remote(path), None, None, Some(10));
        assert!(
            judge(&with(remote(path), None, None, Some(11)), Some(&installed), None, None)
                .available
        );
        let verdict =
            judge(&with(remote(path), None, None, Some(10)), Some(&installed), None, None);
        assert!(!verdict.available && verdict.settled, "{verdict:?}");
        assert!(verdict.note.unwrap().contains("only the size"));
    }

    #[test]
    fn another_path_or_name_is_another_file() {
        let installed = with(
            remote("/production/2d29876d/linux/x64/Cursor-3.23.12-x86_64.AppImage"),
            Some("\"a\""),
            None,
            None,
        );
        // The same version, built again from another commit.
        let rebuilt = with(
            remote("/production/9f00aa11/linux/x64/Cursor-3.23.12-x86_64.AppImage"),
            Some("\"a\""),
            None,
            None,
        );
        assert!(judge(&rebuilt, Some(&installed), Some("3.23.12"), None).available);

        let mut renamed = installed.clone();
        renamed.name = Some("Cursor-3.23.12-1-x86_64.AppImage".to_string());
        assert!(judge(&renamed, Some(&installed), Some("3.23.12"), None).available);
    }

    /// A CDN that hands out another host name for the same file has not
    /// changed it. The host is in the hops, never in what is compared.
    #[test]
    fn the_same_path_on_another_host_is_the_same_file() {
        let installed = with(remote("/builds/App.AppImage"), Some("\"a\""), Some(1), Some(10));
        let mut moved = installed.clone();
        moved.hops = vec!["https://edge-7.cdn.example.net/builds/App.AppImage".to_string()];
        assert_eq!(judge(&moved, Some(&installed), None, None), Verdict::new(false, true, None));
    }

    #[test]
    fn without_a_record_the_date_decides() {
        let offered =
            with(remote("/desktop/tutanota-desktop-linux.AppImage"), None, Some(500), None);
        let verdict = judge(&offered, None, None, Some(400));
        assert!(verdict.available && !verdict.settled, "{verdict:?}");
        assert!(verdict.note.unwrap().contains("judged by date"));
        let verdict = judge(&offered, None, None, Some(500));
        assert!(!verdict.available && verdict.settled, "{verdict:?}");
    }

    #[test]
    fn a_server_that_says_nothing_leaves_it_to_an_update() {
        let offered = remote("/download");
        let installed = remote("/download");
        for verdict in [
            judge(&offered, Some(&installed), None, Some(1)),
            judge(&offered, None, None, Some(1)),
            judge(&offered, None, None, None),
        ] {
            assert!(!verdict.available && !verdict.settled, "{verdict:?}");
            assert!(verdict.note.unwrap().contains("re-downloads"));
        }
    }

    #[test]
    fn a_record_follows_the_stamp() {
        let mut entry = DesktopEntry::new();
        let remote = remote("/a.AppImage");
        record_in(&mut entry, Some(&remote));
        assert_eq!(entry.get(KEY_REMOTE), None);

        entry.set(KEY_SHA1, format!("{SHA1} 3 17.000000005"));
        record_in(&mut entry, Some(&remote));
        assert_eq!(entry.get(KEY_REMOTE), Some(remote.record(SHA1).as_str()));

        record_in(&mut entry, None);
        assert_eq!(entry.get(KEY_REMOTE), None);
    }
}
