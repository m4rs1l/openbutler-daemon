//! The compaction engine: one summarisation request inside the measured
//! budget, or a sliced chain when the conversation does not fit its own
//! window - with every refusal carried as data, never as an exception
//! that could lock the owner out of their own chat (the 2026-11-03
//! incident: a `context: 262144` request under a 395k conversation
//! killed every message before it left the machine).
//!
//! Source: legacy `compaction.ts` L183-410 (`compactConversation`) and
//! L684-916 (`compactBySlicing`, `streamNotesRequest`), read 2026-11-18.
//!
//! Adaptations, both documented: the `onStart`/`onStepUsage`/`onTextDelta`
//! callbacks of the source land with the session hooks (C3) that need
//! them, so this engine reports usage inside [`CompactionResult`] and
//! emits no transcript rows itself; cancellation (`abortSignal`) has no
//! caller yet and arrives with the same slice.

use serde_json::{Value, json};
use uuid::Uuid;

use super::conversation::{
    COMPACTION_SUMMARY_MARKER, append_transcript_archive_reference, continue_message,
};
use super::core::{
    COMPACTION_PROMPT, POST_COMPACTION_SETTLED_RATIO, SLICE_MERGE_INSTRUCTION, SLICE_SYSTEM_PROMPT,
    SLICED_CONDENSE_ROUNDS, SLICED_NOTE_MIN_TOKENS, SLICED_NOTES_SHARE, SLICED_TAIL_SHARE,
    auto_compaction_trigger_tokens, estimate_context_tokens, estimate_text_tokens, estimate_tokens,
};
use super::projection::{project_transcript, recovery_messages, without_executors};
use super::slicing::split_into_slices;
use crate::sidecar::{Lane, SidecarDoor};
use openbutler_protocol::sidecar::{SidecarCall, SidecarEvent, SidecarProvider, SidecarTool};

/// The checkpoint instruction's recovery suffix, byte-exact from the
/// source (`compaction.ts` L209).
const RECOVERY_SUFFIX: &str = "\n\nThis is a one-time recovery from a full context window. Earlier raw reasoning is omitted from this request; the most recent reasoning, existing checkpoints, visible dialogue, and all available tool data remain. Historical tool reference strings have been decoded into readable fields without dropping their input or output. Do not invent facts from omitted reasoning. The complete original transcript remains archived.";

/// The synthetic notes message that opens the final sliced request.
const SLICED_NOTES_PREFIX: &str = "Merged continuation notes covering the full transcript before the newest dialogue, produced by summarizing it slice by slice:\n\n";

/// The session hooks the engine fires around and during the main
/// stream — the source's `onStart` / `onTextDelta` /
/// `onReasoningDelta` callbacks in one Copy bundle. The sliced chain
/// fires `on_start` only: its note streams stay internal and never
/// touch the transcript.
#[derive(Default, Clone, Copy)]
pub struct EngineHooks<'a> {
    pub on_start: Option<&'a (dyn Fn() + Send + Sync)>,
    pub on_text: Option<&'a (dyn Fn(&str) + Send + Sync)>,
    pub on_reasoning: Option<&'a (dyn Fn(&str) + Send + Sync)>,
}

/// Everything one compaction attempt needs. The caller resolves the
/// provider from settings and owns the door; the engine owns the
/// arithmetic and the stream.
#[derive(Debug, Clone)]
pub struct CompactionRequest {
    /// Namespaces every call id (`{tag}:{uuid}`) - production uses
    /// `compact`; tests point it at a fake-sidecar behavior.
    pub call_tag: String,
    pub provider: SidecarProvider,
    pub model: String,
    /// Request-level provider options (reasoning/cache metadata).
    pub provider_options: Option<Value>,
    pub system: String,
    pub messages: Vec<Value>,
    /// The active transcript for the checkpoint's view; defaults to
    /// `messages` when absent (source `transcriptMessages`).
    pub transcript: Option<Vec<Value>>,
    /// Tool schemas (ToolSet shape); executors are stripped for every
    /// estimate and never sent - schemas only reach the door.
    pub tools: Value,
    /// Added to the completed checkpoint only; never changes the
    /// summarisation request prefix.
    pub archive_path: Option<String>,
    pub context_window: Option<i64>,
    pub output_limit: Option<i64>,
    pub previous_context: Option<i64>,
    pub previous_context_estimate: Option<i64>,
    /// Manual compaction: skip the trigger and the no-reduction guard.
    pub force: bool,
    /// One-time recovery projection (the documented suffix above).
    pub recovery: bool,
}

