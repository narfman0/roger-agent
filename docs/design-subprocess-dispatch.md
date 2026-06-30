# Design: Subprocess dispatch (task 4.5 / Phase 3 #7)

Make `kind = "claude-code"` and `kind = "opencode"` backends actually spawn the
respective CLI as a subprocess and stream its output, instead of being silently
treated as OpenAI HTTP (which is what `Config::build_client` does today — it
ignores `kind` entirely).

## Why this is more than two enum arms

Every backend so far is a *stateless HTTP chat completion*: hand it
`&[ChatMessage]`, get streamed text back on `mpsc::Sender<String>`, roger owns the
history and the tool loop. `claude`/`opencode` invert all three:

| Concern        | HTTP backend (today)              | Subprocess backend (claude/opencode)          |
|----------------|-----------------------------------|-----------------------------------------------|
| **Lifetime**   | one request, seconds              | a *process*, seconds→minutes                  |
| **State**      | stateless; roger replays history  | stateful session (`--session-id`/`--resume`)  |
| **Tools**      | roger's `ToolExecutor` loop       | the agent runs its *own* loop (file/bash/web) |
| **Wire format**| OpenAI SSE                        | newline-delimited JSON events (own schema)    |
| **Failure**    | HTTP status                       | exit code + error events + stderr             |

This is the first backend with a process lifecycle and an agentic, self-tooling
execution model. That mismatch — not the wiring — is the design work. It is also
why `CommsConfig` (`idle_timeout_ms`, `absolute_ceiling_ms`,
`max_concurrent_children`, `soft_worker_cap`) and per-profile `comms`/
`latency_class` exist in `config.rs` but are currently unused: they were scaffolded
for exactly this.

## Architecture: enum, not trait

`ProfileLlm` holds `Vec<Arc<LlmClient>>` where `LlmClient` is concrete HTTP. To let
a fallback chain mix kinds (config already does: `reason = local gemma HTTP →
cloud-heavy claude-code`), introduce a backend sum type and make `ProfileLlm` hold
`Vec<Arc<Backend>>`:

```rust
enum Backend {
    Http(LlmClient),              // existing OpenAI-compatible client, unchanged
    Subprocess(SubprocessBackend) // claude-code and opencode
}

struct SubprocessBackend {
    flavor: SubprocessKind,       // ClaudeCode | OpenCode  → arg-building + parsing
    program: String,              // "claude" | "opencode"
    model: String,
    base_url: String,             // → ANTHROPIC_BASE_URL (gateway)
    auth_token: Option<String>,   // → ANTHROPIC_AUTH_TOKEN
    workdir: PathBuf,             // sandbox — NOT roger's cwd (see Safety)
    limits: ProcLimits,           // idle/absolute timeouts, max-budget, max-turns
    allowed_tools: Vec<String>,
    permission_mode: String,
}
```

`Backend` exposes the same three methods (`chat`, `chat_stream`,
`chat_with_tools`) plus `model()`/`model_chain()` via `match`. `ProfileLlm`'s
fallback loop is unchanged — it already advances on `Err`, and a dead subprocess
(spawn failure / nonzero exit / timeout) is just another `Err`, so a claude-code
primary transparently fails over to an HTTP fallback.

Enum over `dyn Backend`+`async_trait` because: only 3 kinds, zero new deps, and it
matches the codebase's existing `BackendKind` enum-and-`match` style. claude and
opencode share one struct parameterized by `flavor` rather than two near-duplicates
— they differ only in flags, env, and event schema.

`Config::build_client` gains a `match backend.kind` that builds `Http(...)` or
`Subprocess(...)`. This is the one place the `kind` field finally gets read.

## Resolving the three mismatches

### 1. History → stateless-per-turn (defer session mapping)

For 4.5, **do not** use `--resume`/`--session-id`. Render roger's windowed history
into the prompt each turn, same as the HTTP path sees `&[ChatMessage]`. Reasons:

- Single source of truth stays roger's per-room history (survives SIGHUP, restart,
  `/clear`, and fallback to an HTTP backend that needs the same messages).
- claude/opencode sessions are keyed by cwd + an id roger would have to persist and
  reconcile — a second, divergent history store.

claude takes the prompt as an arg (`-p "<prompt>"`); the system prompt goes via
`--append-system-prompt`. opencode takes the message positionally. Fold the
transcript into the prompt string (the budget already bounds it).

*Future (separate task):* map a roger room → a persistent agent session via
`--session-id`, so the coding agent keeps its working context across turns. Out of
scope here.

### 2. Tools → the subprocess owns its loop

