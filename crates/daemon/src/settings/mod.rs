//! Persisted configuration: the durability boundary first, the schema
//! layer (load, closed parsers, write authority) on top of it.
//!
//! Authority map: `transaction` owns lock/journal/atomic writes, `schema`
//! owns what a document may say, `load` is the only reader of the two
//! files, `save` is the only writer.

pub mod listen;
mod load;
pub mod mcp;
mod remember;
mod save;
pub mod schema;
pub mod setup;
pub mod transaction;
pub mod workspace;

pub use load::{config_path, load_secrets, load_settings, settings_path};
pub use remember::remember_provider_model;
pub use save::{save_settings, save_settings_with_secrets};
pub use schema::{
    ApprovalMode, CustodianSettings, DaemonSettings, McpServerState, ModalCredentials,
    ProviderModelSpec, ProviderSecrets, ProviderState, RoleBinding, SecretsStore, SettingsStore,
    TeamRole, TeamSettings, TelegramSettings, Theme, WorkerCaps, default_settings,
};

/// A settings refusal. The message *is* the contract: closed schemas report
/// the known set, ranges report name and bounds, and the wording matches the
/// established surface byte for byte.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
/// Settings load/save failure (the message a client sees).
pub struct SettingsError(pub String);

/// A loaded settings copy. Write authority comes from reading the file
/// first: the revision records what this copy saw, and `save_settings`
/// refuses to replace configuration the copy never read — the guard that
/// stops a defaults object from wiping a live store.
#[derive(Debug)]
/// A loaded settings copy: the store plus its disk revision.
pub struct LoadedSettings {
    pub store: SettingsStore,
    /// `Some(revision)` after a load; `None` only for a store built from
    /// defaults that has never touched disk.
    pub(crate) revision: Option<String>,
}

impl LoadedSettings {
    /// A store with no write history: it may create an empty machine but
    /// must never replace files it did not read.
    pub fn never_loaded(store: SettingsStore) -> Self {
        Self {
            store,
            revision: None,
        }
    }
}

impl std::ops::Deref for LoadedSettings {
    type Target = SettingsStore;

    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

impl std::ops::DerefMut for LoadedSettings {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.store
    }
}

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
