//! Activity-ledger feeds: `ledger.events` / `ledger.actions` /
//! `events.retry` response shapes.
//!
//! Source: `activity-ledger.ts` `ObservedEventRow` / `ToolActionRow`
//! (read 2026-11-16). `payload`, `effect`, `input` and `result` stay
//! untyped JSON values: the source casts parse-failure fallbacks
//! (`{}` / `null`) straight through, and a stricter Rust type would
//! refuse rows the established client happily displays.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One observed event as the feed returns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerEvent {
    pub id: String,
    pub source: String,
    pub kind: String,
    pub dedupe_key: String,
    pub workspace_root: Option<String>,
    pub occurred_at: String,
    pub received_at: String,
    pub status: String,
    pub attempts: i64,
    pub payload: Value,
    pub error: Option<String>,
}

/// One recorded tool action as the feed returns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerAction {
    pub request_id: String,
    pub workspace_root: String,
    pub session_id: String,
    pub event_id: Option<String>,
    pub provider_id: String,
    pub name: String,
    pub state: String,
    pub effect: Value,
    pub input: Value,
    pub result: Option<Value>,
    pub created_at: String,
    pub updated_at: String,
    pub error: Option<String>,
}
