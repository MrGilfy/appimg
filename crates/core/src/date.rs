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

/// An HTTP date to the second, as seconds since the epoch: what a
/// `Last-Modified` header says, `Sat, 19 Sep 2026 01:52:51 GMT`. HTTP dates
/// are always in GMT. The day is found the way [`from_http_date`] finds it,
/// the time is the `hh:mm:ss` behind it. The asctime spelling, which puts
/// the year last, is not read, nor is a two-digit year.
pub fn http_date_seconds(text: &str) -> Option<i64> {
    let fields: Vec<&str> = text.split([' ', '\t', '-', ',']).filter(|f| !f.is_empty()).collect();

    fields.windows(4).find_map(|window| {
        let (Some(day), Some(month), Some(year), Some(time)) = (
            day_of_month(window[0]),
            month_number(window[1]),
            year_number(window[2]),
            time_of_day(window[3]),
        ) else {
            return None;
        };
        Some(days_since_epoch(year, month, day) * 86_400 + time)
    })
}

/// `01:52:51` as seconds since midnight.
fn time_of_day(field: &str) -> Option<i64> {
    let mut parts = field.split(':');
    let mut next = |max: i64| -> Option<i64> {
        let part = parts.next()?;
        let value: i64 = part.parse().ok().filter(|_| part.len() == 2)?;
        (0..=max).contains(&value).then_some(value)
    };
    // 60 for a leap second, which HTTP allows.
    let (hours, minutes, seconds) = (next(23)?, next(59)?, next(60)?);
    parts.next().is_none().then_some(hours * 3600 + minutes * 60 + seconds)
}

/// Days from 1970-01-01 to the given day, after Howard Hinnant's
/// `days_from_civil`. Years before 1970 never get here.
fn days_since_epoch(year: u32, month: u32, day: u32) -> i64 {
    let year = i64::from(year) - i64::from(month <= 2);
    let era = year / 400;
    let year_of_era = year - era * 400;
    let month_from_march = i64::from((month + 9) % 12);
    let day_of_year = (153 * month_from_march + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
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

    #[test]
    fn an_http_date_to_the_second() {
        assert_eq!(http_date_seconds("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(http_date_seconds("Sat, 19 Sep 2026 01:52:51 GMT"), Some(1_789_782_771));
        // Across a leap day and a century, in the dashed spelling.
        assert_eq!(http_date_seconds("Wednesday, 01-Mar-2000 00:00:00 GMT"), Some(951_868_800));
        for text in [
            "Sat, 19 Sep 2026 GMT",
            "Sat, 19 Sep 2026 1:52:51 GMT",
            "Sat, 19 Sep 2026 24:00:00 GMT",
            "Sat, 19 Sep 26 01:52:51 GMT",
            "Sun Nov  6 08:49:37 1994",
            "",
        ] {
            assert_eq!(http_date_seconds(text), None, "{text:?}");
        }
    }
}