/// The result of a compaction attempt as data: a refusal to compact is
/// a fact about the budget and conversation, never an exceptional
/// failure.
#[derive(Debug, Clone, PartialEq)]
pub enum CompactOutcome {
    Compacted {
        result: CompactionResult,
    },
    Skipped {
        reason: SkipReason,
    },
    Impossible {
        reason: ImpossibleReason,
        window: Option<i64>,
        estimated_tokens: i64,
        shortfall_tokens: i64,
        cause: Option<String>,
    },
}

/// A compaction that ran: the summary, the messages it produced, the
/// summarisation usage, and the before/after estimates.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionResult {
    pub summary: String,
    pub messages: Vec<Value>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub before_tokens: i64,
    pub after_tokens: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    NoBudget,
    BelowThreshold,
}

impl SkipReason {
    fn as_str(self) -> &'static str {
        match self {
            SkipReason::NoBudget => "no-budget",
            SkipReason::BelowThreshold => "below-threshold",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImpossibleReason {
    ExceedsWindow,
    NoReduction,
}

impl ImpossibleReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ImpossibleReason::ExceedsWindow => "exceeds-window",
            ImpossibleReason::NoReduction => "no-reduction",
        }
    }
}

/// One summarisation attempt: either the outcome as data, or the
/// engine's own refusal sentences as `Err` (stream errors, cut-off
/// summaries, non-text-only finishes - the caller shows them, the
/// original context was kept).
pub async fn compact_conversation(
    door: &SidecarDoor,
    request: CompactionRequest,
    hooks: EngineHooks<'_>,
) -> Result<CompactOutcome, String> {
    let prompt = if request.recovery {
        format!("{COMPACTION_PROMPT}{RECOVERY_SUFFIX}")
    } else {
        COMPACTION_PROMPT.to_owned()
    };
    let prompt_tokens = estimate_text_tokens(&prompt);
    let stripped_tools = without_executors(&request.tools);
    let context_estimate =
        estimate_context_tokens(&request.system, &request.messages, &stripped_tools);
    let request_messages = if request.recovery {
        recovery_messages(request.messages.clone())
    } else {
        request.messages.clone()
    };
    let request_estimate = if request.recovery {
        estimate_context_tokens(&request.system, &request_messages, &stripped_tools)
    } else {
        context_estimate
    };
    // Provider usage is authoritative; the byte estimate only accounts
    // for growth since the last measured request, or cold-starts.
    let estimated_growth = request
        .previous_context_estimate
        .map(|previous| (context_estimate - previous).max(0))
        .unwrap_or(0);
    let mut active_context = request
        .previous_context
        .map(|previous| previous + estimated_growth)
        .unwrap_or(context_estimate);
    if request.recovery {
        // This request has a different prefix: rebase the admission
        // estimate without changing saved provider usage.
        active_context =
            request_estimate.max(active_context - (context_estimate - request_estimate).max(0));
    }
    let estimated_context = active_context + prompt_tokens;
    let window = request.context_window;

    if !request.force {
        // The source's truthy window check: null and zero both mean no
        // budget for an automatic attempt.
        let Some(trigger_window) = window.filter(|value| *value > 0) else {
            return Ok(CompactOutcome::Skipped {
                reason: SkipReason::NoBudget,
            });
        };
        let trigger = auto_compaction_trigger_tokens(trigger_window, prompt_tokens);
        if active_context < trigger {
            return Ok(CompactOutcome::Skipped {
                reason: SkipReason::BelowThreshold,
            });
        }
    }

    // Output ceiling: the caller's limit plus the remaining window.
    let mut output_limits: Vec<i64> = Vec::new();
    if let Some(limit) = request.output_limit.filter(|value| *value > 0) {
        output_limits.push(limit);
    }
    if let (Some(window), Some(previous)) = (window, request.previous_context) {
        let remaining = window - estimated_context;
        // A growth estimate cannot prove overflow: let the provider
        // validate the full request when it disagrees with the last
        // real input measurement.
        output_limits.push(if remaining > 0 || request.recovery {
            remaining
        } else {
            window - previous - prompt_tokens
        });
    }
    let max_output = output_limits.iter().copied().min();
    let window_slack = match (window, request.previous_context) {
        (Some(window), Some(_)) => Some(window - estimated_context),
        _ => None,
    };
    let summary_budget_too_small = max_output
        .is_some_and(|maximum| maximum < 1 || window_slack.is_some_and(|slack| slack < 1024));

    if let Some(maximum) = max_output
        && summary_budget_too_small
    {
        // The full summarisation request does not fit. Try the sliced
        // fallback first - a window smaller than the conversation must
        // degrade, not kill the turn.
        if let Some(window) = window
            && !request.messages.is_empty()
        {
            match compact_by_slicing(door, &request, window, hooks.on_start).await? {
                Some(Sliced::Result(result)) => {
                    return Ok(CompactOutcome::Compacted {
                        result: CompactionResult {
                            before_tokens: estimated_context,
                            after_tokens: estimate_context_tokens(
                                &request.system,
                                &result.messages,
                                &stripped_tools,
                            ),
                            ..result
                        },
                    });
                }
                Some(Sliced::Failure(cause)) => {
                    return Ok(CompactOutcome::Impossible {
                        reason: ImpossibleReason::ExceedsWindow,
                        window: Some(window),
                        estimated_tokens: estimated_context,
                        shortfall_tokens: window_slack
                            .map(|slack| (1_024 - slack).max(1))
                            .unwrap_or(1 - maximum),
                        cause: Some(cause),
                    });
                }
                None => {}
            }
        }
        // Report the arithmetic as data; the caller decides what the
        // transcript shows.
        return Ok(CompactOutcome::Impossible {
            reason: ImpossibleReason::ExceedsWindow,
            window,
            estimated_tokens: estimated_context,
            shortfall_tokens: window_slack
                .map(|slack| (1_024 - slack).max(1))
                .unwrap_or(1 - maximum),
            cause: None,
        });
    }

    // The summarisation request: the prompt rides as the final user
    // message, tools as schemas only (nothing can execute).
    let mut call_messages = request_messages.clone();
    call_messages.push(json!({ "role": "user", "content": prompt }));
    let call = build_call(
        &request,
        &request.system,
        call_messages,
        max_output,
        tool_set_to_sidecar(&request.tools),
    );
    // The source fires `onStart` here: after the budget arithmetic, the
    // row exists from the first moment the stream can begin (C3's
    // session hook inserts the running compaction row).
    if let Some(on_start) = hooks.on_start {
        on_start();
    }
    let drained = drain_call(door, call, hooks).await?;
    let summary = interpret_summary(&drained, max_output)?;

    // The checkpoint: conversation data only - system instructions and
    // tool definitions are rebuilt independently by the caller.
    let mut checkpoint_text = format!("{COMPACTION_SUMMARY_MARKER}{summary}");
    if let Some(archive_path) = &request.archive_path {
        checkpoint_text = append_transcript_archive_reference(&checkpoint_text, archive_path);
    }
    let checkpoint = json!({
        "role": "assistant",
        "content": checkpoint_text,
        "providerOptions": { "openbutler": { "compactionCheckpoint": true } },
    });
    // Middle-part budget: the settled share minus the fixed costs
    // (system, tool schemas, checkpoint, continue message).
    let max_middle = window.map(|window| {
        (window as f64 * POST_COMPACTION_SETTLED_RATIO).floor() as i64
            - estimate_text_tokens(&request.system)
            - estimate_tokens(&stripped_tools)
            - estimate_tokens(&checkpoint)
            - estimate_tokens(&continue_message())
    });
    let transcript_source = request.transcript.as_ref().unwrap_or(&request.messages);
    let projected = project_transcript(transcript_source, max_middle);
    let mut messages = Vec::with_capacity(projected.len() + 2);
    messages.push(checkpoint);
    messages.extend(projected);
    messages.push(continue_message());
    let after_tokens = estimate_context_tokens(&request.system, &messages, &stripped_tools);
    if !request.force && after_tokens >= context_estimate {
        // A compaction that does not shrink is not applied.
        return Ok(CompactOutcome::Impossible {
            reason: ImpossibleReason::NoReduction,
            window,
            estimated_tokens: after_tokens,
            shortfall_tokens: 0,
            cause: None,
        });
    }
    Ok(CompactOutcome::Compacted {
        result: CompactionResult {
            summary,
            messages,
            input_tokens: drained.usage.as_ref().and_then(|usage| usage.input),
            output_tokens: drained.usage.as_ref().and_then(|usage| usage.output),
            before_tokens: estimated_context,
            after_tokens,
        },
    })
}

