# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Text continuations carry the stream they belong to (`text` | `reasoning`).
  The patch builder labels each delta from the entry it grows, so a client
  applies the label instead of inferring one from the index; ordinary text
  keeps its old wire shape and an older host reads as text.

- `catalog.listProviders` and `catalog.listModels`: read-only picker actions
  over the host catalog (id, name, model count, package; then the models with
  their declared limits). They answer before an attachment and say
  `loaded: false` instead of an empty list when no catalog is in hand.

- `settings.setup`, `keys.add` and `keys.remove` accept a signed-in remote
  session. The admin account is the trust boundary, so a remote admin can
  configure the host it owns; `admin.create`, `token.*`, `pair.create`,
  `logs.tail`, `mcp.manage` add and `sessions.prune` stay local-only.
- Component installers (`install.sh`, `install.ps1`): select the release for
  the platform, verify it against `SHA256SUMS`, unpack into
  `~/.openbutler/bin` (`%USERPROFILE%\.openbutler\bin`), run the installed
  binary to confirm it reports the requested version, and write a marker so a
  second run is a no-op. `--version`, `--prefix`, `--no-modify-path` and
  `--quiet` are supported; `--prefix` lets the new stack live beside a legacy
  install until the user is ready to switch.
- A release workflow: one tag publishes three platform archives
  (`openbutlerd-<version>-<target>`) carrying the daemon, the bundled sidecar
  (`sidecar/dist/`, esbuild output, no `node_modules`) and the pinned Node
  runtime (24.22.3), plus a `SHA256SUMS` file. The tag must equal the
  workspace version or the build fails.
- The daemon resolves its Node runtime in one order: `OPENBUTLER_NODE`, then
  the runtime shipped beside the binary (`<exe dir>/node/node[.exe]`), then
  `PATH`. Before this, a packaged host ignored the runtime it shipped with and
  used whatever `node` the machine happened to have first.
- The admin account: one username and one argon2id verifier in `admin.json`
  (a PHC string, so its salt and cost travel with the record and a later cost
  raise does not lock anyone out). Created on the host machine only, by
  `admin.create`; a second create is refused and a broken store admits nobody.
- Device sessions: `login` answers with `LoginSuccess { deviceId, token,
  expiresAt }`, and every later connection presents that token as `auth`. The
  store keeps salted SHA-256 only, expires after thirty days, refreshes
  `lastSeenAt` at most once an hour, and refuses an eleventh device instead of
  evicting one.
- The sign-in lock: five consecutive failures hold the door for sixty seconds,
  shared by every connection, and a success clears it.
- The remote door refuses to open without an account: the host keeps running
  locally, records `host.listen_refused`, and never binds a port whose first
  frame could create the account.
- `admin.create` answers with the host's own sentence, so the client prints a
  line it did not compose.

- The Vercel AI Gateway is a provider. `@ai-sdk/gateway` was missing from
  the sidecar registry while the catalog marks a provider with it (id
  `vercel`), so configuring one produced `unknown provider`. The package
  is a direct dependency pinned to 4.0.146, the version `ai@7.0.173`
  already resolves, and the handshake now reports twelve keys.

- A host started with `--web` now publishes the browser surface it serves:
  `status` carries `web.bind` and `web.pairUrl`, and `web.report` answers
  with them. Before, the publish call existed but nothing in the boot path
  made it, so the surface was never reported. `start_lock::host_status`
  exposes the raw status answer.
- `settings.setup.review` composes the wizard's Review block (`{lines}`)
  from the exact payload `settings.setup` takes, through one shared parse:
  the empty-set sentence, one bullet per provider with the default marker
  and its key count, and the default-model row. A review writes nothing.
- `mcp.manage` lists, adds and removes global MCP servers: the list
  screen is the host's (section, table or the source's empty sentence, and
  the management hint), `add` validates through the settings loader's own
  `parse_mcp_server_state` and refuses unseen-id collisions with the
  source's sentence, `remove` refuses an unknown id. Writes go through the
  settings pair save and reload the live copy.
- Structured logging: `logging` writes one JSON record per line to
  `<data>/logs/openbutler.log`, redacting credential-shaped keys and values
  and rotating at 5 MiB with three kept files (the source's `logging.ts`
  defaults). Boot writes `host.started`, the remote listener its
  `host.log` line, and every exit `host.stopped`.
- `logs.tail` defaults to that file, filters parsed records by `level` and
  exact `event`, and `format: "json"` returns the raw records.
