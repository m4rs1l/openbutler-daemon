//! `usage.report`: the usage screen's whole state, computed here.
//!
//! Sources (read 2026-11-26): `cli/usage.ts` (the `--days` window and its
//! default, the label, the session `slice(0, 8)`, the `provider/model`
//! split), `usage/report.ts` `bucketUsage` / `summarizeUsage` /
//! `renderUsageReport` and the formatters (`formatTokens`, `formatCost`,
//! `formatDuration`, `sparkline`).
//!
//! The daemon answers with decisions — the label, the summary, the day
//! buckets **and the finished lines** — and the CLI only places them on
//! the page. `--json` hands the same computed `label`/`summary`/`days`
//! back as the machine envelope; nothing is re-derived client-side.
//!
//! New daemon contract (the source computed this in the CLI process, a
//! backend-owned violation): the payload is
//! `{days?, session?, model?}`; `days` defaults to 30 and must be a whole
//! number in `1..=3650` (`days must be a whole number between 1 and 3650.`),
//! `session`/`model` are optional non-empty strings. The answer is
//! `{label, summary, days, lines}`.
//!
//! One deliberate divergence, shared with `config.report`: the source
//! opened the ledger from the client process; the daemon reads its own
//! `state.db` read-only (the same numbers, one owner).

use std::collections::{BTreeMap, HashSet};

use crate::shape::to_fixed;
use std::path::Path;

use chrono::TimeZone;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::ledger;

use super::require;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const DEFAULT_DAYS: i64 = 30;
const MAX_DAYS: i64 = 3650;
/// The session/model filter size gate — well beyond any id either field
/// carries, and small enough that a hostile payload cannot bloat a query.
const MAX_FILTER_BYTES: usize = 512;

/// `UsageTotals` — summed counters, one bucket.
#[derive(Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Totals {
    requests: i64,
    fresh_input_tokens: i64,
    input_tokens: i64,
    output_tokens: i64,
    reasoning_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cost_usd: f64,
    duration_ms: i64,
    tokens: i64,
}

/// `UsageDay` — one local calendar day and its totals.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageDay {
    day: String,
    #[serde(flatten)]
    totals: Totals,
}

/// `UsageSummary` — the window's numbers, in the source's field order.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
    days: usize,
    requests: i64,
    fresh_input_tokens: i64,
    input_tokens: i64,
    output_tokens: i64,
    reasoning_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cost_usd: f64,
    busiest: Option<UsageDay>,
    longest_task_ms: i64,
    streak_days: i64,
    priced_rows: i64,
    estimated_rows: i64,
    unknown_rows: i64,
    unpriced_rows: i64,
}

/// Builds the report. `Err` carries a client-visible sentence.
pub fn report(
    data_dir: &Path,
    request: &openbutler_protocol::DaemonRequest,
) -> Result<Value, String> {
    let payload = &request.payload;
    let days = days_of(payload)?;
    let session = optional_string(payload, "session")?;
    let model = optional_string(payload, "model")?;
    // `--model provider/model` splits once; a bare provider filters by
    // provider only, exactly like the source's `split('/', 2)`.
    let (provider, model_id) = match model.as_deref() {
        Some(value) => match value.split_once('/') {
            Some((provider, model_id)) => (Some(provider.to_owned()), Some(model_id.to_owned())),
            None => (Some(value.to_owned()), None),
        },
        None => (None, None),
    };
    let since_ms = ledger::now_ms() - days * DAY_MS;
    let rows = ledger::usage_report_rows(
        data_dir,
        since_ms,
        session.as_deref(),
        provider.as_deref(),
        model_id.as_deref(),
    )
    .map_err(|error| format!("Could not read usage rows: {error}"))?;

    let buckets = bucket_usage(&rows);
    let summary = summarize(&buckets, &rows);
    let mut scope_parts: Vec<String> = Vec::new();
    if let Some(session) = &session {
        let head: String = session.chars().take(8).collect();
        scope_parts.push(format!("session {head}"));
    }
    if let Some(model) = &model {
        scope_parts.push(format!("model {model}"));
    }
    let label = if scope_parts.is_empty() {
        format!("last {days} days")
    } else {
        format!("last {days} days ({})", scope_parts.join(", "))
    };
    let lines = render(&buckets, &summary, &label);
    Ok(json!({
        "label": label,
        "summary": summary,
        "days": buckets,
        "lines": lines,
    }))
}

