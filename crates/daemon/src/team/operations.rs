//! Team business rules — the single owner of validation and canonicalisation.
//!
//! Source: legacy `team/operations.ts` (254 lines, read 2026-11-22). Both
//! transports (the hub's HTTP surface and the hub's own local channel) go
//! through here, so a message is validated and canonicalized the same way no
//! matter which path it takes. The store stays a dumb persistence layer;
//! nothing else writes to it. Every refusal sentence and HTTP status is the
//! source's.

use super::contracts::{MAX_TEAM_MESSAGE_ID_CHARS, TeamMessage};
use super::service::{
    GROUP_RECIPIENT, fingerprint_of, hash_ticket, is_group_recipient, make_file_id,
    make_invite_code, make_message_id, make_request_id, make_ticket, validate_member_name,
};
use super::store::{MessageInput, Removal, TeamFile, TeamStore, TeamStoreError, TeamStoreResult};
use crate::shape::js_len;

/// Largest file the hub will accept and relay, in bytes (`MAX_FILE_BYTES`).
pub const MAX_FILE_BYTES: i64 = 1024 * 1024 * 1024;
/// The message body budget (`MAX_MESSAGE_CHARS`).
pub const MAX_MESSAGE_CHARS: i64 = 8000;

/// A rejected team operation carrying the HTTP status the transport should
/// use. Business rules throw this; the transport only maps it. The rule
/// lives here, once, and never in a transport handler (`TeamError`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct TeamError {
    pub message: String,
    pub status: u16,
}

impl TeamError {
    /// One refusal: the source's sentence plus the status it carries.
    pub fn new(message: impl Into<String>, status: u16) -> Self {
        Self {
            message: message.into(),
            status,
        }
    }
}

/// A store failure carries no business status: the transport's default (500).
impl From<TeamStoreError> for TeamError {
    fn from(error: TeamStoreError) -> Self {
        TeamError::new(error.to_string(), 500)
    }
}

/// `postMessage` either recognised a retry or stored a new message
/// (`PostMessageResult`).
#[derive(Debug, Clone, PartialEq)]
pub enum PostMessageResult {
    Duplicate { msg_id: String },
    Sent { message: TeamMessage },
}

/// One message to post (`postMessage`'s input object; `now` is the hub
/// clock, the single time authority).
#[derive(Debug, Clone, Default)]
pub struct NewMessage {
    pub recipient: String,
    pub body: String,
    pub reply_to: Option<String>,
    pub msg_id: Option<String>,
    pub now: i64,
}

/// One file announcement to register (`registerFile`'s input object).
#[derive(Debug, Clone)]
pub struct NewFile {
    pub recipient: String,
    pub name: String,
    pub size: i64,
    pub sha256: String,
    pub file_id: Option<String>,
    pub now: i64,
}

/// A page of the group channel with the reader's cursor and the channel's
/// high-water mark (`readGroup`).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupRead {
    pub messages: Vec<TeamMessage>,
    pub cursor: i64,
    pub latest_seq: i64,
}

/// A freshly minted invite and the hub's pairing fingerprint
/// (`createInvite`).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteCreated {
    pub code: String,
    pub fingerprint: String,
}

/// The outcome of one join decision (`decideJoin`).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinDecision {
    pub decided: String,
    pub name: String,
}

/// The single owner of team business rules over one store.
pub struct TeamOperations<'a> {
    pub store: &'a TeamStore,
}

/// Store failures map to the transport default through `?`.
fn store_error(error: TeamStoreError) -> TeamError {
    error.into()
}

