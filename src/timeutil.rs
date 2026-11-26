//! Conversions between Windows FILETIME values, `SystemTime`, and ISO 8601.
//!
//! - FILETIME: 100-nanosecond intervals since 1601-01-01 00:00:00 UTC.
//! - Unix time:  seconds since 1970-01-01 00:00:00 UTC.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds between 1601-01-01 and 1970-01-01.
pub const WINDOWS_EPOCH_DIFF_SECS: i64 = 11_644_473_600;
const TICKS_PER_SEC: u64 = 10_000_000;

/// FILETIME of 1970-01-01 00:00:00 UTC.
const UNIX_EPOCH_TICKS: u64 = WINDOWS_EPOCH_DIFF_SECS as u64 * TICKS_PER_SEC;

/// Converts a `SystemTime` into a FILETIME.
///
/// `None` for times before 1601-01-01 or after the FILETIME range.
pub fn system_time_to_filetime(time: SystemTime) -> Option<u64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => {
            let ticks = u64::try_from(after.as_nanos() / 100).ok()?;
            UNIX_EPOCH_TICKS.checked_add(ticks)
        }
        Err(before) => {
            // Round toward the past so a time just before 1970 stays before it.
            let ticks = u64::try_from(before.duration().as_nanos().div_ceil(100)).ok()?;
            UNIX_EPOCH_TICKS.checked_sub(ticks)
        }
    }
}

/// Converts a FILETIME into Unix seconds (fraction truncated).
pub fn filetime_to_unix_secs(filetime: u64) -> i64 {
    (filetime / TICKS_PER_SEC) as i64 - WINDOWS_EPOCH_DIFF_SECS
}

/// Parses an ISO 8601 date-time into a FILETIME.
///
/// Accepted forms (with optional fractional seconds, `Z` or `±HH:MM` offset):
/// `YYYY-MM-DDTHH:MM:SS`, `YYYY-MM-DDTHH:MM:SSZ`,
/// `YYYY-MM-DDTHH:MM:SS+02:00`, `YYYY-MM-DD` (midnight UTC).
///
/// `None` for invalid text and for times before 1601-01-01 UTC.
pub fn parse_iso8601(value: &str) -> Option<u64> {
    let text = value.trim();
    if text.is_empty() {
        return None;
    }

    let (date_part, rest) = match text.find(['T', ' ', 't']) {
        Some(i) => (&text[..i], &text[i + 1..]),
        None => (text, "00:00:00"),
    };

    let mut date_it = date_part.split('-');
    let year: i64 = date_it.next()?.parse().ok()?;
    let month: i64 = date_it.next()?.parse().ok()?;
    let day: i64 = date_it.next()?.parse().ok()?;
    if date_it.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Split off a trailing `Z` or numeric offset.
    let (time_part, offset_secs) = split_offset(rest)?;

    let mut time_it = time_part.split(':');
    let hour: i64 = time_it.next()?.parse().ok()?;
    let minute: i64 = time_it.next().unwrap_or("0").parse().ok()?;
    let sec_frac = time_it.next().unwrap_or("0");
    let (second, frac_ticks) = split_seconds(sec_frac)?;
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=60).contains(&second) {
        return None;
    }

    // Very large years would overflow the day arithmetic; FILETIME ends in 30828.
    if !(1..=30_828).contains(&year) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let unix_secs = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_secs;
    let windows_secs = u64::try_from(unix_secs + WINDOWS_EPOCH_DIFF_SECS).ok()?;
    windows_secs
        .checked_mul(TICKS_PER_SEC)?
        .checked_add(frac_ticks)
}

