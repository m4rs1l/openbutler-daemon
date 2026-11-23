//! The usage sink: one `state.db` row per model request — chat turns and
//! compaction alike — and the cost read that row carries.
//!
//! Sources (read 2026-11-18): `agent-session.ts` `onStepUsage` (L2883-2905)
//! builds the record, `usage/recorder.ts` `recordUsage` assembles and writes
//! the row (`estimateCost` included, see below), `usage/cost-sources.ts`
//! `resolveProviderCost` reads the provider's own statement,
//! `activity-ledger.ts` `UsageRow` (L108-144) and the `openbutler_usage` DDL
//! (L218-257) define the table, `agent.ts` `StepUsageReport` (L591-611) is
//! the per-request capture.
//!
//! Accounting is best effort: every caller treats a failure here as a log
//! line, never as a failed run (the source's `console.error` contract).
//!
//! Cost is a bill or an estimate, never a blend (owner rule, 2026-11-24):
//! when the provider states no bill, the row is priced from the catalog's
//! real per-million prices (`estimateCost`, recorder.ts L59) and labelled
//! `catalog-estimate`; a model the catalog cannot price is `none` — no
//! character-based guessing, ever. The caller resolves the prices from
//! the catalog and hands them in, so this module stays catalog-agnostic.

use chrono::TimeZone;
use rusqlite::params;
use serde_json::Value;

use super::LedgerError;

/// One model request as the accounting layer sees it (`StepUsageReport`,
/// agent.ts L591). Built where the call ends; identities (session, provider,
/// workspace, run kind) are added by whoever owns the row.
#[derive(Debug, Clone)]
/// One step's usage as the sidecar reports it (cost when stated).
pub struct StepUsageReport {
    /// When the provider call began — the source resets it in `prepareStep`,
    /// so the duration covers the request and not the work between requests.
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    /// The step index inside the run: the turn's `steps` counter for chat,
    /// `1` for compaction (each summarisation request is its own single-step
    /// stream, as in the source).
    pub attempt: i64,
    /// `ok` | `error` — the source's constructions only ever produce these
    /// two (the union admits `aborted`/`timeout`, nothing emits them here
    /// or there).
    pub status: &'static str,
    pub tokens: UsageTokens,
    /// Raw provider usage, where an inline cost may sit.
    pub usage_raw: Option<Value>,
    /// Per-step provider metadata, where gateways park their own fields.
    pub provider_metadata: Option<Value>,
    /// The provider's own id for this response → `generation_id`.
    pub response_id: Option<String>,
}

/// The eight token counts (`UsageTokens`, recorder.ts L12-23), stored as
/// signed integers for SQLite; wire values convert losslessly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
/// The token counts of one request (input/output/cached/reasoning).
pub struct UsageTokens {
    pub input_tokens: Option<i64>,
    pub no_cache_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub text_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
}

/// Per-million-token prices as the catalog states them (`TokenPrices`,
/// recorder.ts L45-52).
#[derive(Debug, Clone, Default, PartialEq)]
/// USD per one million tokens, straight from the catalog.
pub struct TokenPrices {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub reasoning: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

/// `UsageRecordInput` (recorder.ts L26-45): the identities plus the report.
/// `turnId` and `headers` are absent because this path never sets them in
/// the source either; the table keeps the columns as NULL.
#[derive(Debug, Clone)]
/// Usage plus its identities: request id, model, generation.
pub struct UsageInput {
    pub session_id: String,
    pub event_id: Option<String>,
    /// `chat` | `compaction` | `worker` | `schedule` | `summary`.
    pub run_kind: &'static str,
    pub workspace_root: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    /// The catalog's real prices for this model, resolved by the caller;
    /// `None` means the catalog cannot price it (→ `none`, never a guess).
    pub prices: Option<TokenPrices>,
    pub report: StepUsageReport,
}

/// The row as the table stores it; only `record_usage` consumes it.
#[derive(Debug, Clone)]
/// One persisted usage row (the state.db table shape).
pub struct UsageRow {
    id: String,
    at_ms: i64,
    ended_ms: i64,
    duration_ms: i64,
    tz_offset_min: i64,
    session_id: String,
    event_id: Option<String>,
    run_kind: &'static str,
    workspace_root: Option<String>,
    provider_id: String,
    model_id: String,
    attempt: i64,
    status: &'static str,
    tokens: UsageTokens,
    usage_source: &'static str,
    prices: Option<TokenPrices>,
    cost_usd: Option<f64>,
    cost_source: &'static str,
    response_id: Option<String>,
}

impl UsageInput {
    /// The source's primary key: `${sessionId}:${startedAt}:${attempt}`.
    pub fn row_id(&self) -> String {
        format!(
            "{}:{}:{}",
            self.session_id, self.report.started_at_ms, self.report.attempt
        )
    }