- `sessions.prune` — the source's retention pass over saved sessions
  (`cli-main.ts` L1465-1526, `retention.ts`, `session-store.ts`), run
  inside the host because the host owns the files: a strict
  modification-time cutoff, the machine custodian never selected, rows
  `  <UTC date>  <id>  <bytes>`, a preview that deletes nothing, and an
  apply that re-reads path/mtime/size before removing the record's own
  directory. Each answer line carries the host's tone
  (`plain`/`info`/`success`/`warn`) so the client only applies its glyph
  palette. `Invalid duration "…"` (the shared retention parser) and
  `apply must be a boolean.` are the refusals; the action is local-only
  (`Session files are pruned on the host machine only.`), preserving the
  source's authority envelope. Deletion is a hard delete like the
  source's `rmSync` — pruning exists to reclaim disk, so routing the
  tree through the platform trash would keep the bytes.
- `keys.list` / `keys.add` / `keys.remove` — the provider key store
  (`cli/keys.ts`): rows carry `…tail` display fragments only; writes trim,
  drop empties, skip duplicates, create the entry enabled and keep
  `providerOrder`; remove clears the keys and keeps the entry. The write
  goes through the secrets document, so `settings.yaml` never carries a
  key. MCP-server and unknown-provider refusals are the source's
  sentences; the writes are local-only (`API keys are managed on the host
  machine only.` — a new credential gate). Pins: the empty row set, the
  append/dedupe path, the remove-keeps-entry path, both classification
  refusals, the keys payload shape, the remote gate and the
  settings/config split.
- `skills.list` — Agent Skill discovery, the `skills.ts` port: the seven
  roots in source order, frontmatter metadata (name must match the
  directory, description 1..=1024 chars, first 8 KiB, symlinks refused),
  dedupe with later roots winning, 128 per scope, ordered builtin →
  workspace → global then by name; the workspace is the stored default,
  else `<data>/custodian`, and an explicit path canonicalizes. Pins: the
  scope ordering, the duplicate override, mismatched names, description
  bounds, an unclosed frontmatter block, invalid directory names, the
  action's roots/default, and an explicit workspace scanned end to end.
- `settings.set-default-workspace` — the workspace bare launches use:
  `{path}` canonicalizes through `canonical_scope_root` (the single
  normalizer every workspace path already resolves through) and saves
  `defaultWorkspaceRoot`; `{}`/`{path: null}` answers it, `null` when
  nobody chose one. Source: `runWorkspaceCommand` (cli-main.ts L340-352);
  the source wrote settings from the CLI process.
- `openbutler about` — the source's `runAbout` screen from the host's
  `about.report`: this CLI's version, the host's version and pid, the
  resolved command shell (the same authority the `execute_command` tool
  uses), the os/arch triple, the two configuration paths (names only) and
  the bundled sidecar. One added row: `Host`, because this build has two
  binaries and the source ran one process.
- `usage.report` — the usage screen's whole state, computed by the host:
  optional `{days, session, model}` in (`days` defaults to 30 and must be a
  whole number in 1..=3650 — `days must be a whole number between 1 and
  3650.`; the filters are optional non-empty strings under 512 bytes) and
  `{label, summary, days, lines}` out. Reads its own `state.db` read-only
  (5 s busy timeout; a missing file or table is no rows), buckets by each
  row's own recorded offset, sums the fresh/cache split, walks the UTC
  streak, splits the cost sources, and renders the source's lines and
  sparkline — the CLI only prints them. Port of `cli/usage.ts` and
  `usage/report.ts` (`bucketUsage`, `summarizeUsage`, `renderUsageReport`).
  The source assembled all of this inside the client process; the daemon
  owns it here. Pins: the byte-exact render (the CLI's old expectation,
  moved), offset rollover, the busiest tie keeping the earliest day, the
  streak stopping at a gap, the fresh-input fallback with its zero floor,
  the cost-source split and both closing lines, the window cut, the
  session/model filters (bare provider included), and both refusals.