claude/opencode run their own agentic tool loops. roger's `ToolExecutor`
(`web_search`/`web_fetch`) is irrelevant to them, so `chat_with_tools` on a
subprocess backend ignores the executor and behaves like `chat_stream`. This is
also the *point* of these backends — they can do what roger's HTTP tools can't
(read/edit files, run bash), which is what the coding room wants.

### 3. Wire format → parse events into the existing accumulator contract

The handler consumes **accumulated** text snapshots (`tx.send(full.clone())`) and
flushes on sentence/time boundaries. Each subprocess backend parses its native
event stream, accumulates assistant text, and sends the running total — structurally
identical to `chat_stream_raw`.

**claude** (`--output-format stream-json --verbose --include-partial-messages`,
verified present in 2.1.196): newline-delimited JSON. Accumulate
`type=="stream_event"` events where `event.delta.type=="text_delta"` →
`event.delta.text`. Take the terminal `type=="result"` event's `result` field as
the authoritative final string. Treat `system/api_retry` and nonzero exit as
errors (→ fallback chain). *The exact `result`-event shape is the one unverified
detail; capture one real run to pin field names before coding the parser.*

**opencode** (`run --format json`, verified present): same accumulate-and-send
loop over its JSON event stream; its schema differs, so it needs its own small
parser keyed off `flavor`.

If `--include-partial-messages` proves flaky, degrade to `--output-format json`
(single final result) → the non-streaming `chat()` path: no incremental edits, just
one final message. Acceptable fallback.

## Process lifecycle (this is where CommsConfig becomes real)

- **Spawn:** `tokio::process::Command`, `.kill_on_drop(true)`,
  `.process_group(0)` so the *whole* tree dies (these agents spawn children).
  stdout piped and read line-by-line; stderr captured for error reporting.
- **Concurrency:** a process-wide `tokio::sync::Semaphore` with
  `max_concurrent_children` permits. Acquire before spawn. On contention: await
  briefly, else reply "busy, try again." HTTP backends are unaffected.
- **Idle timeout:** wrap each `lines.next_line()` in `tokio::time::timeout(idle)`
  using per-profile `idle_timeout_ms` (else the `comms` default). No output for
  that long → kill + `Err`.
- **Absolute ceiling:** a wall-clock `absolute_ceiling_ms` guard around the whole
  run → hard kill.
- **Cost guard:** pass `--max-budget-usd` (claude) so a runaway agent can't burn
  the gateway budget unbounded.
- **Cancellation:** if the Matrix request is abandoned, the spawned tokio task
  drops, `kill_on_drop` reaps the tree.

## Auth & environment

Map the existing backend config to the CLI's gateway env vars — no Anthropic key
ever leaves `srv`:

- `backend.base_url` (`http://srv:4000`) → `ANTHROPIC_BASE_URL`
- `backend.api_key()` (`GATEWAY_VKEY`) → `ANTHROPIC_AUTH_TOKEN`
- `backend.model` → `--model`

Recommend `--bare`: skip hooks/plugins/CLAUDE.md/MCP/local config for reproducible,
fast, sandboxed runs (it also forces explicit credentials, which we supply). Caveat:
bare mode restricts default tools — confirm the allowed set and pass `--allowedTools`
explicitly.

## Safety (the real risk)

An autonomous file-editing, bash-running agent driven by **arbitrary Matrix
messages** is a remote code execution surface. Non-negotiables:

- **Never run in roger's cwd.** Spawn in a dedicated per-room (or per-job) workspace
  dir under e.g. `roger_session/workspaces/<room>/`, passed via `--add-dir` / cwd.
  Otherwise the bot can edit its own source or read `.env`.
- **Permission mode:** non-interactive can't prompt. `default` will refuse edits;
  a useful coding agent needs `acceptEdits` (edits only) or `bypassPermissions`
  (everything). That choice *is* the autonomy/risk dial — see open decisions.
- Ideally isolate further (dedicated unix user or container) before granting bash.

## Decisions (locked)

1. **Autonomy: full agentic** — file edits + bash, `--permission-mode acceptEdits`,
   in a sandboxed workspace dir. This is the coding-room point.
2. **Sandbox: dedicated workdir** — per-room dir under
   `roger_session/workspaces/<room>/`, cwd-isolated from roger's source and `.env`.
   Runs as roger's unix user (no separate user/container for now). **Floor
   requirement: the workdir must never be roger's cwd.** Accepting that a rogue
   `bash` still runs with roger's privileges — revisit (user/container) before this
   is exposed to untrusted rooms.
3. **Async comms is in scope** — 4.5 includes the sync→async promotion and a
   background-worker registry, not just the dispatcher.