/// `days`: absent or `null` takes the source default; anything else must
/// be a whole number the source's guard would accept (`cli/usage.ts`
/// L18).
fn days_of(payload: &Map<String, Value>) -> Result<i64, String> {
    const REFUSAL: &str = "days must be a whole number between 1 and 3650.";
    match payload.get("days") {
        None | Some(Value::Null) => Ok(DEFAULT_DAYS),
        Some(value) => match value.as_i64() {
            Some(days) if (1..=MAX_DAYS).contains(&days) => Ok(days),
            _ => Err(REFUSAL.to_owned()),
        },
    }
}

/// An optional filter: absent or `null` means "no filter", anything else
/// must pass the envelope's own string gate.
fn optional_string(
    payload: &Map<String, Value>,
    name: &'static str,
) -> Result<Option<String>, String> {
    match payload.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => require(Some(value), name, MAX_FILTER_BYTES).map(Some),
    }
}

/// The local day a row belongs to, using the offset recorded with the
/// row (never this machine's clock): shift, then read the UTC date.
fn local_day(at_ms: i64, tz_offset_min: i64) -> String {
    let shifted = at_ms + tz_offset_min * 60_000;
    chrono::Utc
        .timestamp_millis_opt(shifted)
        .single()
        .map(|moment| moment.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn add_row(bucket: &mut Totals, row: &ledger::UsageReportRow) {
    // The prompt count already includes cache hits and writes, so the
    // fresh part is what is left; showing both without this split double
    // counts.
    let cache_read = row.cache_read_tokens.unwrap_or(0);
    let cache_write = row.cache_write_tokens.unwrap_or(0);
    let input = row.input_tokens.unwrap_or(0);
    bucket.requests += 1;
    bucket.fresh_input_tokens += row
        .no_cache_tokens
        .unwrap_or((input - cache_read - cache_write).max(0));
    bucket.input_tokens += input;
    bucket.output_tokens += row.output_tokens.unwrap_or(0);
    bucket.reasoning_tokens += row.reasoning_tokens.unwrap_or(0);
    bucket.cache_read_tokens += cache_read;
    bucket.cache_write_tokens += cache_write;
    bucket.cost_usd += row.cost_usd.unwrap_or(0.0);
    bucket.duration_ms += row.duration_ms.unwrap_or(0);
    // Derived last so the three parts can never disagree with the total.
    bucket.tokens = bucket.fresh_input_tokens + bucket.cache_read_tokens + bucket.output_tokens;
}

/// Groups rows into local days, sorted ascending (BTreeMap: the sort is
/// the map's order).
fn bucket_usage(rows: &[ledger::UsageReportRow]) -> Vec<UsageDay> {
    let mut groups: BTreeMap<String, Totals> = BTreeMap::new();
    for row in rows {
        let day = local_day(row.at_ms, row.tz_offset_min);
        add_row(groups.entry(day.clone()).or_default(), row);
    }
    groups
        .into_iter()
        .map(|(day, totals)| UsageDay { day, totals })
        .collect()
}

/// The window's summary (the source's `summarizeUsage`).
fn summarize(days: &[UsageDay], rows: &[ledger::UsageReportRow]) -> Summary {
    let mut summary = Summary {
        days: days.len(),
        requests: 0,
        fresh_input_tokens: 0,
        input_tokens: 0,
        output_tokens: 0,
        reasoning_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cost_usd: 0.0,
        busiest: None,
        longest_task_ms: 0,
        streak_days: 0,
        priced_rows: 0,
        estimated_rows: 0,
        unknown_rows: 0,
        unpriced_rows: 0,
    };
    for day in days {
        summary.requests += day.totals.requests;
        summary.fresh_input_tokens += day.totals.fresh_input_tokens;
        summary.input_tokens += day.totals.input_tokens;
        summary.output_tokens += day.totals.output_tokens;
        summary.reasoning_tokens += day.totals.reasoning_tokens;
        summary.cache_read_tokens += day.totals.cache_read_tokens;
        summary.cache_write_tokens += day.totals.cache_write_tokens;
        summary.cost_usd += day.totals.cost_usd;
        // Strictly greater: ties keep the earliest day, as in the source.
        if summary
            .busiest
            .as_ref()
            .is_none_or(|current| day.totals.requests > current.totals.requests)
        {
            summary.busiest = Some(day.clone());
        }
    }
    for row in rows {
        summary.longest_task_ms = summary.longest_task_ms.max(row.duration_ms.unwrap_or(0));
        match row.cost_source.as_deref() {
            Some("provider-inline") | Some("provider-lookup") => summary.priced_rows += 1,
            Some("catalog-estimate") => {
                summary.estimated_rows += 1;
                summary.unpriced_rows += 1;
            }
            _ => {
                summary.unknown_rows += 1;
                summary.unpriced_rows += 1;
            }
        }
    }
    // A streak counts back from the newest recorded day without a gap.
    let recorded: HashSet<&str> = days
        .iter()
        .filter(|day| day.totals.requests > 0)
        .map(|day| day.day.as_str())
        .collect();
    if let Some(newest) = days.last().map(|day| day.day.as_str())
        && let Ok(mut cursor) = chrono::NaiveDate::parse_from_str(newest, "%Y-%m-%d")
    {
        while recorded.contains(cursor.format("%Y-%m-%d").to_string().as_str()) {
            summary.streak_days += 1;
            match cursor.pred_opt() {
                Some(previous) => cursor = previous,
                None => break,
            }
        }
    }
    summary
}

/// The token count a person can read at a glance (41.5M, 49.5k, 812),
/// with the source's rounding: `toFixed` rounds half away from zero, so
/// the multiply/round/divide runs in f64 first.
fn format_tokens(value: i64) -> String {
    let value = value as f64;
    if value >= 1_000_000.0 {
        format!("{}M", to_fixed(value / 1_000_000.0, 1))
    } else if value >= 1_000.0 {
        format!("{}k", to_fixed(value / 1_000.0, 1))
    } else {
        format!("{}", value.round() as i64)
    }
}

fn format_cost(value: f64) -> String {
    format!("${}", to_fixed(value, 4))
}

fn format_duration(ms: i64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let seconds = ms as f64 / 1000.0;
    if seconds < 60.0 {
        return format!("{}s", seconds.round() as i64);
    }
    let minutes = (seconds / 60.0) as i64;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    format!("{}h {}m", minutes / 60, minutes % 60)
}

const SPARK: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// One column per value, scaled to the largest, so a month reads as a
/// shape.
fn sparkline(values: &[i64]) -> String {
    let peak = values.iter().copied().max().unwrap_or(0).max(0);
    if peak == 0 {
        return SPARK[0].repeat(values.len());
    }
    values
        .iter()
        .map(|value| {
            let index = ((*value as f64 / peak as f64) * (SPARK.len() - 1) as f64) as usize;
            SPARK[index.min(SPARK.len() - 1)]
        })
        .collect()
}

/// The report's lines (`renderUsageReport`, line for line).
fn render(days: &[UsageDay], summary: &Summary, label: &str) -> Vec<String> {
    if days.is_empty() {
        return vec![format!("No usage recorded {label}.")];
    }
    let width = days.iter().map(|day| day.day.len()).max().unwrap_or(0);
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("Usage {label}"));
    lines.push(format!(
        "  requests {}  input {}  cache read {}  cache write {}  output {}  reasoning {}  cost {}",
        summary.requests,
        format_tokens(summary.fresh_input_tokens),
        format_tokens(summary.cache_read_tokens),
        format_tokens(summary.cache_write_tokens),
        format_tokens(summary.output_tokens),
        format_tokens(summary.reasoning_tokens),
        format_cost(summary.cost_usd),
    ));
    if let Some(busiest) = &summary.busiest {
        lines.push(format!(
            "  busiest {} ({} requests)  longest task {}  streak {}d",
            busiest.day,
            busiest.totals.requests,
            format_duration(summary.longest_task_ms),
            summary.streak_days,
        ));
    }
    if summary.unpriced_rows > 0 {
        lines.push(format!(
            "  cost source: provider {}, catalog estimate {}, unknown {}",
            summary.priced_rows, summary.estimated_rows, summary.unknown_rows
        ));
    } else {
        lines.push("  cost source: provider for every request".to_owned());
    }
    lines.push(String::new());
    lines.push(format!(
        "{day:<width$}  reqs  {input:>7}  {cache:>7}  {output:>7}  {cost:>9}",
        day = "day",
        input = "input",
        cache = "cache",
        output = "output",
        cost = "cost",
        width = width,
    ));
    for day in days {
        lines.push(format!(
            "{day:<width$}  {requests:>4}  {input:>7}  {cache:>7}  {output:>7}  {cost:>9}",
            day = day.day,
            requests = day.totals.requests,
            input = format_tokens(day.totals.fresh_input_tokens),
            cache = format_tokens(day.totals.cache_read_tokens),
            output = format_tokens(day.totals.output_tokens),
            cost = format_cost(day.totals.cost_usd),
            width = width,
        ));
    }
    let shape: Vec<i64> = days
        .iter()
        .map(|day| {
            day.totals.fresh_input_tokens + day.totals.cache_read_tokens + day.totals.output_tokens
        })
        .collect();
    lines.push(String::new());
    lines.push(format!("token shape  {}", sparkline(&shape)));
    lines
}
