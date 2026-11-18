//! The single home for every schedule-facing duration — the source's own
//! rule (`formatScheduleAge`: "Single home for every schedule-facing
//! duration"). All callers pass non-negative or validation-bounded values;
//! JS `Math.round` rounds halves toward +infinity while Rust rounds away
//! from zero, which is observable only for negatives the callers never
//! produce (interval values are floored at 10 seconds by validation).

use chrono::{Datelike, Local, TimeZone, Timelike};

/// `YYYY-MM-DD HH:MM` in server-local wall time (`toLocalIso` with the
/// separator replaced). Out-of-range instants render as the source's
/// `Invalid Date` rather than panicking.
pub fn format_wall(ms: i64) -> String {
    let Some(instant) = Local.timestamp_millis_opt(ms).single() else {
        return "Invalid Date".to_owned();
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        instant.year(),
        instant.month(),
        instant.day(),
        instant.hour(),
        instant.minute()
    )
}

/// Human age/delay: `45s`, `3h 12m`, `2d 4h`. Negative input clamps to
/// zero (`Math.max(0, Math.round(ms / 1000))`).
pub fn format_schedule_age(ms: i64) -> String {
    let seconds = (ms.max(0) as f64 / 1000.0).round() as i64;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 48 {
        return format!("{hours}h {}m", minutes % 60);
    }
    format!("{}d {}h", hours / 24, hours % 24)
}

/// Compact duration for describe lines: `30m`, `1h`, `2d` (`formatDuration`
/// — no zero clamp; callers pass validation-bounded values).
pub fn format_duration(ms: i64) -> String {
    let seconds = (ms as f64 / 1000.0).round() as i64;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    if seconds < 3600 {
        return format!("{}m", seconds / 60);
    }
    if seconds < 86_400 {
        return format!("{}h", seconds / 3600);
    }
    format!("{}d", seconds / 86_400)
}
