//! Questions: the ask/answer queue the owner sees (`requestQuestion`,
//! `answerQuestion` ports) and the channel the `question` tool holds.
use super::*;

impl TurnRunner {
    /// Queues one model question and waits for the owner's answer
    /// (`requestQuestion`, L1850) — the shared queue path the
    /// `question` tool's port also runs.
    pub async fn ask(&self, text: String, options: Option<Vec<String>>) -> Option<String> {
        self.ask_channel().queue_and_wait(text, options).await
    }

    /// The channel a `question` tool call holds: the shared queue, the
    /// stop flag, and the roster wakeup. Cloned into the tool's
    /// `'static` future, so the port never borrows the runner.
    pub(super) fn ask_channel(&self) -> AskChannel {
        AskChannel {
            question: Arc::clone(&self.question),
            stop: Arc::clone(&self.stop),
            publish_notify: Arc::clone(&self.publish_notify),
        }
    }

    /// `session.answer`: id-guarded resolve (`answerQuestion`, L1224 —
    /// a stale id no-ops).
    pub fn answer_question(&self, question_id: &str, answer: Option<String>) -> bool {
        resolve_question(&self.question, question_id, answer)
    }

    /// The waiting question's id (visibility surfaces and tests).
    pub fn pending_question_id(&self) -> Option<String> {
        self.question
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|pending| pending.question_id.clone())
    }
}

// --- question channel --------------------------------------------------------

/// The question channel a `question` tool holds: the shared queue, the
/// stop flag, and the roster wakeup. `Clone` by `Arc`, so the tool's
/// future is `'static` (definitions are plain function pointers).
#[derive(Clone)]
pub(crate) struct AskChannel {
    question: Arc<std::sync::Mutex<Option<QueuedQuestion>>>,
    stop: Arc<AtomicBool>,
    publish_notify: Arc<tokio::sync::Notify>,
}

impl AskChannel {
    /// The queue path behind both the runner's `ask` and the tool port:
    /// queue → wake the turn's publish select → wait; a stop that wins
    /// the race clears the entry (the source's stop handler, L1259).
    async fn queue_and_wait(&self, text: String, options: Option<Vec<String>>) -> Option<String> {
        let question_id = format!(
            "question-{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_millis())
                .unwrap_or(0),
            &Uuid::new_v4().to_string()[..6]
        );
        let (sender, receiver) = tokio::sync::oneshot::channel();
        *self
            .question
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(QueuedQuestion {
            question_id: question_id.clone(),
            text,
            options,
            sender,
        });
        // The roster frame rides the turn owner's select: this code runs
        // under the held `state` lock, where a re-entrant publish would
        // deadlock. The wakeup keeps one permit, so it is never lost.
        self.publish_notify.notify_one();
        let answer = wait_question(receiver, &self.stop).await;
        if answer.is_none() {
            // A stop that won the race: clear the queue entry so the
            // roster stops showing a question nobody can answer.
            let _ = resolve_question(&self.question, &question_id, None);
        }
        answer
    }
}

impl AskPort for AskChannel {
    fn ask(&self, text: String, options: Option<Vec<String>>) -> AskFuture {
        let channel = self.clone();
        Box::pin(async move { channel.queue_and_wait(text, options).await })
    }
}

/// Debug prints the channel, not the queue it holds (the oneshot
/// sender has no meaningful display).
impl std::fmt::Debug for AskChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskChannel").finish_non_exhaustive()
    }
}

/// The id-guarded resolve both the seam and the dismissal path share
/// (`answerQuestion`, L1224 — a stale id no-ops).
fn resolve_question(
    queue: &std::sync::Mutex<Option<QueuedQuestion>>,
    question_id: &str,
    answer: Option<String>,
) -> bool {
    let mut slot = queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot
        .as_ref()
        .is_some_and(|pending| pending.question_id == question_id)
        && let Some(pending) = slot.take()
    {
        let _ = pending.sender.send(answer);
        return true;
    }
    false
}

// --- transcript mapping -------------------------------------------------------

/// Waits a queued question: the answer wins immediately, the owner's
/// stop flag returns `None` (and consumes it, like the approval wait).
async fn wait_question(
    mut receiver: tokio::sync::oneshot::Receiver<Option<String>>,
    stop: &Arc<AtomicBool>,
) -> Option<String> {
    loop {
        tokio::select! {
            biased;
            answer = &mut receiver => return answer.unwrap_or(None),
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                if stop.swap(false, Ordering::SeqCst) {
                    return None;
                }
            }
        }
    }
}
