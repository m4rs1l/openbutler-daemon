//! The schedule engine: wall-clock math, durable run state, the one-line
//! description, and the wake text — the pure half of the scheduler. The
//! timers and the event pump (Sched-B/C) run on top of this.
//!
//! Source: `tools/schedule.ts` (regions `nextDailyOccurrence`..`wallMatches`,
//! `readScheduleRuntime`..`writeScheduleError`, `describeSchedule`,
//! `assembleWakeText`, the formatters, `triggerStateKey`/`readTriggerDue`),
//! read 2026-11-18. Every refusal string and rendered line is the source's
//! own text; deliberate divergences are recorded per file and in
//! `docs/schedule.md`.

mod describe;
mod format;
pub mod pump;
pub(crate) mod runner;
mod runtime;
mod wake;
mod wall;

pub use describe::{ScheduleDescription, describe_schedule};
pub use format::{format_duration, format_schedule_age, format_wall};
pub use runner::{Log, Publish, ScheduleRunner, ScheduleStatus, TriggerState};
pub use runtime::{
    MAX_REPORT_CHARS, ScheduleRunOutcome, ScheduleRunResult, ScheduleRuntime,
    ScheduleRuntimeStatus, read_schedule_runtime, read_trigger_due, trigger_state_key,
    write_schedule_error, write_schedule_run_finished, write_schedule_run_started,
};
pub use wake::{LATE_GRACE_MS, WakeAssembly, assemble_wake_text};
pub use wall::{WallOccurrence, next_daily_occurrence, weekday_number};
