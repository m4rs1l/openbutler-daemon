//! Manual approvals: the queue the gate parks on, and how a decision
//! reaches the run (`requestManualApproval` / `approveToolCall` ports).
use super::*;

impl TurnRunner {
    /// The pending approval for visibility surfaces (c3): its id — the
    /// `approvalId` clients answer — and the gate reason.
    pub fn pending_approval(&self) -> Option<(String, &'static str)> {
        self.approval
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|pending| (pending.request_id.clone(), pending.gate_reason))
    }

    /// `session.approve`: id-guarded resolve (`approveToolCall`, L1201 —
    /// a stale id no-ops), and the send happens under the same lock the
    /// timeout reads, so a decision can never be lost to the clock.
    pub fn resolve_approval(&self, approval_id: &str, approved: bool) -> bool {
        let mut slot = self
            .approval
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot
            .as_ref()
            .is_some_and(|pending| pending.request_id == approval_id)
            && let Some(pending) = slot.take()
        {
            let _ = pending.sender.send(approved);
            return true;
        }
        false
    }

    /// Queues one manual approval and waits out the decision:
    /// `allow`/`always` approve (the source's `decision !== 'deny'`), a
    /// decline declines, the owner's stop denies the pending approval
    /// (the source's stop handler, L1260 — and consumes the flag), and
    /// silence produces the timeout outcome the registry formats.
    pub(super) async fn request_manual_approval(
        &self,
        state: &mut SessionState,
        turn_id: &str,
        request: openbutler_protocol::session::ToolCallRequest,
        gate_reason: &'static str,
        timeout_seconds: u64,
    ) -> ApprovalOutcome {
        let request_id = request.request_id.clone();
        let (sender, mut receiver) = tokio::sync::oneshot::channel();
        *self
            .approval
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(PendingApproval {
            request_id: request_id.clone(),
            request,
            gate_reason,
            sender,
        });
        // The queue becomes visible the moment it exists (the source's
        // `requestManualApproval` publishes before it resolves).
        self.publish_now(state, turn_id, None, true);
        let mut stop_poll = tokio::time::interval(Duration::from_millis(100));
        stop_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tokio::select! {
            // Biased: a decision landing in the same tick as the clock
            // wins — the client's answer is never discarded as a timeout.
            biased;
            approved = &mut receiver => {
                if approved.unwrap_or(false) {
                    ApprovalOutcome::Approved
                } else {
                    ApprovalOutcome::Declined
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(timeout_seconds)) => {
                self.approval
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                ApprovalOutcome::TimedOut { seconds: timeout_seconds }
            }
            _ = stop_poll.tick(), if self.stop.load(Ordering::SeqCst) => {
                self.stop.store(false, Ordering::SeqCst);
                self.approval
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                ApprovalOutcome::Declined
            }
        }
    }
}