    /// `recordUsage`'s row assembly (recorder.ts L610-643), minus the
    /// catalog fields this build cannot state.
    pub fn row(&self) -> UsageRow {
        let report = &self.report;
        let (cost_usd, resolved_source) =
            resolve_provider_cost(report.usage_raw.as_ref(), report.provider_metadata.as_ref());
        let duration_ms = (report.ended_at_ms - report.started_at_ms).max(0);
        // `-new Date(atMs).getTimezoneOffset()` — the local offset *at that
        // instant* (DST-aware); an unparsable stamp stores 0 rather than
        // inventing a zone.
        let tz_offset_min = chrono::Local
            .timestamp_millis_opt(report.started_at_ms)
            .latest()
            .map(|at| i64::from(at.fixed_offset().offset().local_minus_utc()) / 60)
            .unwrap_or(0);
        // usageSource: tokens stated → provider; nothing stated → absent.
        let usage_source =
            if report.tokens.total_tokens.is_some() || report.tokens.input_tokens.is_some() {
                "provider"
            } else {
                "absent"
            };
        // Cost is a bill or an estimate, never a blend: an inline bill
        // wins; otherwise the catalog prices the tokens (`estimateCost`);
        // otherwise the row is priced `none` — no guessing.
        let estimate = if cost_usd.is_none() {
            estimate_cost(&report.tokens, self.prices.as_ref())
        } else {
            None
        };
        let (final_cost, cost_source) = match (cost_usd, estimate) {
            (Some(bill), _) => (Some(bill), resolved_source),
            (None, Some(estimate)) => (Some(estimate), "catalog-estimate"),
            (None, None) => (None, "none"),
        };
        UsageRow {
            id: self.row_id(),
            at_ms: report.started_at_ms,
            ended_ms: report.ended_at_ms,
            duration_ms,
            tz_offset_min,
            session_id: self.session_id.clone(),
            event_id: self.event_id.clone(),
            run_kind: self.run_kind,
            workspace_root: self.workspace_root.clone(),
            provider_id: self.provider_id.clone(),
            model_id: self.model_id.clone(),
            attempt: report.attempt,
            status: report.status,
            tokens: report.tokens.clone(),
            usage_source,
            prices: self.prices.clone(),
            cost_usd: final_cost.map(round_six),
            cost_source,
            response_id: report.response_id.clone(),
        }
    }
}

/// Where a request's real cost comes from: the provider's own answer
/// (`cost-sources.ts`). The paths are one list for every provider; the
/// candidates are checked in order (raw usage first, then per-step
/// metadata), first stated value wins.
const COST_PATHS: [&[&str]; 4] = [
    &["usage", "cost"],
    &["cost"],
    &["market_cost"],
    &["gateway", "cost"],
];

/// The provider's stated cost for this request, or `None` when it states
/// none (`resolveProviderCost`: only an actual number — or a numeric
/// string, `Number(current)` — counts; finite throughout).
fn resolve_provider_cost(
    usage_raw: Option<&Value>,
    provider_metadata: Option<&Value>,
) -> (Option<f64>, &'static str) {
    for candidate in [usage_raw, provider_metadata] {
        if let Some(cost) = read_cost(candidate) {
            return (Some(cost), "provider-inline");
        }
    }
    (None, "none")
}

/// One path walk: objects descend, anything else abandons the path.
/// A decimal string counts the same as a number (`trim().parse()` mirrors
/// `Number()` for costs; JS hex literals like `"0x10"` are not costs).
fn read_cost(value: Option<&Value>) -> Option<f64> {
    for path in COST_PATHS {
        let mut current = value?;
        for key in path {
            match current {
                Value::Object(map) => match map.get(*key) {
                    Some(next) => current = next,
                    None => {
                        current = &Value::Null;
                        break;
                    }
                },
                _ => break,
            }
        }
        match current {
            Value::Number(number) => {
                if let Some(float) = number.as_f64()
                    && float.is_finite()
                {
                    return Some(float);
                }
            }
            Value::String(text) => {
                let trimmed = text.trim();
                if !trimmed.is_empty()
                    && let Ok(parsed) = trimmed.parse::<f64>()
                    && parsed.is_finite()
                {
                    return Some(parsed);
                }
            }
            _ => {}
        }
    }
    None
}

/// `Math.round(value * 1_000_000) / 1_000_000` — the source's formula
/// (`floor(x + 0.5)` semantics), so the six-decimal grid matches for every
/// finite input.
fn round_six(value: f64) -> f64 {
    ((value * 1_000_000.0) + 0.5).floor() / 1_000_000.0
}

/// Catalog estimate for one request (`estimateCost`, recorder.ts L59-77).
/// Cache tokens are billed at their own rate and are already part of the
/// input count, so they are priced separately and removed from the
/// plain-input share; only fields where BOTH a count and a price exist
/// contribute, and a zero total is no estimate at all (the source's
/// `total > 0 ? total : null`).
pub fn estimate_cost(tokens: &UsageTokens, prices: Option<&TokenPrices>) -> Option<f64> {
    let prices = prices?;
    let per_million = |count: Option<i64>, price: Option<f64>| match (count, price) {
        (Some(count), Some(price)) => (count as f64 * price) / 1_000_000.0,
        _ => 0.0,
    };
    let cache_read = tokens.cache_read_tokens.unwrap_or(0);
    let cache_write = tokens.cache_write_tokens.unwrap_or(0);
    let plain_input = tokens.no_cache_tokens.or_else(|| {
        tokens
            .input_tokens
            .map(|input| (input.saturating_sub(cache_read).saturating_sub(cache_write)).max(0))
    });
    let total = per_million(plain_input, prices.input)
        + per_million(tokens.output_tokens, prices.output)
        + per_million(tokens.reasoning_tokens, prices.reasoning)
        + per_million(tokens.cache_read_tokens, prices.cache_read)
        + per_million(tokens.cache_write_tokens, prices.cache_write);
    (total > 0.0).then_some(total)
}

/// The `openbutler_usage` table and its three indexes — the source's DDL
/// (activity-ledger.ts L218-257) verbatim.
pub(crate) fn usage_schema() -> String {
    "CREATE TABLE IF NOT EXISTS openbutler_usage (
        id TEXT PRIMARY KEY,
        at_ms INTEGER NOT NULL,
        ended_ms INTEGER,
        duration_ms INTEGER,
        tz_offset_min INTEGER NOT NULL,
        session_id TEXT NOT NULL,
        turn_id TEXT,
        event_id TEXT,
        run_kind TEXT NOT NULL,
        workspace_root TEXT,
        provider_id TEXT NOT NULL,
        model_id TEXT NOT NULL,
        attempt INTEGER NOT NULL DEFAULT 1,
        status TEXT NOT NULL,
        input_tokens INTEGER,
        no_cache_tokens INTEGER,
        cache_read_tokens INTEGER,
        cache_write_tokens INTEGER,
        output_tokens INTEGER,
        text_tokens INTEGER,
        reasoning_tokens INTEGER,
        total_tokens INTEGER,
        usage_source TEXT NOT NULL,
        context_tokens INTEGER,
        price_input REAL,
        price_output REAL,
        price_reasoning REAL,
        price_cache_read REAL,
        price_cache_write REAL,
        price_version TEXT,
        cost_usd REAL,
        cost_source TEXT,
        upstream_provider TEXT,
        generation_id TEXT
    );
    CREATE INDEX IF NOT EXISTS openbutler_usage_at_idx ON openbutler_usage(at_ms);
    CREATE INDEX IF NOT EXISTS openbutler_usage_session_at_idx ON openbutler_usage(session_id, at_ms);
    CREATE INDEX IF NOT EXISTS openbutler_usage_model_at_idx ON openbutler_usage(provider_id, model_id, at_ms);"
        .to_owned()
}

