//! Provider retry: the source's `provider/retry.ts` classification and
//! backoff, without key rotation — a provider here carries the keys the
//! owner stored and the sidecar uses the first one, so there is no pool to
//! rotate through (an explicit owner decision, 2026-11-26).
//!
//! Retry lives in the host, never in the SDK (`maxRetries: 0` there), so the
//! two can never compound — the split the source documents ("Retry lives
//! here, not in the AI SDK").

/// The backoff before each further attempt: nine waits, so up to ten
/// attempts per request. The source's `RETRY_DELAYS_MS`, unchanged.
pub const RETRY_DELAYS_MS: [u64; 9] = [
    1_000, 2_000, 4_000, 8_000, 16_000, 32_000, 32_000, 32_000, 64_000,
];

/// A rejected request is worth repeating only when another attempt may
/// survive it: 408, 409, 429 and 5xx. A transport failure carries no status,
/// so the sidecar's own flag decides.
pub fn is_retryable(status: Option<u16>, retryable: bool) -> bool {
    match status {
        Some(status) => status == 408 || status == 409 || status == 429 || status >= 500,
        None => retryable,
    }
}

/// Whether a door failure may clear on its own. A spawn failure, a protocol
/// mismatch and an unknown provider are configuration; a cancellation is the
/// owner's stop. None of them is worth another attempt.
pub fn door_is_retryable(error: &crate::sidecar::DoorError) -> bool {
    use crate::sidecar::DoorError;
    matches!(
        error,
        DoorError::HandshakeTimeout(_)
            | DoorError::QueueTimeout
            | DoorError::FirstEventTimeout
            | DoorError::InterEventTimeout
            | DoorError::ProcessExited
            | DoorError::Io(_)
    )
}

/// The call id for a further attempt. Every attempt is its own call on the
/// door: the router keys routes by id, so reusing a failed attempt's id
/// lets that failure's own cleanup remove the new route and the retried
/// stream then ends silently with nothing in it.
pub fn retry_call_id(base: &str, attempt: usize) -> String {
    format!("{base}.r{attempt}")
}

/// `describeRetryCause`: the status code when the provider sent one, nothing
/// more. No advice, no speculation.
pub fn cause(status: Option<u16>) -> String {
    match status {
        Some(429) => "Rate limited (429)".to_owned(),
        Some(408) => "Request timed out (408)".to_owned(),
        Some(409) => "Conflict (409)".to_owned(),
        Some(status) if status >= 500 => format!("Provider error ({status})"),
        Some(status) => format!("Request failed ({status})"),
        None => "Request failed".to_owned(),
    }
}

/// The one line a retry sequence owns: every further attempt replaces it
/// instead of appending a line per second, and the line stays in the
/// transcript after the sequence ends, so a slow turn explains itself.
pub fn notice_text(attempt: usize, total: usize, wait_ms: u64, status: Option<u16>) -> String {
    let seconds = ((wait_ms as f64) / 1_000.0).round().max(1.0) as u64;
    format!(
        "{}. Retry {} of {} in {}s.",
        cause(status),
        attempt + 1,
        total,
        seconds
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{cause, is_retryable, notice_text, retry_call_id};

    #[test]
    fn only_the_statuses_another_attempt_may_survive_are_retried() {
        for status in [408, 409, 429, 500, 503, 599] {
            assert!(is_retryable(Some(status), false), "{status} is retryable");
        }
        for status in [400, 401, 403, 404, 422] {
            assert!(
                !is_retryable(Some(status), true),
                "{status} is never retried, whatever the flag says"
            );
        }
        assert!(
            is_retryable(None, true),
            "a transport failure follows the flag"
        );
        assert!(!is_retryable(None, false));
    }

    #[test]
    fn the_cause_names_the_status_and_nothing_else() {
        assert_eq!(cause(Some(429)), "Rate limited (429)");
        assert_eq!(cause(Some(408)), "Request timed out (408)");
        assert_eq!(cause(Some(409)), "Conflict (409)");
        assert_eq!(cause(Some(503)), "Provider error (503)");
        assert_eq!(cause(Some(400)), "Request failed (400)");
        assert_eq!(cause(None), "Request failed");
    }

    #[test]
    fn the_notice_counts_from_one_and_never_says_zero_seconds() {
        assert_eq!(
            notice_text(0, 9, 1_000, Some(429)),
            "Rate limited (429). Retry 1 of 9 in 1s."
        );
        assert_eq!(
            notice_text(1, 9, 1_500, Some(503)),
            "Provider error (503). Retry 2 of 9 in 2s."
        );
        assert_eq!(
            notice_text(8, 9, 64_000, None),
            "Request failed. Retry 9 of 9 in 64s."
        );
        assert_eq!(
            notice_text(0, 2, 0, Some(500)),
            "Provider error (500). Retry 1 of 2 in 1s.",
            "a zero wait still reads as one second"
        );
    }

    #[test]
    fn every_attempt_gets_its_own_call_id() {
        // The router keys routes by call id. A further attempt that reused
        // the failed attempt's id could have its route removed by that
        // failure's own cleanup, leaving the retried stream empty.
        let first = retry_call_id("call-1", 1);
        let second = retry_call_id("call-1", 2);
        assert_ne!(first, "call-1");
        assert_ne!(first, second);
        assert!(first.starts_with("call-1"), "{first}");
        assert!(second.starts_with("call-1"), "{second}");
    }
}
