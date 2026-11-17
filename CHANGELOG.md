# Changelog

Notable changes to this workspace are recorded here.
Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

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