/// What the sliced chain produced: the final result, or the last
/// reported reason it broke down (travels into the refusal notice so
/// the owner sees WHY the chain stopped - the 2026-11-03 chain failed
/// silently here).
enum Sliced {
    Result(CompactionResult),
    Failure(String),
}

/// Map-reduce fallback for a conversation that does not fit its own
/// window: summarise slice by slice inside the budget, condense the
/// notes, and run the final forced checkpoint on those notes plus a
/// verbatim tail of the newest dialogue. Every request in the chain
/// fits the window by construction; any breakdown returns its reason as
/// data.
async fn compact_by_slicing(
    door: &SidecarDoor,
    request: &CompactionRequest,
    window: i64,
    on_start: Option<&(dyn Fn() + Send + Sync)>,
) -> Result<Option<Sliced>, String> {
    let system_estimate = estimate_text_tokens(&request.system);
    let prompt_estimate = estimate_text_tokens(COMPACTION_PROMPT);
    let instruction_estimate = estimate_text_tokens(SLICE_SYSTEM_PROMPT) + 64;
    let notes_budget = (window as f64 * SLICED_NOTES_SHARE).floor() as i64;
    let note_ceiling = (window as f64 * 0.1).floor() as i64;
    let slice_budget = window - system_estimate - instruction_estimate - notes_budget;
    let hard_cap = window - system_estimate - instruction_estimate - note_ceiling;
    if slice_budget < SLICED_NOTE_MIN_TOKENS {
        return Ok(None);
    }
    let Some(slices) = split_into_slices(&request.messages, slice_budget, hard_cap) else {
        return Ok(None);
    };
    if slices.len() < 2 {
        return Ok(None);
    }
    // The chain fires the hook once, before the first note (the source
    // calls `onStart` here; the nested forced call reuses a no-op).
    if let Some(on_start) = on_start {
        on_start();
    }

    // One transcript row spans the whole chain; the recursion below
    // reuses it instead of opening a second one (C3's hook).
    let note_each = (SLICED_NOTE_MIN_TOKENS.max(notes_budget / slices.len() as i64))
        .min(SLICED_NOTE_MIN_TOKENS.max(note_ceiling));
    let mut notes: Vec<String> = Vec::with_capacity(slices.len());
    for slice in slices {
        let note = stream_notes(
            door,
            request,
            NotesJob::Slice { messages: slice },
            note_each,
        )
        .await;
        match note {
            Ok(text) => notes.push(text),
            Err(failure) => return Ok(Some(Sliced::Failure(failure))),
        }
    }

    // Condense until the notes fit their share. Bounded rounds; a chain
    // that cannot converge gives up as data rather than looping.
    let mut merged = notes;
    let mut round = 0;
    while round < SLICED_CONDENSE_ROUNDS
        && estimate_text_tokens(&merged.join("\n\n")) > notes_budget
    {
        let chunk_budget = SLICED_NOTE_MIN_TOKENS.max(notes_budget / 2);
        let mut chunks: Vec<Vec<String>> = Vec::new();
        let mut chunk: Vec<String> = Vec::new();
        let mut chunk_size = 0;
        for note in &merged {
            let note_size = estimate_text_tokens(note);
            if !chunk.is_empty() && chunk_size + note_size > chunk_budget {
                chunks.push(std::mem::take(&mut chunk));
                chunk_size = 0;
            }
            chunk.push(note.clone());
            chunk_size += note_size;
        }
        if !chunk.is_empty() {
            chunks.push(chunk);
        }
        let mut condensed: Vec<String> = Vec::with_capacity(chunks.len());
        for group in chunks {
            let note = stream_notes(
                door,
                request,
                NotesJob::Merge {
                    text: group.join("\n\n"),
                },
                SLICED_NOTE_MIN_TOKENS
                    .max(notes_budget / 2)
                    .min(note_ceiling),
            )
            .await;
            match note {
                Ok(text) => condensed.push(text),
                Err(failure) => return Ok(Some(Sliced::Failure(failure))),
            }
        }
        merged = condensed;
        round += 1;
    }
    if estimate_text_tokens(&merged.join("\n\n")) > notes_budget {
        return Ok(None);
    }

    // The newest dialogue travels verbatim, bounded so the final
    // request keeps a quarter of the window for the checkpoint itself.
    let tail_cap = (window
        - system_estimate
        - prompt_estimate
        - notes_budget
        - (window as f64 * 0.25).floor() as i64)
        .max(0);
    let tail_budget = ((window as f64 * SLICED_TAIL_SHARE).floor() as i64).min(tail_cap);
    let mut tail: Vec<Value> = Vec::new();
    let mut tail_size = 0;
    for message in request.messages.iter().rev() {
        let size = estimate_tokens(message);
        if tail_size + size > tail_budget {
            break;
        }
        tail.insert(0, message.clone());
        tail_size += size;
    }

    let mut synthetic = vec![json!({
        "role": "user",
        "content": format!("{SLICED_NOTES_PREFIX}{}", merged.join("\n\n===\n\n")),
    })];
    synthetic.extend(tail);
    let synthetic_size = estimate_context_tokens(
        &request.system,
        &synthetic,
        &without_executors(&request.tools),
    );
    let mut nested = request.clone();
    nested.messages = synthetic;
    nested.previous_context = Some(synthetic_size);
    nested.previous_context_estimate = Some(synthetic_size);
    nested.force = true;
    // The recursion crosses the async boundary through a box; the
    // synthetic request always forces, so it terminates.
    let outcome = Box::pin(compact_conversation(door, nested, EngineHooks::default())).await?;
    Ok(Some(match outcome {
        CompactOutcome::Compacted { result } => Sliced::Result(result),
        CompactOutcome::Impossible { reason, cause, .. } => {
            Sliced::Failure(cause.unwrap_or_else(|| reason.as_str().to_owned()))
        }
        CompactOutcome::Skipped { reason } => Sliced::Failure(reason.as_str().to_owned()),
    }))
}