- `ledger.prune` — the settled-history selection from `activity-ledger.ts` with a preview that runs the identical SQL: worker identities and one-shot schedule identities never go, an inbox event waits for its read marker, and an event an action or usage row references is not history yet; only terminal actions with their transitions go. The duration parser (`30d`, `12h`, `90m`, `2w`) and the cutoff clock are the daemon's; `Invalid duration "…"`, `kind must be events or actions.` and `apply must be a boolean.` are the refusals. The source pruned from the CLI with the host stopped; the host is the one writer here, so the action runs inside it. `state.db`'s path now has one name (`ledger::db_path`). Pins: preview vs apply on the same selection with every guard row surviving, terminal-only actions with their transitions, the refusal sentences, and that a missing database counts zero without being created.
- `permissions.manage` — the `openbutler permissions` surface in one atomic
  write: optional `mode`, both timeouts (5-600, the source's flag sentences)
  and `reviewModel` (`<provider>/<model>`, or `none` which clears the
  `review` role and its legacy mirrors). Answers the four display values
  (`formatApprovalMode` labels, `15 seconds (default)` spellings, the
  reviewer pair or `(main model)`) plus `saved`; nothing carried means no
  write. The source wrote settings CLI-side and pushed only the mode to a
  live host. Pins: the default report, mode persistence + live copy, both
  timeout sentences, the role write and the `none` clearing.
- The auth token actions (`token.create` / `token.list` / `token.revoke`) and
  the local-only gate: the store lived CLI-side in the source and the host
  owns it now (salted hashes only; the plaintext is answered once). The
  store gains `list` (id and creation time — never anything secret) and
  `revoke` (no write when the id is absent). Minting credentials is local
  only: `pair.create` and every `token.*` action refuse a remote connection
  (`Pairing codes are minted on the host machine only.` / `Remote auth
  tokens are managed on the host machine only.`) — the source had no gate,
  so a remote client holding a token could mint more. New contract,
  documented in `api-surface.md`. The revoke payload key is `tokenId`: a
  payload key named `id` collides with the envelope's flattened `id` and the
  frame fails to deserialize (the CLI's fake daemon showed it first). Pins:
  mint → verify → list → revoke → empty, the unknown-id sentence, the id
  validation, and the remote gate over all four actions.
- The host-wide connection roster (`dispatch::clients`) and `stop`'s
  `restart` flag (daemon-host.ts L3191-3214): the arm validates the flag
  (`restart must be a boolean.`, null included), answers a `notification`
  frame to every other admitted connection (`OpenButler host restarting` /
  `Sessions will be saved. Reconnecting after the host restarts.`, or the
  stopping wording), writes the source's IPC log line and asks for the
  shutdown. The request is deferred to the connection: the arm sets the
  flag, and the connection performs it only after its response is confirmed
  written (`send_confirmed`) — the source's `writeResponse` before
  `setImmediate(requestStop)`. A serve loop that ended first lost the ack:
  a stop issued right after boot answered the client with a closed
  connection, reproduced and then verified by five rapid start/stop cycles.
  The requester is skipped by sink identity — it reads the stop in
  its own response. Registration is a `server` concern (one sink shared with
  dispatch) and the guard leaves the roster before the writer join: a
  roster-held sink clone would otherwise keep the connection's channel open
  and the join would never return (the malformed-frame close hung until the
  order was made explicit). The source's restart reattach state keys
  (`host.restart.workspace:<key>`) arrive with the attach runtime. Pins: the
  refusal wording (string and null), both wordings, requester exclusion, the
  deferred shutdown request, the roster contract (broadcast, unregister,
  refusing-sink drop) and one two-connection wire test.
- `settings.set-listen` — the saved remote-listener preference
  (`{listen: "host:port"}` saves, `null` clears), validated by the very
  rule the host binds by and written through the settings pair save; the
  live copy is reloaded so nothing reads a stale target. The boot now
  honors `daemon.listen` when no `--listen` flag is given, and
  `run --no-listen` refuses the saved value for that run (the
  `daemonRunOptions` rule) — before this the preference could be saved and
  never bound. Verified live: `daemon start --listen 127.0.0.1:45290`
  bound it and persisted it; a plain `daemon start` afterward bound the
  saved value with no flag; `--no-listen` cleared the section.
- `config.report` — the manager screen's whole state, computed by the
  daemon (`dispatch/config_report.rs`; sources `runConfig`,
  `summarizeProviders`, `readiness`, `formatApprovalMode`,
  `describeReasoningPreference`, read 2026-11-24): MCP rows, default
  workspace, the approval-mode label, the provider table (stored order,
  kind, key count, tool-capable count under the source's null-vs-zero
  rule, the default row's model), the default chain with its source, the
  reasoning line, and readiness with the source's sentences. Counts only —
  never key material — and it passes the view-only gate. Payload-free, so
  no protocol change; 13 pins in `tests/config_report.rs`. Verified live in an isolated data dir: an empty machine prints the source's empty screen; with two providers the first run shows `unknown` plus the download note, and after the background fetch (a 6.9 MB generation) the second run shows the real count (`openai` 36 tool-capable models) while the custom provider stays `unknown`.
- `default_model` (`resolveDefaultModel`/`pickMostUsedModel`/
  `resolveRoleModel` ports): the explicit pair, else the most-used usable
  model inside the 90-day window — usability is the source's rule
  (`hasCredential` OR a catalog-declared env var that is set, plus an
  enabled provider and a source of truth). `ledger::usage_pick_rows` reads
  the window read-only; a missing database is an empty list.
- The catalog now actually downloads: `Host::warm_catalog` starts one
  background fetch (own thread, own current-thread runtime, compare-exchange
  so one per process) when the index is not in hand, and `config.report`
  calls it — before this, nothing in production ever called `get_index`, so
  prices and tool counts could never appear. The report answers
  immediately (`catalogLoaded`/`catalogLoading`) instead of blocking on the
  network the way the source command did.
- The provider catalog store (`catalog/`, source `provider/catalog.ts`):
  models.dev data as an immutable NDJSON generation plus a byte-range
  index — the full payload (measured 6.59 MB / 326 providers / 10 185
  models) downloads under a 64 MiB runaway guard and is never parsed
  whole. Reads are revision-watched with a memo that follows
  cross-process replacements; one provider line verifies its sha256 and
  its id slot (a torn pair answers unknown, never absent). The fetch
  flow keeps the source's rules: fresh cache without a transport call,
  stale cache beating no catalog, a 60 s backoff, and the wrapped
  URL-bearing failure sentence — transport injected (no HTTP dependency
  yet). Design decision: override
  resolution and the derived-view plan for hot paths. 12 pins in
  `tests/catalog_store.rs`. The real transport (`HttpTransport`,
  reqwest 0.13 with platform TLS — rustls's aws-lc-rs needs a native
  toolchain this machine lacks) enforces the 8 s timeout and the ceiling
  twice (declared length first, then streaming), 4 pins in
  `tests/catalog_http.rs` over a loopback server.
- The catalog's derived view (owner design): configured providers and
  models joined with their catalog lines — settings override catalog per
  field, custom ids fall back to their own settings, sources labelled
  (`catalog`/`settings`/`unknown`), `providerOrder` respected. Cached as
  an `Arc` keyed by both revisions (catalog disk × settings), so a stale
  derived answer cannot exist; ambient env stays out (it moves no
  revision). Six pins in `tests/catalog_view.rs`.
- Catalog-backed usage pricing (owner rule: real prices or no estimate —
  never a character-based guess). `UsageInput` carries the caller's
  `TokenPrices`; an inline provider bill always wins, otherwise the
  catalog's per-million prices price the row as `catalog-estimate`
  (`estimateCost`: cache tokens at their own rate, removed from the
  plain-input share, zero total = no estimate) and the five price
  columns are stored. The host installs the store at boot; the turn and
  compaction hooks resolve prices through it. Four sink pins in
  `tests/ledger_usage.rs` plus an end-to-end turn pin in
  `tests/agent_loop.rs`.
- `settings.setup`: the first-run wizard's atomic write, a new contract
  rather than a port (the source wrote the files from the CLI — a client
  write the backend-owned rule forbids). The payload validates first
  (providers array ≤32, per-entry `id`/`keys` shapes, bounded `url`/`npm`
  and defaults with byte-exact refusal wording), then one journaled pair
  save: keys replace in `config.yaml`, `settings.yaml` never sees them,
  an absent `url`/`npm` keeps the previous value (`withProviderFields`
  merge, `settings.ts` L298), the order appends without duplicates, and
  `host.reload_settings` refreshes the cache. Defaults move as a pair
  and both pairs are remembered on the provider (`setDefaultModel`,
  `config-edit.ts` L74), so history and defaults can never disagree.
  8 pins in `tests/settings_setup.rs` (refusals, split contract, merge,
  boundary, remember).
- The daemon entry takes the source's start lock (`daemon.ts`
  `acquireDaemonStartLock`): `daemon.start.lock` in the data dir is
  created exclusively before anything is built, a contender gets the
  status-first answer (`An OpenButler host is already running (pid …)`,
  probed over the endpoint), then the age-based stale rule, then the
  deadline sentence — released token-checked on every path out via
  `Drop`. Deliberate divergences are documented in the module: no
  heartbeat and no owner-pid liveness check (this build keeps no pid
  record; a hard kill within 45 s of boot costs one wait, never a
  wrong answer). Four pins in `tests/start_lock.rs` including a real
  second entry against a live host.
- `attach` opens a new chat: `newChatSession: true` on a fresh
  connection now creates the draft through the same disk path
  `session.new` uses (busy gate included), binds the workspace, and
  answers with the regular attach result and queued snapshot event —
  the missing bootstrap for a CLI one-shot run. Isolated / schedule /
  worker success paths stay M2; one new pin (draft id, empty
  transcript, binding proven by a gated `session.load`) in
  `tests/attach.rs`.
- Team files, tailnet and coordination rules (T3a): `team/files`, the
  crate-level `tailnet` module and the `TeamCoordination` class ported
  from legacy `team/files.ts` (116 lines), `tailnet.ts` (62) and
  `team/coordination.ts` (254), read 2026-11-22. Verified writes meter and
  hash while reading, refuse overshoots and mismatches with the source's
  `TeamTransportError` sentences (400) and clash only on different bytes
  (409); content-addressed stores stay idempotent; the inbox landing spot
  follows node's platform basename plus lexical `resolve()` (`..` climbs,
  `.` disappears). `TeamCoordination` owns every peer rule once: profile
  revisions with the idempotent-retry half, capability reports in source
  order (platform before the group check; `toolCount` with **no** string
  coercion), and the full work machine - all ten actions with their guard
  sentences and statuses, accessibility before the action name, and the
  wake message committed in the same transaction as the row. JS edges are
  pinned deliberately: `String.length` measures UTF-16 (`js_len`),
  `Number()` coerces paging numbers but not revisions, and `Number("")`
  makes a trailing empty octet `100.64.0.0`. 33 pins across
  `tests/tailnet.rs`, `tests/team_files.rs` and
  `tests/team_coordination.rs`.
- Team business rules (T2): `team/operations` and `team/service` ported
  from legacy `team/operations.ts` (254 lines) and `team/service.ts`
  (116 lines), read 2026-11-22. `TeamOperations` owns every rule once —
  recipient canonicalisation, the message/file/id validations with their
  source sentences and HTTP statuses, transaction-scoped post dedup with
  the reply-ack repair, file registration announcements, invite/join
  lifecycles with ticket issuance and audit rows, leave vs remove. The
  service helpers mint the source's id shapes (`m_`/`f_`/`rt_`/`jr_`/
  `t_`), hash tickets, and build the pairing fingerprint, over
  `getrandom` + sha256. `TeamStore::transaction` is generic over the
  operation's error so business refusals keep their status while rolling
  back. Twelve pins in `tests/team_operations.rs`. Also removed a
  test-gaming hack found in review (`remove_member` special-casing the
  literal name "alice"): hubs are never removable in the source, and the
  contradictory store test was the bug — its fixture now retires a
  joined member instead.
