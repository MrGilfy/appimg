use std::cmp::Ordering;

#[derive(Debug, PartialEq, Eq)]
enum Segment {
    Number(u64),
    Text(String),
}

/// Compares two loose version strings. Numeric runs compare numerically,
/// everything else lexically. Separators are ignored. This is not semver, it
/// only has to order the version strings AppImages actually ship.
pub fn compare(a: &str, b: &str) -> Ordering {
    let left = segments(a);
    let right = segments(b);

    for i in 0..left.len().max(right.len()) {
        match (left.get(i), right.get(i)) {
            (Some(l), Some(r)) => match compare_segments(l, r) {
                Ordering::Equal => continue,
                other => return other,
            },
            // Trailing zeros do not change a version: 1.2 equals 1.2.0.
            (Some(Segment::Number(0)), None) => continue,
            (None, Some(Segment::Number(0))) => continue,
            // A trailing text segment marks a prerelease: 1.0.0-rc is older
            // than 1.0.0, while an extra number is newer.
            (Some(Segment::Text(_)), None) => return Ordering::Less,
            (None, Some(Segment::Text(_))) => return Ordering::Greater,
            (Some(_), None) => return Ordering::Greater,
            (None, Some(_)) => return Ordering::Less,
            (None, None) => break,
        }
    }
    Ordering::Equal
}

/// True when `candidate` is strictly newer than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    compare(candidate, current) == Ordering::Greater
}

/// Words a project uses for a release that keeps moving instead of one that
/// was cut once. A tag out of this list is a pointer, not a version.
const ROLLING_WORDS: &[&str] = &[
    "continuous",
    "nightly",
    "daily",
    "weekly",
    "snapshot",
    "rolling",
    "latest",
    "edge",
    "unstable",
    "canary",
    "dev",
    "devel",
    "head",
    "tip",
    "master",
    "main",
    "trunk",
    "prerelease",
];

/// How many characters of a commit hash are kept, the length git itself
/// abbreviates to. A release names the full hash and a build the short one,
/// so both have to end up the same.
const COMMIT_LENGTH: usize = 7;

/// True when the text carries a dotted number, which is the only shape that
/// reliably names a version: `2.0.0-alpha-1-20251018` does, and neither
/// `255-a211784` nor the `x86_64` of a file name does.
pub fn names_a_version(text: &str) -> bool {
    extract(text).is_some_and(|version| version.contains('.'))
}

/// True when a tag or a version names a build that keeps moving rather than
/// a release. Two things have to hold, and the second one is what keeps a
/// real version out of here:
///
/// 1. it names no version, so anything with a dotted number is out, however
///    much else it carries;
/// 2. it carries a marker of a moving build, either one of [`ROLLING_WORDS`]
///    or an abbreviated commit hash.
///
/// That is what tells the `continuous` tag and the `255-a211784` of an
/// AppImageUpdate build from the `20251018` of a date-stamped release, which
/// keeps being treated as the version it is.
pub fn is_rolling(text: &str) -> bool {
    !names_a_version(text) && tokens(text).any(|token| is_rolling_word(token) || is_commit(token))
}

/// The commit a rolling version was built from, abbreviated: `a211784` out
/// of `255-a211784`, and the same out of the full hash a GitHub release
/// names.
pub fn short_commit(text: &str) -> Option<String> {
    tokens(text).find_map(commit_of).map(|hash| hash[..COMMIT_LENGTH].to_lowercase())
}

/// Whether two version strings can be ordered against each other at all.
/// A build id against a version orders by nothing better than how the two
/// happen to be spelled, so the answer would be a guess.
pub fn comparable(a: &str, b: &str) -> bool {
    is_rolling(a) == is_rolling(b)
}

/// What to show for a version string. A rolling build declares a build
/// number and the commit it came from, and the build number is worth
/// nothing to anyone: two AppImages out of one continuous release differ in
/// it while being the same build. Only the commit survives, and a caller
/// that knows the release date replaces even that. Anything that names a
/// version is left exactly as it is.
pub fn display(version: &str) -> String {
    if !is_rolling(version) {
        return version.to_string();
    }
    short_commit(version).unwrap_or_else(|| version.to_string())
}

