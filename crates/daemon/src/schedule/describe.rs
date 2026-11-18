//! The one-line schedule description plus next due (`describeSchedule`):
//! pure, for lists and headers; interval dues come from the caller.
//!
//! Deliberate divergence: an unparseable stored timestamp renders as the
//! raw stored string with no due instead of JS's `Invalid Date` garbage —
//! validation refuses such values on write, so the path is defensive only.

use super::format::{format_duration, format_wall};
use super::wall::{next_daily_occurrence, weekday_numbers};
use crate::state::schedules::ScheduleState;
use chrono::TimeZone;
use serde_json::Value;

/// Human schedule line, server-local wall time, plus the next due instant.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
/// The human description of a schedule (the `list` line).
pub struct ScheduleDescription {
    pub human: String,
    /// Next due epoch ms, or `None` when the adapter has no schedule.
    pub next_due_at: Option<i64>,
}

/// Describes a schedule against the clock (next due for intervals).
pub fn describe_schedule(
    schedule: &ScheduleState,
    now_ms: i64,
    interval_due_ms: Option<i64>,
) -> ScheduleDescription {
    let options = &schedule.trigger.options;
    let mut human = schedule.trigger.adapter.clone();
    let mut next_due_at = None;
    match schedule.trigger.adapter.as_str() {
        "manual" => human = "on demand".to_owned(),
        "at" => {
            let raw = option_string(options, "at");
            match parse_local_iso(&raw) {
                Some(due) => {
                    human = format!("once at {}", format_wall(due));
                    next_due_at = Some(due);
                }
                None => human = format!("once at {raw}"),
            }
        }
        "window" => {
            let raw_from = option_string(options, "from");
            let raw_to = option_string(options, "to");
            let from_display = match parse_local_iso(&raw_from) {
                Some(from) => {
                    next_due_at = Some(if from > now_ms { from } else { now_ms });
                    format_wall(from)
                }
                None => {
                    // `NaN > now` is false: the due is now.
                    next_due_at = Some(now_ms);
                    raw_from
                }
            };
            let to_display = parse_local_iso(&raw_to).map_or(raw_to.clone(), format_wall);
            human = format!("window {from_display}\u{2013}{to_display}");
        }
        "daily" => {
            let at = option_string(options, "at");
            let suffix = match options.get("days") {
                Some(days) => {
                    let joined = days
                        .as_array()
                        .map(|items| {
                            items
                                .iter()
                                .map(|item| js_string(Some(item)))
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .unwrap_or_default();
                    format!("({joined})")
                }
                None => String::new(),
            };
            // Unknown ids become 99 through the shared mapper — the
            // sentinel that never matches, like the source's `undefined`.
            let mapped = weekday_numbers(options.get("days"));
            let occurrence = next_daily_occurrence(now_ms, &at, mapped.as_deref());
            next_due_at = occurrence.ok().map(|found| found.at_ms);
            human = format!("daily {at}{suffix}");
        }
        "interval" => {
            let seconds = options
                .get("everySeconds")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            human = format!("every {}", format_duration(seconds.saturating_mul(1000)));
            next_due_at = interval_due_ms;
        }
        "filesystem" => {
            human = format!("on changes under {}", option_string(options, "path"));
        }
        "memory_pressure" => {
            human = format!(
                "when memory used \u{2265} {}%",
                option_string(options, "usedPercent")
            );
        }
        _ => {}
    }
    ScheduleDescription { human, next_due_at }
}

/// The stored option as a string, with JS `String(...)` fidelity for the
/// shapes validation allows (`undefined`/missing included).
fn option_string(options: &Value, key: &str) -> String {
    js_string(options.get(key))
}

fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_owned(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) => "null".to_owned(),
        Some(other) => other.to_string(),
    }
}

/// Server-local parse of the stored `YYYY-MM-DDTHH:MM` shape (the source's
/// `Date.parse` on an offset-less local ISO value); seconds are accepted for
/// tolerance. Stored values carry no offset — `normalizeScheduleTrigger`
/// removed it at write time.
pub(super) fn parse_local_iso(raw: &str) -> Option<i64> {
    let naive = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S"))
        .ok()?;
    chrono::Local
        .from_local_datetime(&naive)
        .single()
        .map(|instant| instant.timestamp_millis())
}
