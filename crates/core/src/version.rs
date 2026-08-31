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
    fn extracts_versions_from_names() {
        assert_eq!(extract("v1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(extract("App-1.2.3-x86_64.AppImage").as_deref(), Some("1.2.3"));
        assert_eq!(extract("nightly").as_deref(), None);
    }
}
