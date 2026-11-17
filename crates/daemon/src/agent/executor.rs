//! The spawn seam between dispatch and the turn loop: dispatch accepts a
//! turn and hands it over; everything after that is background work whose
//! failures become notices, never the response — the shape of the legacy
//! `controller.sendMessage` fire-and-forget.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use openbutler_protocol::SessionEntryMeta;
use openbutler_protocol::entry::{NoticeTone, SessionEntry};
use tokio::runtime::Handle;

use super::{TurnError, TurnRequest, TurnRunner};
use crate::sidecar::{DoorConfig, SidecarDoor};
use crate::state;

/// What dispatch does with an accepted turn. The receiver is `Arc<Self>`
/// so the spawned task can own a share of the executor.
pub trait TurnExecutor: Send + Sync {
    fn spawn_turn(self: Arc<Self>, session_id: String, request: TurnRequest);
    /// `session.approve`: resolves the pending approval of the session's
    /// running turn (id-guarded) and reports whether one matched — a
    /// stale id or an inactive session is the source's silent no-op.
    /// Synchronous: the queue's locks are all short, never spanning an
    /// await, so dispatch stays a sync surface.
    fn approve(&self, session_id: &str, approval_id: &str, approved: bool) -> bool;
    /// `session.stop`: asks the session's runner to end its turn
    /// (id-less fire-and-forget, like the source's `controller.stop`).
    /// Returns whether a runner existed; a chat with no live turn
    /// reports false and the action still answers.
    fn stop(&self, session_id: &str) -> bool;
    /// `session.answer`: resolves the session's waiting question
    /// (id-guarded; a stale id no-ops like the source's
    /// `answerQuestion`). `None` is the dismissal the source sends.
    fn answer(&self, session_id: &str, question_id: &str, answer: Option<String>) -> bool;
}

/// The production executor: one shared door (spawned on first use — an
/// idle daemon never pays for the sidecar) and one runner per session, so
/// two sends for the same chat can never write its record concurrently.
pub struct DoorExecutor {
    data_dir: PathBuf,
    config: DoorConfig,
    door: tokio::sync::Mutex<Option<SidecarDoor>>,
    runners: Mutex<HashMap<String, Arc<TurnRunner>>>,
    /// The host runners publish live state to; weak so an executor never
    /// pins the host's lifetime.
    host: std::sync::Weak<crate::dispatch::Host>,
}

impl DoorExecutor {
    pub fn new(
        data_dir: PathBuf,
        config: DoorConfig,
        host: std::sync::Weak<crate::dispatch::Host>,
    ) -> Self {
        Self {
            data_dir,
            config,
            door: tokio::sync::Mutex::new(None),
            runners: Mutex::new(HashMap::new()),
            host,
        }
    }

    /// The session's runner, opening the door and the record on first use.
    async fn runner_for(&self, session_id: &str) -> Result<Arc<TurnRunner>, TurnError> {
        if let Some(runner) = self
            .runners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
        {
            return Ok(Arc::clone(runner));
        }
        let door = {
            let mut guard = self.door.lock().await;
            match guard.as_ref() {
                Some(door) => door.clone(),
                None => {
                    let (door, _ready) = SidecarDoor::spawn(self.config.clone()).await?;
                    *guard = Some(door.clone());
                    door
                }
            }
        };
        let runner =
            Arc::new(TurnRunner::open(&self.data_dir, session_id, door, self.host.clone()).await?);
        self.runners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id.to_owned(), Arc::clone(&runner));
        Ok(runner)
    }
}

impl TurnExecutor for DoorExecutor {
    fn spawn_turn(self: Arc<Self>, session_id: String, request: TurnRequest) {
        // The response has already gone out; from here failures are
        // notices on the record. Images are decoded and stored first —
        // the legacy `enqueueMessage` conversion.
        Handle::current().spawn(async move {
            let mut request = request;
            if !request.images.is_empty() {
                match convert_images(&self.data_dir, &session_id, &request.images) {
                    Ok(stored) => {
                        request.attachments.extend(stored);
                        request.images.clear();
                    }
                    Err(error) => {
                        report_turn_error(&self.data_dir, &session_id, &error);
                        return;
                    }
                }
            }
            let outcome = async {
                let runner = self.runner_for(&session_id).await?;
                runner.send(request).await?;
                Ok::<(), TurnError>(())
            }
            .await;
            if let Err(error) = outcome {
                report_turn_error(&self.data_dir, &session_id, &error.to_string());
            }
        });
    }

    fn approve(&self, session_id: &str, approval_id: &str, approved: bool) -> bool {
        // No runner means no running turn — the source's controller
        // would not exist either, and the id simply no-ops. An approve
        // must never spawn the door.
        let runner = self
            .runners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .map(Arc::clone);
        match runner {
            Some(runner) => runner.resolve_approval(approval_id, approved),
            None => false,
        }
    }

    fn stop(&self, session_id: &str) -> bool {
        // Like the source: no runner, no stop (and never spawn the door
        // just to cancel something that is not running).
        let runner = self
            .runners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .map(Arc::clone);
        match runner {
            Some(runner) => {
                runner.stop();
                true
            }
            None => false,
        }
    }

    fn answer(&self, session_id: &str, question_id: &str, answer: Option<String>) -> bool {
        let runner = self
            .runners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .map(Arc::clone);
        match runner {
            Some(runner) => runner.answer_question(question_id, answer),
            None => false,
        }
    }
}

/// The legacy `enqueueMessage` image conversion: decode each payload
/// against its declared type, then store it content-addressed.
fn convert_images(
    data_dir: &Path,
    session_id: &str,
    images: &[super::QueuedImage],
) -> Result<Vec<openbutler_protocol::entry::FileAttachment>, String> {
    images
        .iter()
        .enumerate()
        .map(|(index, image)| {
            let bytes = state::decode_base64_image(
                &image.base64,
                &image.media_type,
                state::MAX_IMAGE_BYTES,
            )?;
            state::store_attachment(
                data_dir,
                session_id,
                &bytes,
                &format!("image-{}.png", index + 1),
                &image.media_type,
            )
        })
        .collect()
}

/// Best-effort `Could not send the message: …` notice (legacy wording).
/// A record that cannot be read cannot carry the notice — the legacy
/// controller held it in memory instead, and this build has no memory.
fn report_turn_error(data_dir: &Path, session_id: &str, error: &str) {
    let Some(path) = state::record_path(data_dir, session_id) else {
        return;
    };
    let Some(mut record) = state::read_session_record(&path, session_id) else {
        return;
    };
    record.entries.push(SessionEntry::Notice {
        meta: SessionEntryMeta {
            at: Some(chrono::Utc::now().timestamp_millis()),
            turn_id: None,
            ..SessionEntryMeta::default()
        },
        text: format!("Could not send the message: {error}"),
        detail: None,
        tone: Some(NoticeTone::Error),
        cause: None,
    });
    let _ = state::save_session(data_dir, &record, true);
}
