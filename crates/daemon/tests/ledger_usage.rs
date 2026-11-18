//! The usage sink: one `state.db` row per model request, for chat turns and
//! compaction alike — the port of `onStepUsage` → `recordUsage`.
//!
//! Sources: `agent-session.ts` `onStepUsage` (L2883-2905, read 2026-11-18),
//! `usage/recorder.ts` `recordUsage` (row assembly, `round`) and
//! `estimateCost`, `usage/cost-sources.ts` `resolveProviderCost`,
//! `activity-ledger.ts` `UsageRow` (L108-144) + `openbutler_usage` DDL
//! (L218-257), `agent.ts` `StepUsageReport` (L591-611).
//!
//! Accounting is best effort: nothing here may fail a run. What it must not
//! do is invent a bill — with no price catalog in this build a request the
//! provider prices is `provider-inline`, one it does not is `none`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::TimeZone;
use openbutler_daemon::ledger::{LedgerWriter, StepUsageReport, UsageInput, UsageTokens};
use serde_json::json;

fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ob-ledger-usage-{tag}-{}-{nanos:x}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One model request as the accounting layer sees it (`StepUsageReport`).
fn report(at_ms: i64) -> StepUsageReport {
    StepUsageReport {
        started_at_ms: at_ms,
        ended_at_ms: at_ms + 1_500,
        attempt: 1,
        status: "ok",
        tokens: UsageTokens {
            input_tokens: Some(1_204),
            no_cache_tokens: Some(1_000),
            cache_read_tokens: Some(180),
            cache_write_tokens: Some(24),
            output_tokens: Some(88),
            text_tokens: Some(48),
            reasoning_tokens: Some(40),
            total_tokens: Some(1_292),
        },
        usage_raw: Some(json!({ "cost": 0.0042 })),
        provider_metadata: None,
        response_id: Some("resp_abc".to_owned()),
    }
}

fn input(run_kind: &'static str, report: StepUsageReport) -> UsageInput {
    UsageInput {
        session_id: "22222222-2222-4222-8222-222222222222".to_owned(),
        event_id: None,
        run_kind,
        workspace_root: Some("C:\\work\\proj".to_owned()),
        provider_id: "gateway".to_owned(),
        model_id: "m".to_owned(),
        report,
    }
}

/// Every column the source's INSERT carries — read back from the database,
/// not from the Rust struct: the row is the contract.
#[test]
fn a_usage_row_records_every_column() {
    let root = temp_root("columns");
    let mut ledger = LedgerWriter::open(&root).expect("writer opens");
    let at_ms = 1_700_000_000_000_i64;
    let usage = input("chat", report(at_ms));

    let row = usage.row();
    ledger.record_usage(&row).expect("row writes");

    let rows = query(
        &root,
        "SELECT id, at_ms, ended_ms, duration_ms, tz_offset_min, session_id, run_kind, workspace_root, provider_id, model_id, attempt, status, input_tokens, no_cache_tokens, cache_read_tokens, cache_write_tokens, output_tokens, text_tokens, reasoning_tokens, total_tokens, usage_source, cost_usd, cost_source, generation_id FROM openbutler_usage",
    );
    assert_eq!(rows.len(), 1, "exactly one row");
    let row = &rows[0];
    assert_eq!(
        row[0],
        "22222222-2222-4222-8222-222222222222:1700000000000:1"
    );
    assert_eq!(row[1], "1700000000000");
    assert_eq!(row[2], "1700000001500");
    assert_eq!(row[3], "1500", "durationMs = ended - at");
    // The timezone stamp is the local offset *at that instant*, the source's
    // `-new Date(atMs).getTimezoneOffset()`.
    let expected_offset = i64::from(
        chrono::Local
            .timestamp_millis_opt(at_ms)
            .unwrap()
            .fixed_offset()
            .offset()
            .local_minus_utc(),
    ) / 60;
    assert_eq!(row[4], expected_offset.to_string());
    assert_eq!(row[5], "22222222-2222-4222-8222-222222222222");
    assert_eq!(row[6], "chat");
    assert_eq!(row[7], "C:\\work\\proj");
    assert_eq!(row[8], "gateway");
    assert_eq!(row[9], "m");
    assert_eq!(row[10], "1", "attempt");
    assert_eq!(row[11], "ok");
    assert_eq!(row[12], "1204");
    assert_eq!(row[13], "1000");
    assert_eq!(row[14], "180");
    assert_eq!(row[15], "24");
    assert_eq!(row[16], "88");
    assert_eq!(row[17], "48");
    assert_eq!(row[18], "40");
    assert_eq!(row[19], "1292");
    assert_eq!(row[20], "provider");
    assert_eq!(row[21], "0.0042", "provider-stated cost, rounded to 1e-6");
    assert_eq!(row[22], "provider-inline");
    assert_eq!(row[23], "resp_abc", "responseId → generation_id");
}

