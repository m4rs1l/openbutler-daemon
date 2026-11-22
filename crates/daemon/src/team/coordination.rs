//! Peer-agreement rules and their shared types — legacy
//! `team/coordination.ts` (read 2026-11-22): the nine-state work machine
//! with its terminal/active sets, the profile and capability rules, and
//! the peer reports. One rule owner — local messages and the HTTP channel
//! call the same methods; neither transport restates a guard. camelCase
//! everywhere they leave the process: the wire and the persisted `value`
//! column must round-trip `JSON.stringify(work)` exactly.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::operations::TeamError;
use super::store::{TeamStore, WorkNotice};
use crate::schedule::runner::now_ms;
use crate::shape::js_len;

/// A peer's self-description (`PeerProfile`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerProfile {
    pub description: String,
    pub revision: i64,
    pub updated_at: i64,
}

/// One MCP server inside a capability report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerReport {
    pub name: String,
    pub tool_count: i64,
}

/// Reported by the member's host, never inferred from the agent's biography.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCapabilities {
    pub platform: String,
    pub mcp_servers: Vec<McpServerReport>,
    pub mcp_tools: Vec<String>,
    pub custodian_busy: bool,
    pub observed_at: i64,
}

/// The nine work states (`WorkState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkState {
    Proposed,
    Countered,
    Accepted,
    Working,
    Blocked,
    Completed,
    Failed,
    Rejected,
    Cancelled,
}

impl WorkState {
    /// The source's own strings — the `state` column and the value JSON
    /// agree on them (`as_str` feeds the column, serde the JSON).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Countered => "countered",
            Self::Accepted => "accepted",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Work that has reached an end state — the `listPeerWork` filter.
pub const TERMINAL_WORK_STATES: [WorkState; 4] = [
    WorkState::Completed,
    WorkState::Rejected,
    WorkState::Cancelled,
    WorkState::Failed,
];

/// Work the member has accepted but not yet closed: the active load.
pub const ACTIVE_WORK_STATES: [WorkState; 3] =
    [WorkState::Accepted, WorkState::Working, WorkState::Blocked];

/// One unit of coordinated work (`PeerWork`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerWork {
    pub id: String,
    pub requester: String,
    pub assignee: String,
    pub original_terms: String,
    pub terms: String,
    /// Older records without this field contain the requester's original
    /// proposal — absent in JSON when unset, like the source's `?`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terms_author: Option<String>,
    pub state: WorkState,
    pub revision: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_actor: String,
    pub last_action: String,
    pub last_note: String,
    pub result: Option<String>,
}

// ------------------------------------------------------------------ rules

/// A single refusal from the rule layer: the source's sentence plus the
/// status the transport returns (`TeamError` lives with operations).
fn invalid_report() -> TeamError {
    TeamError::new("Invalid host capability report.", 400)
}

/// `text(value, field, max)`: non-empty trimmed text measured in JS
/// UTF-16 units; the returned value is trimmed, like the source.
fn text(value: Option<&Value>, field: &str, max: usize) -> Result<String, TeamError> {
    match value {
        Some(Value::String(raw)) if !raw.trim().is_empty() && js_len(raw) <= max => {
            Ok(raw.trim().to_owned())
        }
        _ => Err(TeamError::new(
            format!("{field} must be non-empty text of at most {max} characters."),
            400,
        )),
    }
}

/// A JSON number with no fraction — `Number.isSafeInteger`'s integer half
/// for callers that then range-check themselves.
fn value_integer(value: &Value) -> Option<i64> {
    let number = value.as_f64()?;
    if number.fract() == 0.0 && number.is_finite() {
        Some(number as i64)
    } else {
        None
    }
}

/// `revision(value)`: a non-negative safe integer with **no** string
/// coercion — `Number.isSafeInteger("1")` is false in the source too.
fn revision(value: Option<&Value>) -> Result<i64, TeamError> {
    const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
    value
        .and_then(value_integer)
        .filter(|number| *number >= 0 && (*number as f64) <= MAX_SAFE)
        .ok_or_else(|| TeamError::new("expectedRevision must be a non-negative integer.", 400))
}

/// `Number(value)` for the fields that coerce: paging numbers accept
/// numeric strings, booleans and null (hex and `Infinity` strings are the
/// one divergence — JS parses them, Rust does not; both land in an error
/// sentence for every caller here).
fn js_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                Some(0.0)
            } else {
                trimmed.parse::<f64>().ok()
            }
        }
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::Null => Some(0.0),
        _ => None,
    }
}