**Still deferred:** session-mapped continuity (`--session-id` per room); a full
`/jobs` UI; surfacing tool-step progress ("🔧 editing auth.rs…"). A *minimal*
cancel path is in scope (a 30-min runaway must be killable).

## The orchestrator: sync / async / auto

This is the second half of 4.5 and the part worth getting right. The `comms` field
on each profile selects the mode; `CommsConfig` supplies the budgets.

### Invariant that makes it tractable

Make the **entire response pipeline a single self-contained task** — produce
(LLM/subprocess) → consume (stream → Matrix edits) → fallback → metrics → append to
history → final render → typing off. The task is correct whether or not anyone is
awaiting it. The handler's *only* job is to decide **await it (sync) or detach it
(async)**. History append and final render happen inside the task, so a detached
job still persists its result and updates its message on completion.

### Mode resolution

- **`sync`** — handler awaits the task to completion. Exactly today's UX (typing
  indicator, no placeholder, streamed edits).
- **`async`** — the task posts an immediate "🛠️ Working…" message as its anchor,
  the handler registers it and returns. The task streams edits into that anchor and
  the user is free to send other messages meanwhile.
- **`auto`** — `tokio::select!` the task against a `sleep(sync_budget_ms)` (7s):
  - task finishes first → sync UX, nothing detached.
  - budget fires first → **promote**: post a "still working — I'll update here"
    note, register the task, return. The task keeps streaming edits.

`code` is `async` (expects minutes); `reason` is `auto` (usually fast, occasionally
long); `chat`/`fast` are `sync`.

### Worker registry

A process-wide structure (lives in `BotCtx`, alongside `metrics`):

```rust
struct Workers {
    children: Semaphore,                 // max_concurrent_children — subprocess cap
    jobs: Mutex<HashMap<JobId, JobHandle>>,
    soft_cap: usize,                     // soft_worker_cap — total background jobs
}
struct JobHandle { room: String, started: Instant, model: String, abort: AbortHandle }
```

- A subprocess acquires a `children` permit before spawn; over the cap → queue
  briefly, else reply "busy." HTTP jobs don't take a child permit.
- `soft_cap` bounds *total* detached jobs (incl. promoted HTTP ones); over it →
  warn but allow (soft).
- Each job removes itself from `jobs` on completion. `abort` (from
  `tokio::spawn`'s `AbortHandle`) lets a future `/cancel` drop the task → its
  `kill_on_drop` child tree dies.
- `/status` gains an active-job line now; `/cancel <id>` (and a fuller `/jobs`)
  build on this registry.

### Concurrency & coherence notes

- matrix-sdk dispatches each event on its own task, so a detached 30-min job never
  blocks the sync loop or other rooms — the registry is about *accounting and
  control*, not unblocking the runtime.
- A new message arriving mid-job is answered independently (its own task/profile).
  The job appends its final assistant turn to history when it finishes; interleaved
  messages land in between. Acceptable for the coding-job use case.
- Move flush cadence (`MIN_FLUSH_GAP_MS`/`MAX_FLUSH_WAIT_MS`) and budgets out of
  `const`s and into the reloadable `CommsConfig` (`edit_debounce_ms`,
  `sync_budget_ms`, `idle_timeout_ms`, `absolute_ceiling_ms`).

## Implementation plan (ordered, each a commit on master)

1. **4.5.1 — Backend enum refactor, no behavior change.** Introduce
   `enum Backend { Http(LlmClient) }` (single arm for now), make `ProfileLlm` hold
   `Vec<Arc<Backend>>`, delegate `chat`/`chat_stream`/`chat_with_tools`/`model*`
   via `match`. Existing tests stay green. De-risks the central type change in
   isolation.
2. **4.5.2 — Subprocess backend (sync path).** Add the `Subprocess` arm +
   `SubprocessBackend`; `build_client` dispatches on `kind`. Implement the claude
   stream-json parser (capture one real `claude -p` run first to lock the `result`
   event shape), then the opencode `--format json` parser. Lifecycle: child
   semaphore, idle/absolute timeouts, `--max-budget-usd`, `kill_on_drop` +
   process-group kill. Gateway env (`ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN`),
   sandbox workdir, `acceptEdits`. Verify inline (sync) against the `code` profile.
3. **4.5.3 — Orchestrator (async/auto).** Extract the self-contained response task;
   add the `Workers` registry to `BotCtx`; wire `CommsConfig` into flush cadence and
   budgets; implement sync/async/auto resolution + promotion; `/status` job count +
   minimal `/cancel`.
