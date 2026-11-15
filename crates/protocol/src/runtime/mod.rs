//! What the host reports about itself: process status and the model runtime.
//!
//! [`status`] is the answer to a `status` request; [`view`] is the runtime
//! block every session patch carries.

pub mod status;
pub mod view;

pub use status::{DaemonStatus, LegacyV1Status, TelegramStatus};
pub use view::{ApprovalMode, LocalClientKind, ReasoningLabel, ReasoningSupport, RuntimeViewData};