- The team data layer (T1): `team/{contracts,coordination,store}` ported
  from legacy `team/store.ts` (697 lines, read 2026-11-21). The hub
  store's schema is verbatim — 13 tables, the three source indexes, the
  `kind`/`file_id` in-place migration, `busy_timeout = 5000` + WAL +
  0600 — so a legacy `team.db` stays compatible. Every source method is
  ported (`transaction` closure, meta, invites with used stamps, join
  tickets with the hash on the member, messages with AUTOINCREMENT
  ordering, cursors/acks, profiles/capabilities/work as value-JSON
  columns, audit, presence/summaries with the source's correlated
  subqueries, files). The wire DTOs carry the source's field names and
  camelCase (`TeamMember.role`/`removedAt`, `JoinRequest.id`/`code`,
  `TeamAuditEntry.detail`, `MemberSummary` with profile, capabilities
  and active-work count); the work machine is the nine source states
  with `TERMINAL_WORK_STATES`/`ACTIVE_WORK_STATES`. Seventeen pins in
  `tests/team_store.rs`.
- The `attach` variant ladder (daemon-host.ts L3365-3440, read
  2026-11-21): the selection gate (`isolatedSession !== true` with no
  schedule/worker name answers `Choose a chat or a session to attach.`),
  the custodian workspace refusal (data dir as workspace), the three
  worker-view gates, both 128-byte worker id limits with their source
  sentences (`That session is not a subagent worker.` — this build has
  no worker ledger, which is exactly `readWorkerRecord` returning null;
  `That subagent is not running right now.` for a running view), and
  the schedule lookup (64-byte limit, record + canonical workspace
  compare, one sentence for missing and mismatched alike). Variants
  that pass every gate answer `The session runtime is not available in
  this build.` — deliberately not a source sentence: the M2 session
  runtime is what the source falls back on. `session.load` gained its
  two missing gates: the current-chat shortcut before them (L3621) and
  `Wait for the current run before switching chats.` (L3626).
  `canonical_scope_root` moved to `state/paths.rs` — one normalizer,
  shared by the schedule write path and the attach ladder. Nineteen
  pins: 17 ladder in `tests/attach.rs`, 2 load in
  `tests/session_read_actions.rs`; `tests/common` gained the shared
  `RecordingExecutor` double.
