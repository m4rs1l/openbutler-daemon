//! Wall-clock scheduling: server-local wall time is the only time
//! (`nextDailyOccurrence`..`wallMatches` in the source).
//!
//! Deliberate divergences: the source's `timeZone` parameters and zoned
//! branches exist only for its test fixtures — production always passes
//! `undefined` and reads the server clock, so they are not ported. Gap
//! handling relies on the platform tzdb (`from_local_datetime` is `None`
//! inside a gap, `Ambiguous` takes the earliest instant, matching V8's
//! constructor choice) and the source's own round-trip verification decides
//! whether an instant is acceptable.

use chrono::{Datelike, Local, NaiveDate, TimeZone, Timelike};

/// `WEEKDAY_NUMBERS`: weekday ids to `Date.getDay()` numbers, Sunday = 0.
/// Unknown ids map to `None` (the source's `undefined` never matches a set).
pub fn weekday_number(day: &str) -> Option<i64> {
    Some(match day.to_ascii_lowercase().as_str() {
        "sun" => 0,
        "mon" => 1,
        "tue" => 2,
        "wed" => 3,
        "thu" => 4,
        "fri" => 5,
        "sat" => 6,
        _ => return None,
    })
}

/// The source's `days.map((day) => WEEKDAY_NUMBERS[day.toLowerCase()]!)`
/// over a stored options value: `None` when the key is absent (every day),
/// `Some` otherwise with unknown ids as 99 — the sentinel that never
/// matches a real weekday, exactly like the source's `undefined` in a Set.
pub(super) fn weekday_numbers(days: Option<&serde_json::Value>) -> Option<Vec<i64>> {
    let days = days?;
    Some(
        days.as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|day| day.as_str().and_then(weekday_number).unwrap_or(99))
                    .collect()
            })
            .unwrap_or_default(),
    )
}

/// Absolute epoch ms of one occurrence plus the source's DST-gap marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WallOccurrence {
    pub at_ms: i64,
    /// True when a daylight-saving gap moved the occurrence forward.
    pub shifted: bool,
}

/// Calendar fields in the source's shape (`mo` is 0-based, `weekday` is
/// `Date.getDay()`).
struct WallDate {
    y: i32,
    mo: u32,
    d: u32,
    weekday: i64,
}

const UNRESOLVED: &str = "Wall-clock time could not be resolved.";

/// Next daily occurrence of a wall-clock time. `days` holds `Date.getDay()`
/// numbers and filters weekdays; `None` or an empty list means every day.
/// Errors are the source's two messages, unchanged.
pub fn next_daily_occurrence(
    now_ms: i64,
    at: &str,
    days: Option<&[i64]>,
) -> Result<WallOccurrence, String> {
    let (hours, minutes) =
        parse_clock(at).ok_or_else(|| "daily.at must be \"HH:MM\".".to_owned())?;
    let wanted: Option<std::collections::HashSet<i64>> = days
        .filter(|list| !list.is_empty())
        .map(|list| list.iter().copied().collect());
    let today = wall_date_of(now_ms).ok_or_else(|| UNRESOLVED.to_owned())?;
    for step in 0..8_i64 {
        let date = add_wall_days(&today, step).ok_or_else(|| UNRESOLVED.to_owned())?;
        if let Some(wanted) = &wanted
            && !wanted.contains(&date.weekday)
        {
            continue;
        }
        let resolved = resolve_wall_time(&date, hours, minutes)?;
        if resolved.at_ms > now_ms {
            return Ok(resolved);
        }
    }
    Err("daily trigger has no selected weekday.".to_owned())
}

/// The source's `HH:MM` regex, hand-parsed with identical acceptance: five
/// ASCII digits, `:` in the middle, hours `00-23`, minutes `00-59`.
fn parse_clock(at: &str) -> Option<(u32, u32)> {
    let bytes = at.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return None;
    }
    let digit = |byte: u8| {
        if byte.is_ascii_digit() {
            Some(u32::from(byte - b'0'))
        } else {
            None
        }
    };
    let hours = digit(bytes[0])? * 10 + digit(bytes[1])?;
    let minutes = digit(bytes[3])? * 10 + digit(bytes[4])?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some((hours, minutes))
}

/// Resolves a wall time to an instant, stepping past daylight-saving gaps.
/// Never skipped: the walk tries the requested minute, then +30-minute
/// steps up to eight times.
fn resolve_wall_time(date: &WallDate, hours: u32, minutes: u32) -> Result<WallOccurrence, String> {
    let start = i64::from(hours) * 60 + i64::from(minutes);
    for step in 0..=8_i64 {
        if let Some(at_ms) = try_wall_minutes(date, start + step * 30) {
            return Ok(WallOccurrence {
                at_ms,
                shifted: step > 0,
            });
        }
    }
    Err(UNRESOLVED.to_owned())
}

fn try_wall_minutes(date: &WallDate, total: i64) -> Option<i64> {
    let day_step = total.div_euclid(1440);
    let rest = total - day_step * 1440;
    let wall_date = add_wall_days(date, day_step)?;
    let hours = u32::try_from(rest / 60).ok()?;
    let minutes = u32::try_from(rest % 60).ok()?;
    let candidate = wall_to_ms(&wall_date, hours, minutes)?;
    wall_matches(candidate, &wall_date, hours, minutes).then_some(candidate)
}

/// Wall time to instant in server-local time: the platform constructor
/// resolves gaps forward (no instant exists inside a gap), the caller
/// verifies by round trip — the same contract as the source.
fn wall_to_ms(date: &WallDate, hours: u32, minutes: u32) -> Option<i64> {
    let day = NaiveDate::from_ymd_opt(date.y, date.mo + 1, date.d)?;
    let naive = day.and_hms_opt(hours, minutes, 0)?;
    match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(instant) => Some(instant.timestamp_millis()),
        chrono::LocalResult::Ambiguous(first, _) => Some(first.timestamp_millis()),
        // A gap: no instant reads back as this wall time.
        chrono::LocalResult::None => None,
    }
}

/// Pure-calendar day arithmetic: no clock, no zone, no drift.
fn add_wall_days(base: &WallDate, step: i64) -> Option<WallDate> {
    let day = NaiveDate::from_ymd_opt(base.y, base.mo + 1, base.d)?;
    let shifted = day.checked_add_signed(chrono::Duration::days(step))?;
    Some(WallDate {
        y: shifted.year(),
        mo: shifted.month0(),
        d: shifted.day(),
        weekday: i64::from(shifted.weekday().num_days_from_sunday()),
    })
}

fn wall_date_of(ms: i64) -> Option<WallDate> {
    let instant = Local.timestamp_millis_opt(ms).single()?;
    Some(WallDate {
        y: instant.year(),
        mo: instant.month0(),
        d: instant.day(),
        weekday: i64::from(instant.weekday().num_days_from_sunday()),
    })
}

/// Round-trip verification: the instant must read back as the requested
/// wall fields. This is what rejects a DST-gap candidate.
fn wall_matches(ms: i64, date: &WallDate, hours: u32, minutes: u32) -> bool {
    let Some(instant) = Local.timestamp_millis_opt(ms).single() else {
        return false;
    };
    instant.year() == date.y
        && instant.month0() == date.mo
        && instant.day() == date.d
        && instant.hour() == hours
        && instant.minute() == minutes
}