/// One bounded summarisation request in the slice chain: notes, never
/// checkpoints. A note request ends either as its text or as the reason
/// it failed - the reason travels up into the refusal notice.
enum NotesJob {
    Slice { messages: Vec<Value> },
    Merge { text: String },
}

async fn stream_notes(
    door: &SidecarDoor,
    request: &CompactionRequest,
    job: NotesJob,
    max_output: i64,
) -> Result<String, String> {
    let (system, messages) = match job {
        NotesJob::Slice { messages } => {
            // An empty instruction adds no message (source falsy check).
            (SLICE_SYSTEM_PROMPT.to_owned(), messages)
        }
        NotesJob::Merge { text } => (
            SLICE_SYSTEM_PROMPT.to_owned(),
            vec![json!({
                "role": "user",
                "content": format!("{SLICE_MERGE_INSTRUCTION}\n\n{text}"),
            })],
        ),
    };
    let call = build_call(request, &system, messages, Some(max_output), Vec::new());
    // Everything here is data: a door failure is a note failure, not a
    // thrown error.
    let drained = drain_call(door, call, EngineHooks::default()).await?;
    if let Some(error) = drained.stream_error {
        return Err(error);
    }
    if let Some(finish) = drained.finish.as_deref()
        && (finish == "error" || finish == "content-filter")
    {
        return Err(format!("the note stream finished as {finish}"));
    }
    let trimmed = drained.text.trim();
    if trimmed.is_empty() {
        return Err("the model returned empty notes".to_owned());
    }
    Ok(trimmed.to_owned())
}

