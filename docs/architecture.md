# Architecture

## Overview

Roger is a single-process Rust async application using `matrix-sdk` (tokio-based). It syncs with a Matrix homeserver, listens for messages in allowlisted rooms, and responds via LLM.

## Config system

Two-file design keeps secrets off GitHub:

- `config/profiles.toml` — committed. Defines named LLM profiles (`chat`, `fast`, `code`, etc.) and routing rules. References backends by logical name, not URL.
- `config/backends.<HOST_ROLE>.toml` — **gitignored**. Maps logical backend names to real URLs, models, and `api_key_env` variable names. `HOST_ROLE` env var selects which file to load (default: `local`).

API keys are never in config files — only the *name* of the env var holding them.

## Session persistence

On first login, roger saves `roger_session/session.json` (access_token, device_id, user_id). On restart, it restores from this file instead of re-logging in. This prevents a new device ID on every restart, which would conflict with the SQLite E2EE crypto store.

## Conversation history

`HistoryStore` writes one JSON file per room to `roger_session/history/`. Room IDs are sanitized for filesystem safety. Context window is the last 20 messages, passed as the full messages array to the LLM.

History is room-scoped by default. No cross-room sharing (each room has independent context).

### Token budgeting

Context is selected by token budget rather than a fixed message count.
`windowed_by_tokens` walks history newest-first, keeping messages until an
estimated token budget is hit (~4 chars/token heuristic via `estimate_tokens`);
the latest turn is always kept. The budget is
`context_tokens − max_tokens − system_prompt − 256 margin` (floored at 256),
where `context_tokens` is a per-profile config value (default 8192).

## Context injection

The system message is assembled per turn (`assemble_system_prompt`) by layering,
in order: the base persona (global or per-room `system_prompt`) → operating
instructions (global `[context].operating_file` + optional per-room
`operating_file`) → durable memory (`## Memory (global)` then `## Memory (this
room)`). Operating files and memory files are read **fresh each turn**, so edits
take effect without a SIGHUP. Empty/missing files contribute nothing.

Because the history budget is sized off `estimate_tokens(system_prompt)`, anything
injected here automatically shrinks the history window — injected memory can't
silently overflow the context. Memory lives under `~/.roger/memory/` (`global.md`
plus `rooms/<room>.md`) via `MemoryStore`; it is written by compaction and survives
`/clear` (drop it with `/forget`). `[memory].enabled = false` disables the layer.

## Compaction

When a room's history exceeds the compaction trigger — `[compaction].trigger_fraction`
of that room profile's history window (default 0.8), or the absolute
`[compaction].trigger_tokens` if non-zero — the response handler spawns a
**detached** compaction task (`src/compaction.rs`, guarded so a room is
never compacted twice at once). It keeps the last `keep_recent_turns` verbatim,
sends the older turns to the `[compaction].profile` LLM, and parses a three-section
reply: a **summary**, **room-specific** durable facts, and **broadly-useful** facts.
`HistoryStore::rewrite` replaces the room's history with `[summary] + recent`; the
two fact sets are appended to the per-room and global memory files. When a memory
file exceeds its `[memory].max_*_tokens` cap it is re-summarized in place, so memory
can't grow without bound. If the model returns no usable summary, history is left
intact. There is no nightly job — compaction is purely reactive to size.

## Response UX