/// Gateways park the bill in provider metadata (`gateway.cost`); the raw
/// usage silent case must still find it there — the source checks both.
#[test]
fn provider_metadata_states_the_cost_when_raw_is_silent() {
    let root = temp_root("metadata-cost");
    let mut ledger = LedgerWriter::open(&root).expect("writer opens");
    let mut usage = input("chat", report(1_700_000_000_100));
    usage.report.usage_raw = None;
    usage.report.provider_metadata = Some(json!({ "gateway": { "cost": 0.0025 }, "x": 1 }));

    ledger.record_usage(&usage.row()).expect("row writes");

    let rows = query(&root, "SELECT cost_usd, cost_source FROM openbutler_usage");
    assert_eq!(rows[0][0], "0.0025");
    assert_eq!(rows[0][1], "provider-inline");
}

/// No provider statement, no catalog in this build → `none`. An estimate and
/// a bill are not the same fact, and there is nothing here to estimate with.
#[test]
fn no_cost_statement_is_recorded_as_none() {
    let root = temp_root("cost-none");
    let mut ledger = LedgerWriter::open(&root).expect("writer opens");
    let mut usage = input("chat", report(1_700_000_000_200));
    usage.report.usage_raw = None;
    usage.report.provider_metadata = None;

    ledger.record_usage(&usage.row()).expect("row writes");

    let rows = query(&root, "SELECT cost_usd, cost_source FROM openbutler_usage");
    assert_eq!(rows[0][0], "", "cost_usd stays NULL");
    assert_eq!(rows[0][1], "none");
}

/// A request that stated nothing — usage absent, not estimated.
#[test]
fn absent_tokens_record_usage_source_absent() {
    let root = temp_root("absent");
    let mut ledger = LedgerWriter::open(&root).expect("writer opens");
    let mut usage = input("chat", report(1_700_000_000_300));
    usage.report.tokens = UsageTokens::default();
    usage.report.usage_raw = None;

    ledger.record_usage(&usage.row()).expect("row writes");

    let rows = query(
        &root,
        "SELECT usage_source, input_tokens, total_tokens FROM openbutler_usage",
    );
    assert_eq!(rows[0][0], "absent");
    assert_eq!(rows[0][1], "", "input_tokens NULL");
    assert_eq!(rows[0][2], "", "total_tokens NULL");
}

/// The source's primary key: `sessionId:startedAt:attempt`.
#[test]
fn the_id_is_the_source_key() {
    let usage = input("compaction", report(1_700_000_000_400));
    assert_eq!(
        usage.row_id(),
        "22222222-2222-4222-8222-222222222222:1700000000400:1"
    );
    assert_eq!(usage.report.attempt, 1);
}

/// `INSERT OR IGNORE`: the same id writes once (the source's run() semantics).
#[test]
fn a_duplicate_row_is_ignored() {
    let root = temp_root("idempotent");
    let mut ledger = LedgerWriter::open(&root).expect("writer opens");
    let usage = input("chat", report(1_700_000_000_500));

    ledger.record_usage(&usage.row()).expect("first write");
    ledger
        .record_usage(&usage.row())
        .expect("duplicate ignored");

    let rows = query(&root, "SELECT COUNT(*) FROM openbutler_usage");
    assert_eq!(rows[0][0], "1");
}

/// `Math.max(0, endedMs - atMs)` — a clock skew never stores a negative run.
#[test]
fn duration_never_goes_negative() {
    let root = temp_root("duration");
    let mut ledger = LedgerWriter::open(&root).expect("writer opens");
    let mut usage = input("chat", report(1_700_000_000_600));
    usage.report.ended_at_ms = usage.report.started_at_ms - 5;

    ledger.record_usage(&usage.row()).expect("row writes");

    let rows = query(&root, "SELECT duration_ms FROM openbutler_usage");
    assert_eq!(rows[0][0], "0");
}

/// A cost stated as a decimal string counts the same as a number
/// (`readCost`: `Number(current)` on a non-empty string).
#[test]
fn a_numeric_string_cost_counts() {
    let root = temp_root("string-cost");
    let mut ledger = LedgerWriter::open(&root).expect("writer opens");
    let mut usage = input("chat", report(1_700_000_000_700));
    usage.report.usage_raw = Some(json!({ "cost": "0.5" }));

    ledger.record_usage(&usage.row()).expect("row writes");

    let rows = query(&root, "SELECT cost_usd, cost_source FROM openbutler_usage");
    assert_eq!(rows[0][0], "0.5");
    assert_eq!(rows[0][1], "provider-inline");
}

fn query(data_dir: &Path, sql: &str) -> Vec<Vec<String>> {
    let connection = rusqlite::Connection::open(data_dir.join("state.db")).unwrap();
    let mut statement = connection.prepare(sql).unwrap();
    statement
        .query_map([], |row| {
            // Type-agnostic: TEXT as-is, INTEGER/REAL stringified, NULL as "" —
            // a String-only read stops at the first non-text column.
            let mut values = Vec::new();
            let mut index = 0;
            loop {
                use rusqlite::types::Value;
                match row.get::<_, Value>(index) {
                    Ok(Value::Text(text)) => values.push(text),
                    Ok(Value::Integer(number)) => values.push(number.to_string()),
                    Ok(Value::Real(number)) => values.push(number.to_string()),
                    Ok(Value::Null) => values.push(String::new()),
                    Ok(Value::Blob(_)) | Err(_) => break,
                }
                index += 1;
            }
            Ok(values)
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}