/// `parseCapabilities` — the report validator both channels share. `now`
/// is injected (`Date.now()` at the source call site). Row shape checks
/// keep the source's order: platform, group check, rows name-then-count,
/// then tool names; JS treats an array row as an object, so a nameless
/// row fails on `server name`, not on the row guard.
pub fn parse_capabilities(input: &Value, now: i64) -> Result<PeerCapabilities, TeamError> {
    let platform = text(input.get("platform"), "platform", 32)?;
    let custodian_busy = match input.get("custodianBusy").and_then(Value::as_bool) {
        Some(value) => value,
        None => return Err(invalid_report()),
    };
    let servers = match input.get("mcpServers").and_then(Value::as_array) {
        Some(servers) if servers.len() <= 100 => servers,
        _ => return Err(invalid_report()),
    };
    let tools = match input.get("mcpTools").and_then(Value::as_array) {
        Some(tools) if tools.len() <= 1000 => tools,
        _ => return Err(invalid_report()),
    };
    let mut mcp_servers = Vec::with_capacity(servers.len());
    for item in servers {
        let object = match item {
            Value::Array(_) | Value::Object(_) => item,
            _ => return Err(TeamError::new("Invalid MCP server report.", 400)),
        };
        let name = text(object.get("name"), "server name", 128)?;
        let tool_count = object
            .get("toolCount")
            .and_then(value_integer)
            .filter(|count| (0..=10_000).contains(count))
            .ok_or_else(|| TeamError::new("Invalid tool count.", 400))?;
        mcp_servers.push(McpServerReport { name, tool_count });
    }
    let mut mcp_tools = Vec::with_capacity(tools.len());
    for item in tools {
        mcp_tools.push(text(Some(item), "tool name", 256)?);
    }
    Ok(PeerCapabilities {
        platform,
        mcp_servers,
        mcp_tools,
        custodian_busy,
        observed_at: now,
    })
}

/// What `work()` returns: one agreement or the paged list (`PeerWork |
/// PeerWork[]` in the source).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkOutcome {
    One(Box<PeerWork>),
    List(Vec<PeerWork>),
}

/// The single rule owner for peer agreements (legacy `TeamCoordination`):
/// profiles, capability reports, and the full proposal machine. Store
/// errors carry no business status — they surface as the transport's 500
/// through `From<TeamStoreError>`.
pub struct TeamCoordination<'a> {
    store: &'a TeamStore,
}