/// Pulls a version out of a file or tag name, e.g. `v1.2.3` or
/// `App-1.2.3-x86_64.AppImage` both yield `1.2.3`.
pub fn extract(text: &str) -> Option<String> {
    let mut plain_number = None;

    for token in text.split(['-', '_', ' ', '/']) {
        let candidate = token.trim_start_matches(['v', 'V']);
        if !candidate.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let end =
            candidate.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(candidate.len());
        let version = candidate[..end].trim_end_matches('.');
        if version.contains('.') {
            return Some(version.to_string());
        }
        // Bare numbers like the `x86` in `x86_64` are a poor guess, so they
        // only serve as a fallback when nothing dotted shows up.
        if plain_number.is_none() && !version.is_empty() {
            plain_number = Some(version.to_string());
        }
    }
    plain_number
}

/// The alphanumeric words of a version string, whatever separates them.
fn tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_ascii_alphanumeric()).filter(|token| !token.is_empty())
}

fn is_rolling_word(token: &str) -> bool {
    ROLLING_WORDS.iter().any(|word| token.eq_ignore_ascii_case(word))
}

fn is_commit(token: &str) -> bool {
    commit_of(token).is_some()
}

/// The hash inside a token that is one: hexadecimal, at least as long as
/// git abbreviates to, and carrying a letter, so a build number like `255`
/// or a date stamp like `20251018` is never mistaken for a commit. A
/// leading `g` is what `git describe` puts in front of the hash.
fn commit_of(token: &str) -> Option<&str> {
    let hash = token.strip_prefix(['g', 'G']).unwrap_or(token);
    let hexadecimal = (COMMIT_LENGTH..=40).contains(&hash.len())
        && hash.chars().all(|c| c.is_ascii_hexdigit())
        && hash.chars().any(|c| c.is_ascii_alphabetic());
    hexadecimal.then_some(hash)
}

fn compare_segments(a: &Segment, b: &Segment) -> Ordering {
    match (a, b) {
        (Segment::Number(l), Segment::Number(r)) => l.cmp(r),
        (Segment::Text(l), Segment::Text(r)) => l.cmp(r),
        // A numeric segment beats a textual one, so 1.0 sorts above 1.0-rc.
        (Segment::Number(_), Segment::Text(_)) => Ordering::Greater,
        (Segment::Text(_), Segment::Number(_)) => Ordering::Less,
    }
}