impl super::LedgerWriter {
    /// `recordUsage`: `INSERT OR IGNORE` — the id is the key, so a replayed
    /// report writes once (recorder.ts L612+). Never throws into the run:
    /// callers log `Usage accounting failed: …` and continue.
    pub fn record_usage(&mut self, row: &UsageRow) -> Result<(), LedgerError> {
        self.connection.execute(
            "INSERT OR IGNORE INTO openbutler_usage (
                id, at_ms, ended_ms, duration_ms, tz_offset_min, session_id, turn_id, event_id,
                run_kind, workspace_root, provider_id, model_id, attempt, status,
                input_tokens, no_cache_tokens, cache_read_tokens, cache_write_tokens,
                output_tokens, text_tokens, reasoning_tokens, total_tokens, usage_source,
                context_tokens, price_input, price_output, price_reasoning, price_cache_read,
                price_cache_write, price_version, cost_usd, cost_source, upstream_provider,
                generation_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                       ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22,
                       NULL, ?26, ?27, ?28, ?29, ?30, NULL, ?23, ?24, NULL, ?25)",
            params![
                row.id,
                row.at_ms,
                row.ended_ms,
                row.duration_ms,
                row.tz_offset_min,
                row.session_id,
                row.event_id,
                row.run_kind,
                row.workspace_root,
                row.provider_id,
                row.model_id,
                row.attempt,
                row.status,
                row.tokens.input_tokens,
                row.tokens.no_cache_tokens,
                row.tokens.cache_read_tokens,
                row.tokens.cache_write_tokens,
                row.tokens.output_tokens,
                row.tokens.text_tokens,
                row.tokens.reasoning_tokens,
                row.tokens.total_tokens,
                row.usage_source,
                row.cost_usd,
                row.cost_source,
                row.response_id,
                row.prices.as_ref().map(|prices| prices.input),
                row.prices.as_ref().map(|prices| prices.output),
                row.prices.as_ref().map(|prices| prices.reasoning),
                row.prices.as_ref().map(|prices| prices.cache_read),
                row.prices.as_ref().map(|prices| prices.cache_write),
            ],
        )?;
        Ok(())
    }
}