For every response:
1. `room.typing_notice(true)` — shows the typing indicator in Matrix clients.
2. The producer (`chat_with_tools`) pushes accumulated-text snapshots over an mpsc
   channel. How much arrives incrementally depends on the backend: an HTTP backend
   runs its tool loop and then sends the synthesized final answer as **one** chunk
   (non-streaming, so Matrix clients don't flicker through near-identical edits);
   the claude subprocess sends true token deltas; the opencode subprocess sends the
   whole reply in one chunk. The handler flushes (posts the first message, then
   edits it in place via `m.replace`) on a **sentence boundary** or once the flush
   ceiling passes — whichever first — but never faster than the rate floor. The
   flush decision is `should_flush()`; the cadence comes from
   `CommsConfig::edit_debounce_ms` (`FlushCadence`). There is no placeholder in any
   mode — the typing indicator is the only "working" signal until the first content
   flush posts the message.
3. Final render: edit the message with the complete reply (if it changed), or — for
   a short response that never triggered a flush — send it as a single fresh
   message. `room.typing_notice(false)`.

If the producer errors or yields no content, the handler falls back to a single
non-streaming `chat()` call. Slash commands reply directly with a single message
(no streaming, no placeholder).

## Audio pipeline

1. Matrix sends an `m.audio` event with encrypted media
2. `matrix_sdk::media` downloads and decrypts via `MediaRequestParameters`
3. Raw bytes POST'd to Speaches (`/v1/audio/transcriptions`, model `Systran/faster-whisper-small`)
4. Transcript text fed into normal LLM flow

## LiteLLM proxy

All cloud LLM calls go through a LiteLLM Docker container on `srv:4000`. This:
- Keeps the Anthropic API key on one machine (`srv`) only
- The `ai` machine uses a LiteLLM virtual key (`GATEWAY_VKEY`) with no direct Anthropic access
- Lets backends be swapped without changing roger's config

## Profile routing

`ReloadableState` holds one `ProfileLlm` per profile (`llms: HashMap<profile, ProfileLlm>`),
built from `profiles.toml` at startup and on reload. A profile that has no usable
backend is skipped with a warning; `chat` is required.

### Fallback chains

A `ProfileLlm` wraps an ordered list of clients: the profile's primary `backend`
followed by its `fallback` backends (same profile params, different provider). Each
`chat`/`chat_stream` call tries clients in order, advancing to the next only on a
transport error or non-2xx status; the first client that responds (even with empty
text) ends the chain. This lets a local profile fail over to a cloud provider — e.g.
`chat` runs on LM Studio but falls back to Anthropic via the gateway when LM Studio
is down. Streaming falls over too: failure happens on connect (before any token is
sent), so the user never sees a half-stream from a dead backend.

Each room resolves to a profile via `ReloadableState::llm_for_room`:
1. a runtime `/model` override (`room_profiles`), else
2. the room's `profile` config field, else
3. `chat`.

If the resolved profile has no built client, it falls back to `chat`. The resolved
profile + model are shown in `/status`.

Runtime `/model` overrides are persisted to `roger_session/room_profiles.json`
(`RoomProfileStore`) and reloaded on startup; overrides for profiles that don't
build on this host are dropped on load and on reload.

## Config hot-reload

Reloadable config lives behind `Arc<RwLock<ReloadableState>>` (in `matrix/handler.rs`),
shared by every event handler via the cloned `BotCtx`. `ReloadableState` holds the
LLM client, model name, global system prompt, and per-room configs.

A `SIGHUP` listener task (`reload_on_sighup` in `main.rs`) re-reads `config/` and
swaps the state in place. Reload is fail-safe: a bad config logs a warning and the
running config is kept. Handlers clone what they need out of the lock and release it
before any LLM/network call, so reloads never block in-flight requests.

Fixed for the process lifetime (restart required): Matrix credentials, homeserver,
room allowlist, the logging setup, and the speaches client.

## Logging

`init_logging` builds a layered `tracing` subscriber:
- **stderr** — human-readable, captured by journald under systemd.
- **file** — JSON lines, daily-rotated via `tracing-appender` into `ROGER_LOG_DIR`
  (default `roger_session/logs/`).

A single `EnvFilter` (`RUST_LOG`) gates both sinks. The non-blocking writer's
`WorkerGuard` is held in `main` so buffered logs flush at shutdown.

## Metrics

`Metrics` (`src/metrics.rs`) holds lock-free process-lifetime counters: total
responses, errors, and cumulative latency (for an average). Each completed response
calls `metrics.record(latency_ms, ok)` and emits a structured log line
(`responded` with `room`, `profile`, `model`, `latency_ms`, `ok`) — so the JSON log
sink doubles as a metrics scrape source. Live totals are shown in `/status`.
Counters reset on restart.

## Subagents

Named subagents (`[agents.<name>]` → profile + system prompt + description) let the
model delegate a scoped task and get the result back. The `run_subagent(agent, task)`
tool is advertised (dynamically, with the agent list) only while a `SubagentHost` is
in scope; `handle_message`'s producer scopes one when agents are configured. The host
(`SubagentHostImpl`, which owns the LLM registry via `BotCtx`) resolves the agent's
profile, runs it **headless** (`chat_with_tools` with the stream drained — only the
returned text is used), and returns it as the tool result. `tools.rs` stays decoupled
via the `SubagentHost` trait + `SUBAGENT` task-local (same pattern as `set_workdir`'s
`ROOM_ID`). Subagents inherit the room's `ROOM_ID`/`WORKDIR`, so a `code`-profile
subagent runs claude-code in the room's worktree. Nesting is capped
(`MAX_SUBAGENT_DEPTH`); each level scopes a deeper host. `/agents` lists them and
`/agent <name> <task>` runs one manually.

## Tools & MCP

HTTP backends run a tool loop; the advertised tool list is dynamic, owned by
`ToolExecutor::tool_definitions()` (native tools + MCP), and `chat_with_tools`
pulls it from the executor rather than a static list. Native tools: `web_search`,
`web_fetch`, `read_file`, `write_file`, `list_dir`, `set_workdir`.

MCP servers (`src/mcp.rs`, `[mcp.servers.<name>]`) are connected as stdio child
processes at startup via `rmcp` and kept alive for the process lifetime (restart to
change — not hot-reloaded). Their tools are advertised namespaced
`mcp__<server>__<tool>`; `ToolExecutor::execute` routes those calls back to the
owning server (`McpManager::call`) and flattens the result's text content. A failed
server is logged and skipped, never blocking startup. `/status` shows connected
server + tool counts. Subprocess backends (claude-code/opencode) get MCP via their
own config, not this manager.

## Backend kinds

A profile's chain is built from named backends, dispatched on `kind`
(`Config::build_client` → `llm::Backend`):

- `open-ai` — OpenAI-compatible REST (LM Studio, Ollama, LiteLLM). `Backend::Http`.
- `claude-code` — spawns the `claude` CLI as an agentic subprocess per turn
  (`src/subprocess.rs`, `Backend::Subprocess`). Implemented.
- `open-code` — spawns `opencode run --format json` (same module). Implemented.
  opencode is self-configured (provider + baseURL in its own config), so the
  gateway env vars don't apply and the model is `provider/model`. Its `--format
  json` is not token-streamed — the full reply arrives in one `text` event, so the
  message paints once at the end.

### Subprocess backends

A subprocess backend inverts the HTTP model: the CLI owns its own agentic tool
loop (file edits, bash, web), so roger's `ToolExecutor` is bypassed; history is
passed statelessly each turn (rendered into the prompt — system prompt via
`--append-system-prompt`, no `--session-id`), so the *files in the working
directory* are the persistent state. claude is run with `--print --output-format
stream-json --verbose --include-partial-messages --permission-mode <mode>
--model <m>`; `content_block_delta`/`text_delta` events feed the same accumulated
-text channel as the HTTP streamer, and the terminal `result` event is the
authoritative final string. Auth maps the gateway via `ANTHROPIC_BASE_URL` +
`ANTHROPIC_AUTH_TOKEN` (the Anthropic key never leaves `srv`). Lifecycle: a
process-wide semaphore (`max_concurrent_children`), per-line idle timeout,
wall-clock absolute ceiling, optional `--max-budget-usd`/`--max-turns`, and
`kill_on_drop` + `process_group(0)` + `killpg` for whole-tree kill.

### Working directory selection

The agent's cwd is chosen by the LLM, not roger: a `set_workdir(project)` tool
(in `src/tools.rs`) resolves a name against the `[projects]` map (name → path) and
records it for the room in `RoomWorkdirStore` (`~/.roger/room_workdirs.json`). Per
turn the handler resolves the workdir as **room selection → `comms.default_workdir`**
and passes it to the subprocess via a `WORKDIR` task-local (the room id rides a
`ROOM_ID` task-local so `set_workdir` knows its target); task-locals avoid
threading these through the whole chat-call chain. Roger keeps no scratch space and
leaves no artifacts — the workdir is an external, real project (which may be roger's
own repo). Because roger's own state lives in `~/.roger`, pointing an agent at the
repo doesn't expose its Matrix token/crypto store.

## Worktree isolation

Agentic subprocess jobs (claude-code/opencode) can rewrite files in the room's
workdir — a real repo. When `[worktrees].enabled` (default) and the workdir is a
git repo, `subprocess.rs` runs the job in a throwaway worktree on a fresh branch
(`<prefix>/<room>/<n>` under `[worktrees].base_dir`, default `~/.roger/worktrees`)
instead of the live checkout. `WorktreeGuard::create` records the base revision;
`execute_in` runs the CLI with that worktree as cwd; on completion `finalize`
commits anything the agent left uncommitted, and — comparing HEAD against the base
rev, so the agent's own commits count — either keeps the branch and appends a
`🌿 branch + diffstat` note to the reply, or (no change) removes the worktree and
deletes the empty branch. A `Drop` guard force-removes the worktree if the job is
cancelled before finalize. Non-git workdirs fall back to running directly in the
workdir. Net effect: agent edits land on a branch for review/merge; the running
deployment is never modified out from under itself.

## Orchestrator: per-room queue + comms modes

Each room is a **serial FIFO worker** (`RoomQueues` / `room_worker`). `handle_message`
is a thin entry: it resolves the body (text, or audio transcription), answers control
slash commands *immediately* (bypassing the queue, so `/cancel`/`/status` work during
a job), and otherwise **enqueues** the turn to the room's worker. The worker pulls
turns in arrival order and runs `process_turn` one at a time — so replies and history
stay ordered within a room. Rooms run independently of each other.

The whole response pipeline for a turn (produce → stream → fallback → metrics →
history append → final render) runs as **one self-contained task**
(`run_response_job`). The room worker either **holds** the room until it finishes or
**releases** the room the moment the task detaches, per the profile's `comms` mode
(`ReloadableState::comms_mode_for_profile`):

- `sync` — the worker awaits the task; subsequent turns in the room **wait**.
- `async` — the worker detaches immediately and advances to the next turn while the
  task streams its reply in the background.
- `auto` — `select!` the task against `sleep(sync_budget_ms)`; on timeout it detaches
  and the room advances.

So **sync blocks the room; background work releases it.** No mode posts a placeholder
— the typing indicator is the only "working" signal until the reply appears.

Agentic (subprocess) turns additionally serialize per room via a per-room **agentic
slot** (`Semaphore(1)`): a turn acquires the permit (waiting, FIFO, behind any running
agentic job) and moves it into the job, which holds it for its whole lifetime — so a
backgrounded coding job keeps the slot until done and the next agentic turn queues,
while non-agentic chats still flow past it. Now that jobs run in isolated worktrees,
this is about ordering, not workdir safety.

Background jobs live in a `Workers` registry (`src/workers.rs`) in `BotCtx`: a job
id → handle map with abort handles, powering `/status` (active count), `/jobs`
(list, with room), `/cancel <id>` (abort → kill the subprocess tree), and a soft-cap
warning (`soft_worker_cap`). Flush cadence comes from the reloadable
`CommsConfig::edit_debounce_ms`.