/// Returns `(time_part, offset_seconds)` for the text after the date.
fn split_offset(rest: &str) -> Option<(&str, i64)> {
    let rest = rest.trim();
    if let Some(stripped) = rest.strip_suffix(['Z', 'z']) {
        return Some((stripped.trim_end(), 0));
    }
    // A `+HH:MM` / `-HH:MM` offset (a bare `-` can only be part of a time zone
    // here, because the time part itself has no minus sign).
    if let Some(i) = rest.rfind(['+', '-']) {
        let (time, zone) = rest.split_at(i);
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let zone = zone[1..].replace(':', "");
        if zone.len() != 4 || !zone.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let hours: i64 = zone[..2].parse().ok()?;
        let minutes: i64 = zone[2..].parse().ok()?;
        return Some((time, sign * (hours * 3_600 + minutes * 60)));
    }
    Some((rest, 0))
}

/// Returns `(seconds, 100ns ticks of the fraction)`.
fn split_seconds(text: &str) -> Option<(i64, u64)> {
    match text.split_once('.') {
        None => Some((text.parse().ok()?, 0)),
        Some((secs, frac)) => {
            let secs: i64 = secs.parse().ok()?;
            if frac.is_empty() || !frac.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            // 7 fractional digits = 100ns ticks; pad or truncate.
            let padded: String = format!("{:0<7}", frac);
            let ticks: u64 = padded[..7].parse().ok()?;
            Some((secs, ticks))
        }
    }
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_round_trip_known_value() {
        // 2026-09-23T21:31:00Z
        assert_eq!(parse_iso8601("2026-09-23T21:31:00").unwrap(), 134_346_726_600_000_000);
        assert_eq!(parse_iso8601("2026-09-23T21:31:00Z").unwrap(), 134_346_726_600_000_000);
    }

    #[test]
    fn iso_with_offset() {
        let utc = parse_iso8601("2026-09-23T21:31:00Z").unwrap();
        let moscow = parse_iso8601("2026-09-24T00:31:00+03:00").unwrap();
        assert_eq!(utc, moscow);
    }

    #[test]
    fn iso_date_only() {
        let midnight = parse_iso8601("2026-09-23").unwrap();
        assert_eq!(filetime_to_unix_secs(midnight), 1_790_121_600);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_iso8601("").is_none());
        assert!(parse_iso8601("not a date").is_none());
        assert!(parse_iso8601("2026-13-01").is_none());
    }

    #[test]
    fn system_time_to_filetime_epoch() {
        let epoch = UNIX_EPOCH;
        assert_eq!(system_time_to_filetime(epoch).unwrap(), 116_444_736_000_000_000);
    }

    #[test]
    fn system_time_before_1970_is_supported() {
        use std::time::Duration;
        let day_before = UNIX_EPOCH - Duration::from_secs(86_400);
        assert_eq!(
            system_time_to_filetime(day_before),
            Some(116_444_736_000_000_000 - 86_400 * TICKS_PER_SEC)
        );
        // Windows stores SystemTime in 100 ns ticks, so step by whole ticks.
        let one_tick = UNIX_EPOCH - Duration::from_nanos(100);
        assert_eq!(system_time_to_filetime(one_tick), Some(116_444_735_999_999_999));
        let windows_epoch = UNIX_EPOCH - Duration::from_secs(WINDOWS_EPOCH_DIFF_SECS as u64);
        assert_eq!(system_time_to_filetime(windows_epoch), Some(0));
        // Windows cannot represent this time at all; other platforms can.
        if let Some(before_1601) = windows_epoch.checked_sub(Duration::from_secs(1)) {
            assert_eq!(system_time_to_filetime(before_1601), None);
        }
    }

    #[test]
    fn iso_before_1601_is_rejected() {
        assert_eq!(parse_iso8601("1601-01-01T00:00:00Z"), Some(0));
        assert!(parse_iso8601("1600-12-31T23:59:59Z").is_none());
        assert!(parse_iso8601("1601-01-01T00:30:00+01:00").is_none());
        assert!(parse_iso8601("0000-01-01").is_none());
        assert!(parse_iso8601("99999999999-01-01").is_none());
        assert_eq!(
            parse_iso8601("1969-12-31T23:59:59Z"),
            Some(116_444_736_000_000_000 - TICKS_PER_SEC)
        );
    }
}
