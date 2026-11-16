//! Persisted configuration: the durability boundary first, the schema
//! layer (load, closed parsers, write authority) on top of it.
//!
//! Authority map: `transaction` owns lock/journal/atomic writes, `schema`
//! owns what a document may say, `load` is the only reader of the two files.
//! Save with its write-authority guards lands with the save slice.

mod load;
pub mod schema;
pub mod transaction;

pub use load::{load_secrets, load_settings};
pub use schema::{
    ApprovalMode, CustodianSettings, DaemonSettings, McpServerState, ModalCredentials,
    ProviderModelSpec, ProviderSecrets, ProviderState, RoleBinding, ScheduleState, ScheduleTrigger,
    SecretsStore, SettingsStore, TeamRole, TeamSettings, TelegramSettings, Theme, WorkerCaps,
    default_settings,
};

/// A settings refusal. The message *is* the contract: closed schemas report
/// the known set, ranges report name and bounds, and the wording matches the
/// established surface byte for byte.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SettingsError(pub String);

impl From<std::io::Error> for SettingsError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<transaction::TransactionError> for SettingsError {
    fn from(error: transaction::TransactionError) -> Self {
        Self(error.to_string())
    }
}