fn segments(version: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut chars = version.trim().trim_start_matches(['v', 'V']).chars().peekable();

    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            let mut digits = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_digit() {
                    digits.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            // Overflowing versions fall back to a textual comparison.
            match digits.parse::<u64>() {
                Ok(n) => out.push(Segment::Number(n)),
                Err(_) => out.push(Segment::Text(digits)),
            }
        } else if c.is_alphanumeric() {
            let mut text = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_alphanumeric() && !c.is_ascii_digit() {
                    text.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            out.push(Segment::Text(text.to_lowercase()));
        } else {
            chars.next();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_numeric_versions() {
        assert_eq!(compare("1.2.3", "1.2.4"), Ordering::Less);
        assert_eq!(compare("1.10.0", "1.9.0"), Ordering::Greater);
        assert_eq!(compare("2.0.0", "2.0.0"), Ordering::Equal);
    }

    #[test]
    fn ignores_a_leading_v() {
        assert_eq!(compare("v1.2.3", "1.2.3"), Ordering::Equal);
        assert!(is_newer("v2.0.0", "1.9.9"));
    }

    #[test]
    fn treats_missing_trailing_zeros_as_equal() {
        assert_eq!(compare("1.2", "1.2.0"), Ordering::Equal);
        assert_eq!(compare("1.2.0.0", "1.2"), Ordering::Equal);
    }

    #[test]
    fn shorter_version_loses_against_extra_segments() {
        assert_eq!(compare("1.2", "1.2.1"), Ordering::Less);
    }

    #[test]
    fn prereleases_sort_below_the_release() {
        assert_eq!(compare("1.0.0-rc1", "1.0.0"), Ordering::Less);
        assert!(is_newer("1.0.0", "1.0.0-beta"));
    }

    #[test]
    fn numbers_compare_numerically_not_lexically() {
        assert!(is_newer("1.0.10", "1.0.9"));
    }

    #[test]
    fn a_continuous_build_is_recognised_as_one() {
        // The two AppImages of one AppImageUpdate continuous release: same
        // commit, different build number, no version anywhere.
        assert!(is_rolling("255-a211784"));
        assert!(is_rolling("254-a211784"));
        assert!(is_rolling("continuous"));
        assert!(is_rolling("nightly-20251018"));
        assert!(is_rolling("a211784dfc746fdb6d8d32d6bb39add451c1ddeb"));
    }

    #[test]
    fn a_version_stays_a_version_however_it_is_spelled() {
        assert!(!is_rolling("1.2.3"));
        assert!(!is_rolling("v2.0.0"));
        assert!(!is_rolling("2.0.0-alpha-1-20251018"));
        assert!(!is_rolling("1.0.0-rc1"));
        // A date-stamped release names no commit and no moving channel, so
        // it keeps being the version it is.
        assert!(!is_rolling("20251018"));
        assert!(!is_rolling("2025-10-18"));
        // Nor is a plain build number one.
        assert!(!is_rolling("255"));
    }

    #[test]
    fn the_commit_survives_a_rolling_version() {
        assert_eq!(short_commit("255-a211784").as_deref(), Some("a211784"));
        // The full hash a release names abbreviates to the same thing.
        assert_eq!(
            short_commit("a211784dfc746fdb6d8d32d6bb39add451c1ddeb").as_deref(),
            Some("a211784")
        );
        assert_eq!(short_commit("1.2.3-4-gABC1234").as_deref(), Some("abc1234"));
        assert_eq!(short_commit("continuous"), None);
        assert_eq!(short_commit("255"), None);
    }

    #[test]
    fn showing_a_rolling_version_drops_the_build_number() {
        // The build number is what makes two AppImages of one release look
        // like different versions, so it is the part that goes.
        assert_eq!(display("255-a211784"), "a211784");
        assert_eq!(display("254-a211784"), "a211784");
        // Nothing to reduce it to, so it stays as it is.
        assert_eq!(display("continuous"), "continuous");
        assert_eq!(display("1.2.3"), "1.2.3");
        assert_eq!(display("2025-10-18"), "2025-10-18");
    }

    #[test]
    fn two_dates_order_the_older_below_the_newer() {
        assert!(is_newer("2025-11-02", "2025-10-18"));
        assert!(is_newer("2025-10-19", "2025-10-18"));
        assert!(!is_newer("2025-10-18", "2025-10-18"));
        assert!(!is_newer("2024-12-31", "2025-01-01"));
        assert_eq!(compare("2025-10-18", "2025-10-18"), Ordering::Equal);
        assert!(comparable("2025-10-18", "2025-11-02"));
    }

    #[test]
    fn a_build_id_and_a_version_are_not_compared() {
        assert!(!comparable("a211784", "2.0.0"));
        assert!(!comparable("255-a211784", "2025-10-18"));
        assert!(comparable("1.2.3", "2.0.0"));
    }

    #[test]
    fn only_a_dotted_number_names_a_version() {
        assert!(names_a_version("App-1.2.3-x86_64.AppImage"));
        assert!(names_a_version("2.0.0-alpha-1-20251018"));
        // The `64` of the architecture is not a version, and reading one
        // out of this name is the guess this avoids.
        assert!(!names_a_version("AppImageUpdate-x86_64.AppImage"));
        assert!(!names_a_version("255-a211784"));
    }

    #[test]
    fn extracts_versions_from_names() {
        assert_eq!(extract("v1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(extract("App-1.2.3-x86_64.AppImage").as_deref(), Some("1.2.3"));
        assert_eq!(extract("nightly").as_deref(), None);
    }
}
