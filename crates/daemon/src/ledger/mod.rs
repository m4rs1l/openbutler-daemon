//! The activity ledger over `state.db`: read feeds, the write-ahead
//! tool-action writer, and the per-request usage sink.
//!
//! Source: `activity-ledger.ts` (read 2026-11-16 and 2026-11-17). Reads
//! never create the database; `LedgerWriter` is the one writable open.

mod events;
mod feed;
mod state;
mod usage;
mod writer;

pub use events::{EventInput, OBSERVED_EVENT_STATES};
pub use feed::{list_actions, list_events, retry_event, schedule_events};
pub use state::StateReader;
pub use usage::{StepUsageReport, UsageInput, UsageRow, UsageTokens};
pub use writer::{LedgerWriter, TOOL_ACTION_STATES, ToolActionState, ToolActionUpdate};

/// One refusal from the ledger surface. The message *is* the contract.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("{0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The write-ahead record is the gate in front of every other state
    /// (`recordToolAction` in the source).
    #[error("An action must be recorded before execution.")]
    MissingAction,
    /// A refusal whose message *is* the contract (event validation,
    /// payload ceilings).
    #[error("{0}")]
    Refusal(String),
}

/// The retry marker's timestamp, shaped like JS `toISOString()`.
fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