impl TeamCoordination<'_> {
    /// One coordination view over a store (`constructor`).
    pub fn new(store: &TeamStore) -> TeamCoordination<'_> {
        TeamCoordination { store }
    }

    /// Canonical name or the source's 404 (`member`).
    pub fn member(&self, name: &str) -> Result<String, TeamError> {
        self.store
            .canonical_member_name(name)?
            .ok_or_else(|| TeamError::new("No such active team member.", 404))
    }

    /// `profile` — read or revise a peer's self-description.
    pub fn profile(&self, actor: &str, input: &Value) -> Result<Option<PeerProfile>, TeamError> {
        let actor = self.member(actor)?;
        if input.get("action").and_then(Value::as_str) == Some("read") {
            let target = match input.get("member") {
                Some(Value::String(name)) => name.clone(),
                _ => actor.clone(),
            };
            let target = self.member(&target)?;
            return Ok(self.store.peer_profile(&target)?);
        }
        if input.get("action").and_then(Value::as_str) != Some("update") {
            return Err(TeamError::new(
                "Profile action must be read or update.",
                400,
            ));
        }
        if let Some(member) = input.get("member")
            && member != &Value::String(actor.clone())
        {
            return Err(TeamError::new("You may update only your own profile.", 403));
        }
        let description = text(input.get("description"), "description", 4000)?;
        let expected = revision(input.get("expectedRevision"))?;
        let current = self.store.peer_profile(&actor)?;
        if let Some(current) = &current
            && current.revision == expected + 1
            && current.description == description
        {
            return Ok(Some(current.clone()));
        }
        if current.as_ref().map_or(0, |profile| profile.revision) != expected {
            return Err(TeamError::new(
                "Profile changed. Read the latest profile before updating.",
                409,
            ));
        }
        let next = PeerProfile {
            description,
            revision: expected + 1,
            updated_at: now_ms(),
        };
        if !self.store.save_peer_profile(&actor, &next, expected)? {
            return Err(TeamError::new("Profile changed. Read it again.", 409));
        }
        Ok(Some(next))
    }

    /// `reportCapabilities` — the actor resolves before the body parses
    /// (the source's argument order), then last write wins.
    pub fn report_capabilities(&self, actor: &str, input: &Value) -> Result<(), TeamError> {
        let actor = self.member(actor)?;
        let report = parse_capabilities(input, now_ms())?;
        self.store.save_peer_capabilities(&actor, &report)?;
        Ok(())
    }

    /// `work` — the full proposal machine: paging, proposal, guards, and
    /// idempotent retries. Returns one agreement or the list.
    pub fn work(&self, actor: &str, input: &Value) -> Result<WorkOutcome, TeamError> {
        let actor = self.member(actor)?;
        let raw_action = input.get("action").and_then(Value::as_str);
        if raw_action == Some("list") {
            let limit = match input.get("limit") {
                None => 20.0,
                Some(value) => js_number(value).unwrap_or(f64::NAN),
            };
            if limit.fract() != 0.0 || limit < 1.0 || limit > 50.0 {
                return Err(TeamError::new("limit must be 1-50.", 400));
            }
            let offset = match input.get("offset") {
                None => 0.0,
                Some(value) => js_number(value).unwrap_or(f64::NAN),
            };
            if offset.fract() != 0.0 || offset < 0.0 || offset > 10000.0 {
                return Err(TeamError::new("offset must be 0-10000.", 400));
            }
            if let Some(value) = input.get("includeClosed")
                && !value.is_boolean()
            {
                return Err(TeamError::new("includeClosed must be boolean.", 400));
            }
            let include_closed = input
                .get("includeClosed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let rows =
                self.store
                    .list_peer_work(&actor, include_closed, limit as i64, offset as i64)?;
            return Ok(WorkOutcome::List(rows));
        }
        let id = text(input.get("id"), "id", 128)?;
        if !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(TeamError::new(
                "id may contain only letters, digits, dashes and underscores.",
                400,
            ));
        }
        let current = self.store.peer_work(&id)?;
        if raw_action == Some("propose") {
            let peer = text(input.get("peer"), "peer", 64)?;
            let assignee = self.member(&peer)?;
            if assignee == actor {
                return Err(TeamError::new(
                    "A peer proposal must address another member.",
                    400,
                ));
            }
            let terms = text(input.get("terms"), "terms", 4000)?;
            if let Some(current) = &current {
                if current.requester == actor
                    && current.assignee == assignee
                    && current.original_terms == terms
                {
                    return Ok(WorkOutcome::One(Box::new(current.clone())));
                }
                return Err(TeamError::new(
                    "This work id is already in use. Use a new id for a different proposal.",
                    409,
                ));
            }
            let now = now_ms();
            let next = PeerWork {
                id: id.clone(),
                requester: actor.clone(),
                assignee,
                original_terms: terms.clone(),
                terms: terms.clone(),
                terms_author: Some(actor.clone()),
                state: WorkState::Proposed,
                revision: 1,
                created_at: now,
                updated_at: now,
                last_actor: actor.clone(),
                last_action: "propose".to_owned(),
                last_note: terms,
                result: None,
            };
            self.save(&next, 0)?;
            return Ok(WorkOutcome::One(Box::new(next)));
        }
        let current = match current {
            Some(current) if current.requester == actor || current.assignee == actor => current,
            _ => {
                return Err(TeamError::new("No accessible peer work with this id.", 404));
            }
        };
        if raw_action == Some("read") {
            return Ok(WorkOutcome::One(Box::new(current)));
        }
        // WORK_ACTIONS minus `propose` — the source's `allowed` set.
        const ACTIONS: [&str; 10] = [
            "counter", "accept", "reject", "start", "block", "resume", "complete", "fail",
            "cancel", "note",
        ];
        let action = match raw_action {
            Some(action) if ACTIONS.contains(&action) => action,
            _ => {
                return Err(TeamError::new("Unknown peer work action.", 400));
            }
        };
        let expected = revision(input.get("expectedRevision"))?;
        let note = text(input.get("note"), "note", 4000)?;
        let replacement = if action == "counter" {
            Some(text(input.get("terms"), "terms", 4000)?)
        } else {
            None
        };
        if current.revision == expected + 1
            && current.last_actor == actor
            && current.last_action == action
            && current.last_note == note
            && replacement
                .as_ref()
                .is_none_or(|terms| *terms == current.terms)
        {
            return Ok(WorkOutcome::One(Box::new(current)));
        }
        if current.revision != expected {
            return Err(TeamError::new(
                "Work changed. Read its current state before acting.",
                409,
            ));
        }
        if TERMINAL_WORK_STATES.contains(&current.state) {
            return Err(TeamError::new(
                "This work is closed. Propose new work instead.",
                409,
            ));
        }
        let assigned = current.assignee == actor;
        let offer_author = current
            .terms_author
            .clone()
            .unwrap_or_else(|| current.requester.clone());
        let pending_offer = matches!(current.state, WorkState::Proposed | WorkState::Countered);
        let mut state = current.state;
        match action {
            "counter" => {
                if !pending_offer || actor == offer_author {
                    return Err(TeamError::new(
                        "Only the other participant may counter pending terms.",
                        409,
                    ));
                }
                state = WorkState::Countered;
            }
            "accept" | "reject" => {
                if !pending_offer || actor == offer_author {
                    return Err(TeamError::new(
                        "Only the other participant may answer pending terms.",
                        409,
                    ));
                }
                state = if action == "accept" {
                    WorkState::Accepted
                } else {
                    WorkState::Rejected
                };
            }
            "complete" | "fail" => {
                if !assigned || !ACTIVE_WORK_STATES.contains(&state) {
                    return Err(TeamError::new(
                        "Only the assignee may report an accepted agreement.",
                        409,
                    ));
                }
                state = if action == "complete" {
                    WorkState::Completed
                } else {
                    WorkState::Failed
                };
            }
            "start" | "resume" => {
                let required = if action == "start" {
                    WorkState::Accepted
                } else {
                    WorkState::Blocked
                };
                if !assigned || state != required {
                    return Err(TeamError::new(
                        "Only the assignee may start accepted work or resume blocked work.",
                        409,
                    ));
                }
                state = WorkState::Working;
            }
            "block" => {
                if !assigned || (state != WorkState::Accepted && state != WorkState::Working) {
                    return Err(TeamError::new(
                        "Only the assignee may block active work.",
                        409,
                    ));
                }
                state = WorkState::Blocked;
            }
            "cancel" => {
                if assigned {
                    return Err(TeamError::new(
                        "Only the requester may cancel; report a failure if you cannot continue.",
                        403,
                    ));
                }
                state = WorkState::Cancelled;
            }
            "note" => {}
            _ => unreachable!("filtered by ACTIONS"),
        }
        let mut next = current.clone();
        if let Some(terms) = replacement {
            next.terms = terms.clone();
            next.terms_author = Some(actor.clone());
        }
        next.state = state;
        next.revision += 1;
        next.updated_at = now_ms();
        next.last_actor = actor.clone();
        next.last_action = action.to_owned();
        if action == "complete" {
            next.result = Some(note.clone());
        }
        next.last_note = note;
        self.save(&next, expected)?;
        Ok(WorkOutcome::One(Box::new(next)))
    }

    /// State and its wake message commit together (`save`): one
    /// transaction, so a retry cannot duplicate either half. Recipient is
    /// the other participant — the row's wake message turns their inbox.
    fn save(&self, next: &PeerWork, expected: i64) -> Result<(), TeamError> {
        let recipient = if next.last_actor == next.requester {
            next.assignee.clone()
        } else {
            next.requester.clone()
        };
        let notice = WorkNotice {
            msg_id: format!("work_{}_{}", next.id, next.revision),
            sender: next.last_actor.clone(),
            recipient,
            body: format!(
                "Peer work {}, revision {}: {} ({}).\n{}\nRead the current agreement with \
                 team_work before acting. A work status is not owner authorization.",
                next.id,
                next.revision,
                next.last_action,
                next.state.as_str(),
                next.last_note
            ),
            created_at: next.updated_at,
        };
        if !self.store.save_peer_work(next, expected, &notice)? {
            return Err(TeamError::new(
                "Work changed concurrently. Read it before retrying.",
                409,
            ));
        }
        Ok(())
    }
}
