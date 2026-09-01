//! Dates as `2025-10-18`, the only shape appimg ever shows one in. Two
//! formats arrive from the network and both are read here: the timestamps of
//! the GitHub API and the HTTP date a zsync header carries. Neither needs a
//! calendar, only the day has to survive.

const MONTHS: [&str; 12] =
    ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

/// True when the text is a plain `2025-10-18`. Two of those compare as
/// versions do, which is what makes a date usable as one.
pub fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let number = |range: std::ops::Range<usize>| -> Option<u32> {
        text.get(range).and_then(|field| field.parse().ok())
    };
    matches!((number(0..4), number(5..7), number(8..10)), (Some(_), Some(1..=12), Some(1..=31)))
}

/// The day of an RFC 3339 timestamp: `2025-10-18T19:39:55Z` is the release
/// date `2025-10-18`. The time of day never matters, two builds of one day
/// are the same day's build.
pub fn from_timestamp(text: &str) -> Option<String> {
    let day = text.get(..10)?;
    is_date(day).then(|| day.to_string())
}

/// The day of an HTTP date, which is what `zsyncmake` writes into a header:
/// `Sat, 18 Oct 2025 19:39:31 +0000` is `2025-10-18`. Only the three fields
/// that make up the day are read, wherever in the line they sit.
pub fn from_http_date(text: &str) -> Option<String> {
    let fields: Vec<&str> = text.split([' ', '\t', '-']).filter(|f| !f.is_empty()).collect();

    fields.windows(3).find_map(|window| {
        let (Some(day), Some(month), Some(year)) =
            (day_of_month(window[0]), month_number(window[1]), year_number(window[2]))
        else {
            return None;
        };
        Some(format!("{year:04}-{month:02}-{day:02}"))
    })
}

fn day_of_month(field: &str) -> Option<u32> {
    match field.len() {
        1 | 2 => field.parse().ok().filter(|day| (1..=31).contains(day)),
        _ => None,
    }
}

fn month_number(field: &str) -> Option<u32> {
    let name = field.get(..3)?.to_ascii_lowercase();
    MONTHS.iter().position(|month| *month == name).map(|index| index as u32 + 1)
}

fn year_number(field: &str) -> Option<u32> {
    match field.len() {
        4 => field.parse().ok().filter(|year| (1970..=9999).contains(year)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_a_date_and_nothing_else() {
        assert!(is_date("2025-10-18"));
        assert!(!is_date("2025-13-18"));
        assert!(!is_date("2025-10-00"));
        assert!(!is_date("255-a211784"));
        assert!(!is_date("2025-10-18T19:39:55Z"));
        assert!(!is_date("1.2.3"));
    }

    #[test]
    fn a_github_timestamp_keeps_its_day() {
        assert_eq!(from_timestamp("2025-10-18T19:39:55Z").as_deref(), Some("2025-10-18"));
        assert_eq!(from_timestamp("2025-10-18").as_deref(), Some("2025-10-18"));
        assert_eq!(from_timestamp("never"), None);
    }

    #[test]
    fn an_http_date_keeps_its_day() {
        assert_eq!(
            from_http_date("Sat, 18 Oct 2025 19:39:31 +0000").as_deref(),
            Some("2025-10-18")
        );
        // The day is read wherever it sits, including the dashed spelling
        // older servers use.
        assert_eq!(
            from_http_date("Saturday, 18-Oct-2025 19:39:31 GMT").as_deref(),
            Some("2025-10-18")
        );
        assert_eq!(from_http_date("1 Aug 2026").as_deref(), Some("2026-08-01"));
        assert_eq!(from_http_date("no date in here"), None);
        assert_eq!(from_http_date(""), None);
    }
}