impl TeamOperations<'_> {
    /// Resolves a recipient to its canonical form: the group channel, or an
    /// active member's real name (matched case-insensitively). Unknown or
    /// invalid recipients are refused here, once, for every path.
    pub fn resolve_recipient(&self, recipient: &str) -> Result<String, TeamError> {
        let trimmed = recipient.trim();
        if is_group_recipient(trimmed) {
            return Ok(GROUP_RECIPIENT.to_owned());
        }
        let valid = validate_member_name(trimmed).map_err(|_| {
            TeamError::new(format!("\"{recipient}\" is not a valid recipient."), 400)
        })?;
        let canonical = self
            .store
            .canonical_member_name(&valid)
            .map_err(store_error)?;
        match canonical {
            Some(name) => Ok(name),
            None => Err(TeamError::new(
                format!("No member named \"{valid}\" in this team."),
                404,
            )),
        }
    }

    /// One page of the group channel, oldest first, from the member's
    /// cursor (`readGroup`).
    pub fn read_group(&self, member: &str, limit: i64) -> Result<GroupRead, TeamError> {
        if !(1..=200).contains(&limit) {
            return Err(TeamError::new("Group limit must be 1-200.", 400));
        }
        let cursor = self.store.get_group_cursor(member).map_err(store_error)?;
        let messages = self
            .store
            .group_messages(cursor, limit)
            .map_err(store_error)?;
        let latest_seq = self.store.group_latest_seq().map_err(store_error)?;
        Ok(GroupRead {
            messages,
            cursor,
            latest_seq,
        })
    }

    /// Advances the member's group cursor (`acknowledgeGroup`).
    pub fn acknowledge_group(&self, member: &str, seq: i64) -> Result<i64, TeamError> {
        let latest = self.store.group_latest_seq().map_err(store_error)?;
        if seq < 0 || seq > latest {
            return Err(TeamError::new(
                "Invalid group acknowledgement sequence.",
                400,
            ));
        }
        self.store
            .set_group_cursor(member, seq)
            .map_err(store_error)?;
        self.store.get_group_cursor(member).map_err(store_error)
    }

    /// Validates and stores one message under the hub clock. The recipient
    /// is stored canonically, and a repeated `msg_id` is recognised as a
    /// retry so it is never delivered twice. Replying to a direct message
    /// (`reply_to`) also acknowledges it for the sender, so answering a
    /// message closes it without a separate step.
    pub fn post_message(
        &self,
        sender: &str,
        input: &NewMessage,
    ) -> Result<PostMessageResult, TeamError> {
        if input.body.trim().is_empty() {
            return Err(TeamError::new("A message body cannot be empty.", 400));
        }
        if js_len(&input.body) > MAX_MESSAGE_CHARS as usize {
            return Err(TeamError::new(
                format!("A message body is limited to {MAX_MESSAGE_CHARS} characters."),
                400,
            ));
        }
        let msg_id = match input.msg_id.as_deref().map(str::trim) {
            Some(id) if !id.is_empty() => id.to_owned(),
            _ => make_message_id()?,
        };
        if js_len(&msg_id) > MAX_TEAM_MESSAGE_ID_CHARS {
            return Err(TeamError::new("Message id is too long.", 400));
        }
        let sender_owned = sender.to_owned();
        let input_owned = input.clone();
        self.store
            .transaction(move || -> Result<PostMessageResult, TeamError> {
                if let Some(existing) = self.store.get_message(&msg_id)? {
                    let same_recipient = existing
                        .recipient
                        .eq_ignore_ascii_case(input_owned.recipient.trim());
                    if existing.sender != sender_owned
                        || !same_recipient
                        || existing.kind != "text"
                        || existing.body != input_owned.body
                        || existing.reply_to != input_owned.reply_to
                    {
                        return Err(TeamError::new(
                            "Message id already belongs to a different operation.",
                            409,
                        ));
                    }
                    // Also repair a historical split reply/ack when a matching
                    // operation retries.
                    if let Some(reply) = input_owned.reply_to.as_deref() {
                        self.store
                            .ack_dms(&sender_owned, &[reply], input_owned.now)?;
                    }
                    return Ok(PostMessageResult::Duplicate { msg_id });
                }
                let recipient = self.resolve_recipient(&input_owned.recipient)?;
                let message = self.store.post_message(&MessageInput {
                    msg_id: msg_id.clone(),
                    sender: sender_owned.clone(),
                    recipient,
                    body: input_owned.body.clone(),
                    created_at: input_owned.now,
                    reply_to: input_owned.reply_to.clone(),
                    ..Default::default()
                })?;
                // A reply closes the message it answers: acknowledge any
                // reply_to id that is a direct message this sender owns
                // (ack_dms only touches the sender's own inbox, so a group id
                // or another member's id is a no-op).
                if let Some(reply) = input_owned.reply_to.as_deref() {
                    self.store
                        .ack_dms(&sender_owned, &[reply], input_owned.now)?;
                }
                Ok(PostMessageResult::Sent { message })
            })
    }

    /// Validates a file announcement's rules: canonical recipient, a sane
    /// name, a bounded size, and a well-formed sha256. The bytes are stored
    /// on the hub's disk by the transport before this runs; sending a file
    /// needs no approval, receiving it does (`prepareFile`).
    pub fn prepare_file(&self, sender: &str, input: &NewFile) -> Result<TeamFile, TeamError> {
        let name = input.name.trim();
        if name.is_empty() || js_len(name) > 255 || name.contains(['\r', '\n']) {
            return Err(TeamError::new(
                "A file name must be 1-255 characters without line breaks.",
                400,
            ));
        }
        if input.size < 0 {
            return Err(TeamError::new(
                "A file size must be a non-negative integer.",
                400,
            ));
        }
        if input.size > MAX_FILE_BYTES {
            return Err(TeamError::new(
                format!(
                    "A file is limited to {} MB.",
                    MAX_FILE_BYTES / (1024 * 1024)
                ),
                413,
            ));
        }
        let sha_ok = input.sha256.len() == 64
            && input
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !sha_ok {
            return Err(TeamError::new("A file needs a lowercase hex sha256.", 400));
        }
        let id = match input.file_id.as_deref().map(str::trim) {
            Some(id) if !id.is_empty() => id.to_owned(),
            _ => make_file_id()?,
        };
        // `^f_[0-9a-f]{8,}$` with a 128-byte ceiling.
        let hex_tail = id.strip_prefix("f_").unwrap_or("");
        if id.len() > 128
            || hex_tail.len() < 8
            || !hex_tail
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(TeamError::new("Invalid file transfer id.", 400));
        }
        if let Some(existing) = self.store.get_file(&id).map_err(store_error)? {
            let same_recipient = existing
                .recipient
                .eq_ignore_ascii_case(input.recipient.trim());
            if existing.sender != sender
                || !same_recipient
                || existing.name != name
                || existing.size != input.size
                || existing.sha256 != input.sha256
            {
                return Err(TeamError::new(
                    "Transfer id already belongs to a different file.",
                    409,
                ));
            }
            return Ok(existing);
        }
        let recipient = self.resolve_recipient(&input.recipient)?;
        Ok(TeamFile {
            id,
            name: name.to_owned(),
            size: input.size,
            sha256: input.sha256.clone(),
            sender: sender.to_owned(),
            recipient,
            created_at: input.now,
        })
    }

    /// Records a shared file's metadata and posts the message that announces
    /// it, in one transaction; a retry returns the same announcement
    /// (`registerFile`).
    pub fn register_file(&self, sender: &str, input: &NewFile) -> Result<TeamMessage, TeamError> {
        let file = self.prepare_file(sender, input)?;
        let sender_owned = sender.to_owned();
        let input_owned = input.clone();
        self.store
            .transaction(move || -> Result<TeamMessage, TeamError> {
                if let Some(existing) = self.store.get_file(&file.id)? {
                    if existing.sender != sender_owned
                        || existing.recipient != file.recipient
                        || existing.name != file.name
                        || existing.size != input_owned.size
                        || existing.sha256 != input_owned.sha256
                    {
                        return Err(TeamError::new(
                            "Transfer id already belongs to a different file.",
                            409,
                        ));
                    }
                    return match self.store.file_message(&file.id)? {
                        Some(message) => Ok(message),
                        None => Err(TeamError::new(
                            "File registration is missing its notification.",
                            500,
                        )),
                    };
                }
                self.store.insert_file(&file)?;
                self.store
                    .post_message(&MessageInput {
                        msg_id: make_message_id()?,
                        sender: sender_owned.clone(),
                        recipient: file.recipient.clone(),
                        kind: Some("file".to_owned()),
                        body: file.name.clone(),
                        file_id: Some(file.id.clone()),
                        created_at: input_owned.now,
                        ..Default::default()
                    })
                    .map_err(Into::into)
            })
    }

    /// The file as `member` may see it: addressed to them, shared with the
    /// group, or sent by them (`fileForMember`).
    pub fn file_for_member(&self, member: &str, id: &str) -> Result<TeamFile, TeamError> {
        let file = self
            .store
            .get_file(id)
            .map_err(store_error)?
            .ok_or_else(|| TeamError::new("No such file.", 404))?;
        if file.recipient != member && file.recipient != GROUP_RECIPIENT && file.sender != member {
            return Err(TeamError::new("This file is not addressed to you.", 403));
        }
        Ok(file)
    }

    /// Initializes the hub row (delegates to the store, whose refusal is the
    /// sentence).
    pub fn init_hub(
        &self,
        hub_id: &str,
        team_id: &str,
        team_name: &str,
        hub_name: &str,
        now: i64,
    ) -> TeamStoreResult<()> {
        self.store
            .init_hub(hub_id, team_id, team_name, hub_name, now)
    }

    /// Mints a live invite and records it (`createInvite`).
    pub fn create_invite(
        &self,
        actor: &str,
        ttl_ms: i64,
        now: i64,
    ) -> Result<InviteCreated, TeamError> {
        let initialized = self.store.is_initialized().map_err(store_error)?;
        if !initialized {
            return Err(TeamError::new(
                "Team store is not initialized. Run team init first.",
                400,
            ));
        }
        let code = make_invite_code()?;
        self.store
            .create_invite(&code, ttl_ms, now)
            .map_err(store_error)?;
        let hub_id = self.store.get_meta("hub_id").map_err(store_error)?;
        let hub_id = hub_id.ok_or_else(|| {
            TeamError::new("Team store is not initialized. Run team init first.", 400)
        })?;
        self.store
            .append_audit("invite.created", actor, Some(&code), now)?;
        Ok(InviteCreated {
            code,
            fingerprint: fingerprint_of(&hub_id),
        })
    }

    /// Revokes one invite; a failed revoke writes no audit (`revokeInvite`).
    pub fn revoke_invite(&self, actor: &str, code: &str, now: i64) -> Result<bool, TeamError> {
        if !self.store.revoke_invite(code).map_err(store_error)? {
            return Ok(false);
        }
        self.store
            .append_audit("invite.revoked", actor, Some(code), now)?;
        Ok(true)
    }

    /// Consumes the invite and opens a pending request (`requestJoin`).
    pub fn request_join(
        &self,
        code: &str,
        name: &str,
        now: i64,
    ) -> Result<super::store::JoinRequest, TeamError> {
        let invite = self
            .store
            .get_invite(code, now)
            .map_err(store_error)?
            .ok_or_else(|| TeamError::new("Invite code is invalid.", 404))?;
        if self.store.name_taken(name).map_err(store_error)? {
            return Err(TeamError::new(
                format!("\"{name}\" is already taken in this team. Pick another name."),
                409,
            ));
        }
        if !self
            .store
            .consume_invite(&invite.code, name, now)
            .map_err(store_error)?
        {
            return Err(TeamError::new("Invite code is invalid.", 404));
        }
        let id = make_request_id()?;
        self.store
            .create_join_request(&id, name, &invite.code, now)
            .map_err(store_error)?;
        self.store
            .append_audit("join.requested", name, Some(&invite.code), now)?;
        self.store
            .get_join_request(&id)?
            .ok_or_else(|| TeamError::new("Join request could not be recorded.", 500))
    }

    /// Decides a pending request; approval issues a fresh ticket and the
    /// taken-name race refuses (`decideJoin`).
    pub fn decide_join(
        &self,
        actor: &str,
        request_id: &str,
        approve: bool,
        now: i64,
    ) -> Result<JoinDecision, TeamError> {
        let entry = self
            .store
            .get_join_request(request_id)?
            .ok_or_else(|| TeamError::new("Unknown join request.", 404))?;
        if entry.status != "pending" {
            return Err(TeamError::new(
                format!("Request is already {}.", entry.status),
                409,
            ));
        }
        if !approve {
            self.store.decide_join(request_id, false, None, None, now)?;
            self.store
                .append_audit("join.denied", actor, Some(&entry.name), now)?;
            return Ok(JoinDecision {
                decided: "denied".to_owned(),
                name: entry.name,
            });
        }
        let ticket = make_ticket()?;
        let decided = self.store.decide_join(
            request_id,
            true,
            Some(&ticket),
            Some(&hash_ticket(&ticket)),
            now,
        )?;
        if !decided {
            return Err(TeamError::new(
                format!(
                    "\"{}\" is already taken in this team. Deny the request and invite them to pick another name.",
                    entry.name
                ),
                409,
            ));
        }
        self.store
            .append_audit("join.approved", actor, Some(&entry.name), now)?;
        Ok(JoinDecision {
            decided: "approved".to_owned(),
            name: entry.name,
        })
    }

    /// Removes a member and records it (`removeMember`).
    pub fn remove_member(&self, actor: &str, name: &str, now: i64) -> Result<Removal, TeamError> {
        let outcome = self.store.remove_member(name, now)?;
        if outcome == Removal::Removed {
            self.store
                .append_audit("member.removed", actor, Some(name), now)?;
        }
        Ok(outcome)
    }

    /// A member leaves on its own; the hub cannot (`leave`).
    pub fn leave(&self, name: &str, now: i64) -> Result<(), TeamError> {
        let outcome = self.store.remove_member(name, now)?;
        if outcome == Removal::Hub {
            return Err(TeamError::new(
                "The hub cannot leave its own team. Shut the team down instead.",
                403,
            ));
        }
        if outcome == Removal::Removed {
            self.store.append_audit("member.left", name, None, now)?;
        }
        Ok(())
    }
}
