//! Time, in seconds since 1970. `parse_rfc3339` reads GitHub's `published_at`.

use std::time::{SystemTime, UNIX_EPOCH};

#[must_use]
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Days from the epoch to a civil date (Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `2026-10-26T14:45:12Z`. GitHub's timestamps are always UTC with a `Z`.
#[must_use]
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut date = date.split('-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let mut time = time.split(':').map(str::parse::<i64>);
    let (hour, min, sec) = (time.next()?.ok()?, time.next()?.ok()?, time.next()?.ok()?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    u64::try_from(days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec).ok()
}

/// "12 minutes ago"-style span.
#[must_use]
pub fn span(secs: u64) -> String {
    let plural = |n: u64, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
    match secs {
        0..60 => plural(secs, "second"),
        60..7200 => plural(secs / 60, "minute"),
        7200..172_800 => plural(secs / 3600, "hour"),
        _ => plural(secs / 86_400, "day"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_timestamps_read_as_seconds() {
        assert_eq!(parse_rfc3339("1970-01-02T00:00:01Z"), Some(86_401));
        assert_eq!(parse_rfc3339("2026-10-26T14:45:12Z"), Some(1_793_025_912));
        assert_eq!(parse_rfc3339("2026-10-26 14:45:12"), None);
        assert_eq!(parse_rfc3339("2026-13-26T14:45:12Z"), None);
    }

    #[test]
    fn spans_read_as_words() {
        assert_eq!(span(1), "1 second");
        assert_eq!(span(120), "2 minutes");
        assert_eq!(span(7200), "2 hours");
        assert_eq!(span(172_800), "2 days");
    }
}
