//! The key-value surface of `openbutler_state` — `getState`/`setState` in
//! the source (`activity-ledger.ts:702-713`). Reads never bring a database
//! into being; a database that lacks the table reads as an empty store,
//! because an absent key is a value, not an error. The writable open stays
//! `LedgerWriter` — it owns the schema (the `openbutler_state` DDL lands in
//! its migration, as the source's schema does).

use std::path::Path;

use super::{LedgerError, LedgerWriter, feed};

/// A read-only view of the key-value store.
pub struct StateReader {
    connection: Option<rusqlite::Connection>,
}

impl StateReader {
    /// Open the store for reading; a missing database opens as empty.
    pub fn open(data_dir: &Path) -> Result<StateReader, LedgerError> {
        Ok(StateReader {
            connection: feed::open_readonly(data_dir)?,
        })
    }

    /// `getState`: the stored value, or `None` for a missing key, a missing
    /// database, or a database without the table.
    pub fn get(&self, key: &str) -> Option<String> {
        let connection = self.connection.as_ref()?;
        connection
            .query_row(
                "SELECT value FROM openbutler_state WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .ok()
            .flatten()
    }
}

impl LedgerWriter {
    /// `getState` through the one writable open — the runtime writers read
    /// the durable count before rewriting it.
    pub fn state_get(&self, key: &str) -> Result<Option<String>, LedgerError> {
        match self.connection.query_row(
            "SELECT value FROM openbutler_state WHERE key = ?1",
            [key],
            |row| row.get(0),
        ) {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// `setState`: the source's single upsert
    /// (`INSERT ... ON CONFLICT(key) DO UPDATE SET value = excluded.value`).
    pub fn set_state(&self, key: &str, value: &str) -> Result<(), LedgerError> {
        self.connection.execute(
            "INSERT INTO openbutler_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }
}
