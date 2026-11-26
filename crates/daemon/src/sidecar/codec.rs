//! Line framing for the sidecar's stdout, and the handshake gate.
//!
//! The three-way rule: a line that is not
//! JSON, is not an event object, or exceeds the cap, is a protocol
//! violation that kills the door; a well-formed event whose `type` is
//! unknown is skipped and logged so a newer sidecar never breaks us.

use openbutler_protocol::sidecar::{SidecarEvent, SidecarReady};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::ChildStdout;
use tokio::sync::mpsc;

use super::{DoorConfig, DoorError};

/// Every event type the door understands today.
const KNOWN_EVENT_TYPES: [&str; 7] = [
    "ready",
    "text-delta",
    "reasoning-delta",
    "tool-call",
    "usage",
    "finish",
    "error",
];

enum Decoded {
    Event(SidecarEvent),
    Unknown(String),
    Violation(DoorError),
}

fn violation(detail: impl std::fmt::Display) -> DoorError {
    DoorError::ProtocolViolation(detail.to_string())
}

fn decode(line: &[u8]) -> Decoded {
    let value: serde_json::Value = match serde_json::from_slice(line) {
        Ok(value) => value,
        Err(_) => return Decoded::Violation(violation("line is not JSON")),
    };
    let Some(object) = value.as_object() else {
        return Decoded::Violation(violation("line is not an event object"));
    };
    let Some(kind) = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
    else {
        return Decoded::Violation(violation("event has no `type`"));
    };
    if !KNOWN_EVENT_TYPES.contains(&kind.as_str()) {
        return Decoded::Unknown(kind);
    }
    match serde_json::from_value(value) {
        Ok(event) => Decoded::Event(event),
        Err(_) => Decoded::Violation(violation(format!("malformed `{kind}` event"))),
    }
}

/// Drain the child's stdout until EOF. On EOF this returns, dropping the
/// sender — which the router observes as the process ending.
pub(crate) async fn read_stdout(
    stdout: ChildStdout,
    tx: mpsc::Sender<Result<SidecarEvent, DoorError>>,
    line_cap: usize,
) {
    let mut reader = BufReader::new(stdout);
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) => return,
            Ok(read) => read,
            Err(error) => {
                let _ = tx
                    .send(Err(violation(format!("stdout read failed: {error}"))))
                    .await;
                return;
            }
        };
        buffer.extend_from_slice(&chunk[..read]);
        loop {
            let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') else {
                // No newline yet: the line in progress already exceeds the cap.
                if buffer.len() > line_cap {
                    let _ = tx
                        .send(Err(violation(format!(
                            "line exceeded the {line_cap}-byte cap"
                        ))))
                        .await;
                    return;
                }
                break;
            };
            if newline > line_cap {
                let _ = tx
                    .send(Err(violation(format!(
                        "line exceeded the {line_cap}-byte cap"
                    ))))
                    .await;
                return;
            }
            let mut line: Vec<u8> = buffer.drain(..=newline).collect();
            line.pop(); // the newline itself
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match decode(&line) {
                Decoded::Event(event) => {
                    if tx.send(Ok(event)).await.is_err() {
                        return; // the receiver is gone: stop reading
                    }
                }
                Decoded::Unknown(kind) => {
                    eprintln!("openbutler sidecar: ignoring unknown event type `{kind}`");
                }
                Decoded::Violation(error) => {
                    let _ = tx.send(Err(error)).await;
                    return;
                }
            }
        }
    }
}

/// Wait for the sidecar's `ready` announcement. Anything else arriving
/// first is logged and skipped; silence is the spawn timeout.
pub(crate) async fn handshake(
    events: &mut mpsc::Receiver<Result<SidecarEvent, DoorError>>,
    config: &DoorConfig,
) -> Result<SidecarReady, DoorError> {
    let deadline = tokio::time::Instant::now() + config.spawn_timeout;
    loop {
        let item = match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(item) => item,
            Err(_) => return Err(DoorError::HandshakeTimeout(config.spawn_timeout)),
        };
        match item {
            None => return Err(DoorError::ProcessExited),
            Some(Err(error)) => return Err(error),
            Some(Ok(SidecarEvent::Ready(ready))) => return Ok(ready),
            Some(Ok(_)) => {
                eprintln!("openbutler sidecar: event before the handshake was ignored");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_garbage_line_is_a_violation() {
        assert!(matches!(decode(b"this is not json"), Decoded::Violation(_)));
        assert!(matches!(decode(b"[1,2,3]"), Decoded::Violation(_)));
        assert!(matches!(decode(b"42"), Decoded::Violation(_)));
        assert!(matches!(
            decode(br#"{"note":"no type"}"#),
            Decoded::Violation(_)
        ));
    }

    #[test]
    fn a_malformed_known_event_is_a_violation_not_a_skip() {
        assert!(
            matches!(
                decode(br#"{"type":"text-delta","id":"x"}"#),
                Decoded::Violation(_)
            ),
            "a known type missing fields is malformed, not future-compatible"
        );
    }

    #[test]
    fn an_unknown_type_is_skipped_and_a_known_one_decodes() {
        assert!(
            matches!(decode(br#"{"type":"future-event","id":"x"}"#), Decoded::Unknown(kind) if kind == "future-event")
        );
        assert!(matches!(
            decode(br#"{"type":"text-delta","id":"x","delta":"hi"}"#),
            Decoded::Event(SidecarEvent::TextDelta { .. })
        ));
    }
}
