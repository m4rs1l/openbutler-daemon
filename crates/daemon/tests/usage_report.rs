//! `usage.report` pins: the usage screen computed by the daemon — bucket
//! math by each row's own recorded offset, the summary (busiest tie, UTC
//! streak walk, cost-source split), the source formatters, the day table
//! and the sparkline, byte-exact.
//!
//! Sources (read 2026-11-26): `cli/usage.ts` (flag window, label, the
//! session `slice(0, 8)`), `usage/report.ts` `bucketUsage`,
//! `summarizeUsage`, `renderUsageReport` and the formatters.
//!
//! The rows are written straight into the table the ledger writer
//! creates: `record_usage` would stamp the machine's own timezone offset
//! and id, and the bucket math must not depend on the test machine.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};

use chrono::TimeZone;
use common::temp_data_dir;
use openbutler_daemon::dispatch::{DispatchState, Host, handle};
use openbutler_daemon::ledger::LedgerWriter;
use openbutler_protocol::{DaemonRequest, DaemonResponse};
use serde_json::{Value, json};

/// One usage row with every field the report reads.
struct Seed {
    id: &'static str,
    at_ms: i64,
    tz_offset_min: i64,
    session: &'static str,
    provider: &'static str,
    model: &'static str,
    input: Option<i64>,
    no_cache: Option<i64>,
    cache_read: Option<i64>,
    output: Option<i64>,
    reasoning: Option<i64>,
    cost: Option<f64>,
    duration_ms: i64,
    cost_source: &'static str,
}

fn seed(dir: &Path, rows: &[Seed]) {
    // The writer's schema is the table's definition; open once (dropped
    // immediately), then write rows with exact values.
    drop(LedgerWriter::open(dir).expect("schema"));
    let connection = rusqlite::Connection::open(dir.join("state.db")).expect("open state.db");
    for row in rows {
        connection
            .execute(
                "INSERT INTO openbutler_usage (
                    id, at_ms, ended_ms, duration_ms, tz_offset_min, session_id, run_kind,
                    provider_id, model_id, attempt, status, input_tokens, no_cache_tokens,
                    cache_read_tokens, cache_write_tokens, output_tokens, reasoning_tokens,
                    usage_source, cost_usd, cost_source
                 ) VALUES (?1, ?2, ?2, ?3, ?4, ?5, 'chat', ?6, ?7, 1, 'ok', ?8, ?9, ?10,
                           NULL, ?11, ?12, 'provider', ?13, ?14)",
                rusqlite::params![
                    row.id,
                    row.at_ms,
                    row.duration_ms,
                    row.tz_offset_min,
                    row.session,
                    row.provider,
                    row.model,
                    row.input,
                    row.no_cache,
                    row.cache_read,
                    row.output,
                    row.reasoning,
                    row.cost,
                    row.cost_source,
                ],
            )
            .expect("insert usage row");
    }
}

fn request(payload: Value) -> DaemonRequest {
    let mut request = DaemonRequest::new("r1", "usage.report");
    if let Value::Object(fields) = payload {
        request.payload = fields;
    }
    request
}

fn call(dir: &Path, payload: Value) -> DaemonResponse {
    let host = Host::new();
    handle(&host, dir, &mut DispatchState::new(), &request(payload))
}

fn report_of(response: DaemonResponse) -> Value {
    let DaemonResponse::Success { result, .. } = response else {
        panic!("expected success, got {response:?}");
    };
    result
}

