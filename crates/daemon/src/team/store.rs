//! SQLite-backed team store (hub data layer).
//!
//! Ports the legacy `team/store.ts` schema: same table names, columns, and
//! indexes. The public Rust API is the contract exercised by the team store
//! tests; SQL layout follows the legacy source so the database is compatible
//! with the original team hub.

use std::collections::HashSet;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use super::contracts::TeamMessage;
use super::coordination::{
    ACTIVE_WORK_STATES, PeerCapabilities, PeerProfile, PeerWork, TERMINAL_WORK_STATES,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removal {
    Removed,
    Missing,
    Hub,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MessageInput {
    pub msg_id: String,
    pub sender: String,
    pub recipient: String,
    pub body: String,
    pub created_at: i64,
    pub reply_to: Option<String>,
    pub kind: Option<String>,
    pub file_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkNotice {
    pub msg_id: String,
    pub sender: String,
    pub recipient: String,
    pub body: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamFile {
    pub id: String,
    pub name: String,
    pub size: i64,
    pub sha256: String,
    pub sender: String,
    pub recipient: String,
    pub created_at: i64,
}

/// One member as the source's `TeamMember` interface exposes it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamMember {
    pub name: String,
    pub role: String,
    pub created_at: i64,
    pub last_seen: i64,
    pub removed_at: Option<i64>,
}

/// One invite (`TeamInvite`): listed rows are the live ones, so the usage
/// fields are null until `consume_invite` stamps them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamInvite {
    pub code: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub used_at: Option<i64>,
    pub used_by: Option<String>,
}

/// One join request (`JoinRequest`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinRequest {
    pub id: String,
    pub name: String,
    pub code: String,
    pub created_at: i64,
    pub status: String,
    pub decided_at: Option<i64>,
}

/// One append-only audit row (`TeamAuditEntry` — the field is `detail`,
/// exactly as the source spells it).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamAuditEntry {
    pub seq: i64,
    pub kind: String,
    pub actor: String,
    pub detail: Option<String>,
    pub created_at: i64,
}

/// One row per active member with presence and last activity (`MemberSummary`):
/// the hub derives this so every member sees the same picture.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberSummary {
    pub name: String,
    pub role: String,
    pub online: bool,
    pub last_seen: i64,
    pub last_message_at: Option<i64>,
    pub last_message_to: Option<String>,
    pub profile: Option<PeerProfile>,
    pub capabilities: Option<PeerCapabilities>,
    pub active_work_count: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum TeamStoreError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl TeamStoreError {
    fn msg(text: impl Into<String>) -> Self {
        Self::Message(text.into())
    }
}

/// Every fallible store operation returns this (`Result` alias).
pub type TeamStoreResult<T> = std::result::Result<T, TeamStoreError>;

type Result<T> = TeamStoreResult<T>;

