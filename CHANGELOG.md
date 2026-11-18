# Changelog

Notable changes to this workspace are recorded here.
Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- The `schedule.manage` runtime half: `run` queues one manual event
  into the feed (the source's envelope), `list` reports every stored
  schedule with adapter status, durable runtime and describe line, `log`
  maps the run history (`scheduledAt` surfaced, falsy errors omitted),
  and the CRUD reloads the newly installed runner so its response
  carries the real `runtime` (absent when no runner is wired — the
  source's `undefined`).
- The event state machine (`ledger/feed.rs`): `claim_event` (single
  pending claim), `set_event_state`, `get_event`, and
  `recover_interrupted_events` (the restart split by source with the
  source's messages), plus a shared `schedule_events` with two doors.
- Boot wiring (`main`): the host's persistent `LedgerWriter` (with
  better-sqlite3's 5 s busy timeout), both crash recoveries before the
  first reload, the `ScheduleRunner` on the host, and a reload of every
  stored trigger. `ScheduleRunner` reload/shutdown are sync now — the
  body never awaits and dispatch is a sync surface; adapter tasks spawn
  through the runtime handle captured at construction.
- `ScheduleRuntime`/`ScheduleDescription` serialize to the source's
  wire shape (camelCase keys, absent options stay absent).
- `ScheduleRunner` (`schedule/runner.rs`): trigger lifetimes ported from
  `tools/schedule.ts` L69-415 — serialized reload, Map-style statuses,
  the seven adapters (interval due seeding with missed-interval
  coalescing, one-shot prior-event checks with interrupted retry,
  daily re-arm with `clockShift`, a notify filesystem watcher with one
  debounce window, sysinfo-backed memory sampling), `close`, and the
  10-second `shutdown` deadline. Emit builds the source's event envelope
  through an injected publish callback.
- Observed-event write path (`ledger/events.rs`): `insertEvent` with the
  UTF-16 validation ceilings, the 256 KiB / 32 MiB payload limits, the
  dedupe insert returning the existing id, plus `LedgerWriter::schedule_events`
  and a shared retry body. The writer schema now creates
  `openbutler_events` (status CHECK from `OBSERVED_EVENT_STATES`) — the
  table never existed on a fresh database, so inserts failed there.
  14 pins (`ledger_events`, `schedule_runner`).
- Schedule engine (`schedule/{wall,format,runtime,describe,wake}.rs`):
  the pure scheduler half ported from `tools/schedule.ts` — wall-clock
  occurrence math (server-local only, DST-gap walk with round-trip
  verification), durable run state in `openbutler_state` (count+1,
  8000-code-point report truncation, error-then-start clearing), the
  seven-adapter description line (including `≥` and the EN dash), the
  wake text in the source's line order (LATE boundary exclusive), and
  interval-only trigger-due keys. `ledger/state.rs` adds the
  `openbutler_state` read/write surface, and the `LedgerWriter` schema
  now creates that table (it never existed on a fresh database — the
  writer's own marker upsert would have failed). 17 pins.
- Schedule store (`state/schedules.rs`): schedules moved out of
  `settings.yaml` entirely (owner ruling: schedules are records, not
  configuration) into their own `schedules.json` — the bare-list shape,
  the per-schedule parser messages and the list/duplicate refusals moved
  byte-preserved from the settings half. Writes take the host's store
  lock and the one atomic writer; the settings file no longer carries
  `schedules`/`backgroundTasks` (both now refused by name), and the
  schedule CRUD pins that a schedule never creates or rewrites
  `settings.yaml`. A neutral `shape` module (record/id/length/absolute
  checks) keeps the schema and the store from importing each other.
- Usage accounting (`ledger/usage.rs`, the source's `onStepUsage` →
  `recordUsage`): one `state.db` row per model request for chat turns
  and compaction alike — `sessionId:startedAt:attempt` key with
  `INSERT OR IGNORE`, the eight token counts, duration/timezone
  stamps, `usage_source`, and cost from the provider's own statement
  (`provider-inline` or `none`; no price catalog exists in this build,
  so `catalog-estimate` waits for the runtime model catalog). The
  door's `usage` event now carries the input/output detail splits and
  `raw`; `finish` carries `providerMetadata` and `responseId`, stashed
  from the `finish-step` part (real-runner verified: the response id
  reaches the row end to end). Turn rows land per step with the
  turn's step index as `attempt`; compaction rows fire per drained
  request — notes only on success, the nested forced call inherits
  the hook.
- Streamed checkpoint deltas (`EngineHooks` + `agent/compact.rs`
  stream cells): the summarisation's text and thought stream into the
  transcript while it runs — one growing entry per attempt, tagged
  `internal: compaction` (the provenance the source's merge guard
  keeps apart from the answer), a per-delta roster publish with the
  live index, and the reasoning duration stamped at settle
  (`finish_reasoning_block`). The engine's `on_start` parameter grew
  into the hooks bundle the source passes; the sliced chain fires the
  row hook only — its note streams stay internal. Two pins: the
  engine hooks (both deltas counted around one forced call) and the
  e2e provenance (`Hello world` joined under the compaction tag).
- Compaction at the step boundary (`TurnRunner::compact_at_boundary`):
  every model request asks once, before the next call is built — the
  queued request serves forced (the promise the deferral notice made),
  the automatic half lets the engine's trigger decide with zero calls
  below it, failures land as `warn` with the continue suffix so a
  summarisation never costs the turn, and the budget prefers the
  record's own window over the model specs (source
  `stateContextWindowTokens`). The send loop releases the state guard
  at the boundary so the row hook can take it; the boundary runs on
  the run's own system/provider/model. Two pins in `agent_loop.rs`
  (boundary ordering across three calls, auto-trigger above 40,210
  with the checkpoint already in the first turn request).
- `session.compact` end to end: the fire-and-forget dispatch arm
  (`recovery` validated, seam reached), `TurnExecutor::compact`, and
  the runner hooks (`agent/compact.rs`) — busy deferral stored under
  the `active_current` lock and served by the drain at the run's end,
  the empty-conversation notice, the row lifecycle (running inside the
  engine's `on_start` → done/failed in place), the archive-once
  conversation swap, and the refusal notices with the
  `compaction:<reason>` cause band. Six pins: four in
  `session_compact.rs`, the busy e2e in `agent_loop.rs`, and the
  engine's `on_start` fired-once. `TurnExecutor` gained the `compact`
  seam (all doubles updated); `publish_now` now carries an optional
  turn id (a standalone compaction has no active turn, like the
  source's `activeTurnId`).
- Compaction engine (`compaction/engine.rs`): `compact_conversation`
  with the measured budget arithmetic (both skips with zero calls, the
  output ceiling, window slack), byte-exact guard sentences
  (cut-off/text-only/empty/stream error), the checkpoint build over the
  projection, refusal-as-data outcomes (`exceeds-window`,
  `no-reduction`), the recovery suffix, and the full sliced chain
  (per-slice notes, bounded condense rounds, verbatim tail, forced
  final call) through the real door against the fake sidecar.
  `docs/context-compaction.md` documents the contract.
- Compaction slice chunker (`compaction/slicing.rs`): greedy
  transcript chunking that never splits a tool call from its result -
  a slice closes only while no call inside it is unresolved (the
  `AI_MissingToolResultsError` lesson from 2026-11-03), a message over
  the hard cap refuses the chain as data, and an oversized-but-fitting
  message opens its own slice.
- Compaction projection layer (`compaction/projection.rs`): the
  checkpoint's middle part - tool call/result pairing into bounded
  references (literal excerpts), `missing` markers for unresolved
  calls, latest-media-batch filtering, reasoning/providerOptions
  stripping, recovery decoding (readable tool exchanges, only the
  newest raw reasoning survives), and oldest-first dialogue folding
  with the structural floor (references, media, system and the newest
  user message never fold; overshoot beats deleting the request).
  The `image.ts` media summariser branches arrive with the media
  tools; image/file results render through the generic part list.
- Sidecar door: `call` carries `maxOutputTokens` and request-level
  `providerOptions` (the compaction engine's summary budget and its
  reasoning/cache options), both absent from the wire when unset.
  A new conformance vector pins the round trip and
  `budget_and_provider_options_reach_the_provider_wire` asserts both
  against the provider's actual HTTP body through the real sidecar.
- Compaction's pure core (`compaction/`): the measured trigger
  arithmetic (`max(window - reserve, floor(window/2))`, reserve
  40_210 with the real prompt), the conservative estimators
  (non-ASCII code points cost a full token, media parts 1_024),
  the three checkpoint prompts as mechanical byte-exact extractions
  (sha256-pinned), and the conversation-projection helpers
  (tool references, head/tail preview bounds, archive pointer,
  queued-message order repair). No model call yet - the engine
  arrives with the sidecar wiring.
- `session.select-project`: project binding for a chat that has not
  started - the arm's shape gate (absent `projectId` throws, only
  literal null or a string passes), the controller's three guards and
  the `projects.json` store (folder canonicalisation, strict record
  validation) live in `state/projects.rs`; the binding persists to the
  draft record here where the source holds it in memory.
- `session.command` normalization and the telegram trio: the registry
  resolves `/new` / `/compact` / `/stop` before the switch (payload
  untouched, surface names refused with `Unknown command.`), and the
  telegram actions answer from the source's disabled/not-configured
  fallbacks. The unknown-action refusal now prints
  `Unknown IPC action: <request.action>` — the source's operand and
  wording (the old port drifted to `Unknown action:` without a pin).
  Tests: `tests/session_command.rs` (7).
- Disk-side chat mutations at the dispatch surface: `session.new`
  (busy-gated draft creation with the attach snapshot event),
  `session.flush`, `session.add-entry` (the `parseSessionEntry` port
  with the view-only gate), and the preference writes
  `runtime.save-reasoning` / `runtime.save-show-reasoning`
  (session-scoped) plus `runtime.save-approval-mode`
  (machine-wide, reloads the host copy). `TurnExecutor` gained
  `is_busy`; the attach snapshot builder moved to
  `dispatch/session_write.rs` as the single authority shared by
  `attach` and `session.new`.
- The visible send queue behind `session.send`: `TurnRunner::enqueue`
  pushes mid-run messages onto the roster (published the moment they
  join, legacy L2130), a drain loop runs them as their own turns after
  the run settles (duty flag flips under the queue lock — a direct
  `send` still gets its queue drained), and `session.queue` steers or
  removes by id with the source's refusal sentences
  (`There is no running task to steer.` / `This message is no longer
  queued.` / workspace and source refusals). Steering admission runs at
  the model-step boundary (prepareStep): an admitted message lands in
  the transcript and reopens the run for one more request; the final
  text lands first, exactly like L2474→L2489. `session.stop` honours
  `keepQueue` (queued sends survive only when it is true). 8 tests
  (6 dispatch, 2 e2e: queue+drain, steer admission).
- `session.select-model`: the two `requireString` checks (256/512),
  the accept-immediately answer, and the pick's journey to the next
  turn boundary — `Host::queue_model_pick` records it, `prepare_send`
  reads it for the request (URL/key resolve for the picked provider),
  and the runner takes it into the record before the admission save.
  The settings half is best effort like the source: `rememberProviderModel`
  (`settings/remember.rs`, including the 512-model cap sentence),
  save through the transaction, host copy refreshed; a failure logs
  `Could not record the picked model …` and still answers accepted.
  Effort variants (`describeModelReasoning`) need the runtime catalog
  and are not in this slice. 9 tests (5 remember, 4 dispatch/e2e).
- The door now carries the tool catalog per turn (`sidecar_tool`
  shapes each native definition with the denotation suffix and the
  timeout field): a real provider sees the tools the batch loop can
  execute, instead of an empty list the mock used to hide.

- `session.answer` and the question queue: `TurnExecutor::answer`
  seam (id-guarded like the source's `answerQuestion`), the dispatch
  action with its three validation sentences, `TurnRunner::ask` — the
  queue behind the `question` tool with the owner's stop dismissing it
  (`None`), and the `pendingQuestion` slot in every published frame.
  The roster publish at queue time rides `send`'s tool select — the
  tool runs under the send-held state lock, so a re-entrant publish
  would deadlock; the wakeup is a `Notify` permit, never lost. 3 tests.
- The `question` tool end to end: `ToolDefinition::execute` is now a
  boxed future (`ToolFuture`), `AskPort`/`AskChannel` clone the
  runner's queue into every tool context, and the tool's source
  validation (trim, UTF-16 bounds, options rules, and the
  unavailable/dismissed/answered shapes) ports question.ts byte for
  byte. The batch loop offers `question` beside `execute_command`.
  Pinned by the `question`-scenario e2e (queue → roster → seam answer
  → conversation pair → closing frame) — agent_loop 18.
- `session.stop` and the pending-approval visibility: the fire-and-forget
  stop seam (`TurnExecutor::stop` — the runner's stop flag, never
  spawning the door), and `pendingApproval`/`pendingApprovalId` now
  ride every published frame while the queue waits — the queue publishes
  the moment it exists (the source's `requestManualApproval` publish).
  `keepQueue`/`holdQueue` read as plain `=== true` checks with no
  validation, as in the source; 2 tests (roster visibility + stop wire).
- `attach` (chat path): the source's validation wording, the windowed
  answer (`entriesFrom`/`totalEntries`, web only), the fan-out
  subscription on the connection's own sink, and the snapshot event
  queued behind the response — the connection flushes it after the
  answer is on the wire (`DispatchState::take_pending_events`, with
  `Outbound` implementing `EventSink`). Workspace root resolves like
  the source (record → settings default → the daemon's cwd). 3 tests in
  `tests/attach.rs`; isolated/schedule/worker/new-chat paths need the
  runtime (M2).
- Live publish (c3 producer): `TurnRunner` carries a weak host handle
  and a `last_published` baseline, and publishes on admission, on every
  stream event (text/reasoning/tool-call, with the live streaming
  index) and at the turn end (`busy: false`); `session.send` builds the
  documented bootstrap runtime view the frames carry (catalog-backed
  fields stay empty rather than guessed). Pinned end-to-end:
  `a_running_turn_publishes_live_state_to_the_fanout`.
- Fan-out roster (`dispatch/fanout.rs`): session subscriptions behind an
  `EventSink` trait (the connection's bounded write queue implements it —
  `dispatch` never depends on `server`), `publish` builds the event via
  `make_session_event` and queues it to every subscriber of that session;
  a refusing sink is unsubscribed on the spot (the source's
  `socket.destroy` for an over-limit frame). 4 tests in
  `tests/fanout.rs`.
- Publish unit (`agent/events.rs`): `make_session_event` / `diff_entries`
  port the source's previous/next diff (daemon-host.ts L3711/L3749) —
  id change forces a full snapshot, unchanged entries drop the
  `entriesPatch` key, shrinking transcripts replace-all, pure text
  appends ride `appendText` and any other field change replaces the
  entry (8 tests in `tests/publish_events.rs`).
- Manual approval queue behind `session.approve`: a park/resume
  `TurnExecutor::approve` seam (id-guarded, decision resolved as
  `decision !== "deny"` like the source), the dispatch action with the
  source's validation sentences and accept-immediately answer, and the
  loop's `NeedsApproval` path — approved runs the tool, declined stops
  after the pair, an untouched entry expires with the source's two
  timeout sentences (notice + denial) and a 120 s default from
  `approvalTimeoutSeconds`. The documented interim (immediate decline)
  is gone; the visibility surface (`pending_approval`) stays for c3.
- The batch loop (`agent/mod.rs`): one door call per step, tools
  executed host-side between steps through the shared pipeline, pairs
  persisted before the next request, one settled tool entry per call
  (`tools/format.rs` display text, duration over the execution), the
  stop-only-on-plain-decline notice verbatim, and the approval-queue
  park/resume for `NeedsApproval`. The
  conversation carries the **ai v7** shapes — `tool-call.input` and
  `tool-result.output.{type,json}` (probed against the installed SDK; a
  bare `result` throws inside `downloadAssets`). Authored pieces:
  `Unknown tool: <name>.` fails the run after pairing, `MAX_TOOL_STEPS
  = 24` caps runaway loops. Plus `TurnRequest.approval_mode` /
  `command_shell` threaded from settings, `config::scratch_root`,
  `catalog::native_definition`. Tests: batch execution + declined batch
  (`agent_loop`, 12 total).
- Shared tool pipeline (`tools/pipeline.rs` + `tools/catalog.rs`): one
  `ToolDefinition` shape and one `run_tool_call` sequence every native
  tool takes — source-exact gate rules (full-access never consults
  scope), the denial contract sentences, write-ahead proposal with
  stop-only-on-plain-decline semantics, `toolOutcome` mapping, host
  timeout extraction/stripping with the 1-500 s sentence, registry
  denotation + `_timeoutSeconds` schema injection, and 16 000 unit
  result bounding through the spill writer. `execute_command` is the
  first catalog entry; 6 fail-before tests (`tool_pipeline`).
  Deviations in `docs/tool-pipeline.md` (no redaction engine yet,
  unreachable timeout-exception path).
- `tools/execute_command.rs`: the flagship native tool — input
  validation with the source's sentences, the workspace-scope rules
  (cd walk, install/elevation/home/fetcher/piped-interpreter regexes,
  masked-URL path scan), the shell run with host-owned deadline,
  whole-tree kill, streaming inline cap with spill to a capped file,
  ANSI/control cleaning, and the exact result shapes
  (`execute_command`, 6 tests). `tools/workspace_scope.rs` ports the
  trusted-roots classification (missing-tolerant resolution) and
  `tools/tool_output.rs` the spill writer + notice.
- `tools/shell.rs`: shell resolution for `execute_command` — one
  setting, three shapes (auto walks the source's preference order, a
  preset names a program, a path runs as given), dialect from the
  executable's own filename, per-dialect launcher flags, WSL-stub
  skipping for `git-bash`, environment-stated install roots only, and
  the three exact error sentences. Platform, environment and the
  existence probe are injected, so tests never consult the host
  (`shell_resolution`, 8 tests).
- Attachment store (`state/attachments.rs`): base64 decode with the
  source's character-class and size rules (padding-lenient like
  `Buffer.from`), magic-byte typing, content-addressed
  `sessions/<id>/attachments/<sha256>.<ext>` writes with the
  idempotent re-store, name sanitisation, and hash verification.
  `session.send` now accepts images end to end: dispatch validates the
  shape, the executor decodes and stores them before the turn (the
  legacy `enqueueMessage` conversion, failures as notices), the entry
  carries `workspaceRoot`/`imageCount`/`attachments`, and the model
  message gets the `[image#N="path"]` reference part. Pins:
  `attachment_store` (5 tests), image pass-through in `session_send`,
  references + entry fields in `agent_loop` (10 tests).
- `session.send`: the accept-immediately dispatch contract — the
  `parseQueuedMessage` validation with the source's wording, the
  connection's chat requirement, `prepare_send` resolving record +
  settings + secrets into a prepared turn, and the `TurnExecutor` seam
  (production `DoorExecutor`: one shared door spawned on first use, one
  runner per session, failures as `Could not send the message: …`
  notices). Boot installs it in `main.rs` with the production
  `DoorConfig` (heap belt, 10-minute idle kill). Documented deviation:
  well-formed images are refused loudly instead of stored as
  attachments. Pins: `session_send` (7 tests) plus a model-only label
  test for the workspace and Telegram delivery hints (`agent_loop`, 9).
- `agent/context.rs`: the runtime context builder — fills
  `SystemPromptContext` from disk exactly as legacy `buildAgentSystem`
  (budgeted user notes with the truncation notice, availability reads,
  skill discovery, role base, team block injected rather than loaded
  secretly), so the assembled system prompt carries every live value
  (`prompt_context`, 4 tests); `settings::config_path` becomes the single
  `config.yaml` path authority.
- Door contract: the assembled system prompt rides `SidecarCall.system`
  (absent from the wire when `None`) from `TurnRequest` through
  `streamText`, pinned by a new conformance vector (present + absent
  states) and an end-to-end test that reads it back from the provider's
  request body as the first `role: system` message — the transport's own
  folding, now part of the contract.
- `agent/providers.rs`: the system-prompt context providers — memory
  path layout (first-24-hex workspace key), bounded index/note reads with
  the line-boundary cut and the verbatim truncation notice, profile
  availability states, the role-exact team block, skill roots/discovery
  (override, per-scope cap, metadata-name match) and the UTF-16 index
  budget (`prompt_providers`, 15 tests; pinned retroactively — the
  implementation landed before its tests, documented in the header).
- `ledger` writer: the tool-action write path (c2a) — idempotent
  `proposed` record as the write-ahead gate before execution, one
  transaction per transition chain, boot recovery to `failed` with the
  source's wording, schema CHECK generated from `TOOL_ACTION_STATES`, and
  the 32 000-char JSON envelope (`ledger_actions`, 7 tests).
- `agent/`: the turn loop (M2c c1) — admission save before the model is
  asked, model-only host-context labels, one-assistant-entry streaming with
  the reasoning splice and block duration, settled tool entries, turn
  outcome stamping (`completed`/`cancelled`/`failed`) and the 1 s mid-stream
  dirty save (`agent_loop`, 7 tests against the real sidecar).
- `state/save.rs`: the session write path — recompute-on-save (display
  fields rebuilt from `entries`, `updatedAt` refreshed), the meta-before-  `entries` header order, skip-without-user (`allow_empty` to override),
  the `Cannot save an invalid session.` gate, and the atomic `0600`
  temp-plus-rename write; `conversation`/`conversationArchive` round-trip
  untouched (`session_save`, 5 tests).
- `sidecar/`: the real Node model transport — `ai@7.0.173` exact plus
  eleven provider packages exact on the `provider@4.0.38` line, a lazy
  provider registry keyed by allowlist names, single-step streaming
  (`stopWhen: isStepCount(1)`, `maxRetries: 0`, schema-only tools), the
  door's error taxonomy, and console/stdout ownership. Acceptance suite
  (`sidecar_real`, 7 tests) runs the real SDK against a local mock:
  tool-call stops after exactly one model request, unknown keys refused
  with keys kept off stdout, 429 → rate-limit taxonomy, and per-call
  lazy loading proven from `NODE_DEBUG` traces. A build step
  (`sidecar/build.mjs`, esbuild with per-provider lazy chunks) and the
  bundle acceptance test (`sidecar_bundle`, 1 test) prove the split
  survives bundling: the metafile covers all eleven provider graphs and
  loader traces touch strictly fewer dist files than exist.
- `openbutler-protocol`: wire contract types, the newline frame codec with a
  32 MB cap, envelope validation, and 14 conformance vectors (one captured
  from a live host).
- `openbutler-daemon`: data-directory resolution, the
  session state layer (paths, header-only reads, listing), the IPC server
  (endpoint derivation, accept loop, backpressure), and action dispatch.
- Workspace structure: centralised dependency versions, the
  `unsafe_code = "forbid"` lint, CI gates, MIT license.

### Changed

- Module boundaries now follow authority, with no behavior change:
  - `agent/mod.rs` keeps the runner types, lifecycle and the turn itself;
    the visible queue and steering moved to `agent/queue.rs`, the approval
    queue to `agent/approval.rs`, the ask/answer queue to `agent/question.rs`,
    and transcript entry construction to `agent/entry.rs`.
  - `dispatch/mod.rs` is the router only; the shared record (`DispatchState`
    and `Host`) lives in `dispatch/host.rs`.
  - `src/tools/` holds only what the model calls (one folder per tool); the
    gate, ledger sequence, timeout, formatting and result bounding moved to
    `src/tool_runtime/` (`pipeline.rs`, `catalog.rs`, `format.rs`,
    `workspace_scope.rs`, `tool_output.rs`).
  - Both transport doors sit under `src/server/` (`socket/` for IPC,
    `http/` for the browser surface), each keeping its own authority.