/// What one drained call produced, before any policy interprets it.
#[derive(Default)]
struct Drain {
    text: String,
    tool_calls: usize,
    finish: Option<String>,
    usage: Option<Usage>,
    stream_error: Option<String>,
}

#[derive(Default)]
struct Usage {
    input: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
}

/// The guards, in the source's order: stream error, length, then the
/// non-text-only finishes, then the empty summary. Every sentence is
/// byte-exact; the original context was kept in all of them.
fn interpret_summary(drained: &Drain, max_output: Option<i64>) -> Result<String, String> {
    if let Some(error) = &drained.stream_error {
        return Err(error.clone());
    }
    let budget = max_output
        .map(|maximum| format!("{maximum} tokens requested"))
        .unwrap_or_else(|| "provider default".to_owned());
    let usage = drained.usage.as_ref();
    if drained.finish.as_deref() == Some("length") {
        let used = usage
            .and_then(|usage| usage.output)
            .map(|tokens| format!("{tokens} tokens used"))
            .unwrap_or_else(|| "usage unavailable".to_owned());
        let reasoning = usage
            .and_then(|usage| usage.reasoning)
            .map(|tokens| format!("; {tokens} reasoning tokens"))
            .unwrap_or_default();
        return Err(format!(
            "The compaction summary was cut off by the model output limit ({budget}; {used}{reasoning}); the original context was kept."
        ));
    }
    if matches!(
        drained.finish.as_deref(),
        Some("error") | Some("content-filter")
    ) || drained.tool_calls > 0
    {
        return Err(
            "The model did not finish a text-only compaction summary; the original context was kept."
                .to_owned(),
        );
    }
    let summary = drained.text.trim().to_owned();
    if summary.is_empty() {
        return Err(
            "The model returned an empty compaction summary; the original context was kept."
                .to_owned(),
        );
    }
    Ok(summary)
}