const TEAM_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS team_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS team_members (
  name TEXT PRIMARY KEY,
  name_lower TEXT NOT NULL UNIQUE,
  role TEXT NOT NULL CHECK (role IN ('hub', 'member')),
  ticket_hash TEXT,
  created_at INTEGER NOT NULL,
  last_seen INTEGER NOT NULL,
  removed_at INTEGER
);
CREATE TABLE IF NOT EXISTS team_invites (
  code TEXT PRIMARY KEY,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  used_at INTEGER,
  used_by TEXT
);
CREATE TABLE IF NOT EXISTS team_join_requests (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  code TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('pending', 'approved', 'denied')) DEFAULT 'pending',
  decided_at INTEGER,
  ticket TEXT
);
CREATE TABLE IF NOT EXISTS team_messages (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  msg_id TEXT NOT NULL UNIQUE,
  sender TEXT NOT NULL,
  recipient TEXT NOT NULL,
  kind TEXT NOT NULL DEFAULT 'text',
  body TEXT NOT NULL,
  file_id TEXT,
  created_at INTEGER NOT NULL,
  reply_to TEXT
);
CREATE INDEX IF NOT EXISTS team_messages_recipient_seq_idx ON team_messages(recipient, seq);
CREATE TABLE IF NOT EXISTS team_files (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  size INTEGER NOT NULL,
  sha256 TEXT NOT NULL,
  sender TEXT NOT NULL,
  recipient TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS team_group_cursors (
  member TEXT PRIMARY KEY,
  last_seq INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS team_dm_acks (
  msg_id TEXT PRIMARY KEY,
  reader TEXT NOT NULL,
  acked_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS team_audit (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL,
  actor TEXT NOT NULL,
  detail TEXT,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS team_profiles (
  member TEXT PRIMARY KEY,
  revision INTEGER NOT NULL,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS team_capabilities (
  member TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS team_work (
  id TEXT PRIMARY KEY,
  requester TEXT NOT NULL,
  assignee TEXT NOT NULL,
  state TEXT NOT NULL,
  revision INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  value TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS team_work_requester_idx ON team_work(requester, updated_at);
CREATE INDEX IF NOT EXISTS team_work_assignee_idx ON team_work(assignee, updated_at);
";

pub struct TeamStore {
    conn: Connection,
}

impl TeamStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA busy_timeout = 5000; PRAGMA journal_mode = WAL;")?;
        #[cfg(unix)]
        {
            // The file holds bearer-equivalent material (source: chmodSync 0o600);
            // Windows relies on the directory ACL.
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<()> {
        self.conn.execute_batch(TEAM_SCHEMA)?;
        // Older databases predate the message kind/file columns; add them in place.
        self.ensure_column("team_messages", "kind", "TEXT NOT NULL DEFAULT 'text'")?;
        self.ensure_column("team_messages", "file_id", "TEXT")?;
        Ok(())
    }

    fn ensure_column(&self, table: &str, column: &str, decl: &str) -> Result<()> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let cols = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if !cols.iter().any(|c| c == column) {
            self.conn.execute(
                &format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"),
                [],
            )?;
        }
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM team_meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO team_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn is_initialized(&self) -> Result<bool> {
        Ok(self.get_meta("hub_id")?.is_some())
    }

    pub fn team_id(&self) -> Result<String> {
        Ok(self.get_meta("team_id")?.unwrap_or_default())
    }

    pub fn init_hub(
        &self,
        hub_id: &str,
        team_id: &str,
        team_name: &str,
        creator_name: &str,
        created_at: i64,
    ) -> Result<()> {
        if self.get_meta("hub_id")?.is_some() {
            return Err(TeamStoreError::msg(
                "This machine already initialized a team. Leave or reset it first.",
            ));
        }
        self.conn.execute_batch("BEGIN IMMEDIATE;")?;
        let res = (|| {
            self.set_meta("hub_id", hub_id)?;
            self.set_meta("team_id", team_id)?;
            self.set_meta("team_name", team_name)?;
            self.conn.execute(
                "INSERT INTO team_members (name, name_lower, role, ticket_hash, created_at, last_seen, removed_at)
                 VALUES (?1, ?2, 'hub', NULL, ?3, ?3, NULL)",
                params![creator_name, creator_name.to_lowercase(), created_at],
            )?;
            Ok(())
        })();
        match res {
            Ok(()) => {
                self.conn.execute_batch("COMMIT;")?;
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK;");
                Err(e)
            }
        }
    }

    pub fn list_members(&self) -> Result<Vec<TeamMember>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, role, created_at, last_seen, removed_at FROM team_members WHERE removed_at IS NULL ORDER BY created_at ASC, name ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(TeamMember {
                    name: row.get(0)?,
                    role: row.get(1)?,
                    created_at: row.get(2)?,
                    last_seen: row.get(3)?,
                    removed_at: row.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn prune_invites(&self, now: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM team_invites WHERE expires_at <= ?1 AND used_at IS NULL",
            params![now],
        )?;
        Ok(())
    }

    pub fn list_invites(&self, now: i64) -> Result<Vec<TeamInvite>> {
        self.prune_invites(now)?;
        let mut stmt = self.conn.prepare(
            "SELECT code, created_at, expires_at, used_at, used_by FROM team_invites WHERE used_at IS NULL ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(TeamInvite {
                    code: row.get(0)?,
                    created_at: row.get(1)?,
                    expires_at: row.get(2)?,
                    used_at: row.get(3)?,
                    used_by: row.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn create_invite(&self, code: &str, expires_at: i64, created_at: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO team_invites (code, created_at, expires_at, used_at, used_by) VALUES (?1, ?2, ?3, NULL, NULL)",
            params![code, created_at, expires_at],
        )?;
        Ok(())
    }

    pub fn get_invite(&self, code: &str, now: i64) -> Result<Option<TeamInvite>> {
        self.prune_invites(now)?;
        type InviteRow = (String, i64, i64, Option<i64>, Option<String>);
        let row: Option<InviteRow> = self
            .conn
            .query_row(
                "SELECT code, created_at, expires_at, used_at, used_by FROM team_invites WHERE code = ?1",
                params![code],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()?;
        Ok(
            row.and_then(|(code, created_at, expires_at, used_at, used_by)| {
                if used_at.is_some() {
                    None
                } else {
                    Some(TeamInvite {
                        code,
                        created_at,
                        expires_at,
                        used_at,
                        used_by,
                    })
                }
            }),
        )
    }

    pub fn consume_invite(&self, code: &str, name: &str, now: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE team_invites SET used_at = ?1, used_by = ?2 WHERE code = ?3 AND used_at IS NULL AND expires_at > ?1",
            params![now, name, code],
        )?;
        Ok(changed > 0)
    }

    pub fn revoke_invite(&self, code: &str) -> Result<bool> {
        let changed = self
            .conn
            .execute("DELETE FROM team_invites WHERE code = ?1", params![code])?;
        Ok(changed > 0)
    }

    pub fn name_taken(&self, name: &str) -> Result<bool> {
        let lower = name.to_lowercase();
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM team_members WHERE name_lower = ?1",
                params![lower],
                |row| row.get(0),
            )
            .optional()?;
        Ok(exists.is_some())
    }

    pub fn canonical_member_name(&self, name: &str) -> Result<Option<String>> {
        let lower = name.to_lowercase();
        self.conn
            .query_row(
                "SELECT name FROM team_members WHERE name_lower = ?1 AND removed_at IS NULL",
                params![lower],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn remove_member(&self, name: &str, at: i64) -> Result<Removal> {
        let lower = name.to_lowercase();
        let row: Option<(String, String, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT name, role, removed_at FROM team_members WHERE name_lower = ?1",
                params![lower],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((_, role, removed_at)) = row else {
            return Ok(Removal::Missing);
        };
        if removed_at.is_some() {
            return Ok(Removal::Missing);
        }
        if role == "hub" {
            // A hub member is never removable (store.ts: `role === 'hub'`).
            return Ok(Removal::Hub);
        }
        self.conn.execute(
            "UPDATE team_members SET ticket_hash = NULL, removed_at = ?2 WHERE name_lower = ?1",
            params![lower, at],
        )?;
        Ok(Removal::Removed)
    }

    pub fn list_pending_joins(&self) -> Result<Vec<JoinRequest>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, code, created_at, status, decided_at FROM team_join_requests WHERE status = 'pending' ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(JoinRequest {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    code: row.get(2)?,
                    created_at: row.get(3)?,
                    status: row.get(4)?,
                    decided_at: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get_join_request(&self, req_id: &str) -> Result<Option<JoinRequest>> {
        self.conn
            .query_row(
                "SELECT id, name, code, created_at, status, decided_at FROM team_join_requests WHERE id = ?1",
                params![req_id],
                |row| {
                    Ok(JoinRequest {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        code: row.get(2)?,
                        created_at: row.get(3)?,
                        status: row.get(4)?,
                        decided_at: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn create_join_request(
        &self,
        req_id: &str,
        name: &str,
        invite_code: &str,
        created_at: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO team_join_requests (id, name, code, created_at, status, decided_at, ticket)
             VALUES (?1, ?2, ?3, ?4, 'pending', NULL, NULL)",
            params![req_id, name, invite_code, created_at],
        )?;
        Ok(())
    }

    pub fn decide_join(
        &self,
        req_id: &str,
        approved: bool,
        ticket: Option<&str>,
        ticket_hash: Option<&str>,
        at: i64,
    ) -> Result<bool> {
        if approved && ticket.is_none() {
            return Err(TeamStoreError::msg("Approving a join needs a ticket."));
        }
        self.conn.execute_batch("BEGIN IMMEDIATE;")?;
        let res = (|| {
            let row: Option<(String, String)> = self
                .conn
                .query_row(
                    "SELECT name, status FROM team_join_requests WHERE id = ?1",
                    params![req_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((name, status)) = row else {
                return Ok(false);
            };
            if status != "pending" {
                return Ok(false);
            }
            if approved {
                let ticket = ticket.unwrap_or_default();
                let ticket_hash = ticket_hash.unwrap_or_default();
                if self.name_taken(&name)? {
                    return Ok(false);
                }
                self.conn.execute(
                    "UPDATE team_join_requests SET status = 'approved', decided_at = ?2, ticket = ?3 WHERE id = ?1 AND status = 'pending'",
                    params![req_id, at, ticket],
                )?;
                self.conn.execute(
                    "INSERT INTO team_members (name, name_lower, role, ticket_hash, created_at, last_seen, removed_at)
                     VALUES (?1, ?2, 'member', ?3, ?4, ?4, NULL)",
                    params![name, name.to_lowercase(), ticket_hash, at],
                )?;
                Ok(true)
            } else {
                self.conn.execute(
                    "UPDATE team_join_requests SET status = 'denied', decided_at = ?2, ticket = NULL WHERE id = ?1 AND status = 'pending'",
                    params![req_id, at],
                )?;
                Ok(true)
            }
        })();
        match res {
            Ok(v) => {
                self.conn.execute_batch("COMMIT;")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK;");
                Err(e)
            }
        }
    }

    pub fn find_by_ticket_hash(&self, hash: &str) -> Result<Option<TeamMember>> {
        let row: Option<(String, String, i64, i64, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT name, role, created_at, last_seen, removed_at FROM team_members WHERE ticket_hash = ?1 AND removed_at IS NULL",
                params![hash],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()?;
        Ok(row.map(
            |(name, role, created_at, last_seen, removed_at)| TeamMember {
                name,
                role,
                created_at,
                last_seen,
                removed_at,
            },
        ))
    }

    pub fn peek_ticket(&self, req_id: &str, now: i64, ttl_ms: i64) -> Result<Option<String>> {
        let row: Option<(Option<String>, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT ticket, decided_at FROM team_join_requests WHERE id = ?1 AND status = 'approved'",
                params![req_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((ticket, decided_at)) = row else {
            return Ok(None);
        };
        let Some(ticket) = ticket else {
            return Ok(None);
        };
        if decided_at.is_some_and(|decided| now - decided > ttl_ms) {
            self.conn.execute(
                "UPDATE team_join_requests SET ticket = NULL WHERE id = ?1",
                params![req_id],
            )?;
            return Ok(None);
        }
        Ok(Some(ticket))
    }

    pub fn confirm_ticket(&self, req_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE team_join_requests SET ticket = NULL WHERE id = ?1",
            params![req_id],
        )?;
        Ok(())
    }

    fn read_message_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TeamMessage> {
        Ok(TeamMessage {
            seq: row.get(0)?,
            msg_id: row.get(1)?,
            sender: row.get(2)?,
            recipient: row.get(3)?,
            kind: row.get(4)?,
            body: row.get(5)?,
            file_id: row.get(6)?,
            created_at: row.get(7)?,
            reply_to: row.get(8)?,
        })
    }

    const MESSAGE_COLS: &'static str =
        "seq, msg_id, sender, recipient, kind, body, file_id, created_at, reply_to";

    pub fn post_message(&self, input: &MessageInput) -> Result<TeamMessage> {
        let kind = input.kind.clone().unwrap_or_else(|| "text".to_owned());
        let info = self.conn.execute(
            "INSERT INTO team_messages (msg_id, sender, recipient, kind, body, file_id, created_at, reply_to)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                input.msg_id,
                input.sender,
                input.recipient,
                kind,
                input.body,
                input.file_id,
                input.created_at,
                input.reply_to
            ],
        )?;
        let _ = info;
        let seq = self.conn.last_insert_rowid();
        self.conn
            .query_row(
                &format!(
                    "SELECT {} FROM team_messages WHERE seq = ?1",
                    Self::MESSAGE_COLS
                ),
                params![seq],
                Self::read_message_row,
            )
            .map_err(Into::into)
    }

    pub fn has_message(&self, msg_id: &str) -> Result<bool> {
        let v: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM team_messages WHERE msg_id = ?1",
                params![msg_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(v.is_some())
    }

    pub fn get_message(&self, msg_id: &str) -> Result<Option<TeamMessage>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {} FROM team_messages WHERE msg_id = ?1",
                    Self::MESSAGE_COLS
                ),
                params![msg_id],
                Self::read_message_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn file_message(&self, file_id: &str) -> Result<Option<TeamMessage>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {} FROM team_messages WHERE file_id = ?1 ORDER BY seq LIMIT 1",
                    Self::MESSAGE_COLS
                ),
                params![file_id],
                Self::read_message_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn group_messages(&self, after_seq: i64, limit: i64) -> Result<Vec<TeamMessage>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM team_messages WHERE recipient = 'group' AND seq > ?1 ORDER BY seq ASC LIMIT ?2",
            Self::MESSAGE_COLS
        ))?;
        let rows = stmt
            .query_map(params![after_seq, limit], Self::read_message_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn group_history(&self, before_seq: i64, limit: i64) -> Result<Vec<TeamMessage>> {
        let ceiling = if before_seq > 0 { before_seq } else { i64::MAX };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM team_messages WHERE recipient = 'group' AND seq < ?1 ORDER BY seq DESC LIMIT ?2",
            Self::MESSAGE_COLS
        ))?;
        let rows = stmt
            .query_map(params![ceiling, limit], Self::read_message_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn group_latest_seq(&self) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM team_messages WHERE recipient = 'group'",
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn get_group_cursor(&self, name: &str) -> Result<i64> {
        Ok(self
            .conn
            .query_row(
                "SELECT last_seq FROM team_group_cursors WHERE member = ?1",
                params![name],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    pub fn set_group_cursor(&self, name: &str, seq: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO team_group_cursors (member, last_seq) VALUES (?1, ?2)
             ON CONFLICT(member) DO UPDATE SET last_seq = MAX(last_seq, excluded.last_seq)",
            params![name, seq],
        )?;
        Ok(())
    }

    pub fn dm_history(
        &self,
        a: &str,
        b: &str,
        before_seq: i64,
        limit: i64,
    ) -> Result<Vec<TeamMessage>> {
        let ceiling = if before_seq > 0 { before_seq } else { i64::MAX };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM team_messages
             WHERE ((sender = ?1 AND recipient = ?2) OR (sender = ?2 AND recipient = ?1))
               AND seq < ?3
             ORDER BY seq DESC LIMIT ?4",
            Self::MESSAGE_COLS
        ))?;
        let rows = stmt
            .query_map(params![a, b, ceiling, limit], Self::read_message_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn unread_dm_count(&self, name: &str) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM team_messages m
                 LEFT JOIN team_dm_acks a ON a.msg_id = m.msg_id
                 WHERE m.recipient = ?1 AND a.msg_id IS NULL",
                params![name],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn unread_dms(&self, name: &str, limit: i64) -> Result<Vec<TeamMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.seq, m.msg_id, m.sender, m.recipient, m.kind, m.body, m.file_id, m.created_at, m.reply_to
             FROM team_messages m
             LEFT JOIN team_dm_acks a ON a.msg_id = m.msg_id
             WHERE m.recipient = ?1 AND a.msg_id IS NULL
             ORDER BY m.seq ASC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![name, limit], Self::read_message_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn ack_dms(&self, name: &str, msg_ids: &[&str], at: i64) -> Result<i64> {
        let mut count = 0i64;
        let mut stmt = self.conn.prepare(
            "INSERT INTO team_dm_acks (msg_id, reader, acked_at)
             SELECT msg_id, ?1, ?2 FROM team_messages WHERE msg_id = ?3 AND recipient = ?1
             ON CONFLICT(msg_id) DO NOTHING",
        )?;
        for msg_id in msg_ids {
            let changed = stmt.execute(params![name, at, msg_id])?;
            count += changed as i64;
        }
        Ok(count)
    }

    pub fn peer_profile(&self, name: &str) -> Result<Option<PeerProfile>> {
        let row: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM team_profiles WHERE member = ?1",
                params![name],
                |row| row.get(0),
            )
            .optional()?;
        row.map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }

    pub fn save_peer_profile(
        &self,
        name: &str,
        profile: &PeerProfile,
        expected: i64,
    ) -> Result<bool> {
        let value = serde_json::to_string(profile)?;
        let changed = if expected == 0 {
            self.conn.execute(
                "INSERT OR IGNORE INTO team_profiles (member, revision, value) VALUES (?1, ?2, ?3)",
                params![name, profile.revision, value],
            )?
        } else {
            self.conn.execute(
                "UPDATE team_profiles SET revision = ?2, value = ?3 WHERE member = ?1 AND revision = ?4",
                params![name, profile.revision, value, expected],
            )?
        };
        Ok(changed == 1)
    }

    pub fn peer_work(&self, id: &str) -> Result<Option<PeerWork>> {
        let row: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM team_work WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        row.map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }

    pub fn save_peer_work(
        &self,
        work: &PeerWork,
        expected: i64,
        notice: &WorkNotice,
    ) -> Result<bool> {
        let value = serde_json::to_string(work)?;
        let state = work.state.as_str();
        self.conn.execute_batch("BEGIN IMMEDIATE;")?;
        let res = (|| {
            let changed = if expected == 0 {
                self.conn.execute(
                    "INSERT OR IGNORE INTO team_work (id, requester, assignee, state, revision, updated_at, value)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![work.id, work.requester, work.assignee, state, work.revision, work.updated_at, value],
                )?
            } else {
                self.conn.execute(
                    "UPDATE team_work SET state = ?2, revision = ?3, updated_at = ?4, value = ?5 WHERE id = ?1 AND revision = ?6",
                    params![work.id, state, work.revision, work.updated_at, value, expected],
                )?
            };
            if changed != 1 {
                return Ok(false);
            }
            self.post_message_inner(&MessageInput {
                msg_id: notice.msg_id.clone(),
                sender: notice.sender.clone(),
                recipient: notice.recipient.clone(),
                body: notice.body.clone(),
                created_at: notice.created_at,
                ..Default::default()
            })?;
            Ok(true)
        })();
        match res {
            Ok(v) => {
                self.conn.execute_batch("COMMIT;")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK;");
                Err(e)
            }
        }
    }

    fn post_message_inner(&self, input: &MessageInput) -> Result<()> {
        let kind = input.kind.clone().unwrap_or_else(|| "text".to_owned());
        self.conn.execute(
            "INSERT INTO team_messages (msg_id, sender, recipient, kind, body, file_id, created_at, reply_to)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                input.msg_id,
                input.sender,
                input.recipient,
                kind,
                input.body,
                input.file_id,
                input.created_at,
                input.reply_to
            ],
        )?;
        Ok(())
    }

    /// Peer capabilities (`peerCapabilities`): the persisted host report.
    pub fn peer_capabilities(&self, member: &str) -> Result<Option<PeerCapabilities>> {
        let row: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM team_capabilities WHERE member = ?1",
                params![member],
                |row| row.get(0),
            )
            .optional()?;
        row.map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }

    /// Saves the latest capability report (`savePeerCapabilities`) —
    /// last write wins, no revision guard.
    pub fn save_peer_capabilities(&self, member: &str, report: &PeerCapabilities) -> Result<()> {
        let value = serde_json::to_string(report)?;
        self.conn.execute(
            "INSERT INTO team_capabilities (member, value) VALUES (?1, ?2)
             ON CONFLICT(member) DO UPDATE SET value = excluded.value",
            params![member, value],
        )?;
        Ok(())
    }

    /// Work rows touching `member` (`listPeerWork`): newest first, terminal
    /// states filtered unless the caller asks for them.
    pub fn list_peer_work(
        &self,
        member: &str,
        include_closed: bool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<PeerWork>> {
        let rows = if include_closed {
            let mut stmt = self.conn.prepare(
                "SELECT value FROM team_work WHERE requester = ?1 OR assignee = ?1
                 ORDER BY updated_at DESC, id LIMIT ?2 OFFSET ?3",
            )?;
            stmt.query_map(params![member, limit, offset], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .collect::<Result<Vec<_>>>()?
        } else {
            let sql = "SELECT value FROM team_work WHERE (requester = ?1 OR assignee = ?1)
                 AND state NOT IN (?2, ?3, ?4, ?5)
                 ORDER BY updated_at DESC, id LIMIT ?6 OFFSET ?7";
            let mut stmt = self.conn.prepare(sql)?;
            let terminal: [&str; 4] = [
                TERMINAL_WORK_STATES[0].as_str(),
                TERMINAL_WORK_STATES[1].as_str(),
                TERMINAL_WORK_STATES[2].as_str(),
                TERMINAL_WORK_STATES[3].as_str(),
            ];
            let mut bound: Vec<&dyn rusqlite::ToSql> = vec![&member];
            bound.extend(terminal.iter().map(|state| state as &dyn rusqlite::ToSql));
            bound.push(&limit);
            bound.push(&offset);
            stmt.query_map(bound.as_slice(), |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
                .into_iter()
                .map(|value| serde_json::from_str(&value).map_err(Into::into))
                .collect::<Result<Vec<_>>>()?
        };
        Ok(rows)
    }

    /// Work the member has accepted but not yet closed: the active load
    /// (`activePeerWorkCount`).
    pub fn active_peer_work_count(&self, member: &str) -> Result<i64> {
        let mut count = 0i64;
        for state in ACTIVE_WORK_STATES {
            let row: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM team_work WHERE assignee = ?1 AND state = ?2",
                params![member, state.as_str()],
                |row| row.get(0),
            )?;
            count += row;
        }
        Ok(count)
    }

    /// Domain owners define the unit of work; persistence guarantees its
    /// atomicity (`transaction`). The operation's own error type passes
    /// through unchanged — business refusals keep their status.
    pub fn transaction<T, E>(
        &self,
        op: impl FnOnce() -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E>
    where
        E: From<TeamStoreError>,
    {
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|error| E::from(TeamStoreError::from(error)))?;
        match op() {
            Ok(value) => {
                self.conn
                    .execute_batch("COMMIT")
                    .map_err(|error| E::from(TeamStoreError::from(error)))?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    pub fn append_audit(
        &self,
        kind: &str,
        actor: &str,
        detail: Option<&str>,
        at: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO team_audit (kind, actor, detail, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![kind, actor, detail, at],
        )?;
        Ok(())
    }

    pub fn list_audit(&self, limit: i64) -> Result<Vec<TeamAuditEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, kind, actor, detail, created_at FROM team_audit ORDER BY seq DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], |row| {
                Ok(TeamAuditEntry {
                    seq: row.get(0)?,
                    kind: row.get(1)?,
                    actor: row.get(2)?,
                    detail: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn touch_member(&self, name: &str, at: i64) -> Result<()> {
        let lower = name.to_lowercase();
        self.conn.execute(
            "UPDATE team_members SET last_seen = ?2 WHERE name_lower = ?1 AND removed_at IS NULL",
            params![lower, at],
        )?;
        Ok(())
    }

    pub fn online_member_names(&self, now: i64, window_ms: i64) -> Result<HashSet<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT name FROM team_members WHERE removed_at IS NULL AND (role = 'hub' OR last_seen >= ?1)",
        )?;
        let rows = stmt
            .query_map(params![now - window_ms], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok(rows)
    }

    pub fn member_summaries(&self, now: i64, window_ms: i64) -> Result<Vec<MemberSummary>> {
        let online = self.online_member_names(now, window_ms)?;
        let mut stmt = self.conn.prepare(
            "SELECT name, role, last_seen,
               (SELECT created_at FROM team_messages WHERE sender = m.name ORDER BY seq DESC LIMIT 1),
               (SELECT recipient FROM team_messages WHERE sender = m.name ORDER BY seq DESC LIMIT 1)
             FROM team_members m WHERE removed_at IS NULL ORDER BY created_at ASC, name ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (name, role, last_seen, last_message_at, last_message_to) in rows {
            out.push(MemberSummary {
                online: online.contains(&name),
                profile: self.peer_profile(&name)?,
                capabilities: self.peer_capabilities(&name)?,
                active_work_count: self.active_peer_work_count(&name)?,
                name,
                role,
                last_seen,
                last_message_at,
                last_message_to,
            });
        }
        Ok(out)
    }

    pub fn insert_file(&self, file: &TeamFile) -> Result<()> {
        self.conn.execute(
            "INSERT INTO team_files (id, name, size, sha256, sender, recipient, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                file.id,
                file.name,
                file.size,
                file.sha256,
                file.sender,
                file.recipient,
                file.created_at
            ],
        )?;
        Ok(())
    }

    pub fn get_file(&self, id: &str) -> Result<Option<TeamFile>> {
        self.conn
            .query_row(
                "SELECT id, name, size, sha256, sender, recipient, created_at FROM team_files WHERE id = ?1",
                params![id],
                |row| {
                    Ok(TeamFile {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        size: row.get(2)?,
                        sha256: row.get(3)?,
                        sender: row.get(4)?,
                        recipient: row.get(5)?,
                        created_at: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn files_for_member(&self, name: &str) -> Result<Vec<TeamFile>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, size, sha256, sender, recipient, created_at FROM team_files
             WHERE recipient = ?1 OR recipient = 'group'
             ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map(params![name], |row| {
                Ok(TeamFile {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    size: row.get(2)?,
                    sha256: row.get(3)?,
                    sender: row.get(4)?,
                    recipient: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn remove_file(&self, id: &str) -> Result<bool> {
        let changed = self
            .conn
            .execute("DELETE FROM team_files WHERE id = ?1", params![id])?;
        Ok(changed > 0)
    }
}
