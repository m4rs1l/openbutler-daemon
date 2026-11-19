//! Moved from settings/schema.rs by the split (see dispatch-style refactor notes).

use super::keys::SHELL_PRESETS;
/// `theme`: light or dark (the store's display preference).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
    System,
}

impl Theme {
    /// The stored wire spelling (`light`/`dark`).
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Dark => "Dark",
            Theme::Light => "Light",
            Theme::System => "System",
        }
    }
}

/// Approval policy for tool calls. The original `ask` value migrates to
/// `workspace` at load; nothing else is coerced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// `approvalMode`: how tool calls are gated — the folder-scoped
/// workspace mode, auto-review, or full access.
pub enum ApprovalMode {
    Workspace,
    AutoReview,
    FullAccess,
}

impl ApprovalMode {
    /// The stored wire spelling of the mode.
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalMode::Workspace => "workspace",
            ApprovalMode::AutoReview => "auto-review",
            ApprovalMode::FullAccess => "full-access",
        }
    }
}

/// Team membership role (`hub` | `member`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
/// `team.role`: hub or member — who owns the team connection.
pub enum TeamRole {
    Hub,
    Member,
}

/// `/^[A-Za-z0-9:_-]{1,128}$/` — a model variant id.
pub(super) fn is_variant_id(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 128
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ':' | '_' | '-'))
}

/// A preset name or anything path-shaped.
pub(super) fn is_shell_setting_value(value: &str) -> bool {
    SHELL_PRESETS.contains(&value) || value.contains('/') || value.contains('\\')
}