- The workspace binding contract landed (daemon-host.ts L3179-3311,
  L3478-3499): the pre-attach partition (host-wide actions answer with
  no key), the `workspaceKey` gate (string gate at 32 768 bytes, then
  a normalized match against the connection's binding — case-folded on
  Windows, the exact string on POSIX), the view-only gate over the
  10-entry `VIEW_ONLY_ACTIONS` allow-list, and `session.command`
  resolution that now runs after the gate. `attach` sets the binding
  from the resolved chat workspace and `session.load` rebinds through
  the same authority (`chat_workspace_root`); `session.list` answers
  the whole picker unattached and only the bound workspace when
  attached. Ten gate pins in `tests/attach.rs` plus a two-stage wire
  test (gate sentence → attach → `Unknown IPC action: does.not.exist`)
  pin the order. Test harnesses model a bound client: `bound_state()` /
  `with_binding()` in `tests/common`. The isolated/schedule/worker
  variant success paths stay with the M2 session runtime.
- The event pump (`schedule/pump.rs`): pending schedule events are
  claimed and run to their durable outcome — workspace gate, wake
  assembly, origin-vs-background session (the persistent background id
  lives under `session:{scheduleId}` in the ledger state, minted on the
  first firing and bound to the schedule's workspace), the awaited turn
  via `spawn_turn_for_event` (a completion that settles when the
  enqueued turn ends), last-assistant report, run-finished and the
  `delivered`/`failed` event state. Triggered by `schedule.run` after
  the insert and once at boot; split sync-prepare/await/sync-finalize
  because rusqlite's connection is `!Sync` (writer in a `Mutex`, the
  runner's shape). Four pins (`schedule_pump`).
- `TurnExecutor` seams for the pump: `has_live_session` (read-only
  origin lookup — never creates a session to ask) and
  `spawn_turn_for_event` (the awaited `sendMessage` shape; errors are
  already noticed on the record by the drain), with the queue's
  optional per-turn completion.
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
  Known deviations: no redaction engine yet, and an unreachable
  timeout-exception path.
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

- `status` no longer prints a workspace count. The number was a legacy screen
  value nothing computed; the status reports the pid, the start time, the
  connected clients and the rows the host actually knows.

- The daemon binary is `openbutlerd` (package `openbutler-daemon` unchanged),
  so the process name follows the `dockerd`/`sshd` convention and can never be
  mistaken for the `openbutler` command. The data directory keeps its name on
  purpose — renaming it would move existing data for no user benefit — and the
  stderr prefix follows the binary (`openbutlerd: …`).
- `openbutlerd --version` prints `openbutlerd <version> (daemon) protocol v2`,
  so a machine with both this build and the legacy launcher can tell which one
  answered.
- The catalog is **one file**. Each fetch overwrites `catalog.ndjson`
  (staged, synced, renamed) instead of writing an immutable generation
  beside a separate `catalog-index.json`; the header line inside the file
  carries the format version, the fetch time and, per provider, the byte
  range of its line plus its model counts, so a listing still reads
  kilobytes and a provider read is still one range. A month of daily
  fetches no longer leaves a directory of generations (it had reached 36
  files and 213 MB); the old layout is swept on the next publish.

- The prompt assembly reads the shared home resolver (`state::paths`)
  instead of keeping a second copy that preferred `USERPROFILE` on every
  platform; the POSIX ordering is now the platform’s own.

- `skills.list` answers the screen as well: the table when skills were
  found, otherwise the empty note, every root and the hint, composed in the
  host and printed by the client.
- `status` and `session.list` carry their screens: `DaemonStatus` gains
  `lines` (skipped while empty) and `session.list` answers
  `SessionListAnswer {sessions, lines}`. The protocol owns `ScreenLine` and
  `Tone`, so every consumer decodes one shape and the client only maps a
  tone to its glyph.
- `about.report` and `config.report` answer the **finished screen lines**
  alongside their display values: section bars, kv rows, table padding and
  the empty-machine early stop are the host's layout now, and `shape` gains
  `section_line` / `table_lines` as the one place that layout is written.
  A client prints the lines and owns only its glyph palette.
- `settings.setup` stores a repeated key once: one key is one credential,
  and the store's shape no longer depends on which client sent the list.
- `logs.tail` — the machine log's tail moved into the host, where the file
  lives. The tail is the CLI's algorithm moved verbatim (64 KiB blocks read
  backwards, CRLF stripped); `file` must arrive absolute because the caller's
  shell is the only correct base for a relative path. Local-only: reading
  host files over a remote token is authority the source never granted.
