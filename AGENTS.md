# AGENTS.md

Rust Matrix bot that responds to messages in allowlisted rooms using configurable LLM backends.

## Workflow

Commit directly on `master` and keep going — no feature branches or PRs unless
asked. Build + test before each commit. Push when a unit of work is done.

## Key source files

| File | Purpose |
|------|---------|
| `src/main.rs` | Entry point, wires all components together |
| `src/config.rs` | Config loading: profiles.toml + backends.<HOST_ROLE>.toml + .env |
| `src/llm.rs` | `Backend` enum (HTTP / subprocess) + `ProfileLlm` fallback chains |
| `src/subprocess.rs` | Agentic subprocess backends (claude-code, opencode): spawn, JSON parse, lifecycle |
| `src/workers.rs` | Background-job registry (sync/async/auto), `/jobs` + `/cancel` |
| `src/history.rs` | Per-room conversation history, JSON-backed on disk |
| `src/audio.rs` | Speaches/Whisper audio transcription client |
| `src/matrix/client.rs` | Matrix client build + session persistence |
| `src/matrix/handler.rs` | Event handlers: invite, message, ack+edit flow |

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

- `config/profiles.toml` — committed: LLM profiles, comms budgets, projects, rooms
- `config/backends.<HOST_ROLE>.toml` — **gitignored**: backend kinds, URLs, api_key_env names
- `.env` — **gitignored**: Matrix credentials + gateway key

Never commit `.env` or `backends.*.toml` (except `backends.example.toml`).

## Message / streaming flow

1. Typing indicator sent — the only "working" signal; no placeholder in any mode.
2. The response pipeline runs as one self-contained task; the handler awaits it
   (sync), detaches it (async), or promotes it past the sync budget (auto). Output
   is flushed (first post, then in-place `m.replace` edits) on sentence boundaries
   or a debounce ceiling.
3. See `docs/architecture.md` → Response UX and Orchestrator for the full flow.

## Adding a new backend kind

Add a variant to `BackendKind` in `src/config.rs` (kebab-case serde name), then
handle it in `Config::build_client` (→ `llm::Backend`); for a subprocess kind, add
its arg-building and output parser in `src/subprocess.rs`.