fn lines_of(report: &Value) -> String {
    report["lines"]
        .as_array()
        .expect("lines is an array")
        .iter()
        .map(|line| line.as_str().expect("a line is a string"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Today's UTC midnight as a millisecond anchor, so rows land in the
/// window whatever day the test runs.
fn utc_midnight_today_ms() -> i64 {
    chrono::Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
}

fn day_string(at_ms: i64) -> String {
    chrono::Utc
        .timestamp_millis_opt(at_ms)
        .single()
        .unwrap()
        .format("%Y-%m-%d")
        .to_string()
}

/// A row at a fixed UTC instant, fixed offset, one request.
fn plain(id: &'static str, at_ms: i64, cost_source: &'static str) -> Seed {
    Seed {
        id,
        at_ms,
        tz_offset_min: 0,
        session: "sess-alpha",
        provider: "mock",
        model: "demo",
        input: Some(100),
        no_cache: Some(100),
        cache_read: Some(0),
        output: Some(10),
        reasoning: Some(0),
        cost: Some(0.001),
        duration_ms: 100,
        cost_source,
    }
}

#[test]
fn empty_ledger_says_no_usage() {
    let dir: PathBuf = temp_data_dir("usage-report-empty");
    let report = report_of(call(&dir, json!({ "days": 30 })));

    assert_eq!(report["label"], "last 30 days");
    assert_eq!(lines_of(&report), "No usage recorded last 30 days.");
    assert_eq!(report["summary"]["days"], 0);
    assert_eq!(report["summary"]["requests"], 0);
    assert_eq!(report["summary"]["streakDays"], 0);
    assert!(report["summary"]["busiest"].is_null());
    assert!(report["days"].as_array().unwrap().is_empty());
}

#[test]
fn render_matches_the_source_line_by_line() {
    let dir = temp_data_dir("usage-report-render");
    let midnight = utc_midnight_today_ms();
    // Row A: three days back, zero offset (a plain UTC day).
    let at_a = midnight - 3 * 86_400_000;
    // Row B: one hour before today's UTC midnight with a +180 min
    // recorded offset — its local day rolls into today, and the row's own
    // offset is what counts.
    let at_b = midnight - 3_600_000;
    let day_a = day_string(at_a);
    let day_b = day_string(at_b + 180 * 60_000);
    assert_ne!(day_a, day_b);
    seed(
        &dir,
        &[
            Seed {
                id: "row-a",
                at_ms: at_a,
                tz_offset_min: 0,
                session: "sess-alpha",
                provider: "mock",
                model: "demo",
                input: Some(1_000),
                no_cache: Some(600),
                cache_read: Some(400),
                output: Some(200),
                reasoning: Some(50),
                cost: Some(0.0123),
                duration_ms: 1_500,
                cost_source: "provider-inline",
            },
            Seed {
                id: "row-b",
                at_ms: at_b,
                tz_offset_min: 180,
                session: "sess-beta",
                provider: "mock",
                model: "demo",
                input: Some(2_000),
                no_cache: None,
                cache_read: None,
                output: Some(300),
                reasoning: None,
                cost: Some(0.0456),
                duration_ms: 500,
                cost_source: "provider-inline",
            },
        ],
    );

    let report = report_of(call(&dir, json!({ "days": 30 })));
    let expected = format!(
        "Usage last 30 days\n  \
         requests 2  input 2.6k  cache read 400  cache write 0  output 500  reasoning 50  cost $0.0579\n  \
         busiest {day_a} (1 requests)  longest task 2s  streak 1d\n  \
         cost source: provider for every request\n\
         \n\
         day         reqs    input    cache   output       cost\n\
         {day_a}     1      600      400      200    $0.0123\n\
         {day_b}     1     2.0k        0      300    $0.0456\n\
         \n\
         token shape  ▄█"
    );
    assert_eq!(lines_of(&report), expected);
}

#[test]
fn summary_ties_keep_the_earliest_day_and_streaks_stop_at_a_gap() {
    let dir = temp_data_dir("usage-report-tie");
    seed(
        &dir,
        &[
            plain("d16", 1_763_300_000_000, "provider-inline"), // 2025-11-16
            plain("d18", 1_763_472_800_000, "provider-inline"), // 2025-11-18
            plain("d19", 1_763_559_200_000, "provider-inline"), // 2025-11-19
            plain("d20", 1_763_645_600_000, "provider-inline"), // 2025-11-20
        ],
    );
    // A wide window keeps every row whatever the machine's today is.
    let report = report_of(call(&dir, json!({ "days": 3650 })));
    let summary = &report["summary"];
    assert_eq!(summary["days"], 4);
    assert_eq!(summary["requests"], 4);
    // Ties keep the earliest day (strictly-greater comparison).
    assert_eq!(summary["busiest"]["day"], "2025-11-16");
    // 20 ← 19 ← 18; the gap at 17 stops the walk.
    assert_eq!(summary["streakDays"], 3);
    assert_eq!(summary["longestTaskMs"], 100);
}

#[test]
fn fresh_input_falls_back_to_input_minus_cache() {
    let dir = temp_data_dir("usage-report-fresh");
    seed(
        &dir,
        &[
            // No recorded no-cache split: input - cache read - cache
            // write, floored at zero.
            Seed {
                id: "flush",
                at_ms: 1_764_000_000_000,
                tz_offset_min: 0,
                session: "s",
                provider: "mock",
                model: "demo",
                input: Some(1_000),
                no_cache: None,
                cache_read: Some(900),
                output: Some(0),
                reasoning: None,
                cost: None,
                duration_ms: 10,
                cost_source: "none",
            },
            // The same split with an over-large cache count floors at 0
            // and never goes negative.
            Seed {
                id: "over",
                at_ms: 1_764_000_000_001,
                tz_offset_min: 0,
                session: "s",
                provider: "mock",
                model: "demo",
                input: Some(10),
                no_cache: None,
                cache_read: Some(50),
                output: Some(0),
                reasoning: None,
                cost: None,
                duration_ms: 10,
                cost_source: "none",
            },
            // Recorded splits win over the subtraction.
            Seed {
                id: "split",
                at_ms: 1_764_000_000_002,
                tz_offset_min: 0,
                session: "s",
                provider: "mock",
                model: "demo",
                input: Some(500),
                no_cache: Some(100),
                cache_read: Some(400),
                output: Some(0),
                reasoning: None,
                cost: None,
                duration_ms: 10,
                cost_source: "none",
            },
        ],
    );
    let report = report_of(call(&dir, json!({ "days": 3650 })));
    // 1000 - 900 - 0 = 100, 10 - 50 → 0, recorded 100.
    assert_eq!(report["summary"]["freshInputTokens"], 200);
    assert_eq!(report["summary"]["inputTokens"], 1_510);
    assert_eq!(report["summary"]["cacheReadTokens"], 1_350);
}

#[test]
fn cost_sources_split_priced_estimated_and_unknown() {
    let dir = temp_data_dir("usage-report-cost");
    seed(
        &dir,
        &[
            plain("inline", 1_764_100_000_000, "provider-inline"),
            plain("lookup", 1_764_100_000_001, "provider-lookup"),
            plain("estimate", 1_764_100_000_002, "catalog-estimate"),
            plain("none", 1_764_100_000_003, "none"),
        ],
    );
    let report = report_of(call(&dir, json!({ "days": 3650 })));
    let summary = &report["summary"];
    // A bill is a bill: provider-inline and provider-lookup both count as
    // priced; an estimate is still an unpriced request priced by us; an
    // unknown source is unpriced too.
    assert_eq!(summary["pricedRows"], 2);
    assert_eq!(summary["estimatedRows"], 1);
    assert_eq!(summary["unknownRows"], 1);
    assert_eq!(summary["unpricedRows"], 2);
    assert!(
        lines_of(&report).contains("  cost source: provider 2, catalog estimate 1, unknown 1"),
        "{}",
        lines_of(&report)
    );
}

#[test]
fn cost_source_line_says_every_request_when_nothing_is_unpriced() {
    let dir = temp_data_dir("usage-report-priced");
    seed(&dir, &[plain("only", 1_764_200_000_000, "provider-inline")]);
    let report = report_of(call(&dir, json!({ "days": 3650 })));
    assert!(
        lines_of(&report).contains("  cost source: provider for every request"),
        "{}",
        lines_of(&report)
    );
}

#[test]
fn days_window_drops_older_rows() {
    let dir = temp_data_dir("usage-report-window");
    let midnight = utc_midnight_today_ms();
    seed(
        &dir,
        &[plain("old", midnight - 40 * 86_400_000, "provider-inline")],
    );
    let narrow = report_of(call(&dir, json!({ "days": 30 })));
    assert_eq!(lines_of(&narrow), "No usage recorded last 30 days.");
    let wide = report_of(call(&dir, json!({ "days": 60 })));
    assert_eq!(wide["summary"]["requests"], 1);
    assert_eq!(wide["label"], "last 60 days");
}

#[test]
fn session_and_model_filters_narrow_the_rows() {
    let dir = temp_data_dir("usage-report-filter");
    let at = utc_midnight_today_ms() - 86_400_000;
    let mut keep = plain("keep", at, "provider-inline");
    keep.input = Some(500);
    keep.no_cache = Some(500);
    keep.output = Some(100);
    keep.cost = Some(0.001);
    keep.duration_ms = 300;
    let mut other_session = plain("other-session", at, "provider-inline");
    other_session.session = "sess-beta";
    other_session.input = Some(9_999);
    other_session.no_cache = Some(9_999);
    let mut other_model = plain("other-model", at, "provider-inline");
    other_model.provider = "other";
    other_model.model = "model-x";
    other_model.input = Some(9_999);
    other_model.no_cache = Some(9_999);
    seed(&dir, &[keep, other_session, other_model]);

    // `--model mock/demo` splits once; `--session` compares the full id.
    let report = report_of(call(
        &dir,
        json!({ "days": 30, "session": "sess-alpha", "model": "mock/demo" }),
    ));
    let text = lines_of(&report);
    let lines: Vec<&str> = text.lines().collect();
    // The source's `slice(0, 8)`: the first eight characters, not the
    // whole id.
    assert_eq!(
        lines[0],
        "Usage last 30 days (session sess-alp, model mock/demo)"
    );
    assert!(lines[1].contains("requests 1"), "{}", lines[1]);
    assert!(lines[1].contains("cost $0.0010"), "{}", lines[1]);

    // A bare provider filters by provider only (the source's split
    // leaves the model part undefined).
    let bare = report_of(call(&dir, json!({ "days": 30, "model": "mock" })));
    assert_eq!(bare["summary"]["requests"], 2);
    assert_eq!(bare["label"], "last 30 days (model mock)");
}

#[test]
fn days_must_be_a_whole_number_in_range() {
    let dir = temp_data_dir("usage-report-days");
    for payload in [
        json!({ "days": 0 }),
        json!({ "days": 3651 }),
        json!({ "days": 1.5 }),
        json!({ "days": "7" }),
    ] {
        let DaemonResponse::Failure { error, .. } = call(&dir, payload.clone()) else {
            panic!("expected a refusal for {payload}");
        };
        assert_eq!(error, "days must be a whole number between 1 and 3650.");
    }
    // Absent (or null) means the source default of 30.
    let report = report_of(call(&dir, json!({})));
    assert_eq!(report["label"], "last 30 days");
    let report = report_of(call(&dir, json!({ "days": null })));
    assert_eq!(report["label"], "last 30 days");
}

#[test]
fn session_and_model_must_be_strings() {
    let dir = temp_data_dir("usage-report-names");
    for payload in [
        json!({ "days": 30, "session": "" }),
        json!({ "days": 30, "session": 7 }),
        json!({ "days": 30, "model": "" }),
    ] {
        let DaemonResponse::Failure { .. } = call(&dir, payload.clone()) else {
            panic!("expected a refusal for {payload}");
        };
    }
    let DaemonResponse::Failure { error, .. } = call(&dir, json!({ "days": 30, "session": "" }))
    else {
        panic!("expected a refusal");
    };
    assert_eq!(error, "session must be a non-empty string under 512 bytes.");
}