- `permissions.manage` now composes the whole screen and returns it as
  `lines` (`{tone, text}`, the source's order) alongside the display values
  and `saved`; the client prints it and applies its glyph palette instead of
  deciding which success sentences apply.
- `session.load` answers a **tail window** instead of the whole record:
  optional `entriesWindow` (integer 1..=2000 — `entriesWindow must be an
  integer between 1 and 2000.`; default 200) plus the
  `entriesFrom`/`totalEntries` markers (`SessionRecordWindow`), and older
  pages come from `session.entries-range`. A transcript over the 32 MiB
  frame cap can now be opened at all — the source returned the full
  record and the legacy CLI crashed on such files. Protocol rev bumped
  to `2a4a4af`.
- The daemon clock has one reading: `ledger::now_ms` (epoch
  milliseconds) — `auth`, `sidecar` and `schedule` had three copies of the
  same `SystemTime`/`chrono` body and now delegate. Trigger math, pairing
  codes and idle timers can no longer disagree about "now".
- A fresh draft's model is resolved through the shared default chain
  (`default_model::resolve_role_model`, the `resolveRoleModel('agent')`
  port) instead of copying the explicit pair out of settings: the explicit
  default, else the most-used usable model from the 90-day usage window.
  A `config` screen and a `run` can no longer disagree about which model
  the machine will use.
- The data directory defaults to `openbutler-daemon` on every platform
  (Windows `%LOCALAPPDATA%\openbutler-daemon`, macOS Application
  Support, XDG), and the scratch root to `<tmpdir>/openbutler-daemon`
  — the Rust stack never reads or writes the legacy Node install's
  `openbutler` directory: separate config, state, sessions, endpoint
  (the pipe name derives from the dir) and scratch. `OPENBUTLER_DATA_DIR`
  still overrides both rules. The rename exposed a second copy of
  `scratch_root` in `agent/providers.rs` (the prompt context read one
  path while turn inputs used the other): it is now a re-export of
  `config::scratch_root`, one authority, divergence documented at the
  re-export. Verified live: start/status/stop cycle against the new
  default dir, endpoint independent of a legacy host.
- The wire contract moved out of this workspace: `crates/protocol` is now
  the standalone `openbutler-protocol` repository
  (github.com/m4rs1l/openbutler-protocol), consumed here as a git
  dependency pinned by rev instead of a cross-repo path. Frames,
  envelopes, action payloads and the conformance suites live there - 43
  tests green in their own repo and from a fresh clone; this workspace
  runs 480 tests across 58 targets without them.
- One home for the tailnet address rules: `listen_target.rs` now
  delegates `is_loopback`/`is_tailnet_address` to the crate-level
  `tailnet` module instead of carrying its own octet copies (the
  duplicate was found while porting `tailnet.ts` a second time for the
  team hub; its `is_ipv4_literal` shape gate stays). The production
  interface lister uses `if-addrs 0.15.0` so the team server can pick a
  bind address without shell-outs.
- `attach` admission wording matches the source exactly:
  `isolatedSession` accepts any boolean (only `newChatSession` is
  exactly `true`, `newChatSession must be true.`), the chat-branch
  exclusions and the `projectId` shape (`Invalid project ID.`) now
  cover the new-chat branch too, and `workspaceRoot` runs through
  `requireString` first ("... under 32768 bytes.") before the
  absolute-path sentence.
- The settings schema was split from one 1 561-line
  `settings/schema.rs` into `settings/schema/` — one module per
  authority: `mod.rs` (store types + the `parse_store` dispatcher),
  `provider`, `mcp`, `telegram`, `team` (+ custodian types),
  `custodian`, `secrets`, `store`, `enums`, `keys`, `helpers`. Largest
  file 1 478 → 287 lines. Source parity kept by count (16 `derive` /
  32 `serde` attributes byte-identical to the removed file);
  sibling-only helpers moved to `pub(super)`, parse entry points stay
  `pub`; all gates green with the 18 `settings_schema` pins unchanged.
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

### Fixed

- A turn's opening chunk no longer disappears. Two losses stacked: the
  first publish only seeded the baseline, so a state the client was never
  handed counted as sent, and the patch builder dropped the rows appended
  after a rewritten one — the answer's first row was born in the same publish
  that stamped the reasoning block's duration, so its append never crossed
  the wire and a client printed only the tail of the first word. The attach
  path now seeds the turn runner with the snapshot the client received, and a
  delta always carries the appended tail.

- A transient provider failure no longer ends the turn. The host had no
  retry at all while the SDK runs with `maxRetries: 0`, so a 429 or a 5xx
  surfaced as a failed run; the source retries on its own backoff
  (`provider/retry.ts`: 1-2-4-8-16-32-32-32-64 s, ten attempts).
  `agent::retry` ports that classification and backoff — 408/409/429/5xx
  and transport failures, never another 4xx — and `drain` applies it only
  while nothing has reached the transcript, so a retried step can never
  duplicate output. The sequence owns one notice line replaced in place
  (`Rate limited (429). Retry 1 of 9 in 1s.`), and the wait wakes early
  when the owner stops the run. Key rotation is deliberately not ported
  (one key per provider; owner decision, 2026-11-26).

- A provider configured from the catalog reaches the sidecar. The host
  sent the settings provider id while the registry speaks npm families,
  so every catalog provider whose id differs from its family was refused:
  `stepfun-ai` (npm `@ai-sdk/openai-compatible`) answered `unknown
  provider`. `prepare_turn` now resolves the wire key through
  `sidecar_provider_key` — the npm family when the catalog recorded one,
  the configured id otherwise, which keeps hand-configured providers
  working.

- Stopping a run interrupts a running command. The stop flag was read only
  around approvals, questions and the model stream, so a tool that blocked —
  `execute_command` waiting on its child — ran to completion or to its own
  timeout while the turn stayed busy (measured: 6.5 s for a 6 s command).
  The flag now reaches the tool, the process tree is killed where it stands,
  and the result carries `status: "stopped"` with `The command was stopped by
  the user.` The ledger records `cancelled` rather than `completed`, and the
  tool-result pair lands in the conversation, so the next turn reads which
  command was interrupted instead of a finished one. The command also runs on
  a blocking thread now, so a long command no longer stalls an async worker
  for its whole timeout.

- A skill’s description budget is measured in UTF-16 units on both sides.
  The `skills.list` action already read the source’s rule that way while the
  system prompt compared bytes, so a Turkish description of 600 characters
  appeared in the listing and vanished from the prompt.

- Settings writes survive a crash. `atomic_write` staged
  `<name>.<pid>.tmp` with an exclusive create, so a run that died before the
  rename left a name the OS later reassigned to a new process: every write
  then failed with `AlreadyExists` until the file was found by hand. A stale
  temporary is now reclaimed before the create is retried.

- `settings.setup` refuses a payload that names the same provider twice. The
  merge kept the last entry, so the earlier entry's keys vanished while the
  answer still reported a save; the wizard's "Back to LLM providers" branch
  could produce that shape.

- 	eam invites store the instant they end. TeamOperations::create_invite
  handed its lifetime to the store as if it were the expiry, so a live clock
  made every invite expire in 1970 and no join could ever be admitted. The
  hub clock is now added once, where both halves are known.

- The schedule pump runs against the data directory its caller passes
  instead of reading the machine's global one. A test or an embedding that
  passes its own directory to dispatch therefore no longer spawns a pump
  against the real store, where it could claim and fire the owner's
  schedules.

- `session.send` validates an image's bytes before it answers `accepted`.
  The source decodes each payload inside `parseQueuedMessage`; this port only
  checked the field shapes, so a message whose picture was not a PNG, was
  empty or was over 5 MB was accepted and the loss surfaced later as a
  notice. The parser now decodes against the source's 5 MB cap and passes on
  the canonical base64, keeping its tolerance for whitespace.

- `ledger.prune` and `sessions.prune` refuse a duration whose unit is a
  multibyte character instead of unwinding the handler that read it.
  `parse_duration` split the unit off the last byte of the caller's string,
  and `str::split_at` panics when that index falls inside a character
  (`olderThan: "30ğ"`). The split now takes the last character, so the
  refusal is the source's `Invalid duration …` sentence. Reachable over a
  remote token, because `ledger.prune` accepts one.

- The tool-output spill cap now counts bytes, as its name and documentation
  always claimed. It counted characters against a byte budget, so a command
  emitting multibyte text could write four times the stated 64 MiB ceiling.
  The writer keeps a byte total separate from the character totals the
  notice reports, and cuts on a character boundary.
- `settings.setup` now refuses a remote caller with the credential sentence
  `keys.add` already uses. The wizard writes the same secret store, so
  without the gate a paired remote token could replace any provider's API
  keys through the other door.
- `mcp.manage` refuses `add` and `remove` to a remote caller. A server
  definition is a command the host spawns on its own machine, so a remote
  token could install an arbitrary process that runs at session start.
  `list` stays open to a remote caller.
- The remote TCP listener no longer stops for good on the first failed
  accept. A failed accept (a peer that reset before it was taken, a
  momentary descriptor shortage) ended the accept loop without a message,
  leaving the host running with its remote door closed. The loop now reports
  the failure on stderr, waits 50 ms and keeps listening until shutdown.
- A pairing code is consumed only when the token was minted. Before, the
  code was spent first, so a token-store failure (full disk, unreadable data
  directory) cost the owner the one-time code and the client got an error
  with no way to retry. PairingCodes::redeem_with runs the mint while the
  lock is held and clears the code only on success.
- execute_command spilled overflowing output into the data directory
  root instead of 	ool-output/. The retention sweep that runs when a spill
  closes deletes files older than three days in the directory it was given,
  so a long-running host could age out settings.yaml, the token store and
  the schedules. Spill files now live under 	ool-output/.
- The static file server refuses request paths that name an absolute
  location (`/C:/...`, `//...`). `Path::join` with a drive-prefixed or
  rooted argument discards the base directory on Windows, so such a path
  could read a file outside the bundle. Every segment must now be a plain
  name.

[Unreleased]: https://github.com/m4rs1l/openbutler-daemon/commits/main
