# AGENTS.md

Rust Matrix-native agent/orchestrator: it answers in allowlisted rooms via
configurable LLM backends (HTTP + agentic claude-code/opencode subprocesses),
routes per room, injects operating instructions + durable memory, auto-compacts
history, delegates to named subagents, exposes MCP + skills, and isolates agentic
edits in git worktrees.

## Workflow

Commit directly on `master` and keep going — no feature branches or PRs unless
asked. Build + test before each commit. Push when a unit of work is done.

## Key source files

| File | Purpose |
|------|---------|
| `src/main.rs` | Entry point; resolves `~/.roger` state dir, wires everything |
| `src/config.rs` | Config loading + all `[section]` structs; `build_client` (kind dispatch) |
| `src/llm.rs` | `Backend` enum (HTTP / subprocess) + `ProfileLlm` fallback chains |
| `src/subprocess.rs` | Agentic subprocess backends (claude-code, opencode) + worktree isolation |
| `src/matrix/handler.rs` | Per-room FIFO workers, orchestrator (sync/async/auto), subagents, slash commands |
| `src/workers.rs` | Background-job registry, `/jobs` + `/cancel` |
| `src/tools.rs` | Native tools + dynamic registry (`SubagentHost`, MCP, skills) |
| `src/mcp.rs` | MCP client manager (`rmcp`): connect servers, route `mcp__*` tools |
| `src/memory.rs` / `src/compaction.rs` | Two-tier durable memory; size-triggered compaction |
| `src/skills.rs` | Reusable skills (committed + learned, approval-gated) |
| `src/history.rs` | Per-room history (JSON) + atomic rewrite + per-room lock |
| `src/room_workdirs.rs` | Per-room agentic workdir selections (`set_workdir`) |
| `src/audio.rs` | Speaches/Whisper audio transcription client |
| `src/matrix/client.rs` | Matrix client build + session persistence |

## Build and test

```bash
cargo build
cargo test
```

Requires: Rust 2021 edition, matrix-sdk 0.8, tokio.

## Running locally

Copy `.env.example` (or see README) and `config/backends.example.toml` to `config/backends.local.toml`. Then:

```bash
RUST_LOG=roger=info ./target/debug/roger
```

State lives in `ROGER_STATE_DIR` (default `~/.roger`), **not** the repo: SQLite
crypto store, `session.json` (Matrix tokens), `history/` (per-room JSON), `logs/`
(daily-rotated JSON). The working directory only needs `config/`. Agentic jobs run
in a separate project directory, never roger's state or repo.

## Hot-reload

`SIGHUP` reloads `config/` live (LLM clients, system prompt, per-room settings,
comms config) via `Arc<RwLock<ReloadableState>>` — no Matrix re-login. `kill -HUP
<pid>` or `systemctl --user reload roger`. Credentials, homeserver, room allowlist,
and the subprocess concurrency cap need a restart. See `docs/architecture.md` →
Config hot-reload.

## Logging

`init_logging` (`src/main.rs`): human-readable to stderr + JSON daily-rotated to
`ROGER_LOG_DIR` (default `<state dir>/logs/`). `RUST_LOG` gates both.

## Config

- `config/profiles.toml` — committed: profiles, comms, `[mcp]`, `[worktrees]`,
  `[agents]`, `[context]`, `[memory]`, `[compaction]`, `[projects]`, rooms
- `config/backends.<HOST_ROLE>.toml` — **gitignored**: backend kinds, URLs, api_key_env names
- `config/operating.md`, `config/skills/` — committed operating instructions + seed skills
- `.env` — **gitignored**: Matrix credentials + gateway key

Never commit `.env` or `backends.*.toml` (except `backends.example.toml`).

## Message / streaming flow

1. `handle_message` is a thin entry: resolve body (text/audio), answer control slash
   commands immediately, else **enqueue** to the room's serial FIFO worker.
2. The worker runs one turn at a time via `process_turn`; the response pipeline is
   one self-contained task the worker **holds** (sync) or **releases** (async /
   auto-promoted). Output flushes (first post, then `m.replace` edits) on sentence
   boundaries or a debounce ceiling; no placeholder in any mode.
3. See `docs/architecture.md` → Orchestrator (per-room queue) for the full flow.

## Adding a new backend kind

Add a variant to `BackendKind` in `src/config.rs` (kebab-case serde name), then
handle it in `Config::build_client` (→ `llm::Backend`); for a subprocess kind, add
its arg-building and output parser in `src/subprocess.rs`.