fn build_call(
    request: &CompactionRequest,
    system: &str,
    messages: Vec<Value>,
    max_output: Option<i64>,
    tools: Vec<SidecarTool>,
) -> SidecarCall {
    SidecarCall {
        id: format!("{}:{}", request.call_tag, Uuid::new_v4()),
        provider: request.provider.clone(),
        model: request.model.clone(),
        messages: Value::Array(messages),
        system: Some(system.to_owned()),
        tools,
        max_output_tokens: max_output.map(|maximum| maximum.max(0) as u64),
        provider_options: request.provider_options.clone(),
    }
}

/// ToolSet (legacy shape) to the door's schema-only list: executors
/// never existed on the wire, so this is a field mapping.
fn tool_set_to_sidecar(tools: &Value) -> Vec<SidecarTool> {
    let Some(object) = tools.as_object() else {
        return Vec::new();
    };
    object
        .iter()
        .map(|(name, definition)| SidecarTool {
            name: name.clone(),
            description: definition
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned),
            parameters: definition
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({})),
        })
        .collect()
}

async fn drain_call(
    door: &SidecarDoor,
    call: SidecarCall,
    hooks: EngineHooks<'_>,
) -> Result<Drain, String> {
    let mut stream = door
        .call(Lane::Interactive, call)
        .await
        .map_err(|error| error.to_string())?;
    let mut drain = Drain::default();
    while let Some(item) = stream.next().await {
        match item {
            Ok(SidecarEvent::TextDelta { delta, .. }) => {
                if let Some(hook) = hooks.on_text {
                    hook(&delta);
                }
                drain.text.push_str(&delta);
            }
            Ok(SidecarEvent::ReasoningDelta { delta, .. }) => {
                // The summarisation collects text only; the hook keeps
                // the visible thought stream (source onReasoningDelta).
                if let Some(hook) = hooks.on_reasoning {
                    hook(&delta);
                }
            }
            Ok(SidecarEvent::ToolCall { .. }) => drain.tool_calls += 1,
            Ok(SidecarEvent::Finish { finish_reason, .. }) => drain.finish = finish_reason,
            Ok(SidecarEvent::Usage {
                prompt_tokens,
                completion_tokens,
                reasoning_tokens,
                ..
            }) => {
                drain.usage = Some(Usage {
                    input: prompt_tokens,
                    output: completion_tokens,
                    reasoning: reasoning_tokens,
                });
            }
            Ok(SidecarEvent::Error { message, kind, .. }) => {
                if drain.stream_error.is_none() {
                    drain.stream_error =
                        Some(message.unwrap_or_else(|| format!("sidecar error: {kind:?}")));
                }
            }
            Ok(_) => {}
            Err(error) => {
                if drain.stream_error.is_none() {
                    drain.stream_error = Some(error.to_string());
                }
            }
        }
    }
    Ok(drain)
}
